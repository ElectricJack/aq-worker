//! Reusable Linux tmux hosting. The caller owns authorization and durable task
//! state; a dedicated helper owns every tmux/pane descendant and permit expiry.
#[cfg(feature = "test-support")]
pub mod capsule;
mod command_io;
use command_io::{bounded_command, bounded_command_input};
mod host_plan;
mod launch;
use launch::LaunchRequest;
mod local_ipc;
use host_plan::{Adapter, LaunchDocument};
pub use host_plan::{HostAdapter, HostInput, HostPlan, HostStartup, HostSubject};
mod private_inputs;
pub use private_inputs::reconciliation::{
    LoginRefreshAck, LoginRefreshPhase, LoginRefreshRequest, LoginRefreshStatus, LoginWriteBack,
};
pub use private_inputs::{
    OriginalPrivateMaterial, PrivateCleanup, PrivateLaunchInput, ScopedCodexAuthBinding,
};
pub mod lost_guardian;
pub mod observed;
pub mod ownership;
mod reader_lease;
mod reaper;
mod renewal;
mod state_io;
pub use state_io::durable_new;
use state_io::{durable_replace, read_bytes, read_json};
mod supervisor;
pub use lost_guardian::{StopUnproven, recover_stopped};
pub mod terminal_io;
pub mod terminal_output;
pub mod terminal_viewer;
use anyhow::{Context, Result, anyhow, bail, ensure};
use aq_session_protocol::{
    ArtifactRef, Capability, Digest, LaunchManifest, SessionIdentity, Validate, canonical_digest,
    require_capabilities,
};
pub use ownership::ProcessEvidence;
pub use reaper::MAX_LOCAL_GUARDIANS;
use serde::{Deserialize, Serialize};
use std::{
    collections::BTreeMap,
    fs::{File, OpenOptions},
    io::{Read, Write},
    os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt, PermissionsExt},
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
    time::{Duration, Instant},
};
use uuid::Uuid;
pub mod users;
pub const HOST_PROFILE: &str = "linux-subreaper-tmux-foreground-v1";
/// Handshake timing, not an attempt limit: the longest window of one host
/// permit that is not interactive. Such a permit is reissued before it ends
/// for as long as the server keeps renewing the lease, so this bounds how
/// stale a permit may get, never how long the work runs.
pub const MAX_PERMIT_MS: u64 = 120000;
const MAX_LOCAL_FILE: u64 = 2 * 1024 * 1024;
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ExecutionPermit {
    pub session: SessionIdentity,
    pub manifest_digest: Digest,
    pub sequence: u64,
    pub issued_boot_ms: u64,
    pub expires_boot_ms: u64,
}
impl ExecutionPermit {
    pub fn validate_at(&self, session: &SessionIdentity, digest: &Digest, now: u64) -> Result<()> {
        self.validate_at_scoped(session, digest, now, false)
    }
    fn validate_at_scoped(
        &self,
        session: &SessionIdentity,
        digest: &Digest,
        now: u64,
        interactive: bool,
    ) -> Result<()> {
        self.session.validate()?;
        ensure!(
            &self.session == session && &self.manifest_digest == digest,
            "permit identity/digest mismatch"
        );
        ensure!(
            self.sequence > 0
                && self.expires_boot_ms > self.issued_boot_ms
                // The interactive window is the lease the server granted: no cap here.
                && (interactive || self.expires_boot_ms - self.issued_boot_ms <= MAX_PERMIT_MS)
                && now >= self.issued_boot_ms
                && now < self.expires_boot_ms,
            "expired/invalid execution permit"
        );
        Ok(())
    }
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LocalCommand {
    pub executable: PathBuf,
    pub arguments: Vec<String>,
    pub environment: BTreeMap<String, String>,
}
impl LocalCommand {
    fn validate(&self) -> Result<()> {
        ensure!(
            self.executable.is_absolute() && self.executable.is_file(),
            "harness executable unavailable"
        );
        ensure!(
            self.arguments.len() <= 128 && self.environment.len() <= 32,
            "local command cardinality"
        );
        ensure!(
            self.arguments
                .iter()
                .all(|v| v.len() <= 4096 && !v.contains('\0')),
            "local argument bounds"
        );
        ensure!(
            self.environment
                .iter()
                .all(|(name, value)| Self::allowed_environment(name, value)),
            "local environment bounds/protected key"
        );
        Ok(())
    }
    fn allowed_environment(name: &str, value: &str) -> bool {
        let valid_name = !name.is_empty()
            && name.len() <= 128
            && name.bytes().all(|c| c.is_ascii_alphanumeric() || c == b'_');
        let protected = matches!(
            name,
            "TMUX" | "TMUX_PANE" | "LD_PRELOAD" | "LD_LIBRARY_PATH" | "AQ_SESSION_DIRECTORY"
        );
        valid_name && value.len() <= 4096 && !value.contains('\0') && !protected
    }
}
#[derive(Clone, Debug)]
pub struct AdapterLaunch {
    /// Installed environment qualification; absent means no hard resource enforcement claim.
    pub resource_qualification: Option<ArtifactRef>,
    pub manifest_digest: Digest,
    pub adapter: ArtifactRef,
    pub cli_version: String,
    pub environment: ArtifactRef,
    pub capabilities: Vec<Capability>,
    pub command: LocalCommand,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HarnessReadiness {
    Unknown,
    Ready,
    Exited,
}
/// Exact local transcript metadata from this launched harness. The adapter
/// obtains the native key/member from its launch, never a provider-home search.
#[derive(Clone, Debug)]
pub struct TranscriptSource {
    pub launch: LaunchIntent,
    pub root: PathBuf,
    pub member: String,
    pub native_session_key: aq_session_protocol::Text,
    pub format_id: String,
    pub provider: aq_session_protocol::Text,
    pub account_reference: aq_session_protocol::Text,
}
/// Trusted, version-pinned adapters construct native argv/config. They must
/// refuse unsupported model, scopes, context or hard budgets; no fallback.
pub trait HarnessAdapter {
    fn prepare(&self, manifest: &LaunchManifest, workspace: &Path) -> Result<AdapterLaunch>;
    fn readiness(&self, observation: &HostObservation) -> Result<HarnessReadiness>;
    /// Called only with the exact host launch identity after launch. None is
    /// explicit unavailable observation support, never permission to discover.
    fn transcript_source(&self, _: &LaunchIntent) -> Result<Option<TranscriptSource>> {
        Ok(None)
    }
    /// Optional closed private input; bytes remain outside serialized commands.
    fn private_input(&self, _: &LaunchManifest, _: &Path) -> Result<Option<PrivateLaunchInput>> {
        Ok(None)
    }
}
#[derive(Clone, Debug)]
pub struct ContextMaterial {
    pub artifact: ArtifactRef,
    pub path: PathBuf,
}
/// Exact final payload supplied by a trusted runtime after validating its full
/// published startup descriptor. The host checks the pin and literal bytes; it
/// does not independently qualify renderer/tokenizer semantics or issue authority.
#[derive(Clone, Debug)]
pub struct RenderedStartup {
    manifest: LaunchManifest,
    binding: RenderedBinding,
    bytes: Vec<u8>,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct RenderedBinding {
    descriptor: ArtifactRef,
    payload: ArtifactRef,
    bytes: u64,
}
impl RenderedStartup {
    /// Borrow the exact retained final payload after checking the caller's
    /// expected manifest/descriptor/payload. No constituent reconstruction.
    pub fn verified_payload(
        &self,
        manifest: &LaunchManifest,
        descriptor: &ArtifactRef,
        payload: &ArtifactRef,
    ) -> Result<&[u8]> {
        ensure!(
            canonical_digest(manifest)? == canonical_digest(&self.manifest)?
                && descriptor == &self.binding.descriptor
                && payload == &self.binding.payload,
            "retained startup identity changed"
        );
        self.binding.verify(manifest, &self.bytes)?;
        Ok(&self.bytes)
    }
    pub fn new(
        manifest: LaunchManifest,
        descriptor: ArtifactRef,
        payload: ArtifactRef,
        bytes: Vec<u8>,
    ) -> Result<Self> {
        let binding = RenderedBinding {
            descriptor,
            payload,
            bytes: bytes.len() as u64,
        };
        binding.verify(&manifest, &bytes)?;
        Ok(Self {
            manifest,
            binding,
            bytes,
        })
    }
}
impl RenderedBinding {
    fn verify(&self, manifest: &LaunchManifest, bytes: &[u8]) -> Result<()> {
        manifest.validate()?;
        for reference in [&self.descriptor, &self.payload] {
            reference.id.validate()?;
            reference.version.validate()?;
        }
        ensure!(
            self.matches_descriptor(manifest),
            "exact startup descriptor module required"
        );
        ensure!(
            self.matches_payload(manifest, bytes),
            "final startup payload bytes/budget mismatch"
        );
        Ok(())
    }
    fn matches_descriptor(&self, manifest: &LaunchManifest) -> bool {
        self.descriptor.version.as_str() == "1"
            && self.descriptor.id.as_str()
                == format!("aq.startup-delivery/{}", self.descriptor.digest.as_str())
            && manifest
                .modules
                .iter()
                .filter(|r| r.id.as_str().starts_with("aq.startup-delivery/"))
                .count()
                == 1
            && manifest.modules.contains(&self.descriptor)
    }
    fn matches_payload(&self, manifest: &LaunchManifest, bytes: &[u8]) -> bool {
        let within_budget = self.bytes > 0
            && self.bytes <= 1_048_576
            && self.bytes <= manifest.context.startup_byte_budget;
        within_budget
            && bytes.len() as u64 == self.bytes
            && Digest::of(bytes) == self.payload.digest
            && std::str::from_utf8(bytes).is_ok()
    }
}
enum StartupInput<'a> {
    Constituents(&'a LaunchManifest),
    Rendered(&'a RenderedStartup),
    Observed(&'a observed::ObservedStartup),
    Plan(&'a HostPlan, &'a [u8]),
}
/// Memory/CPU/process limits are enforced by the qualified environment, not by
/// this tmux host. Callers must choose admission-only or supply exact evidence.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(
    tag = "kind",
    content = "artifact",
    rename_all = "snake_case",
    deny_unknown_fields
)]
pub enum ResourcePolicy {
    CallerAdmission(ArtifactRef),
    RequireQualifiedEnvironment(ArtifactRef),
}
#[derive(Clone, Debug)]
pub struct LocalEnvironment {
    pub resource_policy: ResourcePolicy,
    pub workspace: PathBuf,
    pub helper_executable: PathBuf,
    pub tmux_executable: PathBuf,
    pub tmux_version: String,
    pub credential_profiles: Vec<ArtifactRef>,
    pub context_material: Vec<ContextMaterial>,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LaunchIntent {
    pub launch_id: Uuid,
    pub session: SessionIdentity,
    pub manifest_digest: Digest,
    pub configuration_digest: Digest,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GuardianIdentity {
    pub intent: LaunchIntent,
    pub nonce: Uuid,
    pub process: ProcessEvidence,
    pub machine_boot_id: String,
    pub pid_namespace: String,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HostPhase {
    Waiting,
    Starting,
    Running,
    Stopping,
    Stopped,
    StopUncertain,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PaneIdentity {
    pub server: ProcessEvidence,
    pub pane: ProcessEvidence,
    pub pane_id: String,
    pub session_name: String,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HostObservation {
    pub guardian: GuardianIdentity,
    pub phase: HostPhase,
    pub pane: Option<PaneIdentity>,
    pub permit_sequence: u64,
    pub permit_deadline_boot_ms: u64,
    pub observed_boot_ms: u64,
    pub reaped_count: u64,
    pub kernel_wait_echild: bool,
    pub root_status: Option<i32>,
    pub stop_reason: Option<String>,
}
/// These callbacks must return only after caller-owned state is durable.
/// Failure or caller death never grants permission to silently replay launch.
pub trait LaunchAuthority {
    fn record_launch_intent(&mut self, intent: &LaunchIntent) -> Result<()>;
    fn record_guardian(&mut self, identity: &GuardianIdentity) -> Result<()>;
    fn record_start_intent(
        &mut self,
        intent: &LaunchIntent,
        guardian: &GuardianIdentity,
    ) -> Result<()>;
    fn record_started(&mut self, observation: &HostObservation) -> Result<()>;
    fn checkpoint(&mut self, _stage: &str) -> Result<()> {
        Ok(())
    }
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct SealedLaunch {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    agent_identity: Option<users::AgentIdentity>,
    launch_id: Uuid,
    manifest: LaunchDocument,
    permit: ExecutionPermit,
    workspace: PathBuf,
    helper: PathBuf,
    tmux: PathBuf,
    tmux_version: String,
    command: LocalCommand,
    resource_policy: ResourcePolicy,
    resource_qualification: Option<ArtifactRef>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    rendered_startup: Option<RenderedBinding>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    observed: Option<observed::Spec>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    private_input: Option<private_inputs::PrivateInputRecord>,
}
impl SealedLaunch {
    fn validate_permit_at(
        &self,
        permit: &ExecutionPermit,
        session: &SessionIdentity,
        digest: &Digest,
        now: u64,
    ) -> Result<()> {
        if let LaunchDocument::Plan(plan) = &self.manifest {
            return plan.validate_permit_at(permit, supervisor::machine()?.0.parse()?, now);
        }
        let interactive = self.manifest.terminal().input_enabled
            && self
                .observed
                .as_ref()
                .is_some_and(observed::Spec::interactive);
        permit.validate_at_scoped(session, digest, now, interactive)
    }
    fn wall_deadline(&self) -> Result<u64> {
        if let LaunchDocument::Plan(plan) = &self.manifest {
            return Ok(plan.deadline_boot_ms);
        }
        let wall = self
            .permit
            .issued_boot_ms
            .checked_add(self.manifest.resources().wall_time_ms)
            .context("wall deadline overflow")?;
        Ok(self
            .observed
            .as_ref()
            .map_or(wall, |spec| wall.min(spec.hard_deadline_boot_ms)))
    }
    fn intent(&self) -> Result<LaunchIntent> {
        Ok(LaunchIntent {
            launch_id: self.launch_id,
            session: self.manifest.session().clone(),
            manifest_digest: canonical_digest(&self.manifest)?,
            configuration_digest: Digest::of(&serde_json::to_vec(self)?),
        })
    }
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct BoundRequest {
    guardian: GuardianIdentity,
    permit: ExecutionPermit,
}
/// Opaque evidence obtained only by inspecting an exact guardian's durable
/// ECHILD receipt. It is separate from all legacy worker stop-proof types.
#[derive(Clone, Debug, Serialize)]
pub struct StoppedProof {
    observation: HostObservation,
}
impl StoppedProof {
    pub fn guardian(&self) -> &GuardianIdentity {
        &self.observation.guardian
    }
    pub fn observation(&self) -> &HostObservation {
        &self.observation
    }
}
struct RenewalLock {
    file: File,
    pid: u32,
}
impl RenewalLock {
    fn acquire(path: &Path) -> Result<Self> {
        use std::os::fd::AsRawFd;
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
            .open(path)?;
        ensure!(
            file.metadata()?.is_file(),
            "renewal lock must be regular file"
        );
        ensure!(
            unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } == 0,
            "renewal admission busy"
        );
        Ok(Self {
            file,
            pid: std::process::id(),
        })
    }
}
impl Drop for RenewalLock {
    fn drop(&mut self) {
        use std::os::fd::AsRawFd;
        if self.pid == std::process::id() {
            let _ = unsafe { libc::flock(self.file.as_raw_fd(), libc::LOCK_UN) };
        }
    }
}
pub struct SessionHost;
pub struct SessionHandle {
    directory: PathBuf,
    identity: GuardianIdentity,
    private_source: Option<File>,
}
impl SessionHost {
    pub fn launch(
        root: &Path,
        launch_id: Uuid,
        manifest: &LaunchManifest,
        permit: ExecutionPermit,
        local: &LocalEnvironment,
        adapter: &impl HarnessAdapter,
        authority: &mut impl LaunchAuthority,
    ) -> Result<SessionHandle> {
        LaunchRequest {
            root,
            launch_id,
            startup: StartupInput::Constituents(manifest),
            permit,
            local,
            agent_identity: None,
        }
        .start(Adapter::Task(adapter), authority)
        .map(|(handle, _)| handle)
    }
    /// Stage one already-rendered payload unchanged. The caller must validate
    /// the complete descriptor and retained inputs before constructing startup.
    pub fn launch_rendered(
        root: &Path,
        launch_id: Uuid,
        startup: &RenderedStartup,
        permit: ExecutionPermit,
        local: &LocalEnvironment,
        adapter: &impl HarnessAdapter,
        authority: &mut impl LaunchAuthority,
    ) -> Result<SessionHandle> {
        LaunchRequest {
            root,
            launch_id,
            startup: StartupInput::Rendered(startup),
            permit,
            local,
            agent_identity: None,
        }
        .start(Adapter::Task(adapter), authority)
        .map(|(handle, _)| handle)
    }
    /// Sandbox-only launch: seal the agent identity with the exact command.
    #[allow(clippy::too_many_arguments)]
    pub fn launch_as_agent(
        root: &Path,
        launch_id: Uuid,
        manifest: &LaunchManifest,
        permit: ExecutionPermit,
        local: &LocalEnvironment,
        adapter: &impl HarnessAdapter,
        authority: &mut impl LaunchAuthority,
        agent: users::AgentIdentity,
    ) -> Result<SessionHandle> {
        agent.validate_workspace(&local.workspace)?;
        LaunchRequest {
            root,
            launch_id,
            startup: StartupInput::Constituents(manifest),
            permit,
            local,
            agent_identity: Some(agent),
        }
        .start(Adapter::Task(adapter), authority)
        .map(|(handle, _)| handle)
    }
    /// Recovery is stop-and-preserve only; it never creates a server or pane.
    pub fn recover(root: &Path, expected: &LaunchIntent) -> Result<SessionHandle> {
        let directory = std::fs::canonicalize(root)?.join(expected.launch_id.to_string());
        let config: SealedLaunch = read_json(&directory.join("launch.json"))?;
        ensure!(
            config.intent()? == *expected,
            "recovery launch binding mismatch"
        );
        let identity: GuardianIdentity = read_json(&directory.join("identity.json"))?;
        ensure!(identity.intent == *expected, "recovery guardian mismatch");
        let handle = SessionHandle {
            directory,
            identity,
            private_source: None,
        };
        handle.stop()?;
        Ok(handle)
    }
    /// Explicit relay observation of an already-running exact physical guardian.
    /// This neither resumes a stopped host nor calls record_started or renews a
    /// permit. Legacy recovery retains its stop-and-preserve behavior above.
    pub fn observe_existing(root: &Path, expected: &GuardianIdentity) -> Result<SessionHandle> {
        let directory = std::fs::canonicalize(root)?.join(expected.intent.launch_id.to_string());
        let _directory_guard = terminal_io::DirectoryGuard::existing(&directory)?;
        let config: SealedLaunch = read_json(&directory.join("launch.json"))?;
        ensure!(
            config.intent()? == expected.intent,
            "observed host launch mismatch"
        );
        let identity: GuardianIdentity = read_json(&directory.join("identity.json"))?;
        ensure!(identity == *expected, "observed physical guardian mismatch");
        let handle = SessionHandle {
            directory,
            identity,
            private_source: None,
        };
        let state = handle.observe()?;
        let wall_deadline = config.wall_deadline()?;
        ensure!(
            state.phase == HostPhase::Running
                && ownership::raw_boot_ms() < state.permit_deadline_boot_ms
                && ownership::raw_boot_ms() < wall_deadline
                && !handle.directory.join("stop.request").exists(),
            "observation requires unexpired running host"
        );
        let pane = state.pane.as_ref().context("observed host pane missing")?;
        for process in [&pane.server, &pane.pane] {
            ensure!(
                ownership::pidfd_is_live(&ownership::verified_pidfd(process)?)?,
                "observed host process no longer live"
            );
        }
        Ok(handle)
    }
}
impl SessionHandle {
    pub fn directory(&self) -> &Path {
        &self.directory
    }
    pub fn identity(&self) -> &GuardianIdentity {
        &self.identity
    }
    pub fn observe(&self) -> Result<HostObservation> {
        let mut s: HostObservation = read_json(&self.directory.join("state.json"))?;
        ensure!(
            s.guardian == self.identity,
            "guardian state identity mismatch"
        );
        if s.phase != HostPhase::Stopped && !supervisor::guardian_live(&self.identity) {
            // The guardian may have durably saved ECHILD after our first read
            // and exited before liveness checking. Read the final receipt again.
            s = read_json(&self.directory.join("state.json"))?;
            ensure!(
                s.guardian == self.identity,
                "final guardian state identity mismatch"
            );
            if s.phase != HostPhase::Stopped {
                s.phase = HostPhase::StopUncertain;
                s.stop_reason = Some("guardian lost; stopped writers unproven".into());
            }
        }
        if s.phase == HostPhase::Stopped {
            ensure!(s.kernel_wait_echild, "missing kernel stopped proof");
        }
        Ok(s)
    }
    fn request(&self, name: &str, permit: &ExecutionPermit) -> Result<()> {
        let request = BoundRequest {
            guardian: self.identity.clone(),
            permit: permit.clone(),
        };
        let path = self.directory.join(name);
        if path.exists() {
            let old: BoundRequest = read_json(&path)?;
            ensure!(
                old.guardian == request.guardian && old.permit == request.permit,
                "changed request replay"
            );
            return Ok(());
        }
        durable_new(&path, &serde_json::to_vec(&request)?)
    }
    pub fn stopped_proof(&self) -> Result<StoppedProof> {
        let observation = self.observe()?;
        ensure!(
            observation.phase == HostPhase::Stopped && observation.kernel_wait_echild,
            "no exact kernel stopped proof"
        );
        Ok(StoppedProof { observation })
    }
    pub fn stop(&self) -> Result<()> {
        let state = self.observe()?;
        if state.phase == HostPhase::Stopped {
            return Ok(());
        }
        ensure!(
            state.phase != HostPhase::StopUncertain,
            "guardian lost; stop remains uncertain"
        );
        let config: SealedLaunch = read_json(&self.directory.join("launch.json"))?;
        self.request("stop.request", &config.permit)
    }
    pub fn wait_stopped(&mut self, timeout: Duration) -> Result<HostObservation> {
        ensure!(timeout <= Duration::from_secs(10), "stop wait bounds");
        let end = Instant::now() + timeout;
        loop {
            let s = self.observe()?;
            if s.phase == HostPhase::Stopped {
                let reaped_deadline = Instant::now() + Duration::from_millis(250);
                while supervisor::guardian_live(&self.identity) {
                    ensure!(
                        Instant::now() < reaped_deadline,
                        "guardian has proof but did not exit within bounded wait"
                    );
                    std::thread::sleep(Duration::from_millis(2));
                }
                return Ok(s);
            }
            ensure!(
                s.phase != HostPhase::StopUncertain,
                "guardian lost; no stopped-writer proof"
            );
            ensure!(Instant::now() < end, "stop proof timeout");
            std::thread::sleep(Duration::from_millis(5));
        }
    }
    /// Development text inspection only; not a full terminal-state snapshot.
    /// -N forbids starting a missing server, and no pane creation command exists.
    pub fn capture_text(&self) -> Result<String> {
        let state = self.observe()?;
        ensure!(
            state.phase == HostPhase::Running,
            "live capture unavailable; inspect separately retained output"
        );
        let config: SealedLaunch = read_json(&self.directory.join("launch.json"))?;
        let pane = state.pane.context("pane not started")?;
        let output = tmux(
            &config,
            &self.directory,
            &["capture-pane", "-p", "-t", &pane.pane_id],
        )?;
        ensure!(output.len() <= 65536, "capture text limit");
        Ok(String::from_utf8_lossy(&output).into_owned())
    }
}
fn tmux(config: &SealedLaunch, directory: &Path, args: &[&str]) -> Result<Vec<u8>> {
    bounded_command(
        Command::new(&config.tmux)
            .arg("-N")
            .arg("-S")
            .arg(directory.join("tmux.sock"))
            .args(args),
    )
    .map_err(|error| anyhow!("{error} (tmux {})", args.first().unwrap_or(&"")))
}
/// Helper entrypoint must run in a dedicated single-threaded executable.
pub fn helper_main() -> Result<()> {
    let args: Vec<_> = std::env::args_os().skip(1).collect();
    ensure!(args.len() >= 2, "helper needs mode and launch directory");
    let mode = args[0].to_str().context("helper mode")?;
    let directory = Path::new(&args[1]);
    match mode {
        "guardian" => supervisor::run(directory),
        "pane" => supervisor::exec_pane(directory),
        #[cfg(feature = "test-support")]
        "fixture" => supervisor::fixture(
            directory,
            args.get(2)
                .and_then(|s| s.to_str())
                .unwrap_or("interactive"),
        ),
        _ => bail!("unknown helper mode"),
    }
}

#[cfg(test)]
mod tests;

#[cfg(test)]
mod io_tests;

#[cfg(test)]
mod rendered_tests;
