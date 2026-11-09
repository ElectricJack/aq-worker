//! Concrete closed runtime joining the installed image and native host owner.
use super::*;
use crate::native::{
    adapter::{
        ExpectedAdapter,
        codex::CodexExecProfile,
        named_exec::{NamedExecInstallation, NamedExecManifest},
        result_client::{InstalledResultClient, ResultClientManifest},
        result_profile::{ExpectedResultAdapter, ResultAdapterDefinition},
    },
    launch::{NativeCodexHostProfile, NativeHostLaunchResult, NativeStopOutcome},
    runner::sandbox::confinement::{AuthorizedConfinement, ConfinementMode},
};
use aq_session_host::{
    HostPhase,
    observed::{NamedReadonlyEnvironment, NamedReadonlyExecPool},
};
use aq_session_protocol::{ArtifactRef, Digest, HarnessSettings, ModelSettings};
use aq_terminal_bridge::http::{NodeCredential, RelayHttp};
use serde::Deserialize;
use std::path::PathBuf;

const CODEX_INSTALLATION_BUDGET_BYTES: u64 = 512 * 1024 * 1024;

#[derive(Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CodexSettings {
    pub host_root: PathBuf,
    pub harness: HarnessSettings,
    pub model: ModelSettings,
    pub permissions_digest: Digest,
    pub columns: u16,
    pub rows: u16,
}
#[derive(Clone, Deserialize)]
#[serde(tag = "kind", deny_unknown_fields)]
pub enum ExecutorConfig {
    #[serde(rename = "codex-0.166-exec-stdin-v1")]
    Codex { settings: CodexSettings },
    /// Operator-selected trusted-node profile. This does not promise isolation
    /// from other processes running as the worker UID.
    #[serde(rename = "codex-0.166-trusted-node-named-readonly-v1")]
    TrustedNodeNamedReadonly {
        settings: CodexSettings,
        environment: ArtifactRef,
    },
    #[serde(rename = "codex-0.166-trusted-node-named-readonly-result-local-v1")]
    TrustedNodeNamedReadonlyResultLocal {
        settings: CodexSettings,
        environment: ArtifactRef,
        client_manifest: PathBuf,
        client_manifest_digest: Digest,
        composite_definition: PathBuf,
    },
    /// MS1: Claude Code in a docker-local sandbox (`runner::claude_docker`).
    #[serde(rename = "claude-2.1.318-docker-local-result-local-v1")]
    ClaudeDockerLocalResultLocal {
        settings: Box<super::claude_docker::ClaudeDockerSettings>,
        composite_definition: PathBuf,
    },
    #[serde(rename = "codex-0.166-chatgpt-interactive-result-local-v1")]
    ChatgptInteractiveResultLocal {
        settings: CodexSettings,
        environment: ArtifactRef,
        client_manifest: PathBuf,
        client_manifest_digest: Digest,
        composite_definition: PathBuf,
        terminal_clock_skew_ms: u64,
    },
}
impl CodexSettings {
    fn validate(&self, config: &NativeConfig) -> Result<()> {
        ensure!(
            self.harness.harness.as_str() == "codex"
                && self.harness.cli_version.as_str()
                    == crate::native::adapter::codex::CODEX_CLI_VERSION
                && self.harness.adapter == config.adapter
                && self.model.provider.as_str() == "openai"
                && self.model.spending_budget.is_none()
                && (16..=1_048_576).contains(&self.model.max_output_tokens)
                && matches!(self.model.reasoning.as_str(), "low" | "medium" | "high"),
            "unsupported closed Codex settings"
        );
        ensure!(
            self.columns >= 80
                && self.columns <= 240
                && self.rows > 0
                && self.rows <= 120
                && u32::from(self.columns) * u32::from(self.rows) <= 12000,
            "unsupported Codex geometry"
        );
        ensure!(
            self.host_root.is_absolute()
                && self.host_root.starts_with(&config.state_dir)
                && self.host_root != config.state_dir
                && self.host_root.canonicalize()? == self.host_root
                && !self.host_root.starts_with(&config.git_root),
            "Codex host root must be a separate private state child"
        );
        aq_session_protocol::require_capabilities(
            &self.harness.required_capabilities,
            &config.capabilities,
        )?;
        for capability in self
            .harness
            .required_capabilities
            .iter()
            .chain(config.capabilities.iter())
        {
            ensure!(
                matches!(
                    capability.id.as_str(),
                    "aq.profiled-launch"
                        | "aq.terminal"
                        | "aq.harness.startup-context"
                        | "aq.harness.confined-worktree"
                        | "aq.harness.container-confinement"
                        | "aq.harness.stop"
                ) && capability.version == 1,
                "unsupported mandatory Codex capability"
            );
        }
        // Advertised capabilities may offer both modes; the selected harness may
        // not require both. The frozen manifest must declare exactly one and
        // `prepare_inputs` binds it to these settings; the mapper routes container
        // only through an `AuthorizedConfinement` (adapter/codex.rs).
        ensure!(
            !(self.declares(aq_session_protocol::NATIVE_CONFINEMENT_CAPABILITY)
                && self.declares(aq_session_protocol::CONTAINER_CONFINEMENT_CAPABILITY)),
            "Codex settings must declare exactly one confinement mode"
        );
        Ok(())
    }
    fn declares(&self, id: &str) -> bool {
        self.harness
            .required_capabilities
            .iter()
            .any(|c| c.id.as_str() == id && c.version == 1)
    }
}
/// No Debug/Serialize. Node transport authentication stays in the relay client;
/// the host receives only exact scoped provider material through its owner.
pub struct CodexExecutor {
    relay: RelayHttp,
    profile: Option<Arc<NativeCodexHostProfile>>,
    result_root: Option<PathBuf>,
    installation_reserved_bytes: Option<u64>,
    result_transfer: Option<crate::transfer_client::TransferClient>,
    /// Sandbox-issued proof for container confinement; absent means native only.
    confinement: Option<AuthorizedConfinement>,
    #[cfg(feature = "test-support")]
    development_provider: Option<String>,
}
impl CodexExecutor {
    pub fn configured(worker: &WorkerConfig, config: &NativeConfig) -> Result<Self> {
        let local = matches!(
            &config.executor,
            Some(ExecutorConfig::ChatgptInteractiveResultLocal { .. })
        ) && reqwest::Url::parse(&worker.server_url)
            .is_ok_and(|u| u.scheme() == "http");
        if !local {
            return Self::new(worker);
        }
        ensure!(
            worker.cf_client_id.is_none() && worker.cf_client_secret.is_none(),
            "local interactive node does not use proxy credentials"
        );
        let credential = NodeCredential::new(worker.credential.clone())?;
        Ok(Self {
            relay: RelayHttp::trusted_local_node(&worker.server_url, &credential)?,
            profile: None,
            result_root: None,
            installation_reserved_bytes: None,
            result_transfer: Some(crate::transfer_client::TransferClient::from_config(
                worker, 1,
            )?),
            confinement: None,
            #[cfg(feature = "test-support")]
            development_provider: None,
        })
    }
    pub fn new(worker: &WorkerConfig) -> Result<Self> {
        ensure!(
            worker.cf_client_id.is_none() && worker.cf_client_secret.is_none(),
            "native relay proxy credentials are not supported by this executor"
        );
        let credential = NodeCredential::new(worker.credential.clone())?;
        Ok(Self {
            relay: RelayHttp::new(&worker.server_url, &credential)?,
            profile: None,
            result_root: None,
            installation_reserved_bytes: None,
            result_transfer: Some(crate::transfer_client::TransferClient::from_config(
                worker, 1,
            )?),
            confinement: None,
            #[cfg(feature = "test-support")]
            development_provider: None,
        })
    }
    /// Attach the sandbox's confinement authorization before preflight. A
    /// container-confined harness preflights only with a `Container` proof.
    pub fn with_confinement(mut self, confinement: AuthorizedConfinement) -> Result<Self> {
        ensure!(
            self.profile.is_none() && self.confinement.is_none(),
            "confinement must be attached once, before preflight"
        );
        self.confinement = Some(confinement);
        Ok(self)
    }
    /// Separate installation-cache reservation metadata. This includes the
    /// result client's additional pool and does not reinterpret task memory.
    pub fn installation_reserved_bytes(&self) -> Option<u64> {
        self.installation_reserved_bytes
    }
    #[cfg(feature = "test-support")]
    pub fn development_loopback(worker: &WorkerConfig) -> Result<Self> {
        ensure!(
            worker.cf_client_id.is_none() && worker.cf_client_secret.is_none(),
            "native relay proxy credentials are not supported by this executor"
        );
        let origin = reqwest::Url::parse(&worker.server_url)?;
        ensure!(
            origin.host_str().is_some_and(|host| {
                host.trim_start_matches('[')
                    .trim_end_matches(']')
                    .parse::<std::net::IpAddr>()
                    .is_ok_and(|ip| ip.is_loopback())
            }),
            "fixture native executor requires numeric loopback origin"
        );
        let credential = NodeCredential::new(worker.credential.clone())?;
        let mut executor = Self::from_relay(RelayHttp::development_loopback(
            &worker.server_url,
            &credential,
        )?)?;
        executor.result_transfer = Some(crate::transfer_client::TransferClient::from_config(
            worker, 1,
        )?);
        Ok(executor)
    }
    /// Finite qualification override: still uses actual named installation
    /// preflight and the owned scoped transport, with a numeric-loopback upstream.
    #[cfg(feature = "test-support")]
    pub fn with_development_provider(mut self, origin: &str) -> Result<Self> {
        ensure!(
            self.profile.is_none(),
            "provider override must precede preflight"
        );
        self.development_provider =
            Some(crate::native::adapter::codex::development_origin(origin)?);
        Ok(self)
    }
    /// Joins a separately preflighted, real installed host to the same executor
    /// path for finite loopback qualification; unavailable in production builds.
    #[cfg(all(test, feature = "test-support"))]
    pub(crate) fn development_preflighted(
        relay: RelayHttp,
        profile: Arc<NativeCodexHostProfile>,
    ) -> Result<Self> {
        let mut executor = Self::from_relay(relay)?;
        executor.profile = Some(profile);
        Ok(executor)
    }
    #[cfg(feature = "test-support")]
    fn from_relay(relay: RelayHttp) -> Result<Self> {
        Ok(Self {
            relay,
            profile: None,
            result_root: None,
            installation_reserved_bytes: None,
            result_transfer: None,
            confinement: None,
            #[cfg(feature = "test-support")]
            development_provider: None,
        })
    }
}
impl NativeExecutor for CodexExecutor {
    fn preflight(&mut self, config: &NativeConfig) -> Result<()> {
        ensure!(self.profile.is_none(), "Codex executor already preflighted");
        let interactive_clock = match &config.executor {
            Some(ExecutorConfig::ChatgptInteractiveResultLocal {
                terminal_clock_skew_ms,
                ..
            }) => {
                ensure!(
                    *terminal_clock_skew_ms <= 1000,
                    "interactive operator clock bound"
                );
                Some(*terminal_clock_skew_ms)
            }
            _ => None,
        };
        let (settings, environment, result) = match &config.executor {
            Some(ExecutorConfig::TrustedNodeNamedReadonly {
                settings,
                environment,
            }) => (settings, environment, None),
            Some(ExecutorConfig::TrustedNodeNamedReadonlyResultLocal {
                settings,
                environment,
                client_manifest,
                client_manifest_digest,
                composite_definition,
            }) => (
                settings,
                environment,
                Some((
                    client_manifest,
                    client_manifest_digest,
                    composite_definition,
                )),
            ),
            Some(ExecutorConfig::ChatgptInteractiveResultLocal {
                settings,
                environment,
                client_manifest,
                client_manifest_digest,
                composite_definition,
                ..
            }) => (
                settings,
                environment,
                Some((
                    client_manifest,
                    client_manifest_digest,
                    composite_definition,
                )),
            ),
            Some(ExecutorConfig::ClaudeDockerLocalResultLocal { .. }) => {
                anyhow::bail!("Claude docker-local settings select the Claude executor")
            }
            Some(ExecutorConfig::Codex { .. }) => {
                anyhow::bail!(
                    "legacy sealed-memfd executor is not qualified for Codex tools; no admission attempted"
                )
            }
            None => anyhow::bail!("closed Codex executor settings absent"),
        };
        settings.validate(config)?;
        // Before any installation work: container needs the sandbox's proof.
        let mode = crate::native::adapter::codex::selected_confinement(
            &settings.harness,
            self.confinement.as_ref(),
            interactive_clock.is_some(),
        )?;
        ensure!(
            result.is_none() || mode == ConfinementMode::Native,
            "result-local Codex mapping is qualified for native confinement only"
        );
        ensure!(
            config.adapter.id.as_str() != "aq.codex-result-local-adapter" || result.is_some(),
            "known result adapter requires the result-enabled executor"
        );
        ensure!(
            environment == &settings.harness.environment,
            "trusted-node environment differs from selected harness"
        );
        ensure!(
            result.is_none() || self.result_transfer.is_some(),
            "native result source transport absent"
        );
        let definition = if let Some((_, client_digest, definition_path)) = result {
            let definition =
                ResultAdapterDefinition::decode(&config::private_file(definition_path, 16384)?)?;
            ensure!(
                definition.interactive() == interactive_clock.is_some(),
                "executor and selected result mode differ"
            );
            definition.validate_selected(ExpectedResultAdapter {
                adapter: &config.adapter,
                runtime: &config.runtime,
                environment,
                permission_digest: &settings.permissions_digest,
            })?;
            ensure!(
                definition.client_manifest.digest == *client_digest,
                "result client installation differs from selected adapter"
            );
            Some(definition)
        } else {
            None
        };
        // An artifact reference is not a kernel observation. This retained owner
        // checks cap/NNP and namespace premises; installation loading checks actual
        // readonly mount/FD/path identities. External provisioning qualification
        // remains independently pinned by the selected environment artifact.
        let environment_owner = NamedReadonlyEnvironment::inspect(environment.clone())?;
        let pool = NamedReadonlyExecPool::new(
            CODEX_INSTALLATION_BUDGET_BYTES,
            1,
            environment_owner.clone(),
        )?;
        let manifest =
            NamedExecManifest::decode(&config::private_file(&config.adapter_manifest, 16384)?)?;
        let installed = NamedExecInstallation::open(
            manifest,
            ExpectedAdapter {
                manifest_digest: &config.adapter_manifest_digest,
                adapter: &config.adapter,
                runtime: &config.runtime.runtime,
                qualification: &config.runtime.qualification,
                renderer: &config.renderer,
            },
            environment,
            &pool,
        )?;
        ensure!(
            installed.interactive() == interactive_clock.is_some(),
            "executor and selected named image mode differ"
        );
        let mapper = CodexExecProfile {
            harness: settings.harness.clone(),
            model: settings.model.clone(),
            permissions_digest: settings.permissions_digest.clone(),
            renderer: config.renderer.clone(),
            confinement: self.confinement.clone(),
        };
        let selected_client_manifest = definition.as_ref().map(|d| d.client_manifest.clone());
        let profile = match (result, definition) {
            (Some((client_path, _, _)), Some(definition)) => {
                let manifest =
                    ResultClientManifest::decode(&config::private_file(client_path, 16384)?)?;
                let client = InstalledResultClient::open_selected(
                    manifest,
                    definition.expected_client()?,
                    &definition.client_manifest,
                    &environment_owner,
                )?;
                NativeCodexHostProfile::new_named_result(
                    settings.host_root.clone(),
                    installed,
                    mapper,
                    settings.columns,
                    settings.rows,
                    client,
                    definition,
                )?
            }
            (None, None) => NativeCodexHostProfile::new_named(
                settings.host_root.clone(),
                installed,
                mapper,
                settings.columns,
                settings.rows,
            )?,
            _ => anyhow::bail!("native result factory selection differs"),
        };
        let profile = if let Some(skew) = interactive_clock {
            profile.with_interactive_clock(skew)?
        } else {
            profile
        };
        #[cfg(feature = "test-support")]
        let profile = if let Some(origin) = &self.development_provider {
            profile.with_development_provider(origin)?
        } else {
            profile
        };
        let result_pins = profile.result_installation_pins();
        ensure!(
            result_pins
                .as_ref()
                .is_none_or(|pins| pins.adapter == config.adapter)
                && result_pins.as_ref().map(|pins| &pins.client_manifest)
                    == selected_client_manifest.as_ref(),
            "native result installation differs from selected definition revisions"
        );
        self.installation_reserved_bytes = Some(
            CODEX_INSTALLATION_BUDGET_BYTES
                .checked_add(
                    result_pins
                        .as_ref()
                        .map_or(0, |pins| pins.reserved_client_bytes),
                )
                .context("native installation image reservation overflow")?,
        );
        self.result_root = result_pins.map(|_| config.state_dir.join("result"));
        self.profile = Some(Arc::new(profile));
        Ok(())
    }
    fn execute<'a>(
        &'a mut self,
        pipeline: Arc<InputPipeline>,
        grant: DurablePhaseExecution,
        shutdown: watch::Receiver<bool>,
    ) -> ExecutionFuture<'a> {
        Box::pin(async move {
            ensure!(
                !*shutdown.borrow(),
                "shutdown requested before native launch"
            );
            let profile = self
                .profile
                .as_ref()
                .context("Codex executor not preflighted")?
                .clone();
            let id = grant.history().attempt_id();
            let terminal_clock = profile.terminal_clock();
            let result_pins = profile.result_installation_pins();
            let result_enabled = result_pins.is_some();
            let transfer = if result_enabled {
                Some(
                    self.result_transfer
                        .clone()
                        .context("result source transport absent")?,
                )
            } else {
                None
            };
            let launched = if let Some(pins) = result_pins {
                let root = self
                    .result_root
                    .clone()
                    .context("result profile root absent")?;
                let channel = pipeline
                    .create_result_channel_in_namespace(&grant, root, pins)
                    .await?;
                pipeline
                    .launch_phase_native_result(&grant, profile, channel)
                    .await?
            } else {
                ensure!(
                    self.result_root.is_none(),
                    "result root without selected result profile"
                );
                pipeline.launch_phase_native(&grant, profile).await?
            };
            // No Started publication is invented. The retained closure requires its own
            // no-Started reconciliation owner.
            let what =
                "native host did not establish Running; retained closure requires reconciliation";
            let host = running_or_stopped(&pipeline, id, launched, what).await?;
            drive_running_owned(
                pipeline,
                host,
                grant.deadline_ns(),
                self.relay.clone(),
                shutdown,
                transfer,
                terminal_clock,
                false,
                None,
            )
            .await
        })
    }
}

