//! Linux ownership profile: trusted per-launch single-threaded subreaper.
//! /proc child lists discover targets; only waitpid(__WALL)==ECHILD certifies
//! termination. Lost guardian without its durable proof remains uncertain.
use crate::journal::ProcessEvidence;
use anyhow::{Context, Result, bail, ensure};
use aq_worker_protocol::{AttemptRef, MAX_GRANT_MS, NodeIdentity, ProcessOutcome};
use serde::{Deserialize, Serialize};
use std::{
    fs::{File, OpenOptions},
    io::{Read, Write},
    os::{
        fd::{AsRawFd, FromRawFd, OwnedFd},
        linux::net::SocketAddrExt,
        unix::{
            fs::OpenOptionsExt,
            net::{SocketAddr, UnixListener, UnixStream},
        },
    },
    path::{Path, PathBuf},
    process::{Child, Command},
    time::Duration,
};
use uuid::Uuid;

pub const PROFILE: &str = "linux-subreaper-guardian-v1";
const MAX_CONTROL: usize = 16 * 1024;
const TERM_GRACE_MS: u64 = 100;
const STOP_LIMIT_MS: u64 = 5_000;
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct GuardianIdentity {
    pub launch_id: Uuid,
    pub node_identity: NodeIdentity,
    pub attempt: AttemptRef,
    pub spec_digest: String,
    pub nonce: Uuid,
    pub process: ProcessEvidence,
    pub machine_boot_id: String,
    pub pid_namespace: String,
    pub profile: String,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LaunchBinding {
    pub launch_id: Uuid,
    pub identity: NodeIdentity,
    pub attempt: AttemptRef,
    pub spec_digest: String,
    pub executable: PathBuf,
    pub initial_grant_sequence: u64,
    pub initial_deadline_ms: u64,
    pub logical_origin_ms: u64,
    pub system_origin_ms: u64,
    pub clock_offset_file: Option<PathBuf>,
    pub clock_offset_origin: u64,
}
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum GuardianPhase {
    Waiting,
    Running,
    Stopping,
    Stopped,
    StopUncertain,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GuardianState {
    pub identity: GuardianIdentity,
    pub phase: GuardianPhase,
    pub root_process: Option<ProcessEvidence>,
    pub root_status: Option<i32>,
    pub grant_sequence: u64,
    pub deadline_ms: u64,
    pub stop_reason: Option<ProcessOutcome>,
    pub reaped_count: u64,
    pub kernel_wait_echild: bool,
    pub stopped_at_ms: Option<u64>,
    pub observed_at_ms: u64,
    pub clock_uncertain: bool,
}
#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "command", rename_all = "snake_case", deny_unknown_fields)]
pub enum Operation {
    Status,
    Start {
        grant_sequence: u64,
        deadline_ms: u64,
    },
    Renew {
        grant_sequence: u64,
        deadline_ms: u64,
    },
    Stop {
        reason: ProcessOutcome,
    },
}
#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Control {
    token: String,
    identity: GuardianIdentity,
    operation: Operation,
}
#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Reply {
    state: GuardianState,
    error: Option<String>,
}

