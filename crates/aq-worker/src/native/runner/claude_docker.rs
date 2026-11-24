//! MS1 executor: Claude Code 2.1.318 in a docker-local sandbox, driven from the
//! node through the existing native pipeline (grant, Started/Stopped, result
//! channel, source capture and report are unchanged).
use super::*;
mod sandbox;
use crate::native::{
    adapter::result_profile::{ExpectedResultAdapter, ResultAdapterDefinition},
    launch::claude_docker::{
        ClaudeDockerHostConfig, ClaudeDockerHostProfile, CodexDockerHost, DockerHarness,
        select_harness,
    },
    result::ResultInstallationPins,
};
use aq_session_protocol::{ArtifactRef, Digest};
use aq_terminal_bridge::http::{NodeCredential, RelayHttp};
pub(crate) use sandbox::stop_attempt_sandbox;
pub use sandbox::{
    destroy_claimed_sandboxes, release_claimed_sandboxes, release_preserved_sandbox,
    write_back_login,
};
use serde::Deserialize;
use std::path::PathBuf;

#[derive(Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ClaudeDockerSettings {
    pub host: ClaudeDockerHostConfig,
    pub environment: ArtifactRef,
    /// Accepted and ignored since spec 0014 (any offered plan is launched). It can
    /// be deleted from templates; it is still read so installed ones load.
    #[serde(default)]
    pub permissions_digest: Option<Digest>,
    /// Codex in the same sandbox template, for offers that select the `codex`
    /// harness. Absent: this node launches Claude only.
    #[serde(default)]
    pub codex: Option<CodexDockerSettings>,
}

/// The node's Codex harness beside Claude: the same docker-local host, sandbox
/// template and driver, with Codex's own result adapter selection.
#[derive(Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CodexDockerSettings {
    /// The selected Codex docker-local result adapter
    /// (`aq.codex-docker-result-local-adapter`) that Codex offers name.
    pub adapter: ArtifactRef,
    /// The template environment that Codex offers name (`harness.environment`).
    pub environment: ArtifactRef,
    /// Its exact canonical result adapter Definition bytes (0600 file).
    pub composite_definition: PathBuf,
    /// Codex CLI inside the template image (`/opt/aq/bin/codex`).
    pub executable: PathBuf,
    /// The pinned Codex helper bundle (`packaging/native-runtime`, the directory
    /// `[manual.codex] runtime_bundle` names). Codex's shell tool needs it.
    #[serde(default)]
    pub runtime_bundle: Option<PathBuf>,
}

/// The result pins of one selected docker-local result adapter definition.
fn result_pins(
    adapter: &ArtifactRef,
    definition: &ResultAdapterDefinition,
) -> Result<ResultInstallationPins> {
    let pins =
        ResultInstallationPins::of(adapter, definition, definition.permission_digest.clone());
    pins.validate()?;
    Ok(pins)
}
/// Codex pins from the node's Codex settings: a Codex docker-local definition,
/// selected independently of the Claude one.
fn codex_result_pins(
    config: &NativeConfig,
    codex: &CodexDockerSettings,
) -> Result<ResultInstallationPins> {
    ensure!(
        codex.adapter != config.adapter && codex.executable.is_absolute(),
        "Codex docker-local settings must select their own result adapter"
    );
    let definition = ResultAdapterDefinition::decode(&config::private_file(
        &codex.composite_definition,
        16384,
    )?)?;
    ensure!(
        definition.is_codex_docker(),
        "selected Codex result adapter is not the Codex docker-local profile"
    );
    definition.validate_selected(ExpectedResultAdapter {
        adapter: &codex.adapter,
        runtime: &config.runtime,
        environment: &codex.environment,
    })?;
    result_pins(&codex.adapter, &definition)
}
/// Spec 0017: an offer in an admitted environment names that environment's result
/// adapter Definition (`harness.adapter`) and definition (`harness.environment`). The
/// node's installed pins hold for them; the result channel then requires the offer's
/// retained Definition to select exactly these pins, else it refuses before launch.
#[cfg(feature = "project_environments")]
pub fn offered_pins(
    installed: &ResultInstallationPins,
    offered: &aq_session_protocol::HarnessSettings,
) -> ResultInstallationPins {
    let (adapter, environment) = (offered.adapter.clone(), offered.environment.clone());
    ResultInstallationPins {
        adapter,
        environment,
        ..installed.clone()
    }
}
/// The result pins for the harness an offer selects (`harness.harness`).
pub fn launch_pins(
    harness: &str,
    claude: Option<&ResultInstallationPins>,
    codex: Option<&ResultInstallationPins>,
) -> Result<(DockerHarness, ResultInstallationPins)> {
    let selected = select_harness(harness, codex.is_some())?;
    let pins = match selected {
        DockerHarness::Claude => claude.context("Claude result pins absent")?,
        DockerHarness::Codex => codex.context("Codex result pins absent")?,
    };
    Ok((selected, pins.clone()))
}

