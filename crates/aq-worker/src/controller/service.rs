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

/// Deliberate absence of an admission transport, distinct from its failures.
#[derive(Debug)]
pub struct ProviderUnavailable;
impl std::fmt::Display for ProviderUnavailable {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("B2 registration/issuance provider is not installed; admission disabled")
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
    /// Own identity and root for the service lifetime. B2 will compose native
    /// offer/acceptance observations with reserve/accept_with/launch. There is
    /// deliberately no guessed polling or issuance route in this service.
    pub async fn run(&mut self, mut shutdown: watch::Receiver<bool>) -> Result<()> {
        if *shutdown.borrow() {
            return self.set_draining(true);
        }
        let registration = tokio::select! {
            result = self.register() => result,
            _ = shutdown.changed() => return self.set_draining(true),
        };
        if let Err(error) = registration {
            if !error.is::<ProviderUnavailable>() {
                return Err(error);
            }
            // No registration means no admission. Retain the root lock and
            // heartbeat service; actual provider/transport failures still fail.
            let mut state = self.state().clone();
            state.last_refusal = Some(RefusalReason::GrantDenied);
            self.journal.persist(state)?;
            tracing::warn!("{ProviderUnavailable}");
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
