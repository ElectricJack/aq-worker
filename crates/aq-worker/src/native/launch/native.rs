//! Production native profile with an exact installed image and retained inputs.
use super::*;
use crate::native::adapter::{
    codex::{CodexExecInput, CodexExecProfile, CodexResultInput, PreparedCodexExec},
    named_exec::NamedExecInstallation,
    native_exec::NativeExecInstallation,
    responses_transport::{ResponsesScope, ScopedResponsesTransport, UpstreamEndpoint},
    result_client::InstalledResultClient,
    result_profile::{ExpectedResultAdapter, ResultAdapterDefinition},
};
use crate::native::{journal::RuntimeIdentity, result::ResultInstallationPins};
use aq_session_host::{
    AdapterLaunch, HarnessAdapter, HarnessReadiness, LocalEnvironment, OriginalPrivateMaterial,
    PrivateLaunchInput, ResourcePolicy, ScopedCodexAuthBinding,
    observed::{
        NamedReadonlyExecImage, NamedReadonlyObservationProfile, NativeExecImage,
        NativeExecObservationProfile, ObservedAdapterQualification, ObservedHarnessAdapter,
        ObservedStartup,
    },
};
use std::path::PathBuf;

pub struct NativeCodexHostProfile {
    pub(super) root: RootGuard,
    root_path: PathBuf,
    installation: Mutex<Installation>,
    mapper: CodexExecProfile,
    digest: Digest,
    columns: u16,
    rows: u16,
    result: Option<ResultInstallation>,
    terminal_clock: Option<aq_session_host::terminal_io::QualifiedClock>,
    #[cfg(any(test, feature = "test-support"))]
    development_provider: Option<String>,
}
struct ResultInstallation {
    client: InstalledResultClient,
    definition: ResultAdapterDefinition,
    pins: ResultInstallationPins,
}
impl NativeCodexHostProfile {
    pub fn new(
        root: PathBuf,
        installation: NativeExecInstallation,
        mapper: CodexExecProfile,
        columns: u16,
        rows: u16,
    ) -> Result<Self> {
        Self::build(
            root,
            Installation::Sealed(installation),
            mapper,
            columns,
            rows,
        )
    }
    pub fn new_named(
        root: PathBuf,
        installation: NamedExecInstallation,
        mapper: CodexExecProfile,
        columns: u16,
        rows: u16,
    ) -> Result<Self> {
        // The final CLI environment contains no TMPDIR override, so its release
        // helper-alias policy uses /tmp, not the worker's test TMPDIR.
        ensure!(
            !root.starts_with("/tmp"),
            "named native state root must be outside CLI temporary root"
        );
        ensure!(
            installation.environment() == &mapper.harness.environment,
            "named image environment differs from selected harness"
        );
        NamedReadonlyObservationProfile::new(mapper.renderer.clone(), columns, rows)?;
        Self::build(
            root,
            Installation::Named(installation),
            mapper,
            columns,
            rows,
        )
    }
    /// Explicit result-enabled profile. The client and composite definition are
    /// independently selected and retained; no ordinary profile gains a channel.
    pub fn new_named_result(
        root: PathBuf,
        installation: NamedExecInstallation,
        mapper: CodexExecProfile,
        columns: u16,
        rows: u16,
        client: InstalledResultClient,
        definition: ResultAdapterDefinition,
    ) -> Result<Self> {
        let installed = installation.installation();
        let runtime = RuntimeIdentity {
            runtime: installed.runtime.clone(),
            qualification: installed.qualification.clone(),
        };
        definition.validate_selected(ExpectedResultAdapter {
            adapter: &mapper.harness.adapter,
            runtime: &runtime,
            environment: &mapper.harness.environment,
            permission_digest: &mapper.permissions_digest,
        })?;
        definition.validate_installed_client(&client)?;
        let pins = ResultInstallationPins {
            adapter: mapper.harness.adapter.clone(),
            contract: client.contract().clone(),
            client_manifest: client.manifest_artifact(),
            client_executable: client.artifact().clone(),
            environment: client.environment().clone(),
            recipe: definition.recipe.clone(),
            permission_digest: mapper.permissions_digest.clone(),
            reserved_client_bytes: client.reserved_bytes(),
        };
        pins.validate()?;
        let mut profile = Self::new_named(root, installation, mapper, columns, rows)?;
        profile.digest = crate::native::journal::canonical_digest(
            &serde_json::json!({"base_profile":profile.digest,"result_installation":pins}),
        )?;
        profile.result = Some(ResultInstallation {
            client,
            definition,
            pins,
        });
        Ok(profile)
    }
    /// Installation metadata only, never an execution/channel constructor.
    pub fn with_interactive_clock(mut self, maximum_skew_ms: u64) -> Result<Self> {
        ensure!(
            lock(&self.installation)?.interactive()
                && self
                    .result
                    .as_ref()
                    .is_some_and(|r| r.definition.interactive()),
            "interactive clock requires explicitly selected interactive result profile"
        );
        self.terminal_clock = Some(
            aq_session_host::terminal_io::QualifiedClock::assume_bounded_skew(maximum_skew_ms)?,
        );
        self.digest = crate::native::journal::canonical_digest(
            &serde_json::json!({"profile":self.digest,"interactive_clock_skew_ms":maximum_skew_ms}),
        )?;
        Ok(self)
    }
    pub fn terminal_clock(&self) -> Option<aq_session_host::terminal_io::QualifiedClock> {
        self.terminal_clock
    }
    pub fn result_installation_pins(&self) -> Option<ResultInstallationPins> {
        self.result.as_ref().map(|result| result.pins.clone())
    }
    fn build(
        root: PathBuf,
        mut installation: Installation,
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
                && (16..=1_048_576).contains(&mapper.model.max_output_tokens)
                && matches!(mapper.model.reasoning.as_str(), "low" | "medium" | "high"),
            "unsupported native Codex settings"
        );
        installation.revalidate()?;
        let m = installation.installation();
        ensure!(
            m.adapter == mapper.harness.adapter && m.renderer == mapper.renderer,
            "native installed mapper references differ"
        );
        let upstream_policy = if installation.interactive() {
            "native-codex-chatgpt-login-v1"
        } else {
            "fixed-openai-responses-https-v1"
        };
        let digest = crate::native::journal::canonical_digest(
            &serde_json::json!({"schema_version":1,"profile":"codex-scoped-responses-native-host-v1","image_profile":installation.kind(),"upstream_policy":upstream_policy,"root":root,"installation":installation.digest(),"harness":mapper.harness,"model":mapper.model,"permissions":mapper.permissions_digest,"renderer":mapper.renderer,"columns":columns,"rows":rows}),
        )?;
        Ok(Self {
            root: guard,
            root_path: root,
            installation: Mutex::new(installation),
            mapper,
            digest,
            columns,
            rows,
            result: None,
            terminal_clock: None,
            #[cfg(any(test, feature = "test-support"))]
            development_provider: None,
        })
    }
    /// Isolated fixture upstream for the owned transport. This cannot enable
    /// the production executor or supply a direct provider route to the child.
    #[cfg(any(test, feature = "test-support"))]
    pub fn with_development_provider(mut self, origin: &str) -> Result<Self> {
        ensure!(
            !lock(&self.installation)?.interactive(),
            "native ChatGPT interactive profile does not use a finite provider override"
        );
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
enum Installation {
    Sealed(NativeExecInstallation),
    Named(NamedExecInstallation),
}
impl Installation {
    fn interactive(&self) -> bool {
        matches!(self, Self::Named(i) if i.interactive())
    }
    fn kind(&self) -> &'static str {
        match self {
            Self::Sealed(_) => "sealed-native-v1",
            Self::Named(i) if i.interactive() => "named-readonly-interactive-pty-v1",
            Self::Named(_) => "named-readonly-native-v1",
        }
    }
    fn digest(&self) -> &Digest {
        match self {
            Self::Sealed(i) => i.digest(),
            Self::Named(i) => i.digest(),
        }
    }
    fn installation(&self) -> &crate::native::adapter::AdapterManifest {
        match self {
            Self::Sealed(i) => i.installation(),
            Self::Named(i) => i.installation(),
        }
    }
    fn revalidate(&mut self) -> Result<()> {
        match self {
            Self::Sealed(i) => i.revalidate(),
            Self::Named(i) => i.revalidate(),
        }
    }
    fn material<'a>(
        &self,
        input: &'a crate::native::materials::InstalledMaterial,
        receipt: &aq_admission::MaterialReceipt,
        binding: &aq_session_protocol::LaunchBinding,
    ) -> Result<crate::native::adapter::MaterialHandoff<'a>> {
        match self {
            Self::Sealed(i) => i.material(input, receipt, binding),
            Self::Named(i) => i.material(input, receipt, binding),
        }
    }
    fn prepare(
        &mut self,
        mapper: &CodexExecProfile,
        input: CodexExecInput<'_>,
    ) -> Result<PreparedCodexExec> {
        match self {
            Self::Sealed(i) => mapper.prepare_native_planned(i, input),
            Self::Named(i) => mapper.prepare_named_planned(i, input),
        }
    }
    fn image(&self) -> Result<Image> {
        match self {
            Self::Sealed(i) => Ok(Image::Sealed(i.image()?)),
            Self::Named(i) if i.interactive() => Ok(Image::NamedInteractive(i.image()?)),
            Self::Named(i) => Ok(Image::Named(i.image()?)),
        }
    }
}
enum Image {
    Sealed(NativeExecImage),
    Named(NamedReadonlyExecImage),
    NamedInteractive(NamedReadonlyExecImage),
}
impl Image {
    fn startup(
        self,
        rendered: aq_session_host::RenderedStartup,
        terminal: aq_session_protocol::TerminalIdentity,
        renderer: ArtifactRef,
        columns: u16,
        rows: u16,
        hard_ms: u64,
    ) -> Result<ObservedStartup> {
        match self {
            Self::Sealed(image) => ObservedStartup::native_exec(
                rendered,
                terminal,
                image,
                NativeExecObservationProfile::new(renderer, columns, rows)?,
                hard_ms,
            ),
            Self::NamedInteractive(image) => ObservedStartup::named_readonly_interactive(
                rendered,
                terminal,
                image,
                NamedReadonlyObservationProfile::new(renderer, columns, rows)?,
                hard_ms,
            ),
            Self::Named(image) => ObservedStartup::named_readonly_exec(
                rendered,
                terminal,
                image,
                NamedReadonlyObservationProfile::new(renderer, columns, rows)?,
                hard_ms,
            ),
        }
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
        let m = lock(&profile.installation)?.installation().clone();
        ensure!(
            a.reservation.runtime.runtime == m.runtime
                && a.reservation.runtime.qualification == m.qualification,
            "native runtime installation differs from grant"
        );
        let pins = HostPins {
            root: (profile.root_path.clone(), profile.root.identity()),
            profile_digest: profile.digest.clone(),
            programs: [
                m.executable.artifact,
                m.helper.artifact,
                m.tmux.artifact,
                profile.mapper.renderer.clone(),
            ],
            size: (profile.columns, profile.rows),
        };
        let intent = launch_intent(scope, state.revision, a, pins)?;
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
    let interactive = installed.interactive();
    if interactive {
        drop(crate::native::adapter::codex::validated_chatgpt_auth_json(
            &handoff, m,
        )?);
    } else {
        drop(crate::native::adapter::codex::validated_api_key(
            &handoff, m,
        )?);
    }
    let lease = &provider.metadata().delivery.lease;
    let input = CodexExecInput {
        offer: &offer,
        expected_manifest: &i.binding.launch_manifest_digest,
        payload,
        workspace: &scope.inputs.workspace().workspace().path,
        private_home: &home,
    };
    let recipe = match (&profile.result, entry.result_channel.as_deref()) {
        (None, None) => {
            ensure!(
                profile.mapper.harness.adapter.id.as_str() != "aq.codex-result-local-adapter",
                "known result adapter requires the result-enabled profile and actual channel"
            );
            installed.prepare(&profile.mapper, input)?
        }
        (Some(result), Some(channel)) => {
            let Installation::Named(named) = &mut *installed else {
                anyhow::bail!("result mapping requires the named-readonly image profile")
            };
            profile.mapper.prepare_named_result_planned(
                named,
                input,
                CodexResultInput {
                    binding: &i.binding,
                    execution_digest: &scope.digest,
                    definition: &result.definition,
                    client: &result.client,
                    channel,
                },
            )?
        }
        _ => anyhow::bail!("result profile and actual execution-owned channel differ"),
    };
    let image = installed.image()?;
    let installation = installed.installation().clone();
    drop(installed);
    let (private, recipe) = if interactive {
        let private = PrivateLaunchInput::codex_chatgpt_auth_json(
            provider.source_directory()?,
            OriginalPrivateMaterial {
                digest: lease.material_digest.clone(),
                bytes: lease.material_bytes,
                record_digest: provider.receipt()?.private_binding.digest,
            },
            i.binding.clone(),
            i.launch_id,
            scope.deadline.min(scope.hard) / 1_000_000,
        )?;
        (private, recipe)
    } else {
        let upstream = UpstreamEndpoint::openai();
        #[cfg(any(test, feature = "test-support"))]
        let upstream = if let Some(origin) = &profile.development_provider {
            UpstreamEndpoint::development_loopback(origin)?
        } else {
            upstream
        };
        let transport = tokio::runtime::Handle::try_current()?.block_on(
            ScopedResponsesTransport::start_retained(
                ResponsesScope::new(i.binding.clone(), &m.model, scope.deadline.min(scope.hard))?,
                upstream,
                provider.retained_copy()?,
                m,
            ),
        )?;
        let origin = transport.origin().to_owned();
        let capability = transport.local_capability();
        {
            let mut physical = lock(&entry.physical)?;
            ensure!(
                physical.transport.is_none(),
                "native transport already owned"
            );
            if entry.stop.load(Ordering::SeqCst) {
                transport.cancel();
            }
            physical.transport = Some(transport);
        }
        let private = PrivateLaunchInput::codex_scoped_capability(
            provider.source_directory()?,
            OriginalPrivateMaterial {
                digest: lease.material_digest.clone(),
                bytes: lease.material_bytes,
                record_digest: provider.receipt()?.private_binding.digest,
            },
            ScopedCodexAuthBinding {
                binding: i.binding.clone(),
                launch_id: i.launch_id,
                origin: origin.clone(),
                deadline_boot_ms: scope.deadline.min(scope.hard) / 1_000_000,
            },
            capability,
        )?;
        let recipe = recipe.with_scoped_provider(&origin)?;
        (private, recipe)
    };
    {
        let owner = lock(owner)?;
        entry.current(&owner)?;
    }
    let startup = image.startup(
        scope.inputs.startup().clone(),
        i.terminal.clone(),
        i.renderer.clone(),
        i.columns,
        i.rows,
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