pub struct ClaudeDockerExecutor {
    node_id: uuid::Uuid,
    relay: RelayHttp,
    transfer: crate::transfer_client::TransferClient,
    profile: Option<Arc<ClaudeDockerHostProfile>>,
    pins: Option<ResultInstallationPins>,
    /// Set when the node also runs Codex (`ClaudeDockerSettings::codex`).
    codex_pins: Option<ResultInstallationPins>,
    result_root: Option<PathBuf>,
    conversation_outbox: Option<PathBuf>,
    /// The node's enrollment (origin, credential, Access), for the agent channel.
    issuance: aq_agent_channel::NodeLink,
    /// The node's observation store, set by the service before the preflight.
    pub observations: Option<aq_observation_store::node::Observations>,
    pub transcript_bound: Option<aq_handoff_note::TranscriptBound>,
}
impl ClaudeDockerExecutor {
    pub fn configured(worker: &WorkerConfig) -> Result<Self> {
        let access = match (&worker.cf_client_id, &worker.cf_client_secret) {
            (Some(id), Some(secret)) => Some((id, secret)),
            (None, None) => None,
            _ => anyhow::bail!("Access client id and secret must both be set"),
        };
        let credential = NodeCredential::new(worker.credential.clone())?;
        let local = reqwest::Url::parse(&worker.server_url).is_ok_and(|u| u.scheme() == "http");
        let mut relay = if local {
            RelayHttp::trusted_local_node(&worker.server_url, &credential)?
        } else {
            RelayHttp::new(&worker.server_url, &credential)?
        };
        if let Some((id, secret)) = access {
            relay = relay.with_access(id, secret)?;
        }
        Ok(Self {
            node_id: worker.node_id,
            relay,
            transfer: crate::transfer_client::TransferClient::from_config(worker, 1)?,
            profile: None,
            pins: None,
            codex_pins: None,
            result_root: None,
            conversation_outbox: None,
            observations: None,
            transcript_bound: None,
            issuance: aq_agent_channel::NodeLink::new(
                &worker.server_url,
                &worker.credential,
                worker.cf_client_id.as_deref(),
                worker.cf_client_secret.as_deref(),
            )?,
        })
    }
}
impl ClaudeDockerExecutor {
    pub fn with_conversation_outbox(mut self, directory: Option<PathBuf>) -> Self {
        self.conversation_outbox = directory;
        self
    }
}
impl NativeExecutor for ClaudeDockerExecutor {
    fn preflight(&mut self, config: &NativeConfig) -> Result<()> {
        ensure!(
            self.profile.is_none(),
            "Claude executor already preflighted"
        );
        let Some(super::codex::ExecutorConfig::ClaudeDockerLocalResultLocal {
            settings,
            composite_definition,
        }) = &config.executor
        else {
            anyhow::bail!("Claude docker-local executor settings absent");
        };
        let definition =
            ResultAdapterDefinition::decode(&config::private_file(composite_definition, 16384)?)?;
        ensure!(
            definition.is_claude_docker(),
            "selected result adapter is not the Claude docker-local profile"
        );
        definition.validate_selected(ExpectedResultAdapter {
            adapter: &config.adapter,
            runtime: &config.runtime,
            environment: &settings.environment,
        })?;
        let pins = result_pins(&config.adapter, &definition)?;
        let codex_pins = settings
            .codex
            .as_ref()
            .map(|codex| codex_result_pins(config, codex))
            .transpose()?;
        // The service keeps it outside the state, in a short runtime dir (socket paths).
        ensure!(
            settings.host.renderer == config.renderer
                && !config.state_dir.starts_with(&settings.host.root)
                && !settings.host.root.starts_with(&config.git_root),
            "Claude host root must be separate from the state and Git roots"
        );
        let mut profile = ClaudeDockerHostProfile::new(settings.host.clone())?
            .with_issuance(self.issuance.clone())
            .with_conversation_outbox(self.conversation_outbox.clone());
        if let Some(codex) = &settings.codex {
            profile = profile.with_codex(CodexDockerHost {
                executable: codex.executable.clone(),
                runtime_bundle: codex.runtime_bundle.clone(),
            })?;
        }
        profile.observations = self.observations.clone();
        profile.transcript_bound = self.transcript_bound;
        self.profile = Some(Arc::new(profile));
        self.pins = Some(pins);
        self.codex_pins = codex_pins;
        self.result_root = Some(config.state_dir.join("result"));
        Ok(())
    }
    fn execute<'a>(
        &'a mut self,
        pipeline: Arc<InputPipeline>,
        grant: DurablePhaseExecution,
        shutdown: watch::Receiver<bool>,
    ) -> ExecutionFuture<'a> {
        Box::pin(async move {
            ensure!(!*shutdown.borrow(), "shutdown requested before launch");
            let profile = self
                .profile
                .clone()
                .context("Claude executor not preflighted")?;
            let id = grant.history().attempt_id();
            // The offer selects the harness; the launch profile maps the same manifest.
            let harness = {
                let state = pipeline.snapshot().await?;
                let attempt = state.attempts.get(&id).context("native attempt absent")?;
                let m = &attempt.reservation.offer.offered.offer.manifest;
                m.harness.clone()
            };
            let (_, mut pins) = launch_pins(
                harness.harness.as_str(),
                self.pins.as_ref(),
                self.codex_pins.as_ref(),
            )?;
            // Spec 0017: held for the whole attempt; an unheld environment is refused here.
            #[cfg(feature = "project_environments")]
            let _lease = {
                let held = &profile.config().docker_local.environments;
                let lease = held.select(&pins.environment, &harness.environment)?;
                // Boot offers keep the installed pins, so their adapter is compared exactly.
                if lease.is_some() {
                    pins = offered_pins(&pins, &harness);
                }
                lease
            };
            let root = self.result_root.clone().context("result root absent")?;
            pins.permission_digest = pipeline.phase_permission_digest(&grant).await?;
            let channel = pipeline
                .create_result_channel_in_namespace(&grant, root, pins)
                .await?;
            let launched = pipeline
                .launch_phase_claude_docker_result(&grant, profile.clone(), channel)
                .await?;
            let what = "Claude driver host did not establish Running; retained closure requires reconciliation";
            let host = super::codex::running_or_stopped(&pipeline, id, launched, what).await?;
            // MS1 review S2: the host's stop proof does not cover the sandbox,
            // and its short stop grace kills the driver first. So the sandbox
            // stop is confirmed BEFORE the host stop; else Stopped is withheld.
            let driver_dir = host.claude_driver_dir();
            let docker_local = profile.config().docker_local.clone();
            let (docker, node) = (docker_local.clone(), self.node_id);
            let pre_stop = sandbox::before_host_stop(
                docker_local,
                (node, id),
                driver_dir.clone(),
                self.issuance.clone(),
            );
            let driven = super::codex::RunningHost::new(pipeline, host, grant.deadline_ns())
                .drive(
                    self.relay.clone(),
                    shutdown,
                    super::codex::DriveOptions {
                        transfer: Some(self.transfer.clone()),
                        local_renewal: true,
                        pre_stop: Some(pre_stop),
                        ..Default::default()
                    },
                )
                .await;
            if let Some(dir) = &driver_dir {
                crate::native::launch::wipe_claude_driver_secrets(dir);
            }
            // Retained and reported, so claimed: the stopped sandbox goes unless the driver
            // preserved it (work never copied back). One retry; a service slot's settle
            // (release_claimed_sandboxes) or the operator releases what is left.
            let host_root = driver_dir.as_deref().and_then(std::path::Path::parent);
            if let (Ok(()), Some(root)) = (&driven, host_root) {
                sandbox::release_reported_sandbox(&docker, root, node, id).await;
            }
            driven
        })
    }
}

