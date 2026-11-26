mod admission;
mod session;
use super::*;
use std::{mem::size_of, net::Ipv4Addr};

fn socket_option(fd: &OwnedFd, option: i32) -> Option<i32> {
    let mut value = 0i32;
    let mut length = size_of::<i32>() as libc::socklen_t;
    let result = unsafe {
        libc::getsockopt(
            fd.as_raw_fd(),
            libc::SOL_SOCKET,
            option,
            (&mut value as *mut i32).cast(),
            &mut length,
        )
    };
    (result == 0).then_some(value)
}

pub(super) fn listener(fd: OwnedFd, origin: &str) -> Result<TcpListener> {
    ensure!(
        socket_option(&fd, libc::SO_TYPE) == Some(libc::SOCK_STREAM),
        "capsule listener type"
    );
    ensure!(
        socket_option(&fd, libc::SO_ACCEPTCONN) == Some(1),
        "capsule listener state"
    );
    let listener = TcpListener::from(fd);
    let address = listener.local_addr()?;
    ensure!(
        address.ip() == std::net::IpAddr::V4(Ipv4Addr::LOCALHOST)
            && address.port() > 0
            && origin == format!("http://{address}"),
        "capsule listener exact origin"
    );
    listener.set_nonblocking(true)?;
    Ok(listener)
}
pub(super) fn verify_image(process: &ProcessEvidence, image: &seal::Pin) -> Result<()> {
    let pidfd = ownership::verified_pidfd(process)?;
    ensure!(
        ownership::pidfd_is_live(&pidfd)?,
        "capsule exec process exited"
    );
    let file = File::open(format!("/proc/{}/exe", process.pid))?;
    let metadata = file.metadata()?;
    let seals = unsafe { libc::fcntl(file.as_raw_fd(), libc::F_GET_SEALS) };
    ensure!(
        (metadata.dev(), metadata.ino(), metadata.len())
            == (image.device, image.inode, image.bytes)
            && seals >= 0
            && seals & seal::SEALS == seal::SEALS,
        "capsule exact actual sealed executable"
    );
    ensure!(
        ownership::process_evidence(process.pid)? == *process && ownership::pidfd_is_live(&pidfd)?,
        "capsule exec incarnation changed"
    );
    Ok(())
}
fn parent(pid: u32) -> Result<u32> {
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat"))?;
    Ok(stat
        .rsplit_once(") ")
        .context("capsule process stat")?
        .1
        .split_whitespace()
        .nth(1)
        .context("capsule parent")?
        .parse()?)
}
fn namespaces(pid: u32, inner: u32, init: u32) -> Result<()> {
    let status = std::fs::read_to_string(format!("/proc/{pid}/status"))?;
    let ids = status
        .lines()
        .find_map(|line| line.strip_prefix("NSpid:"))
        .context("capsule NSpid")?
        .split_whitespace()
        .map(str::parse::<u32>)
        .collect::<std::result::Result<Vec<_>, _>>()?;
    ensure!(
        (2..=32).contains(&ids.len()) && (ids.first(), ids.last()) == (Some(&pid), Some(&inner)),
        "capsule translated actual PID"
    );
    for kind in ["user", "mnt", "pid", "net", "ipc", "uts"] {
        let actual = std::fs::metadata(format!("/proc/{pid}/ns/{kind}"))?;
        let outer = std::fs::metadata(format!("/proc/thread-self/ns/{kind}"))?;
        let initial = std::fs::metadata(format!("/proc/{init}/ns/{kind}"))?;
        ensure!(
            actual.ino() != outer.ino() && actual.ino() == initial.ino(),
            "capsule namespace identity mismatch"
        );
    }
    Ok(())
}
fn cleanup(init: &OwnedFd) -> Result<u64> {
    ownership::signal(init, libc::SIGKILL)?;
    let end = ownership::raw_boot_ms() + CLEANUP_MS;
    let mut reaped = 0;
    loop {
        let batch = ownership::reap_children()?;
        reaped += batch.processes.len() as u64;
        if batch.kernel_echild {
            return Ok(reaped);
        }
        ensure!(
            ownership::raw_boot_ms() < end,
            "capsule physical cleanup uncertain"
        );
        ownership::signal_children_bounded(libc::SIGKILL)?;
        std::thread::sleep(Duration::from_millis(2));
    }
}
fn caller_still_waiting(control: &wire::Socket) -> Result<()> {
    let mut poll = libc::pollfd {
        fd: control.raw(),
        events: libc::POLLIN,
        revents: 0,
    };
    ensure!(
        unsafe { libc::poll(&mut poll, 1, 0) } >= 0 && poll.revents == 0,
        "capsule caller/control lost before exec witness"
    );
    Ok(())
}
pub(super) fn run(control: wire::Socket) -> Result<()> {
    let worker = unsafe { libc::getppid() };
    ensure!(
        unsafe { libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGKILL, 0, 0, 0) } == 0
            && unsafe { libc::getppid() } == worker,
        "capsule guardian parent fence"
    );
    ownership::become_subreaper()?;
    let _emergency = ownership::EmergencyReaper;
    let plan = receive_plan(&control, worker)?;
    let files = receive_files(&control, worker, &plan)?;
    let (inner, setup_socket) = wire::Socket::pair()?;
    let guardian_pid = std::process::id();
    let bootstrap = unsafe { libc::fork() };
    ensure!(bootstrap >= 0, "capsule setup fork");
    if bootstrap == 0 {
        drop(control);
        drop(inner);
        let result = setup::run(plan, files, setup_socket, guardian_pid);
        if let Err(error) = result {
            eprintln!("capsule trusted setup refused: {error:#}");
        }
        unsafe { libc::_exit(111) }
    }
    drop(setup_socket);
    drop(files);
    session::Session {
        control: &control,
        worker,
        plan: &plan,
    }
    .supervise(inner, bootstrap)
}

