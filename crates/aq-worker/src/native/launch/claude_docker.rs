//! MS1 production host profile: the private session host runs the node-side
//! docker-local driver (`crate::sandbox_driver`) as its single pane program.
//! The driver, not this process, owns the sandbox; this profile owns only the
//! host, its pinned programs and the driver's private launch inputs.
use super::*;
use crate::backend::docker_local::DockerLocalConfig;
use crate::backend::{Ownership, ResourceLimits, TemplateDigest};
use aq_session_host::{
    AdapterLaunch, HarnessAdapter, HarnessReadiness, LocalCommand,
    observed::{
        NativeExecObservationProfile, NativeExecPool, ObservedAdapterQualification,
        ObservedHarnessAdapter,
    },
};
use aq_session_protocol::{ArtifactRef, Validate};
use serde::{Deserialize, Serialize};
use sha2::Digest as _;
use std::{
    io::{Read, Write},
    os::unix::fs::{DirBuilderExt, OpenOptionsExt},
    path::{Path, PathBuf},
};

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PinnedFile {
    pub path: PathBuf,
    pub artifact: ArtifactRef,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ClaudeDockerHostConfig {
    pub root: PathBuf,
    pub helper: PinnedFile,
    pub tmux: PinnedFile,
    /// The aq-worker binary that serves `sandbox-harness`.
    pub driver: PinnedFile,
    pub renderer: ArtifactRef,
    pub columns: u16,
    pub rows: u16,
    pub docker_local: DockerLocalConfig,
    pub template: TemplateDigest,
    /// Claude Code inside the template image.
    pub claude_executable: PathBuf,
    pub max_workspace_bytes: u64,
}
pub struct ClaudeDockerHostProfile {
    pub(super) config: ClaudeDockerHostConfig,
    pub(super) root: RootGuard,
    pub(super) digest: Digest,
    pool: NativeExecPool,
}
fn hash_file(path: &Path, max: u64) -> Result<(String, u64)> {
    let mut f = std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK | libc::O_CLOEXEC)
        .open(path)?;
    let m = f.metadata()?;
    ensure!(
        m.is_file() && m.len() > 0 && m.len() <= max,
        "pinned program bound"
    );
    let mut hash = sha2::Sha256::new();
    let mut scratch = vec![0u8; 1 << 20];
    let mut total = 0u64;
    loop {
        let n = f.read(&mut scratch)?;
        if n == 0 {
            break;
        }
        total += n as u64;
        ensure!(total <= max, "pinned program grew");
        hash.update(&scratch[..n]);
    }
    Ok((format!("{:x}", hash.finalize()), total))
}
impl ClaudeDockerHostProfile {
    pub fn new(config: ClaudeDockerHostConfig) -> Result<Self> {
        ensure!(
            config.driver.artifact.id.as_str() == "aq.claude-docker-local-driver"
                && config.columns >= 80
                && config.columns <= 240
                && config.rows > 0
                && config.rows <= 120
                && u32::from(config.columns) * u32::from(config.rows) <= 12000
                && config.claude_executable.is_absolute()
                && config.max_workspace_bytes > 0,
            "closed Claude docker-local host profile"
        );
        for pin in [&config.helper, &config.tmux, &config.driver] {
            pin.artifact.id.validate()?;
            pin.artifact.version.validate()?;
            ensure!(
                pin.path.is_absolute() && std::fs::canonicalize(&pin.path)? == pin.path,
                "pinned program path canonical"
            );
        }
        ensure!(
            config
                .docker_local
                .templates
                .iter()
                .any(|t| t.digest == config.template),
            "Claude template is not allowlisted for docker-local"
        );
        let root = RootGuard::open(&config.root, None)?;
        let digest = crate::native::journal::canonical_digest(&config)?;
        let (_, driver_bytes) = hash_file(
            &config.driver.path,
            aq_session_host::observed::MAX_NATIVE_EXEC_BYTES,
        )?;
        let pool = NativeExecPool::new(
            driver_bytes
                .checked_add(1 << 20)
                .context("driver pool size")?
                .min(aq_session_host::observed::MAX_NATIVE_EXEC_POOL_BYTES),
            1,
        )?;
        let profile = Self {
            config,
            root,
            digest,
            pool,
        };
        profile.verify_installation()?;
        Ok(profile)
    }
    pub fn verify_installation(&self) -> Result<()> {
        self.root.verify()?;
        for pin in [&self.config.helper, &self.config.tmux, &self.config.driver] {
            let (digest, _) =
                hash_file(&pin.path, aq_session_host::observed::MAX_NATIVE_EXEC_BYTES)?;
            ensure!(
                digest == pin.artifact.digest.as_str(),
                "pinned program changed"
            );
        }
        Ok(())
    }
    pub fn config(&self) -> &ClaudeDockerHostConfig {
        &self.config
    }
}
pub(super) struct Adapter {
    command: LocalCommand,
    executable: ArtifactRef,
    renderer: ArtifactRef,
    manifest_digest: Digest,
}
impl HarnessAdapter for Adapter {
    fn prepare(&self, m: &aq_session_protocol::LaunchManifest, _: &Path) -> Result<AdapterLaunch> {
        ensure!(
            crate::native::journal::canonical_digest(m)? == self.manifest_digest,
            "Claude driver manifest changed"
        );
        Ok(AdapterLaunch {
            resource_qualification: None,
            manifest_digest: self.manifest_digest.clone(),
            adapter: m.harness.adapter.clone(),
            cli_version: m.harness.cli_version.as_str().into(),
            environment: m.harness.environment.clone(),
            capabilities: m.harness.required_capabilities.clone(),
            command: self.command.clone(),
        })
    }
    fn readiness(&self, _: &HostObservation) -> Result<HarnessReadiness> {
        Ok(HarnessReadiness::Unknown)
    }
}
impl ObservedHarnessAdapter for Adapter {
    fn observed_qualification(
        &self,
        _: &aq_session_protocol::LaunchManifest,
    ) -> Result<ObservedAdapterQualification> {
        Ok(ObservedAdapterQualification {
            executable: self.executable.clone(),
            renderer: self.renderer.clone(),
        })
    }
}
fn private_write(path: &Path, bytes: &[u8]) -> Result<()> {
    let mut f = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .open(path)?;
    f.write_all(bytes)?;
    f.sync_all()?;
    Ok(())
}
/// Deletes the private token file unless the driver was started (it then owns
/// the file and deletes it after reading).
struct TokenGuard(Option<PathBuf>);
impl TokenGuard {
    fn disarm(&mut self) {
        self.0 = None;
    }
}
impl Drop for TokenGuard {
    fn drop(&mut self) {
        if let Some(path) = self.0.take() {
            let _ = std::fs::remove_file(path);
        }
    }
}
fn wall_ms() -> Result<u64> {
    Ok(std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)?
        .as_millis()
        .try_into()?)
}
pub(super) fn bootstrap(
    owner: &Arc<Mutex<Owned>>,
    entry: &Arc<Entry>,
    profile: &Arc<ClaudeDockerHostProfile>,
) -> Result<()> {
    use crate::native::runner::sandbox::confinement::ConfinementMode;
    let scope = entry.scope.as_ref().context("native launch scope absent")?;
    let (offer, runtime) = {
        let mut owner = lock(owner)?;
        entry.current(&owner)?;
        let state = owner.journal.state();
        let a = &state.attempts[&entry.id];
        let offer = a.reservation.offer.clone();
        let accepted = a.phase_scope()?;
        let grant = accepted.grant;
        let (device, inode) = profile.root.identity();
        let intent = NativeLaunchIntent {
            execution_digest: scope.digest.clone(),
            binding: grant.binding.clone(),
            grant_id: grant.grant_id,
            logical_guardian_id: accepted.guardian_id,
            launch_id: accepted.launch_id,
            reservation_id: accepted.reservation_id,
            runtime: a.reservation.runtime.clone(),
            terminal: aq_session_protocol::TerminalIdentity {
                session: grant.binding.session.clone(),
                terminal_id: Uuid::new_v4(),
                epoch: 1,
            },
            host_root: profile.config.root.clone(),
            root_device: device,
            root_inode: inode,
            pid_namespace: std::fs::read_link("/proc/self/ns/pid")?
                .to_str()
                .context("PID namespace text")?
                .into(),
            profile_digest: profile.digest.clone(),
            executable: profile.config.driver.artifact.clone(),
            helper: profile.config.helper.artifact.clone(),
            tmux: profile.config.tmux.artifact.clone(),
            renderer: profile.config.renderer.clone(),
            columns: profile.config.columns,
            rows: profile.config.rows,
            deadline_ns: scope.deadline,
            hard_deadline_ns: scope.hard,
            created_ns: boot_ns()?,
            created_revision: state.revision,
            started_operation_id: Uuid::new_v4(),
            stopped_operation_id: Uuid::new_v4(),
        };
        let runtime = a.reservation.runtime.clone();
        owner.journal.begin_native_launch(entry.id, intent)?;
        entry.snapshot(&owner.journal)?;
        entry.current(&owner)?;
        (offer, runtime)
    };
    drop(runtime);
    profile.verify_installation()?;
    let h = lock(&entry.history)?
        .clone()
        .context("native launch intent absent")?;
    let i = &h.intent;
    let m = &offer.offered.offer.manifest;
    // Exactly one Anthropic subscription material for this manifest.
    let logins: Vec<_> = scope
        .inputs
        .materials()
        .iter()
        .filter(|material| {
            let lease = &material.metadata().delivery.lease;
            lease.provider.as_str() == "anthropic"
                && m.credentials.iter().any(|r| {
                    r.credential_profile == lease.credential_profile
                        && r.purpose.as_str() == lease.purpose.as_str()
                })
        })
        .collect();
    ensure!(
        logins.len() == 1,
        "Claude docker-local requires one exact Anthropic login material"
    );
    let token = logins[0].read_private()?;
    let token = crate::sandbox_driver::parse_token(&token)?;
    let mode = ConfinementMode::declared(m)?;
    ensure!(
        mode == ConfinementMode::Container,
        "Claude docker-local runs in container confinement only (C2: no user-namespace mapping)"
    );
    let home = PathBuf::from("/aq/home");
    let recipe = crate::native::adapter::claude::map(
        &m.harness,
        &m.model,
        crate::native::adapter::claude::ClaudePaths {
            executable: &profile.config.claude_executable,
            workspace: Path::new(crate::backend::docker_local::config::WORK_TARGET),
            private_home: &home,
        },
        mode,
    )?;
    let (node_id, boot_id) = match &i.binding.session {
        aq_session_protocol::SessionIdentity::Remote {
            node_id, boot_id, ..
        } => (*node_id, *boot_id),
        _ => anyhow::bail!("Claude docker-local requires a remote node session"),
    };
    let now = boot_ns()?;
    let hard = scope.deadline.min(scope.hard);
    let deadline_ms = wall_ms()?
        .checked_add(hard.saturating_sub(now) / 1_000_000)
        .context("sandbox deadline overflow")?;
    let private = profile.config.root.join(format!("driver-{}", i.launch_id));
    std::fs::DirBuilder::new().mode(0o700).create(&private)?;
    let token_file = private.join("token");
    let mut guard = TokenGuard(Some(token_file.clone()));
    private_write(&token_file, &token)?;
    drop(token);
    let result_dir = entry
        .result_channel
        .as_ref()
        .map(|channel| channel.root().to_owned());
    let driver = crate::sandbox_driver::DriverConfig {
        docker_local: profile.config.docker_local.clone(),
        template: profile.config.template.clone(),
        owner: Ownership {
            node_id,
            boot_id,
            attempt_id: i.binding.attempt_id,
        },
        hard_deadline_ms: deadline_ms,
        resources: ResourceLimits {
            cpu_millis: u32::try_from(m.resources.cpu_millis)?,
            memory_bytes: m.resources.memory_bytes,
            pids: m.resources.process_limit,
            disk_bytes: None,
        },
        workspace: scope.inputs.workspace().workspace().path.clone(),
        token_file,
        executable: recipe
            .executable
            .to_str()
            .context("Claude executable UTF-8")?
            .into(),
        arguments: recipe.arguments,
        environment: recipe.environment,
        settings_json: String::from_utf8(recipe.settings_json)?,
        result_dir,
        max_workspace_bytes: profile.config.max_workspace_bytes,
    };
    let driver_file = private.join("driver.json");
    private_write(&driver_file, &serde_json::to_vec(&driver)?)?;
    let image = profile
        .pool
        .load_verified(&profile.config.driver.path, &profile.config.driver.artifact)?;
    let startup = aq_session_host::observed::ObservedStartup::native_exec(
        scope.inputs.startup().clone(),
        i.terminal.clone(),
        image,
        NativeExecObservationProfile::new(i.renderer.clone(), i.columns, i.rows)?,
        scope.hard / 1_000_000,
    )?;
    let permit = ExecutionPermit {
        session: i.binding.session.clone(),
        manifest_digest: i.binding.launch_manifest_digest.clone(),
        sequence: 1,
        issued_boot_ms: boot_ns()? / 1_000_000,
        // Non-interactive local permits are capped; the executor renews them.
        expires_boot_ms: (hard / 1_000_000)
            .min(boot_ns()? / 1_000_000 + aq_session_host::MAX_PERMIT_MS - 5_000),
    };
    let local = aq_session_host::LocalEnvironment {
        resource_policy: aq_session_host::ResourcePolicy::CallerAdmission(
            m.harness.environment.clone(),
        ),
        workspace: scope.inputs.workspace().workspace().path.clone(),
        helper_executable: profile.config.helper.path.clone(),
        tmux_executable: profile.config.tmux.path.clone(),
        tmux_version: "tmux 3.4".into(),
        credential_profiles: m
            .credentials
            .iter()
            .map(|c| c.credential_profile.clone())
            .collect(),
        context_material: vec![],
    };
    let mut environment = std::collections::BTreeMap::new();
    if let Some(dir) = &driver.result_dir {
        environment.insert(
            "AQ_AGENT_RESULT_DIR".to_string(),
            dir.to_str().context("result dir UTF-8")?.to_string(),
        );
    }
    environment.insert("RUST_LOG".to_string(), "warn".to_string());
    let adapter = Adapter {
        command: LocalCommand {
            executable: profile.config.driver.path.clone(),
            arguments: vec![
                "sandbox-harness".into(),
                "--driver-config".into(),
                driver_file.to_str().context("driver path UTF-8")?.into(),
            ],
            environment,
        },
        executable: profile.config.driver.artifact.clone(),
        renderer: i.renderer.clone(),
        manifest_digest: crate::native::journal::canonical_digest(m)?,
    };
    let mut authority = authority::Authority {
        owner: owner.clone(),
        entry: entry.clone(),
    };
    let pending = SessionHost::prepare_observed(
        &profile.config.root,
        i.launch_id,
        &startup,
        permit,
        &local,
        &adapter,
        &mut authority,
    )?;
    let (mut observer, baseline) = pending.open_baseline_observer()?;
    let mut released = pending.release_after_baseline(&baseline, &mut authority)?;
    let until = Instant::now() + Duration::from_secs(5);
    let mut bootstrap_output = Vec::new();
    let mut bootstrap_bytes = 0usize;
    loop {
        let events = observer.poll()?;
        bootstrap_bytes += events
            .iter()
            .map(|e| match e {
                aq_session_host::terminal_output::ControlEvent::Bytes(b) => b.len(),
                _ => 0,
            })
            .sum::<usize>();
        ensure!(
            bootstrap_bytes <= 65536 && bootstrap_output.len() + events.len() <= 64,
            "Claude driver bootstrap output bound"
        );
        bootstrap_output.extend(events);
        if entry.stop.load(Ordering::SeqCst) {
            released.stop()?;
            anyhow::bail!("Claude driver host stopped before Running");
        }
        if let Some(handle) = released.poll_started(&mut authority)? {
            let mut state = lock(&entry.physical)?;
            state.handle = Some(handle);
            state.observer = Some(observer);
            state.pending_output = bootstrap_output;
            state.outcome = Outcome::Running;
            guard.disarm();
            return Ok(());
        }
        ensure!(
            boot_ns()? < hard && Instant::now() < until,
            "Claude driver exec observation expired"
        );
        std::thread::sleep(Duration::from_millis(2));
    }
}
