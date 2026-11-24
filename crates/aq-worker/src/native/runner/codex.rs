//! Concrete closed runtime joining the installed image and native host owner.
use super::*;
mod preflight;
mod running;
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
use aq_session_protocol::{ArtifactRef, Digest, HarnessSettings};
use aq_terminal_bridge::http::{NodeCredential, RelayHttp};
pub(crate) use running::{DriveOptions, RunningHost};
use serde::Deserialize;
use std::path::PathBuf;

const CODEX_INSTALLATION_BUDGET_BYTES: u64 = 512 * 1024 * 1024;

#[derive(Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CodexSettings {
    pub host_root: PathBuf,
    pub harness: HarnessSettings,
    /// Accepted and ignored since spec 0014 (any offered plan is launched). It can
    /// be deleted from templates; it is still read so installed ones load.
    #[serde(default)]
    pub permissions_digest: Option<Digest>,
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
                && self.harness.adapter == config.adapter,
            "unsupported closed Codex settings"
        );
        ensure!(
            (80..=240).contains(&self.columns)
                && (1..=120).contains(&self.rows)
                && u32::from(self.columns) * u32::from(self.rows) <= 12000,
            "unsupported Codex geometry"
        );
        ensure!(
            self.valid_host_root(config)?,
            "Codex host root must be a separate private state child"
        );
        self.validate_capabilities(config)
    }
    fn valid_host_root(&self, config: &NativeConfig) -> Result<bool> {
        Ok(self.host_root.is_absolute()
            && self.host_root.starts_with(&config.state_dir)
            && self.host_root != config.state_dir
            && self.host_root.canonicalize()? == self.host_root
            && !self.host_root.starts_with(&config.git_root))
    }
    fn validate_capabilities(&self, config: &NativeConfig) -> Result<()> {
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
        self.initialize(config)
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
            let launched = if let Some(mut pins) = result_pins {
                pins.permission_digest = pipeline.phase_permission_digest(&grant).await?;
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
            RunningHost::new(pipeline, host, grant.deadline_ns())
                .drive(
                    self.relay.clone(),
                    shutdown,
                    DriveOptions {
                        transfer,
                        clock: terminal_clock,
                        ..Default::default()
                    },
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