/// Installed executor selected by the operator configuration.
pub enum InstalledExecutor {
    Codex(Box<CodexExecutor>),
    Claude(Box<ClaudeDockerExecutor>),
}
impl InstalledExecutor {
    pub fn configured(worker: &WorkerConfig, config: &NativeConfig) -> Result<Option<Self>> {
        Ok(match &config.executor {
            None => None,
            Some(super::codex::ExecutorConfig::ClaudeDockerLocalResultLocal { .. }) => Some(
                Self::Claude(Box::new(ClaudeDockerExecutor::configured(worker)?)),
            ),
            Some(_) => Some(Self::Codex(Box::new(CodexExecutor::configured(
                worker, config,
            )?))),
        })
    }
}
impl NativeExecutor for InstalledExecutor {
    fn preflight(&mut self, config: &NativeConfig) -> Result<()> {
        match self {
            Self::Codex(e) => e.preflight(config),
            Self::Claude(e) => e.preflight(config),
        }
    }
    fn execute<'a>(
        &'a mut self,
        pipeline: Arc<InputPipeline>,
        grant: DurablePhaseExecution,
        shutdown: watch::Receiver<bool>,
    ) -> ExecutionFuture<'a> {
        match self {
            Self::Codex(e) => e.execute(pipeline, grant, shutdown),
            Self::Claude(e) => e.execute(pipeline, grant, shutdown),
        }
    }
}

