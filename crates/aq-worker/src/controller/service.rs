use super::*;
use aq_session_protocol::LaunchManifest;
use serde::Deserialize;
use std::path::PathBuf;
use tokio::sync::watch;

/// Operator configuration, not a server request or assignment document.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RunConfig {
    pub state_dir: PathBuf,
    pub node: NodeConfig,
    pub docker_local: crate::backend::docker_local::DockerLocalConfig,
}
impl RunConfig {
    pub fn load(path: &Path) -> Result<Self> {
        use std::io::Read;
        let mut bytes = Vec::new();
        std::fs::File::open(path)?
            .take(1_048_577)
            .read_to_end(&mut bytes)?;
        ensure!(
            bytes.len() <= 1_048_576,
            "controller configuration too large"
        );
        let config: Self = serde_json::from_slice(&bytes)?;
        ensure!(
            config.state_dir.is_absolute(),
            "controller root must be absolute"
        );
        // Every advertised template must be allowed by this actual backend.
        for template in &config.node.templates {
            ensure!(
                config
                    .docker_local
                    .templates
                    .iter()
                    .any(|allowed| allowed.digest.as_str()
                        == format!("sha256:{}", template.digest.as_str())),
                "advertised template is not configured in backend"
            );
        }
        Ok(config)
    }
}
/// Explicit production fence until B2 publishes its authenticated transport.
/// Never substitute a legacy endpoint or synthesize a grant locally.
pub struct UnavailableProvider;
#[derive(Debug)]
pub struct ProviderUnavailable;
impl std::fmt::Display for ProviderUnavailable {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("B2 registration/issuance provider is not installed")
    }
}
impl std::error::Error for ProviderUnavailable {}
impl PermitProvider for UnavailableProvider {
    type Request = ();
    async fn register(&mut self, _: &Registration) -> Result<()> {
        Err(ProviderUnavailable.into())
    }
    async fn issue(
        &mut self,
        _: &(),
        _: &SandboxIdentity,
        _: &LaunchManifest,
    ) -> Result<IssuedMaterial> {
        Err(ProviderUnavailable.into())
    }
}
impl<B: PlacementBackend, P: PermitProvider> Controller<B, P> {
    /// Keep the foundation connector alive when B2 is absent. The controller
    /// remains unregistered (GrantDenied), owns its root, and never admits work.
    /// Other controller errors and terminal connector failures still stop run.
    pub async fn run_with_heartbeat(
        &mut self,
        shutdown: watch::Receiver<bool>,
        heartbeat: impl Future<Output = std::result::Result<(), crate::client::CallError>>,
    ) -> Result<()> {
        tokio::pin!(heartbeat);
        let result = tokio::select! {
            result = self.run(shutdown) => match result {
                Err(e) if e.is::<ProviderUnavailable>() => {
                    tracing::warn!("{e:#}; admission fenced; foundation heartbeat remains active");
                    heartbeat.await.map_err(anyhow::Error::from)
                }
                result => result,
            },
            result = &mut heartbeat => result.map_err(anyhow::Error::from),
        };
        // Drain on either service exit while preserving the primary cause.
        let drain = self.set_draining(true);
        result.and(drain)
    }

    /// Own identity and root for the service lifetime. B2 will compose native
    /// offer/acceptance observations with reserve/accept_with/launch. There is
    /// deliberately no guessed polling or issuance route in this service.
    pub async fn run(&mut self, mut shutdown: watch::Receiver<bool>) -> Result<()> {
        if *shutdown.borrow() {
            return self.set_draining(true);
        }
        tokio::select! {
            result = self.register() => result?,
            _ = shutdown.changed() => return self.set_draining(true),
        }
        loop {
            if *shutdown.borrow() {
                return self.set_draining(true);
            }
            if shutdown.changed().await.is_err() {
                return self.set_draining(true);
            }
        }
    }
}
