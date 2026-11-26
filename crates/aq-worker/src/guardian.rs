//! Linux ownership profile: trusted per-launch single-threaded subreaper.
//! /proc child lists discover targets; only waitpid(__WALL)==ECHILD certifies
//! termination. Lost guardian without its durable proof remains uncertain.
use anyhow::{Context, Result, bail, ensure};
use aq_session_host::ownership::ProcessEvidence;
use aq_worker_protocol::{AttemptRef, MAX_GRANT_MS, NodeIdentity, ProcessOutcome};
use serde::{Deserialize, Serialize};
use std::{
    fs::{File, OpenOptions},
    io::Write,
    os::{
        fd::OwnedFd,
        unix::{
            fs::OpenOptionsExt,
            net::{UnixListener, UnixStream},
        },
    },
    path::{Path, PathBuf},
    process::{Child, Command},
    time::Duration,
};
use uuid::Uuid;

pub const PROFILE: &str = "linux-subreaper-guardian-v1";
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
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workspace: Option<PathBuf>,
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
        write_new_file(&temporary, bytes)?;
        std::fs::rename(&temporary, path)?;
        File::open(parent)?.sync_all()?;
        Ok(())
    })();
    let _ = std::fs::remove_file(temporary);
    result
}
pub fn durable_new(path: &Path, bytes: &[u8]) -> Result<()> {
    write_new_file(path, bytes)?;
    File::open(path.parent().unwrap())?.sync_all()?;
    Ok(())
}
fn write_new_file(path: &Path, bytes: &[u8]) -> Result<()> {
    let mut f = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(path)?;
    f.write_all(bytes)?;
    f.sync_all()?;
    Ok(())
}
use aq_session_host::ownership::{
    guardian::{
        connect_control, host_identity, peer_pid, probe_subreaper, read_bounded, socket_address,
        write_bounded,
    },
    pidfd, signal,
};
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
    probe_subreaper()
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
use aq_session_host::ownership::signal_children;

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
mod runtime;
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
    let identity = guardian_identity(&binding)?;
    let state = waiting_state(&binding, identity);
    save(directory, &state)?;
    durable_new(
        &directory.join("guardian-identity.json"),
        &serde_json::to_vec(&state.identity)?,
    )?;
    let _emergency_reaper = EmergencyReaper;
    runtime::Guardian::new(directory, binding, state).serve(listener, &token)
}
fn guardian_identity(binding: &LaunchBinding) -> Result<GuardianIdentity> {
    let (host, namespace) = host_identity()?;
    Ok(GuardianIdentity {
        launch_id: binding.launch_id,
        node_identity: binding.identity.clone(),
        attempt: binding.attempt.clone(),
        spec_digest: binding.spec_digest.clone(),
        nonce: Uuid::new_v4(),
        process: crate::fixture::process_evidence(std::process::id())?,
        machine_boot_id: host,
        pid_namespace: namespace,
        profile: PROFILE.into(),
    })
}
fn waiting_state(binding: &LaunchBinding, identity: GuardianIdentity) -> GuardianState {
    GuardianState {
        identity,
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
    }
}