#[cfg(test)]
mod sandbox_stop_tests {
    use super::sandbox::stop_owned_sandboxes;
    use crate::backend::{
        CreateRequest, FakeBackend, Fault, Ownership, PlacementBackend, PrivateMaterial,
        SandboxState, TemplateDigest,
    };
    use uuid::Uuid;
    const TEMPLATE: &str =
        "sha256:1111111111111111111111111111111111111111111111111111111111111111";
    async fn running(
        backend: &mut FakeBackend,
        node: Uuid,
        attempt: Uuid,
    ) -> crate::backend::SandboxIdentity {
        let owner = Ownership {
            node_id: node,
            boot_id: Uuid::new_v4(),
            attempt_id: attempt,
        };
        let request = CreateRequest::new(
            owner,
            TemplateDigest::parse(TEMPLATE).unwrap(),
            u64::MAX / 2,
            vec![],
        );
        let sandbox = backend.create(&request).await.unwrap();
        backend
            .deliver_private_material(&sandbox, PrivateMaterial::new(b"fixture".to_vec()))
            .await
            .unwrap();
        backend.start_runner(&sandbox).await.unwrap();
        assert_eq!(
            backend.inspect(&sandbox).await.unwrap().state,
            SandboxState::Running
        );
        sandbox
    }

    /// MS1 review S2: the attempt's sandbox is stopped and confirmed before the
    /// host stop is reported; an unconfirmed stop is an error (stop uncertain),
    /// and other attempts' sandboxes are never touched.
    #[tokio::test]
    async fn attempt_sandbox_stop_must_be_confirmed() {
        let (node, attempt, other) = (Uuid::new_v4(), Uuid::new_v4(), Uuid::new_v4());
        let mut backend = FakeBackend::default();
        let mine = running(&mut backend, node, attempt).await;
        let theirs = running(&mut backend, node, other).await;
        stop_owned_sandboxes(&mut backend, node, attempt)
            .await
            .unwrap();
        assert_eq!(
            backend.inspect(&mine).await.unwrap().state,
            SandboxState::Stopped
        );
        assert_eq!(
            backend.inspect(&theirs).await.unwrap().state,
            SandboxState::Running
        );
        // Already stopped: nothing to do.
        stop_owned_sandboxes(&mut backend, node, attempt)
            .await
            .unwrap();

        backend.inject(Fault::StopFailure);
        let error = stop_owned_sandboxes(&mut backend, node, other)
            .await
            .unwrap_err();
        assert!(format!("{error:#}").contains("stop uncertain"), "{error:#}");
        assert_eq!(
            backend.inspect(&theirs).await.unwrap().state,
            SandboxState::Running
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn worker(id: Option<&str>, secret: Option<&str>) -> WorkerConfig {
        WorkerConfig {
            server_url: "https://aq.example.com".into(),
            node_id: uuid::Uuid::from_u128(1),
            credential: "node-credential".into(),
            cf_client_id: id.map(Into::into),
            cf_client_secret: secret.map(Into::into),
        }
    }
    #[test]
    fn claude_executor_accepts_access_enrollment_and_refuses_half() {
        assert!(ClaudeDockerExecutor::configured(&worker(Some("id"), Some("s3cret"))).is_ok());
        assert!(ClaudeDockerExecutor::configured(&worker(None, None)).is_ok());
        for half in [worker(Some("id"), None), worker(None, Some("s"))] {
            let e = ClaudeDockerExecutor::configured(&half).err().unwrap();
            assert!(e.to_string().contains("must both be set"), "{e:#}");
        }
    }
}
