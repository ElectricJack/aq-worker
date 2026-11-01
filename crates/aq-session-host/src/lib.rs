//! Reusable Linux tmux hosting. The caller owns authorization and durable task
//! state; a dedicated helper owns every tmux/pane descendant and permit expiry.
#[cfg(feature = "test-support")]
pub mod capsule;
mod local_ipc;
mod private_inputs;
pub use private_inputs::{
    OriginalPrivateMaterial, PrivateCleanup, PrivateLaunchInput, ScopedCodexAuthBinding,
};
pub mod observed;
pub mod ownership;
mod reader_lease;
mod reaper;
mod supervisor;
pub mod terminal_io;
pub mod terminal_output;
use anyhow::{Context, Result, bail, ensure};
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
pub const HOST_PROFILE: &str = "linux-subreaper-tmux-foreground-v1";
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
        self.session.validate()?;
        ensure!(
            &self.session == session && &self.manifest_digest == digest,
            "permit identity/digest mismatch"
        );
        ensure!(
            self.sequence > 0
                && self.expires_boot_ms > self.issued_boot_ms
                && self.expires_boot_ms - self.issued_boot_ms <= MAX_PERMIT_MS
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
        for (k, v) in &self.environment {
            ensure!(
                !k.is_empty()
                    && k.len() <= 128
                    && k.bytes().all(|c| c.is_ascii_alphanumeric() || c == b'_')
                    && v.len() <= 4096
                    && !v.contains('\0')
                    && !matches!(
                        k.as_str(),
                        "TMUX"
                            | "TMUX_PANE"
                            | "LD_PRELOAD"
                            | "LD_LIBRARY_PATH"
                            | "AQ_SESSION_DIRECTORY"
                    ),
                "local environment bounds/protected key"
            );
        }
        Ok(())
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
/// Trusted, version-pinned adapters construct native argv/config. They must
/// refuse unsupported model, scopes, context or hard budgets; no fallback.
pub trait HarnessAdapter {
    fn prepare(&self, manifest: &LaunchManifest, workspace: &Path) -> Result<AdapterLaunch>;
    fn readiness(&self, observation: &HostObservation) -> Result<HarnessReadiness>;
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
            self.descriptor.version.as_str() == "1"
                && self.descriptor.id.as_str()
                    == format!("aq.startup-delivery/{}", self.descriptor.digest.as_str())
                && manifest
                    .modules
                    .iter()
                    .filter(|r| r.id.as_str().starts_with("aq.startup-delivery/"))
                    .count()
                    == 1
                && manifest.modules.contains(&self.descriptor),
            "exact startup descriptor module required"
        );
        ensure!(
            self.bytes > 0
                && self.bytes <= 1_048_576
                && self.bytes <= manifest.context.startup_byte_budget
                && bytes.len() as u64 == self.bytes
                && Digest::of(bytes) == self.payload.digest
                && std::str::from_utf8(bytes).is_ok(),
            "final startup payload bytes/budget mismatch"
        );
        Ok(())
    }
}
enum StartupInput<'a> {
    Constituents(&'a LaunchManifest),
    Rendered(&'a RenderedStartup),
    Observed(&'a observed::ObservedStartup),
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
    launch_id: Uuid,
    manifest: LaunchManifest,
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
    fn wall_deadline(&self) -> Result<u64> {
        let wall = self
            .permit
            .issued_boot_ms
            .checked_add(self.manifest.resources.wall_time_ms)
            .context("wall deadline overflow")?;
        Ok(self
            .observed
            .as_ref()
            .map_or(wall, |spec| wall.min(spec.hard_deadline_boot_ms)))
    }
    fn intent(&self) -> Result<LaunchIntent> {
        Ok(LaunchIntent {
            launch_id: self.launch_id,
            session: self.manifest.bindings.session.clone(),
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
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct RenewalLedger {
    latest: Option<BoundRequest>,
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
        Self::launch_inner(
            root,
            launch_id,
            StartupInput::Constituents(manifest),
            permit,
            local,
            adapter,
            authority,
        )
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
        Self::launch_inner(
            root,
            launch_id,
            StartupInput::Rendered(startup),
            permit,
            local,
            adapter,
            authority,
        )
        .map(|(handle, _)| handle)
    }
    fn launch_inner(
        root: &Path,
        launch_id: Uuid,
        startup: StartupInput<'_>,
        permit: ExecutionPermit,
        local: &LocalEnvironment,
        adapter: &impl HarnessAdapter,
        authority: &mut impl LaunchAuthority,
    ) -> Result<(SessionHandle, Option<observed::gate::Client>)> {
        let manifest = match &startup {
            StartupInput::Constituents(m) => *m,
            StartupInput::Rendered(r) => &r.manifest,
            StartupInput::Observed(r) => &r.rendered.manifest,
        };
        let reaper_slot = reaper::Slot::reserve()?;
        manifest.validate()?;
        ensure!(!launch_id.is_nil(), "nil launch ID");
        let digest = canonical_digest(manifest)?;
        permit.validate_at(
            &manifest.bindings.session,
            &digest,
            ownership::raw_boot_ms(),
        )?;
        ensure!(
            local.workspace.is_absolute()
                && local.workspace.is_dir()
                && std::fs::canonicalize(&local.workspace)? == local.workspace,
            "workspace must be canonical existing directory"
        );
        ensure!(
            local.helper_executable.is_absolute()
                && local.helper_executable.is_file()
                && local.tmux_executable.is_absolute()
                && local.tmux_executable.is_file(),
            "helper/tmux unavailable"
        );
        ensure!(
            local.tmux_version.len() <= 128 && !local.tmux_version.is_empty(),
            "tmux version bounds"
        );
        let version = bounded_command(Command::new(&local.tmux_executable).arg("-V"))?;
        ensure!(
            String::from_utf8(version)?.trim() == local.tmux_version,
            "tmux version mismatch"
        );
        for secret in &manifest.credentials {
            ensure!(
                local
                    .credential_profiles
                    .contains(&secret.credential_profile),
                "credential profile unavailable"
            );
        }
        let prepared = adapter.prepare(manifest, &local.workspace)?;
        ensure!(
            prepared.manifest_digest == digest
                && prepared.adapter == manifest.harness.adapter
                && prepared.cli_version == manifest.harness.cli_version.as_str()
                && prepared.environment == manifest.harness.environment,
            "adapter/CLI/environment mismatch"
        );
        require_capabilities(
            &manifest.harness.required_capabilities,
            &prepared.capabilities,
        )?;
        if let ResourcePolicy::RequireQualifiedEnvironment(required) = &local.resource_policy {
            ensure!(
                prepared.resource_qualification.as_ref() == Some(required),
                "hard resources require exact installed-environment qualification"
            );
        }
        prepared.command.validate()?;
        let private_input = adapter.private_input(manifest, &local.workspace)?;
        ensure!(
            local.context_material.len() <= 128,
            "context material bounds"
        );
        let rendered = match &startup {
            StartupInput::Rendered(r) => Some(*r),
            StartupInput::Observed(r) => Some(&r.rendered),
            _ => None,
        };
        if let StartupInput::Observed(observed) = &startup {
            ensure!(
                prepared.command.executable == observed.program.source(),
                "observed adapter executable mismatch"
            );
        }
        let (context, rendered_startup) = if let Some(rendered) = rendered {
            ensure!(
                local.context_material.is_empty(),
                "rendered startup cannot append constituent material"
            );
            rendered.binding.verify(manifest, &rendered.bytes)?;
            (vec![rendered.bytes.clone()], Some(rendered.binding.clone()))
        } else {
            let mut context = Vec::new();
            for reference in &manifest.context.references {
                if reference.delivery == aq_session_protocol::ContextDelivery::Startup {
                    let material = local
                        .context_material
                        .iter()
                        .find(|c| c.artifact == reference.artifact)
                        .context("startup context material unavailable")?;
                    let bytes = read_bytes(&material.path, 1048576)?;
                    ensure!(
                        bytes.len() as u64 == reference.bytes
                            && Digest::of(&bytes) == reference.artifact.digest,
                        "startup context digest/size mismatch"
                    );
                    context.push(bytes);
                }
            }
            (context, None)
        };
        std::fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(root)?;
        ensure!(
            !std::fs::symlink_metadata(root)?.file_type().is_symlink(),
            "host root symlink"
        );
        let metadata = std::fs::metadata(root)?;
        ensure!(
            metadata.uid() == unsafe { libc::geteuid() }
                && metadata.permissions().mode() & 0o077 == 0,
            "host root must be owner-only"
        );
        let root = std::fs::canonicalize(root)?;
        let directory = root.join(launch_id.to_string());
        std::fs::DirBuilder::new()
            .mode(0o700)
            .create(&directory)
            .context("launch directory already exists; never reuse")?;
        ensure!(
            directory.join("tmux.sock").as_os_str().len() < 104,
            "tmux socket path too long"
        );
        File::open(&root)?.sync_all()?;
        let publication = private_input
            .map(|input| private_inputs::Publication::reserve(&directory, input))
            .transpose()?;
        let config = SealedLaunch {
            launch_id,
            manifest: manifest.clone(),
            permit,
            workspace: local.workspace.clone(),
            helper: local.helper_executable.clone(),
            tmux: local.tmux_executable.clone(),
            tmux_version: local.tmux_version.clone(),
            command: prepared.command,
            resource_policy: local.resource_policy.clone(),
            resource_qualification: prepared.resource_qualification,
            rendered_startup,
            private_input: publication.as_ref().map(|p| p.record()),
            observed: match &startup {
                StartupInput::Observed(r) => Some(r.spec.clone()),
                _ => None,
            },
        };
        if let Some(spec) = &config.observed {
            spec.validate(&config)?;
        }
        if config.private_input.is_some() {
            ensure!(
                config.observed.as_ref().is_some_and(|s| s.native_exec())
                    && config.command.environment.get("CODEX_HOME")
                        == Some(
                            &directory
                                .join("codex-home")
                                .to_str()
                                .context("private home UTF8")?
                                .to_owned()
                        ),
                "private input requires exact native home"
            );
        }
        let intent = config.intent()?;
        durable_new(
            &directory.join("launch.json"),
            &serde_json::to_vec(&config)?,
        )?;
        for (index, bytes) in context.into_iter().enumerate() {
            durable_new(&directory.join(format!("context-{index}.bin")), &bytes)?;
        }
        authority.record_launch_intent(&intent)?;
        authority.checkpoint("launch-intent-durable")?;
        let private_source = publication.map(|p| p.publish()).transpose()?;
        private_inputs::verify_at_exec(&config, &directory)?;
        let mut command = Command::new(&config.helper);
        command
            .arg("guardian")
            .arg(&directory)
            .env_clear()
            .env("PATH", "/usr/bin:/bin")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        #[cfg(feature = "test-support")]
        if config.observed.is_some() {
            // Private synthetic-test bootstrap diagnostics; never enabled by a
            // production build and never attached to native harness stderr.
            command.stderr(
                OpenOptions::new()
                    .write(true)
                    .create_new(true)
                    .mode(0o600)
                    .open(directory.join("guardian-test-stderr.log"))?,
            );
        }
        #[cfg(feature = "test-support")]
        if let Some(stage) = std::env::var_os("AQ_SESSION_HOST_FAULT_STAGE") {
            command.env("AQ_SESSION_HOST_FAULT_STAGE", stage);
        }
        let child = command.spawn().context("guardian spawn uncertain")?;
        let guardian_fd = reaper_slot.track(child)?;
        authority.checkpoint("guardian-spawned")?;
        let end = Instant::now() + Duration::from_secs(3);
        let identity = loop {
            if let Ok(id) = read_json::<GuardianIdentity>(&directory.join("identity.json")) {
                ensure!(id.intent == intent, "guardian intent mismatch");
                break id;
            }
            ensure!(
                ownership::pidfd_is_live(&guardian_fd)?,
                "guardian exited before identity"
            );
            ensure!(Instant::now() < end, "guardian identity timeout");
            std::thread::sleep(Duration::from_millis(2));
        };
        let handle = SessionHandle {
            directory,
            identity,
            private_source,
        };
        authority.record_guardian(&handle.identity)?;
        authority.checkpoint("guardian-identity-durable")?;
        let observed_client = if let StartupInput::Observed(r) = &startup {
            Some(observed::gate::Client::admit(
                &handle.directory,
                &handle.identity,
                &r.program,
                config.permit.expires_boot_ms.min(config.wall_deadline()?),
            )?)
        } else {
            None
        };
        authority.record_start_intent(&intent, &handle.identity)?;
        authority.checkpoint("start-intent-durable")?;
        handle.request("start.request", &config.permit)?;
        authority.checkpoint("start-sent")?;
        let end = Instant::now() + Duration::from_secs(3);
        let observed = loop {
            let s = handle.observe()?;
            if let (Some(client), Some(spec)) = (&observed_client, &config.observed)
                && client
                    .closed(
                        spec.baseline_id,
                        config.permit.expires_boot_ms.min(config.wall_deadline()?),
                    )?
                    .is_some()
            {
                let current = handle.observe()?;
                ensure!(
                    current.phase == HostPhase::Starting,
                    "observed gate must remain Starting"
                );
                return Ok((handle, observed_client));
            }
            if s.phase == HostPhase::Running {
                break s;
            }
            ensure!(
                !matches!(s.phase, HostPhase::Stopped | HostPhase::StopUncertain),
                "session ended before startup: {:?}",
                s.stop_reason
            );
            ensure!(Instant::now() < end, "tmux startup timeout");
            std::thread::sleep(Duration::from_millis(2));
        };
        authority.record_started(&observed)?;
        authority.checkpoint("start-receipt-durable")?;
        Ok((handle, None))
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
    pub fn renew(&self, permit: &ExecutionPermit) -> Result<()> {
        let _lock = RenewalLock::acquire(&self.directory.join("renew.lock"))?;
        let state = self.observe()?;
        ensure!(
            state.phase == HostPhase::Running,
            "renewal requires live running session"
        );
        let now = ownership::raw_boot_ms();
        permit.validate_at(
            &self.identity.intent.session,
            &self.identity.intent.manifest_digest,
            now,
        )?;
        ensure!(
            now < state.permit_deadline_boot_ms,
            "expired current authority cannot renew"
        );
        let config: SealedLaunch = read_json(&self.directory.join("launch.json"))?;
        let wall = config.wall_deadline()?;
        ensure!(
            permit.expires_boot_ms <= wall && !self.directory.join("stop.request").exists(),
            "renewal exceeds wall limit or cancelled session"
        );
        let path = self.directory.join("renewals.json");
        let mut ledger: RenewalLedger = if path.exists() {
            read_json(&path)?
        } else {
            RenewalLedger { latest: None }
        };
        if let Some(old) = &ledger.latest {
            ensure!(
                old.guardian == self.identity,
                "renewal ledger guardian mismatch"
            );
        }
        if let Some(old) = ledger
            .latest
            .as_ref()
            .filter(|r| r.permit.sequence == permit.sequence)
        {
            ensure!(old.permit == *permit, "changed renewal sequence payload");
        } else {
            ensure!(
                permit.sequence > state.permit_sequence
                    && ledger
                        .latest
                        .as_ref()
                        .is_none_or(|r| permit.sequence > r.permit.sequence),
                "stale renewal sequence"
            );
            ledger.latest = Some(BoundRequest {
                guardian: self.identity.clone(),
                permit: permit.clone(),
            });
            durable_replace(&path, &serde_json::to_vec(&ledger)?)?;
        }
        // A crash after ledger fsync but before publication is retriable. Older
        // sequences refuse as stale and cannot overwrite newer authority.
        if permit.sequence > state.permit_sequence
            && ledger
                .latest
                .as_ref()
                .is_some_and(|r| r.permit.sequence == permit.sequence)
        {
            let pending = self.directory.join("renew.request");
            if pending.exists() {
                let old: BoundRequest = read_json(&pending)?;
                ensure!(
                    old.guardian == self.identity,
                    "pending renewal guardian mismatch"
                );
                if old.permit.sequence == permit.sequence {
                    ensure!(old.permit == *permit, "changed pending renewal payload");
                    return Ok(());
                }
                ensure!(
                    old.permit.sequence < permit.sequence,
                    "pending renewal ahead of ledger"
                );
            }
            durable_replace(
                &pending,
                &serde_json::to_vec(&BoundRequest {
                    guardian: self.identity.clone(),
                    permit: permit.clone(),
                })?,
            )?;
        }
        Ok(())
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
fn read_bytes(path: &Path, max: u64) -> Result<Vec<u8>> {
    let file = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(path)?;
    ensure!(
        file.metadata()?.file_type().is_file(),
        "local material/state must be regular file"
    );
    ensure!(file.metadata()?.len() <= max, "local file too large");
    let mut bytes = Vec::new();
    file.take(max + 1).read_to_end(&mut bytes)?;
    ensure!(bytes.len() as u64 <= max, "local file grew beyond bound");
    Ok(bytes)
}
fn read_json<T: serde::de::DeserializeOwned>(path: &Path) -> Result<T> {
    Ok(serde_json::from_slice(&read_bytes(path, MAX_LOCAL_FILE)?)?)
}
pub fn durable_new(path: &Path, bytes: &[u8]) -> Result<()> {
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(path)?;
    file.write_all(bytes)?;
    file.sync_all()?;
    File::open(path.parent().context("state parent")?)?.sync_all()?;
    Ok(())
}
fn durable_replace(path: &Path, bytes: &[u8]) -> Result<()> {
    let temp = path.with_extension(format!("{}.tmp", Uuid::new_v4()));
    durable_new(&temp, bytes)?;
    std::fs::rename(&temp, path)?;
    File::open(path.parent().context("state parent")?)?.sync_all()?;
    Ok(())
}
fn bounded_command(command: &mut Command) -> Result<Vec<u8>> {
    bounded_command_input(command, None, Duration::from_millis(250))
}
fn bounded_command_input(
    command: &mut Command,
    input: Option<&[u8]>,
    timeout: Duration,
) -> Result<Vec<u8>> {
    use std::os::fd::AsRawFd;
    ensure!(
        timeout <= Duration::from_millis(250) && !timeout.is_zero(),
        "local command deadline bounds"
    );
    ensure!(
        input.is_none_or(|v| v.len() <= 65536),
        "local command input bounds"
    );
    // A guardian remains single-threaded and reaps all its command children in
    // its ownership loop. Other callers reserve parent registry capacity first.
    let slot = if supervisor::in_guardian() {
        None
    } else {
        Some(reaper::Slot::reserve()?)
    };
    let mut child = command
        .env_clear()
        .env("PATH", "/usr/bin:/bin")
        .stdin(if input.is_some() {
            Stdio::piped()
        } else {
            Stdio::null()
        })
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()?;
    let result = (|| -> Result<Vec<u8>> {
        let mut stdin = child.stdin.take();
        if let Some(stdin) = &stdin {
            let flags = unsafe { libc::fcntl(stdin.as_raw_fd(), libc::F_GETFL) };
            ensure!(
                flags >= 0
                    && unsafe {
                        libc::fcntl(stdin.as_raw_fd(), libc::F_SETFL, flags | libc::O_NONBLOCK)
                    } == 0,
                "nonblocking command stdin unavailable"
            );
        }
        let mut written = 0usize;
        let mut stdout = child.stdout.take().context("command stdout")?;
        let flags = unsafe { libc::fcntl(stdout.as_raw_fd(), libc::F_GETFL) };
        ensure!(
            flags >= 0
                && unsafe {
                    libc::fcntl(stdout.as_raw_fd(), libc::F_SETFL, flags | libc::O_NONBLOCK)
                } == 0,
            "nonblocking command stdout unavailable"
        );
        let end = Instant::now() + timeout;
        let mut output = Vec::new();
        let mut status = None;
        let mut eof = false;
        let mut buffer = [0u8; 4096];
        loop {
            ensure!(Instant::now() < end, "local tmux command total deadline");
            if let (Some(bytes), Some(pipe)) = (input, stdin.as_mut()) {
                match pipe.write(&bytes[written..]) {
                    Ok(n) => written += n,
                    Err(e)
                        if e.kind() == std::io::ErrorKind::WouldBlock
                            || e.kind() == std::io::ErrorKind::Interrupted => {}
                    Err(e) => return Err(e.into()),
                }
                if written == bytes.len() {
                    stdin.take();
                }
            }
            loop {
                match stdout.read(&mut buffer) {
                    Ok(0) => {
                        eof = true;
                        break;
                    }
                    Ok(n) => {
                        ensure!(output.len() + n <= 65536, "local tmux output bounds");
                        output.extend_from_slice(&buffer[..n]);
                    }
                    Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => break,
                    Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
                    Err(e) => return Err(e.into()),
                }
            }
            if status.is_none() {
                status = child.try_wait()?;
            }
            if eof && let Some(status) = status {
                ensure!(status.success(), "local tmux command failed");
                return Ok(output);
            }
            ensure!(Instant::now() < end, "local tmux command total deadline");
            std::thread::sleep(Duration::from_millis(1));
        }
    })();
    if result.is_err() {
        let _ = child.kill();
        if !matches!(child.try_wait(), Ok(Some(_)))
            && let Some(slot) = slot
        {
            let _ = slot.track(child);
        }
    }
    result
}

fn tmux(config: &SealedLaunch, directory: &Path, args: &[&str]) -> Result<Vec<u8>> {
    bounded_command(
        Command::new(&config.tmux)
            .arg("-N")
            .arg("-S")
            .arg(directory.join("tmux.sock"))
            .args(args),
    )
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
mod tests {
    use super::*;
    fn session() -> SessionIdentity {
        SessionIdentity::Central {
            authority_id: Uuid::from_u128(1),
            session_id: Uuid::from_u128(2),
            generation: 1,
        }
    }
    #[test]
    fn independent_permit_is_exact_bounded_and_expiring() {
        let s = session();
        let digest = Digest::of(b"manifest");
        let p = ExecutionPermit {
            session: s.clone(),
            manifest_digest: digest.clone(),
            sequence: 1,
            issued_boot_ms: 100,
            expires_boot_ms: 200,
        };
        assert!(p.validate_at(&s, &digest, 100).is_ok());
        assert!(p.validate_at(&s, &digest, 200).is_err());
        assert!(p.validate_at(&s, &digest, 99).is_err());
        assert!(p.validate_at(&s, &Digest::of(b"changed"), 100).is_err());
        let other = SessionIdentity::Central {
            authority_id: Uuid::from_u128(3),
            session_id: Uuid::from_u128(2),
            generation: 1,
        };
        assert!(p.validate_at(&other, &digest, 100).is_err());
        let mut excessive = p;
        excessive.expires_boot_ms = 100 + MAX_PERMIT_MS + 1;
        assert!(excessive.validate_at(&s, &digest, 100).is_err());
    }
    #[test]
    fn local_command_bounds_and_protected_environment() {
        let mut c = LocalCommand {
            executable: "/bin/true".into(),
            arguments: vec!["literal; $(text)".into()],
            environment: Default::default(),
        };
        c.validate().unwrap();
        c.environment
            .insert("LD_PRELOAD".into(), "untrusted".into());
        assert!(c.validate().is_err());
        c.environment.clear();
        c.arguments = vec!["x".repeat(4097)];
        assert!(c.validate().is_err());
    }
}

#[cfg(test)]
mod io_tests {
    use super::*;
    #[test]
    #[ignore = "subprocess output fixture invoked by bounded output test"]
    fn large_stdout_fixture() {
        std::io::stdout().write_all(&vec![b'x'; 60000]).unwrap();
    }
    #[test]
    fn command_stdout_above_pipe_capacity_is_drained_before_exit() {
        let output = bounded_command(Command::new(std::env::current_exe().unwrap()).args([
            "--ignored",
            "--exact",
            "io_tests::large_stdout_fixture",
            "--nocapture",
        ]))
        .unwrap();
        assert!(output.windows(60000).any(|w| w.iter().all(|b| *b == b'x')));
    }
    #[test]
    fn command_input_and_output_above_pipe_capacity_progress_under_one_deadline() {
        let bytes: Vec<_> = (0..60000).map(|n| (n % 251) as u8).collect();
        let output = bounded_command_input(
            &mut Command::new("/bin/cat"),
            Some(&bytes),
            Duration::from_millis(250),
        )
        .unwrap();
        assert_eq!(output, bytes);
        let started = Instant::now();
        assert!(
            bounded_command_input(
                Command::new("/bin/sleep").arg("2"),
                Some(&bytes),
                Duration::from_millis(50)
            )
            .is_err()
        );
        assert!(started.elapsed() < Duration::from_secs(1));
    }
    #[test]
    fn nonregular_and_symlink_material_is_refused() {
        use std::os::unix::{ffi::OsStrExt, fs::symlink};
        let temp = tempfile::TempDir::new().unwrap();
        let path = temp.path().join("file");
        std::fs::write(&path, b"material").unwrap();
        let link = temp.path().join("link");
        symlink(&path, &link).unwrap();
        assert!(read_bytes(&link, 100).is_err());
        let fifo = temp.path().join("fifo");
        let c = std::ffi::CString::new(fifo.as_os_str().as_bytes()).unwrap();
        assert_eq!(unsafe { libc::mkfifo(c.as_ptr(), 0o600) }, 0);
        assert!(read_bytes(&fifo, 100).is_err());
    }
}

#[cfg(test)]
mod rendered_tests {
    use super::*;
    fn fixture(bytes: &[u8]) -> (LaunchManifest, ArtifactRef, ArtifactRef) {
        let mut manifest: LaunchManifest =
            serde_json::from_str(include_str!("../tests/fixtures/manifest.json")).unwrap();
        let digest =
            Digest::of(b"synthetic descriptor; full descriptor validation belongs to runtime");
        let descriptor = ArtifactRef {
            id: aq_session_protocol::Text::new(format!("aq.startup-delivery/{}", digest.as_str()))
                .unwrap(),
            version: aq_session_protocol::Text::new("1").unwrap(),
            digest,
        };
        manifest.modules.push(descriptor.clone());
        let payload = ArtifactRef {
            id: aq_session_protocol::Text::new("final-startup").unwrap(),
            version: aq_session_protocol::Text::new("1").unwrap(),
            digest: Digest::of(bytes),
        };
        (manifest, descriptor, payload)
    }
    #[test]
    fn final_payload_is_literal_and_bound_to_exact_single_descriptor() {
        let bytes = "framing\nfixture\n雪\0literal".as_bytes();
        let (m, d, p) = fixture(bytes);
        let prepared =
            RenderedStartup::new(m.clone(), d.clone(), p.clone(), bytes.to_vec()).unwrap();
        assert_eq!(prepared.bytes, bytes);
        let mut missing = m.clone();
        missing.modules.retain(|r| r != &d);
        assert!(RenderedStartup::new(missing, d.clone(), p.clone(), bytes.to_vec()).is_err());
        let mut multiple = m.clone();
        let mut other = d.clone();
        other.digest = Digest::of(b"other");
        other.id = aq_session_protocol::Text::new(format!(
            "aq.startup-delivery/{}",
            other.digest.as_str()
        ))
        .unwrap();
        multiple.modules.push(other);
        assert!(RenderedStartup::new(multiple, d.clone(), p.clone(), bytes.to_vec()).is_err());
        assert!(
            RenderedStartup::new(m.clone(), d.clone(), p.clone(), b"changed".to_vec()).is_err()
        );
        let mut bounded = m.clone();
        bounded.context.startup_byte_budget = 7;
        assert!(RenderedStartup::new(bounded, d.clone(), p.clone(), bytes.to_vec()).is_err());
        let (m, d, p) = fixture(&[0xff]);
        assert!(RenderedStartup::new(m, d, p, vec![0xff]).is_err());
        let (m, d, p) = fixture(&[]);
        assert!(RenderedStartup::new(m, d, p, vec![]).is_err());
    }
    #[test]
    fn legacy_sealed_configuration_omits_new_optional_field() {
        let (manifest, _, _) = fixture(b"payload");
        let permit = ExecutionPermit {
            session: manifest.bindings.session.clone(),
            manifest_digest: canonical_digest(&manifest).unwrap(),
            sequence: 1,
            issued_boot_ms: 1,
            expires_boot_ms: 100,
        };
        let sealed = SealedLaunch {
            launch_id: Uuid::from_u128(1),
            manifest,
            permit,
            workspace: "/tmp/work".into(),
            helper: "/tmp/helper".into(),
            tmux: "/usr/bin/tmux".into(),
            tmux_version: "tmux 3.4".into(),
            command: LocalCommand {
                executable: "/bin/true".into(),
                arguments: vec![],
                environment: Default::default(),
            },
            resource_policy: ResourcePolicy::CallerAdmission(ArtifactRef {
                id: aq_session_protocol::Text::new("fixture").unwrap(),
                version: aq_session_protocol::Text::new("1").unwrap(),
                digest: Digest::of(b"fixture"),
            }),
            resource_qualification: None,
            rendered_startup: None,
            observed: None,
            private_input: None,
        };
        let bytes = serde_json::to_vec(&sealed).unwrap();
        let value: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert!(value.get("rendered_startup").is_none());
        assert!(value.get("observed").is_none());
        let read: SealedLaunch = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(serde_json::to_vec(&read).unwrap(), bytes);
        assert_eq!(read.intent().unwrap(), sealed.intent().unwrap());
    }
}