/// Shared actual-host lifecycle used by the concrete executor and pinned local
/// physical-host fault tests. Opaque LocalPhaseHost cannot be reconstructed.
/// The Running host of a launch, or (after stopping and persisting what did start) `what`.
pub(super) async fn running_or_stopped(
    pipeline: &InputPipeline,
    id: Uuid,
    launched: NativeHostLaunchResult,
    what: &'static str,
) -> Result<crate::native::launch::LocalPhaseHost> {
    let NativeHostLaunchResult::Running(host) = launched else {
        match pipeline.stop_phase_host(id).await? {
            NativeStopOutcome::StoppedPendingJournal(_) => {
                pipeline.persist_phase_host_stop(id).await?
            }
            NativeStopOutcome::Unknown => pipeline.persist_phase_host_uncertain(id).await?,
        }
        anyhow::bail!(what)
    };
    Ok(host)
}
#[cfg(all(test, feature = "test-support"))]
pub(crate) async fn drive_running_with_pre_stop(
    pipeline: Arc<InputPipeline>,
    host: crate::native::launch::LocalPhaseHost,
    deadline_ns: u64,
    relay_client: RelayHttp,
    shutdown: watch::Receiver<bool>,
    pre_stop: PreStop,
) -> Result<()> {
    drive_running_owned(
        pipeline,
        host,
        deadline_ns,
        relay_client,
        shutdown,
        None,
        None,
        false,
        Some(pre_stop),
    )
    .await
}
#[cfg(all(test, feature = "test-support"))]
pub(crate) async fn drive_running(
    pipeline: Arc<InputPipeline>,
    host: crate::native::launch::LocalPhaseHost,
    deadline_ns: u64,
    relay_client: RelayHttp,
    shutdown: watch::Receiver<bool>,
) -> Result<()> {
    drive_running_owned(
        pipeline,
        host,
        deadline_ns,
        relay_client,
        shutdown,
        None,
        None,
        false,
        None,
    )
    .await
}
#[allow(clippy::too_many_arguments)]
/// MS1 docker-local: the driver's exit record decides Success or Failure; a
/// missing or unreadable record (None) leaves the natural end Unknown.
pub(crate) fn driver_proven_cause(
    cause: crate::native::result::ResultStopCause,
    driver: Option<bool>,
) -> crate::native::result::ResultStopCause {
    use crate::native::result::ResultStopCause;
    match driver {
        Some(true) => ResultStopCause::HarnessSucceeded,
        Some(false) => ResultStopCause::Failure,
        None => cause,
    }
}
#[cfg(test)]
mod driver_cause_tests {
    use super::driver_proven_cause;
    use crate::native::launch::driver_exit_success;
    use crate::native::result::ResultStopCause;
    use aq_admission::transport::ProcessOutcome;
    /// MS1 review S5: a missing, unreadable or malformed exit.json is Unknown,
    /// never Success; only an explicit record decides.
    #[test]
    fn missing_or_unreadable_exit_record_maps_to_unknown() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("exit.json");
        assert_eq!(driver_exit_success(&path), None, "missing");
        std::fs::write(&path, b"{not json").unwrap();
        assert_eq!(driver_exit_success(&path), None, "malformed");
        std::fs::write(&path, br#"{"success":"yes"}"#).unwrap();
        assert_eq!(driver_exit_success(&path), None, "wrong type");
        std::fs::write(&path, br#"{"success":false}"#).unwrap();
        assert_eq!(driver_exit_success(&path), Some(false));
        std::fs::write(&path, br#"{"success":true}"#).unwrap();
        assert_eq!(driver_exit_success(&path), Some(true));
        let dirpath = dir.path().join("unreadable");
        std::fs::create_dir(&dirpath).unwrap();
        assert_eq!(
            driver_exit_success(&dirpath),
            None,
            "unreadable (a directory)"
        );
        for natural in [ResultStopCause::Natural, ResultStopCause::ResultRecorded] {
            assert!(matches!(
                driver_proven_cause(natural, None).process_outcome(),
                ProcessOutcome::Unknown
            ));
            assert!(matches!(
                driver_proven_cause(natural, Some(false)).process_outcome(),
                ProcessOutcome::Failed
            ));
            assert!(matches!(
                driver_proven_cause(natural, Some(true)).process_outcome(),
                ProcessOutcome::Success
            ));
        }
    }
}
/// Physical stop of resources outside the host's process tree (a sandbox),
/// run before the host stop is reported. An error means stop uncertain.
pub(crate) type PreStop = Box<
    dyn FnOnce() -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<()>> + Send>> + Send,
>;
#[allow(clippy::too_many_arguments)]
pub(super) async fn drive_running_owned(
    pipeline: Arc<InputPipeline>,
    host: crate::native::launch::LocalPhaseHost,
    deadline_ns: u64,
    relay_client: RelayHttp,
    mut shutdown: watch::Receiver<bool>,
    transfer: Option<crate::transfer_client::TransferClient>,
    terminal_clock: Option<aq_session_host::terminal_io::QualifiedClock>,
    local_renewal: bool,
    pre_stop: Option<PreStop>,
) -> Result<()> {
    use crate::native::result::ResultStopCause;
    let id = host.attempt_id();
    let mut cause = ResultStopCause::Failure;
    let result_enabled = transfer.is_some();
    // Install the independent reader before any network Started wait.
    let mut control_receipts = None;
    let mut operation = async {
        let relay = pipeline.start_phase_output(&host, relay_client)?;
        let state = pipeline.snapshot().await?;
        let terminal_profile = super::profile::TerminalProfile::for_attempt(
            state
                .attempts
                .get(&id)
                .context("native Running attempt absent")?,
        )?;
        tokio::select! {
            receipt = terminal_profile.report_started(&pipeline, id) => { receipt?; },
            _ = stop_requested(&mut shutdown, deadline_ns) => {
                anyhow::bail!("stop requested while Started publication outcome is unknown");
            }
        }
        let control = if let Some(clock) = terminal_clock {
            Some(pipeline.start_phase_control(&host, &relay, clock).await?)
        } else {
            None
        };
        control_receipts = control.clone();
        let controller = control.map(super::publish_output::ControlPublisher::start);
        let _publisher = super::publish_output::OutputPublisher::start(relay);
        let mut renewal_sequence = 1u64;
        let mut next_renewal = std::time::Instant::now() + std::time::Duration::from_secs(30);
        let mut next_cancel_poll = std::time::Instant::now() + std::time::Duration::from_secs(3);
        loop {
            // Node-owned (sandboxed) hosts also observe owner cancellation
            // through the node poll; the session host stops the harness.
            if local_renewal && std::time::Instant::now() >= next_cancel_poll {
                next_cancel_poll = std::time::Instant::now() + std::time::Duration::from_secs(5);
                match tokio::time::timeout(
                    std::time::Duration::from_secs(4),
                    pipeline.poll_cancellation(id),
                )
                .await
                {
                    Ok(Ok(Some(cancellation))) => {
                        tracing::warn!(attempt_id = %id, %cancellation, "owner cancellation observed; stopping the harness");
                        cause = ResultStopCause::Cancelled;
                        break;
                    }
                    Ok(Ok(None)) => {}
                    _ => tracing::debug!(attempt_id = %id, "cancellation poll unavailable"),
                }
            }
            if local_renewal && std::time::Instant::now() >= next_renewal {
                renewal_sequence += 1;
                if let Err(error) = host.renew_local(renewal_sequence, deadline_ns / 1_000_000) {
                    tracing::warn!(attempt_id = %id, "local host permit renewal refused: {error:#}");
                }
                next_renewal = std::time::Instant::now() + std::time::Duration::from_secs(30);
            }
            if controller
                .as_ref()
                .is_some_and(|c| c.authentication_required())
            {
                anyhow::bail!("Codex interactive authentication required; session quarantined");
            }
            let cancelled = *shutdown.borrow();
            if let Some(stop) = loop_stop_cause(
                cancelled,
                || Ok(super::super::http::boot_ns()? >= deadline_ns),
                || Ok(host.observe()?.phase == HostPhase::Running),
                result_enabled.then(|| pipeline.poll_result_claim(id)),
            )
            .await?
            {
                cause = stop;
                break;
            }
            tokio::select! {
                _=tokio::time::sleep(std::time::Duration::from_millis(250))=>{},
                changed=shutdown.changed()=>{if changed.is_err(){break;}}
            }
        }
        Ok::<(), anyhow::Error>(())
    }
    .await;
    // MS1 docker-local: only the driver's proven harness exit decides
    // success or failure; an unproven natural end stays Unknown.
    if local_renewal
        && matches!(
            cause,
            ResultStopCause::Natural | ResultStopCause::ResultRecorded
        )
    {
        cause = driver_proven_cause(cause, host.claude_driver_succeeded());
    }
    // Stop what the host's proof does not cover (the sandbox) first, so the
    // host's short stop grace cannot leave it running behind a Stopped report.
    let outside_stopped = match pre_stop {
        Some(stop) => stop().await,
        None => Ok(()),
    };
    // Creating this future fences launch/current transport synchronously.
    // Physical cleanup must not wait for a slow local projection fsync.
    let stop = pipeline.stop_phase_host(id);
    let (intake_closed, stopped_physical) = tokio::join!(pipeline.close_result_intake(id), stop);
    // Neither a relay error nor an uncertain ACK can bypass real stop.
    match stopped_physical? {
        NativeStopOutcome::StoppedPendingJournal(_) => pipeline.persist_phase_host_stop(id).await?,
        NativeStopOutcome::Unknown => {
            pipeline.persist_phase_host_uncertain(id).await?;
            anyhow::bail!("native stop uncertain; retained history requires reconciliation")
        }
    }
    if let Err(error) = outside_stopped {
        // The host is stopped, but a sandbox may still run: never report
        // Stopped (the server then records no stop, so B4 cannot release the
        // task). reconcile-native replays Stopped once the sandbox stop is
        // confirmed.
        pipeline.persist_phase_host_uncertain(id).await?;
        return Err(error.context("sandbox stop uncertain; Stopped withheld for reconciliation"));
    }
    // Stop and its journal precede the bounded metadata-only final flush.
    // A failed flush retains the execution error but does not gate B1 recording.
    if let Some(control) = control_receipts
        && let Err(error) = control.flush_control_receipts().await
        && operation.is_ok()
    {
        operation = Err(error);
    }
    let stopped = async {
        let state = pipeline.snapshot().await?;
        let terminal_profile = super::profile::TerminalProfile::for_attempt(
            state
                .attempts
                .get(&id)
                .context("native stopped attempt absent")?,
        )?;
        terminal_profile.report_stopped(&pipeline, id).await
    }
    .await;
    if let Some(transfer) = transfer {
        if stopped.is_err() {
            let _ = pipeline
                .result_needs_reconciliation(id, aq_agent_result_local::RecoveryCode::StopUnknown)
                .await;
        }
        stopped?;
        intake_closed?;
        let result = pipeline
            .retain_stopped_result(id, cause)
            .await?
            .context("explicit local result claim absent; stopped attempt retained")?;
        let source = async {
            pipeline.capture_result_source(&result).await?;
            // Original source limits also bound the number of chunks. Each
            // finite request preserves the exact original operation on error.
            let snapshot = pipeline.snapshot().await?;
            let max_steps = crate::native::result::transfer_bound(
                snapshot.attempts[&id]
                    .result
                    .as_ref()
                    .context("result history absent")?,
            )?;
            for _ in 0..max_steps {
                if pipeline
                    .transfer_result_source_step(&result, &transfer)
                    .await?
                {
                    return Ok::<(), anyhow::Error>(());
                }
            }
            anyhow::bail!("native result source transfer step bound exhausted")
        }
        .await;
        if source.is_err() {
            let _ = pipeline
                .result_needs_reconciliation(
                    id,
                    aq_agent_result_local::RecoveryCode::SourceRetentionUnknown,
                )
                .await;
        }
        source?;
        let report = pipeline.record_result_report(&result).await;
        if report.is_err() {
            let _ = pipeline
                .result_needs_reconciliation(id, aq_agent_result_local::RecoveryCode::ReportUnknown)
                .await;
        }
        report?;
    } else {
        intake_closed?;
        stopped?;
    }
    operation?;
    Ok(())
}

/// One run-loop decision. Cancellation, deadline expiry and natural pane exit
/// keep their precedence and are observed before the result claim is polled, so
/// an expired grant is never turned into a claim-recording error. A durably
/// recorded in-session result claim then ends the session at once instead of
/// holding it open until the wall deadline and reporting Expired (I0 run 13c).
async fn loop_stop_cause<P>(
    cancelled: bool,
    expired: impl FnOnce() -> Result<bool>,
    running: impl FnOnce() -> Result<bool>,
    poll_result: Option<P>,
) -> Result<Option<crate::native::result::ResultStopCause>>
where
    P: std::future::Future<Output = Result<bool>>,
{
    use crate::native::result::ResultStopCause;
    if cancelled {
        return Ok(Some(ResultStopCause::Cancelled));
    }
    if expired()? {
        return Ok(Some(ResultStopCause::Expired));
    }
    if !running()? {
        return Ok(Some(ResultStopCause::Natural));
    }
    if let Some(poll) = poll_result
        && poll.await?
    {
        return Ok(Some(ResultStopCause::ResultRecorded));
    }
    Ok(None)
}

async fn stop_requested(shutdown: &mut watch::Receiver<bool>, deadline_ns: u64) {
    loop {
        if *shutdown.borrow() || !super::super::http::boot_ns().is_ok_and(|now| now < deadline_ns) {
            return;
        }
        tokio::select! {
            _ = tokio::time::sleep(std::time::Duration::from_millis(250)) => {},
            changed = shutdown.changed() => { if changed.is_err() { return; } }
        }
    }
}

#[cfg(test)]
mod tests;
