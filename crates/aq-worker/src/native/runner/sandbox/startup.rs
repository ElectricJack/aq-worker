//! Additive startup input: old fixture adapters remain usable; an observed
//! runtime carries the pre-release specification and the actual channel owner.
use super::{channels::SandboxChannels, journal, output_runtime::OutputRuntime};
use anyhow::{Result, ensure};
use aq_session_host::{
    HarnessAdapter, LocalEnvironment, SessionHandle,
    observed::{ObservedHarnessAdapter, ObservedStartup},
};
use std::{path::Path, sync::Arc, time::Duration};
use tokio::sync::watch;

pub enum SandboxStartup<'a> {
    /// Compatibility seam for existing unobserved B6a1 fixture callers.
    Unobserved(&'a dyn HarnessAdapter),
    Observed {
        startup: &'a ObservedStartup,
        adapter: &'a dyn ObservedHarnessAdapter,
        /// The caller supplies trusted subscription demand through this owner.
        channels: Arc<SandboxChannels>,
    },
}
impl<'a, A: HarnessAdapter> From<&'a A> for SandboxStartup<'a> {
    fn from(adapter: &'a A) -> Self {
        Self::Unobserved(adapter)
    }
}
impl<'a> SandboxStartup<'a> {
    pub(super) fn adapter(&self) -> &'a dyn HarnessAdapter {
        match self {
            Self::Unobserved(adapter) => *adapter,
            Self::Observed { adapter, .. } => *adapter,
        }
    }
    pub fn observed(
        startup: &'a ObservedStartup,
        adapter: &'a impl ObservedHarnessAdapter,
        channels: Arc<SandboxChannels>,
    ) -> Self {
        Self::Observed {
            startup,
            adapter,
            channels,
        }
    }
    pub(super) fn validate(
        &self,
        frozen: &super::FrozenLaunch,
        http: Option<&super::auth::RunnerHttp>,
    ) -> Result<()> {
        if let Self::Observed {
            startup, channels, ..
        } = self
        {
            ensure!(
                aq_session_protocol::canonical_digest(startup.manifest())?
                    == aq_session_protocol::canonical_digest(&frozen.manifest)?,
                "observed startup manifest mismatch"
            );
            channels.validate_authority(
                http.ok_or_else(|| anyhow::anyhow!("observed sandbox requires permit client"))?,
            )?;
        }
        Ok(())
    }
    #[allow(clippy::too_many_arguments)]
    pub(super) async fn launch(
        self,
        root: &Path,
        frozen: &super::FrozenLaunch,
        permit: aq_session_host::ExecutionPermit,
        local: &LocalEnvironment,
        journal: &mut journal::Journal,
        agent: Option<aq_session_host::users::AgentIdentity>,
        mut shutdown: watch::Receiver<bool>,
    ) -> Result<(SessionHandle, Option<OutputRuntime>)> {
        use crate::native::launch::sandbox as host;
        match self {
            Self::Unobserved(adapter) => Ok((
                host::launch(
                    root,
                    frozen.launch_id,
                    &frozen.manifest,
                    permit,
                    local,
                    &HarnessRef(adapter),
                    journal,
                    agent,
                )?,
                None,
            )),
            Self::Observed {
                startup,
                adapter,
                channels,
            } => {
                let pending = host::prepare_observed(
                    root,
                    frozen.launch_id,
                    startup,
                    permit,
                    local,
                    &ObservedRef(adapter),
                    journal,
                    agent,
                )?;
                let (observer, baseline) = pending.open_baseline_observer()?;
                let output = channels.open_output(&baseline, &frozen.manifest.terminal)?;
                let mut released = pending.release_after_baseline(&baseline, journal)?;
                let mut runtime = OutputRuntime::start(channels, output, shutdown.clone());
                runtime.drain(observer, shutdown.clone());
                let until = tokio::time::Instant::now() + Duration::from_secs(3);
                loop {
                    if *shutdown.borrow() {
                        released.stop()?;
                        anyhow::bail!("cancelled before observed Running");
                    }
                    if let Some(handle) = released.poll_started(journal)? {
                        return Ok((handle, Some(runtime)));
                    }
                    ensure!(
                        tokio::time::Instant::now() < until,
                        "observed Running deadline"
                    );
                    tokio::select! {
                        changed = shutdown.changed() => { if changed.is_err() { anyhow::bail!("observed startup cancelled"); } }
                        _ = tokio::time::sleep(Duration::from_millis(2)) => {}
                    }
                }
            }
        }
    }
}
struct HarnessRef<'a>(&'a dyn HarnessAdapter);
impl HarnessAdapter for HarnessRef<'_> {
    fn prepare(
        &self,
        m: &aq_session_protocol::LaunchManifest,
        p: &Path,
    ) -> Result<aq_session_host::AdapterLaunch> {
        self.0.prepare(m, p)
    }
    fn readiness(
        &self,
        o: &aq_session_host::HostObservation,
    ) -> Result<aq_session_host::HarnessReadiness> {
        self.0.readiness(o)
    }
    fn private_input(
        &self,
        m: &aq_session_protocol::LaunchManifest,
        p: &Path,
    ) -> Result<Option<aq_session_host::PrivateLaunchInput>> {
        self.0.private_input(m, p)
    }
}
struct ObservedRef<'a>(&'a dyn ObservedHarnessAdapter);
impl HarnessAdapter for ObservedRef<'_> {
    fn prepare(
        &self,
        m: &aq_session_protocol::LaunchManifest,
        p: &Path,
    ) -> Result<aq_session_host::AdapterLaunch> {
        self.0.prepare(m, p)
    }
    fn readiness(
        &self,
        o: &aq_session_host::HostObservation,
    ) -> Result<aq_session_host::HarnessReadiness> {
        self.0.readiness(o)
    }
    fn private_input(
        &self,
        m: &aq_session_protocol::LaunchManifest,
        p: &Path,
    ) -> Result<Option<aq_session_host::PrivateLaunchInput>> {
        self.0.private_input(m, p)
    }
}
impl ObservedHarnessAdapter for ObservedRef<'_> {
    fn observed_qualification(
        &self,
        m: &aq_session_protocol::LaunchManifest,
    ) -> Result<aq_session_host::observed::ObservedAdapterQualification> {
        self.0.observed_qualification(m)
    }
}