fn receive_plan(control: &wire::Socket, worker: i32) -> Result<PlanRecord> {
    let packet = control.recv::<Message>(ownership::raw_boot_ms() + MAX_LIFETIME_MS)?;
    ensure!(
        packet.credentials.pid == worker && packet.fd.is_none(),
        "capsule original caller"
    );
    let Message::Plan(plan) = packet.value else {
        bail!("capsule first message must be plan")
    };
    let now = ownership::raw_boot_ms();
    ensure!(
        admission::has_live_deadline(&plan, now),
        "capsule initial original deadline"
    );
    Ok(plan)
}

fn receive_files(control: &wire::Socket, worker: i32, plan: &PlanRecord) -> Result<Vec<File>> {
    let mut files = Vec::new();
    ensure!(plan.grants.len() <= 10, "capsule descriptor count");
    for index in 0..plan.grants.len() + 3 {
        let packet = control.recv::<Message>(plan.deadline)?;
        ensure!(
            packet.credentials.pid == worker
                && matches!(packet.value, Message::Descriptor { nonce, index: received } if nonce == plan.nonce && received == index),
            "capsule exact descriptor sequence"
        );
        files.push(File::from(
            packet.fd.context("capsule missing retained descriptor")?,
        ));
    }
    admission::validate(plan, &mut files)?;
    Ok(files)
}
fn finish_report(control: &wire::Socket, worker: i32, nonce: Uuid, report: Message) -> Result<()> {
    if control
        .send(&report, None, ownership::raw_boot_ms() + 100)
        .is_ok()
    {
        // Keep a late Stop from resetting a queued report when the endpoint
        // closes with unread data. ACK is delivery only: ECHILD is established.
        let until = ownership::raw_boot_ms() + 1000;
        while ownership::raw_boot_ms() < until {
            let Ok(packet) = control.recv::<Message>(until) else {
                break;
            };
            ensure!(
                packet.credentials.pid == worker && packet.fd.is_none(),
                "capsule stop ACK sender"
            );
            match packet.value {
                Message::StoppedAck { nonce: received } if received == nonce => break,
                Message::Stop { nonce: received } if received == nonce => {}
                _ => bail!("capsule stop ACK binding"),
            }
        }
    }
    Ok(())
}
