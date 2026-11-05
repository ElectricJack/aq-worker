//! MS1 executor: Claude Code 2.1.318 in a docker-local sandbox, driven from the
//! node through the existing native pipeline (grant, Started/Stopped, result
//! channel, source capture and report are unchanged).
use super::*;
use crate::native::{
    adapter::result_profile::{ExpectedResultAdapter, ResultAdapterDefinition},
    launch::{
        NativeHostLaunchResult, NativeStopOutcome,
        claude_docker::{ClaudeDockerHostConfig, ClaudeDockerHostProfile},
    },
    result::ResultInstallationPins,
};
use aq_session_protocol::{ArtifactRef, Digest};
use aq_terminal_bridge::http::{NodeCredential, RelayHttp};
use serde::Deserialize;
use std::path::PathBuf;

#[derive(Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ClaudeDockerSettings {
    pub host: ClaudeDockerHostConfig,
    pub environment: ArtifactRef,
    pub permissions_digest: Digest,
}

pub struct ClaudeDockerExecutor {
    node_id: uuid::Uuid,
    relay: RelayHttp,
    transfer: crate::transfer_client::TransferClient,
    profile: Option<Arc<ClaudeDockerHostProfile>>,
    pins: Option<ResultInstallationPins>,
    result_root: Option<PathBuf>,
}
impl ClaudeDockerExecutor {
    pub fn configured(worker: &WorkerConfig) -> Result<Self> {
        ensure!(
            worker.cf_client_id.is_none() && worker.cf_client_secret.is_none(),
            "local Claude docker-local node does not use proxy credentials"
        );
        let credential = NodeCredential::new(worker.credential.clone())?;
        let local = reqwest::Url::parse(&worker.server_url).is_ok_and(|u| u.scheme() == "http");
        Ok(Self {
            node_id: worker.node_id,
            relay: if local {
                RelayHttp::trusted_local_node(&worker.server_url, &credential)?
            } else {
                RelayHttp::new(&worker.server_url, &credential)?
            },
            transfer: crate::transfer_client::TransferClient::from_config(worker, 1)?,
            profile: None,
            pins: None,
            result_root: None,
        })
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
            permission_digest: &settings.permissions_digest,
        })?;
        let pins = ResultInstallationPins {
            adapter: config.adapter.clone(),
            contract: definition.contract.clone(),
            client_manifest: definition.client_manifest.clone(),
            client_executable: definition.client_executable.clone(),
            environment: definition.environment.clone(),
            recipe: definition.recipe.clone(),
            permission_digest: definition.permission_digest.clone(),
            reserved_client_bytes: definition.reserved_client_bytes,
        };
        pins.validate()?;
        ensure!(
            settings.host.renderer == config.renderer
                && settings.host.root.starts_with(&config.state_dir)
                && settings.host.root != config.state_dir
                && !settings.host.root.starts_with(&config.git_root),
            "Claude host root must be a separate private state child"
        );
        self.profile = Some(Arc::new(ClaudeDockerHostProfile::new(
            settings.host.clone(),
        )?));
        self.pins = Some(pins);
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
            let pins = self.pins.clone().context("Claude result pins absent")?;
            let root = self.result_root.clone().context("result root absent")?;
            let id = grant.history().attempt_id();
            let channel = pipeline
                .create_result_channel_in_namespace(&grant, root, pins)
                .await?;
            let launched = pipeline
                .launch_phase_claude_docker_result(&grant, profile.clone(), channel)
                .await?;
            let host = match launched {
                NativeHostLaunchResult::Running(host) => host,
                NativeHostLaunchResult::StoppedBeforeStarted(_)
                | NativeHostLaunchResult::Unknown(_) => {
                    match pipeline.stop_phase_host(id).await? {
                        NativeStopOutcome::StoppedPendingJournal(_) => {
                            pipeline.persist_phase_host_stop(id).await?
                        }
                        NativeStopOutcome::Unknown => {
                            pipeline.persist_phase_host_uncertain(id).await?
                        }
                    }
                    anyhow::bail!(
                        "Claude driver host did not establish Running; retained closure requires reconciliation"
                    )
                }
            };
            let driven = super::codex::drive_running_owned(
                pipeline,
                host,
                grant.deadline_ns(),
                self.relay.clone(),
                shutdown,
                Some(self.transfer.clone()),
                None,
                true,
            )
            .await;
            // The host stop kills the driver; make sure its sandbox is stopped
            // too (preserving its disk). A destroyed or exited one is left as is.
            if let Err(error) = stop_attempt_sandbox(&profile, self.node_id, id).await {
                tracing::warn!(attempt_id = %id, "sandbox stop after host stop failed: {error:#}");
            }
            driven
        })
    }
}

/// Stops (never destroys) any still-running sandbox owned by this attempt.
async fn stop_attempt_sandbox(
    profile: &ClaudeDockerHostProfile,
    node: uuid::Uuid,
    attempt: uuid::Uuid,
) -> Result<()> {
    use crate::backend::{PlacementBackend, SandboxState, docker_local::DockerLocalBackend};
    let mut backend =
        DockerLocalBackend::connect_unix(profile.config().docker_local.clone()).await?;
    for entry in backend.list_owned(node).await? {
        if entry.sandbox.owner.attempt_id == attempt && entry.state == SandboxState::Running {
            let stopped = backend.stop(&entry.sandbox).await;
            tracing::warn!(attempt_id = %attempt, sandbox = %entry.sandbox.backend_id,
                confirmed = stopped.is_confirmed(), "attempt sandbox stopped after host stop");
        }
    }
    Ok(())
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