pub fn durable_replace(path: &Path, bytes: &[u8]) -> Result<()> {
    let parent = path.parent().context("state parent")?;
    let temporary = parent.join(format!(".guardian.{}.tmp", Uuid::new_v4()));
    let result = (|| -> Result<()> {
        let mut f = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&temporary)?;
        f.write_all(bytes)?;
        f.sync_all()?;
        std::fs::rename(&temporary, path)?;
        File::open(parent)?.sync_all()?;
        Ok(())
    })();
    let _ = std::fs::remove_file(temporary);
    result
}
pub fn durable_new(path: &Path, bytes: &[u8]) -> Result<()> {
    let mut f = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(path)?;
    f.write_all(bytes)?;
    f.sync_all()?;
    File::open(path.parent().unwrap())?.sync_all()?;
    Ok(())
}
fn socket_name(id: Uuid) -> String {
    format!("aq-worker-guardian-{}-{id}", unsafe { libc::geteuid() })
}
fn socket_address(id: Uuid) -> Result<SocketAddr> {
    Ok(SocketAddr::from_abstract_name(socket_name(id).as_bytes())?)
}
fn connect_control(id: Uuid) -> Result<UnixStream> {
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
fn read_bounded(stream: &mut UnixStream, budget: Duration) -> Result<Vec<u8>> {
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
fn write_bounded(stream: &mut UnixStream, bytes: &[u8], budget: Duration) -> Result<()> {
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
fn peer_pid(stream: &UnixStream) -> Result<u32> {
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
fn pidfd(pid: u32) -> Result<OwnedFd> {
    // SAFETY: syscall accepts a PID and zero flags; successful descriptor is owned.
    let fd = unsafe { libc::syscall(libc::SYS_pidfd_open, pid, 0) } as i32;
    ensure!(
        fd >= 0,
        "pidfd_open unavailable: {}",
        std::io::Error::last_os_error()
    );
    Ok(unsafe { OwnedFd::from_raw_fd(fd) })
}
fn signal(fd: &OwnedFd, sig: i32) -> Result<()> {
    // SAFETY: owned pidfd and null siginfo are the kernel-supported signal API.
    let rc = unsafe {
        libc::syscall(
            libc::SYS_pidfd_send_signal,
            fd.as_raw_fd(),
            sig,
            std::ptr::null::<libc::siginfo_t>(),
            0,
        )
    };
    if rc < 0 && std::io::Error::last_os_error().raw_os_error() != Some(libc::ESRCH) {
        bail!("pidfd signal: {}", std::io::Error::last_os_error());
    }
    Ok(())
}
pub fn probe_profile() -> Result<()> {
    ensure!(
        cfg!(target_os = "linux"),
        "unsupported ownership profile: Linux required"
    );
    let self_fd = pidfd(std::process::id())?;
    signal(&self_fd, 0)?;
    std::fs::read_to_string(format!("/proc/self/task/{}/children", std::process::id()))
        .context("procfs child discovery unavailable")?;
    let endpoint = Uuid::new_v4();
    let listener = UnixListener::bind_addr(&socket_address(endpoint)?)?;
    listener.set_nonblocking(true)?;
    let client = connect_control(endpoint)?;
    let (server, _) = listener.accept()?;
    ensure!(
        peer_pid(&client)? == std::process::id() && peer_pid(&server)? == std::process::id(),
        "Unix peer credential probe failed"
    );
    ensure!(
        std::env::var_os("AQ_WORKER_DISABLE_GUARDIAN").is_none(),
        "unsupported ownership profile: guardian disabled locally"
    );
    // Only async-signal-safe syscalls run in this post-fork probe child.
    let child = unsafe { libc::fork() };
    ensure!(child >= 0, "subreaper probe fork failed");
    if child == 0 {
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
    let mut status = 0;
    ensure!(
        unsafe { libc::waitpid(child, &mut status, 0) } == child
            && libc::WIFEXITED(status)
            && libc::WEXITSTATUS(status) == 0,
        "unsupported ownership profile: subreaper unavailable"
    );
    Ok(())
}
fn host_identity() -> Result<(String, String)> {
    Ok((
        std::fs::read_to_string("/proc/sys/kernel/random/boot_id")?
            .trim()
            .into(),
        std::fs::read_link("/proc/self/ns/pid")?
            .to_string_lossy()
            .into_owned(),
    ))
}
pub(crate) fn validate_identity(identity: &GuardianIdentity) -> Result<OwnedFd> {
    ensure!(identity.profile == PROFILE, "unsupported guardian profile");
    let (host, namespace) = host_identity()?;
    ensure!(
        identity.machine_boot_id == host && identity.pid_namespace == namespace,
        "guardian host/namespace mismatch"
    );
    let fd = pidfd(identity.process.pid)?;
    ensure!(
        crate::fixture::process_evidence(identity.process.pid)? == identity.process,
        "guardian PID identity changed"
    );
    Ok(fd)
}
pub fn read_identity(directory: &Path) -> Result<GuardianIdentity> {
    Ok(serde_json::from_slice(&std::fs::read(
        directory.join("guardian-identity.json"),
    )?)?)
}
pub fn read_state(directory: &Path, identity: &GuardianIdentity) -> Result<GuardianState> {
    let state: GuardianState =
        serde_json::from_slice(&std::fs::read(directory.join("guardian-state.json"))?)?;
    ensure!(
        state.identity == *identity,
        "guardian receipt identity mismatch"
    );
    if state.phase == GuardianPhase::Stopped {
        ensure!(
            state.kernel_wait_echild && state.stopped_at_ms.is_some(),
            "missing kernel stopped proof"
        );
    }
    Ok(state)
}
pub fn control(
    directory: &Path,
    identity: &GuardianIdentity,
    operation: Operation,
    after_send: impl FnOnce() -> Result<()>,
) -> Result<GuardianState> {
    let _live = validate_identity(identity)?;
    let mut stream = connect_control(identity.launch_id)?;
    ensure!(
        peer_pid(&stream)? == identity.process.pid,
        "guardian endpoint peer changed"
    );
    let token = std::fs::read_to_string(directory.join("control.token"))?;
    let bytes = serde_json::to_vec(&Control {
        token,
        identity: identity.clone(),
        operation,
    })?;
    write_bounded(&mut stream, &bytes, Duration::from_millis(100))?;
    stream.shutdown(std::net::Shutdown::Write)?;
    after_send()?;
    let bytes = read_bounded(&mut stream, Duration::from_millis(250))?;
    let reply: Reply = serde_json::from_slice(&bytes)?;
    ensure!(
        reply.state.identity == *identity,
        "guardian response identity changed"
    );
    if let Some(error) = reply.error {
        bail!("guardian refused control: {error}");
    }
    Ok(reply.state)
}
fn save(directory: &Path, state: &GuardianState) -> Result<()> {
    durable_replace(
        &directory.join("guardian-state.json"),
        &serde_json::to_vec(state)?,
    )
}
fn logical_now(binding: &LaunchBinding, previous: &mut u64) -> Result<u64> {
    let offset = match &binding.clock_offset_file {
        Some(path) => std::fs::read_to_string(path)?.trim().parse::<u64>()?,
        None => 0,
    };
    let now = binding
        .logical_origin_ms
        .saturating_add(crate::clock::raw_boot_ms().saturating_sub(binding.system_origin_ms))
        .saturating_add(
            offset
                .checked_sub(binding.clock_offset_origin)
                .context("guardian offset regressed")?,
        );
    ensure!(now >= *previous, "guardian clock regressed");
    *previous = now;
    Ok(now)
}
fn children() -> Result<Vec<u32>> {
    std::fs::read_to_string(format!("/proc/self/task/{}/children", std::process::id()))?
        .split_whitespace()
        .map(|pid| Ok(pid.parse()?))
        .collect()
}
fn reap(state: &mut GuardianState, root_pid: Option<u32>) -> Result<bool> {
    loop {
        let mut status = 0;
        // SAFETY: single-threaded guardian is sole reaper; __WALL includes clone
        // children. ECHILD is the kernel proof, never an empty /proc scan.
        let pid = unsafe { libc::waitpid(-1, &mut status, libc::WNOHANG | libc::__WALL) };
        if pid > 0 {
            state.reaped_count += 1;
            if Some(pid as u32) == root_pid {
                state.root_status = Some(status);
            }
            continue;
        }
        if pid == 0 {
            return Ok(false);
        }
        let error = std::io::Error::last_os_error();
        if error.raw_os_error() == Some(libc::ECHILD) {
            return Ok(true);
        }
        if error.raw_os_error() == Some(libc::EINTR) {
            continue;
        }
        return Err(error.into());
    }
}
fn signal_children(sig: i32) -> Result<()> {
    // Reaping happens only outside this loop, so listed child PIDs cannot be
    // reused before pidfd_open. Descendant adoption can add entries; repeat.
    for child in children()? {
        match pidfd(child) {
            Ok(fd) => signal(&fd, sig)?,
            Err(error) => {
                if crate::fixture::process_evidence(child).is_ok() {
                    return Err(error);
                }
            }
        }
    }
    Ok(())
}

struct EmergencyReaper;
impl Drop for EmergencyReaper {
    fn drop(&mut self) {
        let end = crate::clock::raw_boot_ms().saturating_add(STOP_LIMIT_MS);
        loop {
            let mut status = 0;
            let pid = unsafe { libc::waitpid(-1, &mut status, libc::WNOHANG | libc::__WALL) };
            if pid > 0 {
                continue;
            }
            if pid < 0 && std::io::Error::last_os_error().raw_os_error() == Some(libc::ECHILD) {
                break;
            }
            let _ = signal_children(libc::SIGKILL);
            if crate::clock::raw_boot_ms() >= end {
                break;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
    }
}
pub fn run(directory: &Path) -> Result<()> {
    probe_profile()?;
    ensure!(
        std::process::id() != 1,
        "guardian cannot be PID-namespace init"
    );
    // SAFETY: changes this dedicated, single-threaded process before any fork.
    ensure!(
        unsafe { libc::prctl(libc::PR_SET_CHILD_SUBREAPER, 1, 0, 0, 0) } == 0,
        "subreaper unavailable"
    );
    let mut subreaper = 0;
    ensure!(
        unsafe { libc::prctl(libc::PR_GET_CHILD_SUBREAPER, &mut subreaper, 0, 0, 0) } == 0
            && subreaper == 1,
        "subreaper verification failed"
    );
    durable_new(
        &directory.join("guardian.claim"),
        b"one guardian per launch\n",
    )?;
    let binding: LaunchBinding =
        serde_json::from_slice(&std::fs::read(directory.join("guardian-launch.json"))?)?;
    let token = std::fs::read_to_string(directory.join("control.token"))?;
    ensure!(!token.is_empty(), "empty control token");
    let listener = UnixListener::bind_addr(&socket_address(binding.launch_id)?)?;
    listener.set_nonblocking(true)?;
    let (host, namespace) = host_identity()?;
    let identity = GuardianIdentity {
        launch_id: binding.launch_id,
        node_identity: binding.identity.clone(),
        attempt: binding.attempt.clone(),
        spec_digest: binding.spec_digest.clone(),
        nonce: Uuid::new_v4(),
        process: crate::fixture::process_evidence(std::process::id())?,
        machine_boot_id: host,
        pid_namespace: namespace,
        profile: PROFILE.into(),
    };
    let mut state = GuardianState {
        identity: identity.clone(),
        phase: GuardianPhase::Waiting,
        root_process: None,
        root_status: None,
        grant_sequence: binding.initial_grant_sequence,
        deadline_ms: binding.initial_deadline_ms,
        stop_reason: None,
        reaped_count: 0,
        kernel_wait_echild: false,
        stopped_at_ms: None,
        observed_at_ms: binding.logical_origin_ms,
        clock_uncertain: false,
    };
    save(directory, &state)?;
    durable_new(
        &directory.join("guardian-identity.json"),
        &serde_json::to_vec(&identity)?,
    )?;
    let _emergency_reaper = EmergencyReaper;
    let mut root_child: Option<Child> = None;
    let mut root_pid = None;
    let mut last_now = 0;
    let mut stop_started = None;
    loop {
        let now = match logical_now(&binding, &mut last_now) {
            Ok(now) => now,
            Err(_) => {
                state.stop_reason = Some(ProcessOutcome::Expired);
                state.phase = GuardianPhase::Stopping;
                state.clock_uncertain = true;
                last_now
            }
        };
        state.observed_at_ms = now;
        if now >= state.deadline_ms
            && !matches!(
                state.phase,
                GuardianPhase::Stopped | GuardianPhase::Stopping
            )
        {
            state.phase = GuardianPhase::Stopping;
            state.stop_reason = Some(ProcessOutcome::Expired);
            save(directory, &state)?;
        }
        if reap(&mut state, root_pid)? && state.phase != GuardianPhase::Waiting {
            state.phase = GuardianPhase::Stopped;
            state.kernel_wait_echild = true;
            state.stopped_at_ms = Some(now);
            save(directory, &state)?;
            return Ok(());
        }
        // A root exiting with surviving descendants is not task completion.
        // Stop the adopted leftovers before publishing any stopped proof.
        if state.root_status.is_some() && state.phase == GuardianPhase::Running {
            state.phase = GuardianPhase::Stopping;
            state.stop_reason = Some(ProcessOutcome::Failed);
            save(directory, &state)?;
        }
        if state.phase == GuardianPhase::Stopping {
            // Termination budgets use independent raw BOOTTIME even when the
            // authority/fixture clock is unreadable or has regressed.
            let raw_now = crate::clock::raw_boot_ms();
            let started = *stop_started.get_or_insert(raw_now);
            let sig = if raw_now.saturating_sub(started) < TERM_GRACE_MS {
                libc::SIGTERM
            } else {
                libc::SIGKILL
            };
            signal_children(sig)?;
            if raw_now.saturating_sub(started) >= STOP_LIMIT_MS {
                state.phase = GuardianPhase::StopUncertain;
                save(directory, &state)?;
                return Ok(());
            }
        }
        for _ in 0..8 {
            if state.phase == GuardianPhase::Stopping {
                break;
            }
            let fresh = logical_now(&binding, &mut last_now);
            if fresh.as_ref().map_or(true, |now| *now >= state.deadline_ms) {
                state.phase = GuardianPhase::Stopping;
                state.stop_reason = Some(ProcessOutcome::Expired);
                save(directory, &state)?;
                break;
            }
            if state.phase == GuardianPhase::Stopping {
                break;
            }
            match listener.accept() {
                Ok((mut stream, _)) => {
                    let read_ms = 20;
                    #[cfg(feature = "test-support")]
                    let read_ms = std::env::var("AQ_GUARDIAN_TEST_READ_MS")
                        .ok()
                        .map(|v| v.parse::<u64>().unwrap().clamp(1, 500))
                        .unwrap_or(read_ms);
                    stream.set_read_timeout(Some(Duration::from_millis(read_ms)))?;
                    #[cfg(feature = "test-support")]
                    if let Some(path) = std::env::var_os("AQ_GUARDIAN_TEST_READ_MARKER") {
                        durable_replace(
                            Path::new(&path),
                            b"reading control body
",
                        )?;
                    }
                    stream.set_write_timeout(Some(Duration::from_millis(20)))?;
                    let received = read_bounded(&mut stream, Duration::from_millis(read_ms));
                    let Ok(bytes) = received else {
                        continue;
                    };
                    if peer_pid(&stream).is_err() {
                        continue;
                    }
                    let Ok(request) = serde_json::from_slice::<Control>(&bytes) else {
                        continue;
                    };
                    if request.token != token || request.identity != identity {
                        continue;
                    }
                    let result = (|| -> Result<()> {
                        // A read may stall. Never validate Start/Renew against
                        // the stale timestamp from before receiving its bytes.
                        let now = logical_now(&binding, &mut last_now)?;
                        state.observed_at_ms = now;
                        match request.operation {
                            Operation::Status => {}
                            Operation::Stop { reason } => {
                                ensure!(
                                    matches!(
                                        reason,
                                        ProcessOutcome::Cancelled
                                            | ProcessOutcome::Expired
                                            | ProcessOutcome::Failed
                                    ),
                                    "invalid stop reason"
                                );
                                state.stop_reason = Some(reason);
                                state.phase = GuardianPhase::Stopping;
                                save(directory, &state)?;
                            }
                            Operation::Start {
                                grant_sequence,
                                deadline_ms,
                            } => {
                                ensure!(
                                    state.phase == GuardianPhase::Waiting && root_pid.is_none(),
                                    "launch already started or terminal"
                                );
                                ensure!(
                                    now < state.deadline_ms
                                        && now < deadline_ms
                                        && deadline_ms <= binding.initial_deadline_ms
                                        && grant_sequence == binding.initial_grant_sequence,
                                    "expired/mismatched start grant"
                                );
                                state.phase = GuardianPhase::Running;
                                state.deadline_ms = deadline_ms;
                                save(directory, &state)?;
                                let spawn_now = logical_now(&binding, &mut last_now)?;
                                ensure!(
                                    spawn_now < state.deadline_ms,
                                    "grant expired before writer spawn"
                                );
                                let child = Command::new(&binding.executable)
                                    .arg("--launch-dir")
                                    .arg(directory)
                                    .stdin(std::process::Stdio::null())
                                    .stdout(std::process::Stdio::null())
                                    .stderr(std::process::Stdio::null())
                                    .spawn()?;
                                root_pid = Some(child.id());
                                state.root_process =
                                    Some(crate::fixture::process_evidence(child.id())?);
                                root_child = Some(child);
                                save(directory, &state)?;
                            }
                            Operation::Renew {
                                grant_sequence,
                                deadline_ms,
                            } => {
                                ensure!(
                                    state.phase == GuardianPhase::Running
                                        && now < state.deadline_ms
                                        && now < deadline_ms
                                        && deadline_ms <= now.saturating_add(MAX_GRANT_MS)
                                        && grant_sequence > state.grant_sequence,
                                    "late/stale guardian renewal"
                                );
                                state.grant_sequence = grant_sequence;
                                state.deadline_ms = deadline_ms;
                                save(directory, &state)?;
                            }
                        }
                        Ok(())
                    })();
                    let reply = Reply {
                        state: state.clone(),
                        error: result.err().map(|e| e.to_string()),
                    };
                    let _ = write_bounded(
                        &mut stream,
                        &serde_json::to_vec(&reply)?,
                        Duration::from_millis(20),
                    );
                }
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => break,
                Err(error) => return Err(error.into()),
            }
        }
        // Keep Child alive for ownership metadata; all reaping uses waitpid.
        let _ = &root_child;
        std::thread::sleep(Duration::from_millis(5));
    }
}
