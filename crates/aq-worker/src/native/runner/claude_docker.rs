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
            // MS1 review S2: the host's stop proof covers only its own process
            // tree, not the sandbox container, and the host kills the driver
            // within its short stop grace while a container stop takes longer.
            // So the sandbox is stopped (and confirmed) BEFORE the host stop is
            // reported; an unconfirmed stop withholds Stopped (stop uncertain).
            let docker_local = profile.config().docker_local.clone();
            let node = self.node_id;
            let pre_stop: super::codex::PreStop = Box::new(move || {
                Box::pin(async move { stop_attempt_sandbox(&docker_local, node, id).await })
            });
            super::codex::drive_running_owned(
                pipeline,
                host,
                grant.deadline_ns(),
                self.relay.clone(),
                shutdown,
                Some(self.transfer.clone()),
                None,
                true,
                Some(pre_stop),
            )
            .await
        })
    }
}

/// Stops (never destroys) every sandbox of this attempt that may still run,
/// and fails unless each stop is backend-confirmed. A sandbox already stopped,
/// archived or destroyed is left as is.
pub(crate) async fn stop_attempt_sandbox(
    docker_local: &crate::backend::docker_local::DockerLocalConfig,
    node: uuid::Uuid,
    attempt: uuid::Uuid,
) -> Result<()> {
    let mut backend =
        crate::backend::docker_local::DockerLocalBackend::connect_unix(docker_local.clone())
            .await?;
    stop_owned_sandboxes(&mut backend, node, attempt).await
}

pub(crate) async fn stop_owned_sandboxes<B: crate::backend::PlacementBackend>(
    backend: &mut B,
    node: uuid::Uuid,
    attempt: uuid::Uuid,
) -> Result<()> {
    use crate::backend::SandboxState;
    for entry in backend.list_owned(node).await? {
        if entry.sandbox.owner.attempt_id != attempt
            || matches!(
                entry.state,
                SandboxState::Stopped | SandboxState::Archived | SandboxState::Destroyed
            )
        {
            continue;
        }
        let stopped = backend.stop(&entry.sandbox).await;
        tracing::warn!(attempt_id = %attempt, sandbox = %entry.sandbox.backend_id,
            confirmed = stopped.is_confirmed(), "attempt sandbox stop");
        ensure!(
            stopped.is_confirmed(),
            "sandbox {} stop unconfirmed; the harness may still run (stop uncertain)",
            entry.sandbox.backend_id
        );
    }
    Ok(())
}

/// Operator release of a preserved (stopped, never running) docker-local
/// sandbox of one attempt (SBX5 "an operator explicitly releases it"). The
/// sandbox is destroyed only with the operator's non-empty release reason as
/// destroy evidence; a running or unknown sandbox is refused. Returns the
/// released backend identifiers.
pub async fn release_preserved_sandbox(
    config: &NativeConfig,
    node: uuid::Uuid,
    attempt: uuid::Uuid,
    reason: &str,
) -> Result<Vec<String>> {
    use crate::backend::{
        Confirmation, DestroyAuthorization, PlacementBackend, SandboxState,
        docker_local::DockerLocalBackend,
    };
    let Some(super::codex::ExecutorConfig::ClaudeDockerLocalResultLocal { settings, .. }) =
        &config.executor
    else {
        anyhow::bail!("not a Claude docker-local node configuration");
    };
    ensure!(
        !reason.trim().is_empty() && reason.len() <= 512,
        "an operator release needs a reason (at most 512 bytes)"
    );
    let mut backend = DockerLocalBackend::connect_unix(settings.host.docker_local.clone()).await?;
    let mut released = Vec::new();
    for entry in backend.list_owned(node).await? {
        if entry.sandbox.owner.attempt_id != attempt {
            continue;
        }
        ensure!(
            entry.state == SandboxState::Stopped,
            "sandbox {} is {:?}, not stopped; it is never released while it may run",
            entry.sandbox.backend_id,
            entry.state
        );
        let authorization = DestroyAuthorization::operator_released(
            entry.sandbox.clone(),
            format!("operator-release:{attempt}:{reason}"),
        )?;
        match backend.destroy(&entry.sandbox, &authorization).await {
            Confirmation::Confirmed(_) => released.push(entry.sandbox.backend_id.clone()),
            other => anyhow::bail!(
                "destroy of {} not confirmed: {other:?}",
                entry.sandbox.backend_id
            ),
        }
    }
    ensure!(
        !released.is_empty(),
        "no preserved sandbox of attempt {attempt} on this node"
    );
    Ok(released)
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
    use super::stop_owned_sandboxes;
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
