//! Concrete closed runtime joining the installed image and native host owner.
use super::*;
use crate::native::{
    adapter::{
        ExpectedAdapter,
        codex::CodexExecProfile,
        native_exec::{NativeExecInstallation, NativeExecManifest},
    },
    launch::{NativeCodexHostProfile, NativeHostLaunchResult, NativeStopOutcome},
};
use aq_session_host::{HostPhase, observed::NativeExecPool};
use aq_session_protocol::{Digest, HarnessSettings, ModelSettings};
use aq_terminal_bridge::http::{NodeCredential, RelayHttp};
use serde::Deserialize;
use std::path::PathBuf;

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
                        | "aq.harness.stop"
                ) && capability.version == 1,
                "unsupported mandatory Codex capability"
            );
        }
        Ok(())
    }
}
/// No Debug/Serialize. Node transport authentication stays in the relay client;
/// the host receives only exact scoped provider material through its owner.
pub struct CodexExecutor {
    relay: RelayHttp,
    pool: NativeExecPool,
    profile: Option<Arc<NativeCodexHostProfile>>,
}
impl CodexExecutor {
    pub fn new(worker: &WorkerConfig) -> Result<Self> {
        ensure!(
            worker.cf_client_id.is_none() && worker.cf_client_secret.is_none(),
            "native relay proxy credentials are not supported by this executor"
        );
        let credential = NodeCredential::new(worker.credential.clone())?;
        let _ = RelayHttp::new(&worker.server_url, &credential)?;
        // The pinned CLI mapper has not yet established effective enforcement
        // of the sealed model output limit. Executable pins do not supply that
        // missing capability; ordinary production admission remains refused.
        anyhow::bail!("native Codex model output bound is not qualified; no admission attempted")
    }
    #[cfg(any(test, feature = "test-support"))]
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
        Self::from_relay(RelayHttp::development_loopback(
            &worker.server_url,
            &credential,
        )?)
    }
    #[cfg(any(test, feature = "test-support"))]
    fn from_relay(relay: RelayHttp) -> Result<Self> {
        Ok(Self {
            relay,
            pool: NativeExecPool::new(512 * 1024 * 1024, 1)?,
            profile: None,
        })
    }
}
impl NativeExecutor for CodexExecutor {
    fn preflight(&mut self, config: &NativeConfig) -> Result<()> {
        ensure!(self.profile.is_none(), "Codex executor already preflighted");
        let Some(ExecutorConfig::Codex { settings }) = &config.executor else {
            anyhow::bail!("closed Codex executor settings absent")
        };
        settings.validate(config)?;
        let manifest =
            NativeExecManifest::decode(&config::private_file(&config.adapter_manifest, 16384)?)?;
        let installed = NativeExecInstallation::open(
            manifest,
            ExpectedAdapter {
                manifest_digest: &config.adapter_manifest_digest,
                adapter: &config.adapter,
                runtime: &config.runtime.runtime,
                qualification: &config.runtime.qualification,
                renderer: &config.renderer,
            },
            &self.pool,
        )?;
        let mapper = CodexExecProfile {
            harness: settings.harness.clone(),
            model: settings.model.clone(),
            permissions_digest: settings.permissions_digest.clone(),
            renderer: config.renderer.clone(),
        };
        self.profile = Some(Arc::new(NativeCodexHostProfile::new(
            settings.host_root.clone(),
            installed,
            mapper,
            settings.columns,
            settings.rows,
        )?));
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
            let launched = pipeline.launch_phase_native(&grant, profile).await?;
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
                    // No Started publication is invented. The retained closure
                    // requires its own no-Started reconciliation owner.
                    anyhow::bail!(
                        "native host did not establish Running; retained closure requires reconciliation"
                    )
                }
            };
            drive_running(
                pipeline,
                host,
                grant.deadline_ns(),
                self.relay.clone(),
                shutdown,
            )
            .await
        })
    }
}

/// Shared actual-host lifecycle used by the concrete executor and pinned local
/// physical-host fault tests. Opaque LocalPhaseHost cannot be reconstructed.
pub(crate) async fn drive_running(
    pipeline: Arc<InputPipeline>,
    host: crate::native::launch::LocalPhaseHost,
    deadline_ns: u64,
    relay_client: RelayHttp,
    mut shutdown: watch::Receiver<bool>,
) -> Result<()> {
    let id = host.attempt_id();
    // Install the independent reader before any network Started wait.
    let operation = async {
        let relay = pipeline.start_phase_output(&host, relay_client)?;
        tokio::select! {
            receipt = pipeline.report_phase_terminal_started(id) => { receipt?; },
            _ = stop_requested(&mut shutdown, deadline_ns) => {
                anyhow::bail!("stop requested while Started publication outcome is unknown");
            }
        }
        let _publisher = super::publish_output::OutputPublisher::start(relay);
        loop {
            if *shutdown.borrow()
                || super::super::http::boot_ns()? >= deadline_ns
                || host.observe()?.phase != HostPhase::Running
            {
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
    // Neither a relay error nor an uncertain ACK can bypass real stop.
    match pipeline.stop_phase_host(id).await? {
        NativeStopOutcome::StoppedPendingJournal(_) => pipeline.persist_phase_host_stop(id).await?,
        NativeStopOutcome::Unknown => {
            pipeline.persist_phase_host_uncertain(id).await?;
            anyhow::bail!("native stop uncertain; retained history requires reconciliation")
        }
    }
    let stopped = pipeline.report_phase_terminal_stopped(id).await;
    operation?;
    stopped?;
    Ok(())
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
