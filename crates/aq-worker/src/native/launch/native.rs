//! Production native profile with an exact installed image and retained inputs.
use super::*;
use crate::native::adapter::{
    codex::{CodexExecInput, CodexExecProfile, PreparedCodexExec},
    native_exec::NativeExecInstallation,
};
use aq_session_host::{
    AdapterLaunch, HarnessAdapter, HarnessReadiness, LocalEnvironment, PrivateLaunchInput,
    ResourcePolicy,
    observed::{
        NativeExecObservationProfile, ObservedAdapterQualification, ObservedHarnessAdapter,
        ObservedStartup,
    },
};
use std::path::PathBuf;

pub struct NativeCodexHostProfile {
    pub(super) root: RootGuard,
    root_path: PathBuf,
    installation: Mutex<NativeExecInstallation>,
    mapper: CodexExecProfile,
    digest: Digest,
    columns: u16,
    rows: u16,
    #[cfg(any(test, feature = "test-support"))]
    development_provider: Option<String>,
}
impl NativeCodexHostProfile {
    pub fn new(
        root: PathBuf,
        mut installation: NativeExecInstallation,
        mapper: CodexExecProfile,
        columns: u16,
        rows: u16,
    ) -> Result<Self> {
        let guard = RootGuard::open(&root, None)?;
        NativeExecObservationProfile::new(mapper.renderer.clone(), columns, rows)?;
        ensure!(
            mapper.harness.harness.as_str() == "codex"
                && mapper.harness.cli_version.as_str()
                    == crate::native::adapter::codex::CODEX_CLI_VERSION
                && mapper.model.provider.as_str() == "openai"
                && mapper.model.spending_budget.is_none()
                && matches!(mapper.model.reasoning.as_str(), "low" | "medium" | "high"),
            "unsupported native Codex settings"
        );
        installation.revalidate()?;
        let m = installation.installation();
        ensure!(
            m.adapter == mapper.harness.adapter && m.renderer == mapper.renderer,
            "native installed mapper references differ"
        );
        let digest = crate::native::journal::canonical_digest(
            &serde_json::json!({"schema_version":1,"profile":"codex-native-host-v1","root":root,"installation":installation.digest(),"harness":mapper.harness,"model":mapper.model,"permissions":mapper.permissions_digest,"renderer":mapper.renderer,"columns":columns,"rows":rows}),
        )?;
        Ok(Self {
            root: guard,
            root_path: root,
            installation: Mutex::new(installation),
            mapper,
            digest,
            columns,
            rows,
            #[cfg(any(test, feature = "test-support"))]
            development_provider: None,
        })
    }
    /// Isolated fixture endpoint only. This cannot enable the production executor.
    #[cfg(any(test, feature = "test-support"))]
    pub fn with_development_provider(mut self, origin: &str) -> Result<Self> {
        let origin = crate::native::adapter::codex::development_origin(origin)?;
        self.digest = crate::native::journal::canonical_digest(
            &serde_json::json!({"profile":self.digest,"development_provider":origin}),
        )?;
        self.development_provider = Some(origin);
        Ok(self)
    }
    pub fn digest(&self) -> &Digest {
        &self.digest
    }
}
struct Adapter {
    recipe: PreparedCodexExec,
    executable: ArtifactRef,
    renderer: ArtifactRef,
    private: Mutex<Option<PrivateLaunchInput>>,
}
impl HarnessAdapter for Adapter {
    fn prepare(
        &self,
        m: &aq_session_protocol::LaunchManifest,
        _: &std::path::Path,
    ) -> Result<AdapterLaunch> {
        ensure!(
            crate::native::journal::canonical_digest(m)? == *self.recipe.manifest_digest(),
            "native command manifest changed"
        );
        Ok(AdapterLaunch {
            resource_qualification: None,
            manifest_digest: self.recipe.manifest_digest().clone(),
            adapter: m.harness.adapter.clone(),
            cli_version: m.harness.cli_version.as_str().into(),
            environment: m.harness.environment.clone(),
            capabilities: m.harness.required_capabilities.clone(),
            command: self.recipe.command().clone(),
        })
    }
    fn readiness(&self, _: &HostObservation) -> Result<HarnessReadiness> {
        Ok(HarnessReadiness::Unknown)
    }
    fn private_input(
        &self,
        _: &aq_session_protocol::LaunchManifest,
        _: &std::path::Path,
    ) -> Result<Option<PrivateLaunchInput>> {
        Ok(Some(
            lock(&self.private)?
                .take()
                .context("native private input already consumed")?,
        ))
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
pub(super) fn bootstrap(
    owner: &Arc<Mutex<Owned>>,
    entry: &Arc<Entry>,
    profile: &Arc<NativeCodexHostProfile>,
) -> Result<()> {
    let scope = entry.scope.as_ref().context("native launch scope absent")?;
    let offer = {
        let mut owner = lock(owner)?;
        entry.current(&owner)?;
        let state = owner.journal.state();
        let a = &state.attempts[&entry.id];
        let offer = a.reservation.offer.clone();
        let grant = &a
            .phase_execution
            .as_ref()
            .context("native grant absent")?
            .delivery
            .receipt
            .grant;
        let accepted = &a
            .phase_preparation
            .as_ref()
            .context("native accept absent")?
            .delivery
            .receipt
            .lease;
        let (device, inode) = profile.root.identity();
        let m = lock(&profile.installation)?.installation().clone();
        ensure!(
            a.reservation.runtime.runtime == m.runtime
                && a.reservation.runtime.qualification == m.qualification,
            "native runtime installation differs from grant"
        );
        let intent = NativeLaunchIntent {
            execution_digest: scope.digest.clone(),
            binding: grant.binding.clone(),
            grant_id: grant.grant_id,
            logical_guardian_id: accepted.guardian_id,
            launch_id: accepted.launch_id,
            reservation_id: accepted.hold.reservation.reservation_id,
            runtime: a.reservation.runtime.clone(),
            terminal: aq_session_protocol::TerminalIdentity {
                session: grant.binding.session.clone(),
                terminal_id: Uuid::new_v4(),
                epoch: 1,
            },
            host_root: profile.root_path.clone(),
            root_device: device,
            root_inode: inode,
            pid_namespace: std::fs::read_link("/proc/self/ns/pid")?
                .to_str()
                .context("native PID namespace UTF8")?
                .into(),
            profile_digest: profile.digest.clone(),
            executable: m.executable.artifact,
            helper: m.helper.artifact,
            tmux: m.tmux.artifact,
            renderer: profile.mapper.renderer.clone(),
            columns: profile.columns,
            rows: profile.rows,
            deadline_ns: scope.deadline,
            hard_deadline_ns: scope.hard,
            created_ns: boot_ns()?,
            created_revision: state.revision,
            started_operation_id: Uuid::new_v4(),
            stopped_operation_id: Uuid::new_v4(),
        };
        #[cfg(test)]
        entry.hook(Checkpoint::Outer, &mut owner.journal)?;
        owner.journal.begin_native_launch(entry.id, intent)?;
        entry.snapshot(&owner.journal)?;
        entry.current(&owner)?;
        offer
    };
    let h = lock(&entry.history)?
        .clone()
        .context("native launch intent absent")?;
    let i = &h.intent;
    let home = profile
        .root_path
        .join(i.launch_id.to_string())
        .join("codex-home");
    let payload = scope
        .inputs
        .startup()
        .verified_payload(
            &offer.offered.offer.manifest,
            &offer.startup.artifact()?,
            &offer.startup.payload,
        )?
        .to_vec();
    let m = &offer.offered.offer.manifest;
    let providers: Vec<_> = scope
        .inputs
        .materials()
        .iter()
        .filter(|material| {
            let lease = &material.metadata().delivery.lease;
            lease.provider.as_str() == "openai"
                && m.credentials.iter().any(|r| {
                    r.credential_profile == lease.credential_profile
                        && r.purpose.as_str() == lease.purpose.as_str()
                })
        })
        .collect();
    ensure!(
        providers.len() == 1,
        "native Codex requires one exact provider material"
    );
    let provider = providers[0];
    let mut installed = lock(&profile.installation)?;
    let handoff = installed.material(provider, &provider.receipt()?, &i.binding)?;
    drop(crate::native::adapter::codex::api_key_auth(&handoff, m)?);
    let lease = &provider.metadata().delivery.lease;
    let private = PrivateLaunchInput::codex_api_key(
        provider.source_directory()?,
        lease.material_digest.clone(),
        lease.material_bytes,
        provider.receipt()?.private_binding.digest,
    )?;
    let recipe = profile.mapper.prepare_native_planned(
        &mut installed,
        CodexExecInput {
            offer: &offer,
            expected_manifest: &i.binding.launch_manifest_digest,
            payload,
            workspace: &scope.inputs.workspace().workspace().path,
            private_home: &home,
        },
    )?;
    #[cfg(any(test, feature = "test-support"))]
    let recipe = if let Some(origin) = &profile.development_provider {
        recipe.with_development_provider(origin)?
    } else {
        recipe
    };
    let image = installed.image()?;
    let installation = installed.installation().clone();
    drop(installed);
    {
        let owner = lock(owner)?;
        entry.current(&owner)?;
    }
    let startup = ObservedStartup::native_exec(
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
        expires_boot_ms: scope.deadline.min(scope.hard) / 1_000_000,
    };
    let local = LocalEnvironment {
        resource_policy: ResourcePolicy::CallerAdmission(m.harness.environment.clone()),
        workspace: scope.inputs.workspace().workspace().path.clone(),
        helper_executable: installation.helper.path,
        tmux_executable: installation.tmux.path,
        tmux_version: "tmux 3.4".into(),
        credential_profiles: m
            .credentials
            .iter()
            .map(|c| c.credential_profile.clone())
            .collect(),
        context_material: vec![],
    };
    let adapter = Adapter {
        recipe,
        executable: i.executable.clone(),
        renderer: i.renderer.clone(),
        private: Mutex::new(Some(private)),
    };
    let mut authority = authority::Authority {
        owner: owner.clone(),
        entry: entry.clone(),
    };
    let pending = SessionHost::prepare_observed(
        &profile.root_path,
        i.launch_id,
        &startup,
        permit,
        &local,
        &adapter,
        &mut authority,
    )?;
    let (mut observer, baseline) = pending.open_baseline_observer()?;
    {
        let owner = lock(owner)?;
        entry.current(&owner)?;
    }
    let mut released = pending.release_after_baseline(&baseline, &mut authority)?;
    let until = Instant::now() + Duration::from_secs(3);
    let mut bootstrap_output = Vec::new();
    let mut bootstrap_bytes = 0usize;
    loop {
        let events = observer.poll()?;
        bootstrap_bytes += events
            .iter()
            .map(|e| {
                if let aq_session_host::terminal_output::ControlEvent::Bytes(b) = e {
                    b.len()
                } else {
                    0
                }
            })
            .sum::<usize>();
        ensure!(
            bootstrap_bytes <= 65536 && bootstrap_output.len() + events.len() <= 64,
            "native bootstrap output bound exceeded"
        );
        bootstrap_output.extend(events);
        if entry.stop.load(Ordering::SeqCst) {
            released.stop()?;
            anyhow::bail!("native host stopped before Running");
        }
        if let Some(handle) = released.poll_started(&mut authority)? {
            let mut state = lock(&entry.physical)?;
            state.handle = Some(handle);
            state.observer = Some(observer);
            state.pending_output = bootstrap_output;
            state.outcome = Outcome::Running;
            return Ok(());
        }
        ensure!(
            boot_ns()? < scope.deadline.min(scope.hard) && Instant::now() < until,
            "native exec observation expired"
        );
        std::thread::sleep(Duration::from_millis(2));
    }
}
