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
    pub docker_local: Option<crate::backend::docker_local::DockerLocalConfig>,
    pub boat: Option<crate::backend::boat::BoatConfig>,
    /// C4: advertise templates resolved from the C3b allowlist file.
    #[serde(default)]
    pub template_allowlist: Option<super::allowlist::TemplateAllowlistConfig>,
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
        let mut config: Self = serde_json::from_slice(&bytes)?;
        if let Some(allowlist) = &config.template_allowlist {
            let resolved = super::allowlist::load(allowlist, path)?;
            config.node.templates.extend(resolved);
        }
        ensure!(
            config.state_dir.is_absolute(),
            "controller root must be absolute"
        );
        ensure!(
            config.docker_local.is_some() ^ config.boat.is_some(),
            "configure exactly one placement backend"
        );
        if let Some(boat) = &config.boat {
            boat.validate()?;
        }
        // Every advertised template must be allowed by the selected backend.
        for template in &config.node.templates {
            let digest = format!("sha256:{}", template.digest.as_str());
            let allowed = match (&config.docker_local, &config.boat) {
                (Some(docker), None) => {
                    docker.templates.iter().any(|t| t.digest.as_str() == digest)
                }
                (None, Some(boat)) => boat.templates.iter().any(|t| t.digest.as_str() == digest),
                _ => false,
            };
            ensure!(allowed, "advertised template is not configured in backend");
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
            result = self.run(shutdown.clone()) => match result {
                Err(e) if e.is::<ProviderUnavailable>() => {
                    tracing::warn!("{e:#}; admission fenced; foundation heartbeat remains active");
                    tokio::select! {
                        result = &mut heartbeat => result.map_err(anyhow::Error::from),
                        result = self.maintain(shutdown) => result,
                    }
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
        // Recover stopped handoff before registering a new incarnation.
        self.reconcile_at((self.clock)()?).await?;
        self.sweep_at((self.clock)()?).await?;
        self.retry_handoffs_at((self.clock)()?).await?;
        tokio::select! {
            result = self.register() => result?,
            _ = shutdown.changed() => return self.set_draining(true),
        }
        self.maintain(shutdown).await
    }
    async fn maintain(&mut self, mut shutdown: watch::Receiver<bool>) -> Result<()> {
        let mut tick = tokio::time::interval(std::time::Duration::from_millis(
            self.config.recovery.sweep_interval_ms,
        ));
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        // SBX20: the always-on disk watchdog runs on its own cadence.
        let disk = self.backend.disk_limit();
        let watch_ms = if disk.mode.is_some() {
            disk.watch_interval_ms.max(1)
        } else {
            u64::MAX / 4
        };
        let mut disk_tick = tokio::time::interval(std::time::Duration::from_millis(watch_ms));
        disk_tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        loop {
            if *shutdown.borrow() {
                return self.set_draining(true);
            }
            tokio::select! {
                _ = shutdown.changed() => return self.set_draining(true),
                _ = tick.tick() => {
                    self.recover_once().await?;
                }
                _ = disk_tick.tick(), if disk.mode.is_some() => {
                    self.watch_disk_at((self.clock)()?).await?;
                }
            }
        }
    }
    /// One bounded maintenance pass; also usable by an embedding node event loop.
    pub async fn recover_once(&mut self) -> Result<()> {
        self.sweep_at((self.clock)()?).await?;
        if self.registered {
            let timeout = std::time::Duration::from_millis(self.config.recovery.sweep_interval_ms);
            match tokio::time::timeout(timeout, self.provider.poll(&self.identity().clone())).await
            {
                Ok(Ok(Some(page))) => self.observe_poll(&page, (self.clock)()?)?,
                Ok(Ok(None)) => {}
                _ => tracing::warn!("node poll unavailable; local deadlines remain enforced"),
            }
        }
        self.reconcile_at((self.clock)()?).await?;
        self.retry_handoffs_at((self.clock)()?).await
    }
}
