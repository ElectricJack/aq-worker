//! Linux guardian endpoint, peer identity and subreaper capability checks.
use anyhow::{Context, Result, bail, ensure};
use std::{
    io::{Read, Write},
    os::{
        fd::{AsRawFd, FromRawFd, OwnedFd},
        linux::net::SocketAddrExt,
        unix::net::{SocketAddr, UnixStream},
    },
    time::Duration,
};
use uuid::Uuid;
const MAX_CONTROL: usize = 16 * 1024;

fn socket_name(id: Uuid) -> String {
    format!("aq-worker-guardian-{}-{id}", unsafe { libc::geteuid() })
}
/// The abstract endpoint for this effective user and launch.
pub fn socket_address(id: Uuid) -> Result<SocketAddr> {
    Ok(SocketAddr::from_abstract_name(socket_name(id).as_bytes())?)
}
/// Connect without blocking on a full listen backlog, then switch to timed stream I/O.
pub fn connect_control(id: Uuid) -> Result<UnixStream> {
    // Nonblocking connect refuses a full local backlog instead of blocking the
    // controller. Subsequent reads/writes have absolute exchange deadlines.
    let fd = unsafe {
        libc::socket(
            libc::AF_UNIX,
            libc::SOCK_STREAM | libc::SOCK_CLOEXEC | libc::SOCK_NONBLOCK,
            0,
        )
    };
    ensure!(
        fd >= 0,
        "guardian socket: {}",
        std::io::Error::last_os_error()
    );
    let fd = unsafe { OwnedFd::from_raw_fd(fd) };
    let mut address: libc::sockaddr_un = unsafe { std::mem::zeroed() };
    address.sun_family = libc::AF_UNIX as _;
    let name = socket_name(id);
    ensure!(
        name.len() < address.sun_path.len(),
        "guardian socket name too long"
    );
    for (target, source) in address.sun_path[1..].iter_mut().zip(name.bytes()) {
        *target = source as _;
    }
    let length =
        (std::mem::offset_of!(libc::sockaddr_un, sun_path) + 1 + name.len()) as libc::socklen_t;
    ensure!(
        unsafe {
            libc::connect(
                fd.as_raw_fd(),
                (&address as *const libc::sockaddr_un).cast(),
                length,
            )
        } == 0,
        "guardian connect refused/busy: {}",
        std::io::Error::last_os_error()
    );
    let stream = UnixStream::from(fd);
    stream.set_nonblocking(false)?;
    Ok(stream)
}
/// Read at most 16 KiB up to EOF within a single exchange deadline.
pub fn read_bounded(stream: &mut UnixStream, budget: Duration) -> Result<Vec<u8>> {
    let end = std::time::Instant::now() + budget;
    let mut bytes = vec![];
    let mut chunk = [0u8; 1024];
    loop {
        let remaining = end
            .checked_duration_since(std::time::Instant::now())
            .context("guardian total read deadline")?;
        stream.set_read_timeout(Some(remaining))?;
        match stream.read(&mut chunk) {
            Ok(0) => return Ok(bytes),
            Ok(n) => {
                ensure!(bytes.len() + n <= MAX_CONTROL, "oversized guardian control");
                bytes.extend_from_slice(&chunk[..n]);
            }
            Err(error) if error.kind() == std::io::ErrorKind::Interrupted => {}
            Err(error) => return Err(error.into()),
        }
    }
}
/// Write the complete payload within a single exchange deadline.
pub fn write_bounded(stream: &mut UnixStream, bytes: &[u8], budget: Duration) -> Result<()> {
    let end = std::time::Instant::now() + budget;
    let mut remaining_bytes = bytes;
    while !remaining_bytes.is_empty() {
        let remaining = end
            .checked_duration_since(std::time::Instant::now())
            .context("guardian total write deadline")?;
        stream.set_write_timeout(Some(remaining))?;
        match stream.write(remaining_bytes) {
            Ok(0) => bail!("guardian control short write"),
            Ok(n) => remaining_bytes = &remaining_bytes[n..],
            Err(error) if error.kind() == std::io::ErrorKind::Interrupted => {}
            Err(error) => return Err(error.into()),
        }
    }
    Ok(())
}
/// Return the kernel-reported peer PID after checking its effective UID.
pub fn peer_pid(stream: &UnixStream) -> Result<u32> {
    let mut value = libc::ucred {
        pid: 0,
        uid: 0,
        gid: 0,
    };
    let mut length = std::mem::size_of::<libc::ucred>() as libc::socklen_t;
    // SAFETY: valid ucred buffer and length, owned Unix socket descriptor.
    let result = unsafe {
        libc::getsockopt(
            stream.as_raw_fd(),
            libc::SOL_SOCKET,
            libc::SO_PEERCRED,
            (&mut value as *mut libc::ucred).cast(),
            &mut length,
        )
    };
    ensure!(
        result == 0 && value.uid == unsafe { libc::geteuid() },
        "control peer UID mismatch"
    );
    Ok(value.pid as u32)
}
/// Probe subreaping and ECHILD in a short-lived child without changing the caller’s role.
pub fn probe_subreaper() -> Result<()> {
    // Only async-signal-safe syscalls run in this post-fork probe child.
    let child = unsafe { libc::fork() };
    ensure!(child >= 0, "subreaper probe fork failed");
    if child == 0 {
        probe_child();
    }
    let mut status = 0;
    ensure!(
        unsafe { libc::waitpid(child, &mut status, 0) } == child
            && libc::WIFEXITED(status)
            && libc::WEXITSTATUS(status) == 0,
        "unsupported ownership profile: subreaper unavailable"
    );
    Ok(())
}
// This child only calls async-signal-safe syscalls.
fn probe_child() -> ! {
    let mut enabled = 0;
    let ok = unsafe {
        libc::prctl(libc::PR_SET_CHILD_SUBREAPER, 1, 0, 0, 0) == 0
            && libc::prctl(libc::PR_GET_CHILD_SUBREAPER, &mut enabled, 0, 0, 0) == 0
            && enabled == 1
    };
    let mut status = 0;
    let wall_ok = unsafe {
        libc::waitpid(-1, &mut status, libc::WNOHANG | libc::__WALL) == -1
            && *libc::__errno_location() == libc::ECHILD
    };
    let ok = ok && wall_ok;
    unsafe { libc::_exit(if ok { 0 } else { 41 }) }
}
/// Identify the current machine boot and PID namespace.
pub fn host_identity() -> Result<(String, String)> {
    Ok((
        std::fs::read_to_string("/proc/sys/kernel/random/boot_id")?
            .trim()
            .into(),
        std::fs::read_link("/proc/self/ns/pid")?
            .to_string_lossy()
            .into_owned(),
    ))
}
