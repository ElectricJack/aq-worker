//! Owned native host launch and test-support synthetic process fixtures.
//! ```compile_fail
//! let _: aq_worker::native::launch::LocalPhaseHost = serde_json::from_str("{}").unwrap();
//! ```
pub mod sandbox;
use super::*;
use aq_session_host::{ExecutionPermit, HostObservation, SessionHandle, SessionHost, StoppedProof};
use aq_session_protocol::Digest;
#[cfg(any(test, feature = "test-support"))]
use aq_session_protocol::Validate;
#[cfg(any(test, feature = "test-support"))]
use serde::Serialize;
use std::{
    collections::{BTreeMap, BTreeSet},
    sync::atomic::{AtomicBool, Ordering},
    time::{Duration, Instant},
};
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Checkpoint {
    Outer,
    Configured,
    Guardian,
    StartIntent,
    Baseline,
    Started,
}
mod authority;
mod bootstrap;
pub mod claude_docker;
mod control_relay;
mod native;
mod output_relay;
mod registry;
mod root;
#[cfg(any(test, feature = "test-support"))]
mod synthetic;
use crate::native::journal::launch::{LaunchFailure, NativeHostHistory, NativeLaunchIntent};
pub use control_relay::PhaseControlRelay;
use execution::LaunchExecutionScope;
pub use native::NativeCodexHostProfile;
pub use output_relay::{PhaseOutputRelay, PhaseOutputStatus};
use root::RootGuard;
#[cfg(any(test, feature = "test-support"))]
use synthetic::Adapter;
#[cfg(any(test, feature = "test-support"))]
pub use synthetic::{InstalledFilePin, SyntheticHostConfig, SyntheticHostProfile, SyntheticMode};
fn lock<T>(m: &Mutex<T>) -> Result<std::sync::MutexGuard<'_, T>> {
    m.lock()
        .map_err(|_| anyhow::anyhow!("native host owner poisoned"))
}
#[cfg(test)]
type PauseHook = Arc<Mutex<Option<(Checkpoint, Arc<crate::native::journal::NativePauseGate>)>>>;
#[cfg(test)]
type FaultHook = Arc<Mutex<Option<(Checkpoint, crate::native::journal::WriteFault)>>>;
pub(super) struct Registry {
    control: Mutex<Option<Arc<control_relay::ControlOwner>>>,
    output: Arc<Mutex<output_relay::Outputs>>,
    #[cfg(test)]
    hook: PauseHook,
    #[cfg(test)]
    fault: FaultHook,
    admitted: Mutex<BTreeSet<Uuid>>,
    entries: Mutex<RegistryEntries>,
    bootstrap: Arc<Semaphore>,
    stop_slots: Arc<Semaphore>,
    #[cfg(any(test, feature = "test-support"))]
    pool: aq_session_host::observed::SealedElfPool,
}
struct RegistryEntries {
    entries: BTreeMap<Uuid, Arc<Entry>>,
    fenced: BTreeSet<Uuid>,
}
impl RegistryEntries {
    fn remove_claim(&mut self, entry: &Arc<Entry>) {
        if !self.fenced.contains(&entry.id)
            && self
                .entries
                .get(&entry.id)
                .is_some_and(|current| Arc::ptr_eq(current, entry))
        {
            self.entries.remove(&entry.id);
        }
    }
}
struct ClaimGuard {
    registry: Arc<Registry>,
    entry: Arc<Entry>,
    admitted: bool,
}
impl Drop for ClaimGuard {
    fn drop(&mut self) {
        if self.admitted {
            return;
        }
        if let Ok(mut state) = self.registry.entries.lock() {
            state.remove_claim(&self.entry);
        }
    }
}
struct Entry {
    #[cfg(test)]
    hook: PauseHook,
    #[cfg(test)]
    fault: FaultHook,
    id: Uuid,
    scope: Option<Arc<LaunchExecutionScope>>,
    result_channel: Option<Arc<crate::native::result::VerifiedResultChannel>>,
    profile: Option<Profile>,
    history: Mutex<Option<NativeHostHistory>>,
    stop: AtomicBool,
    physical: Mutex<Physical>,
}
enum Profile {
    #[cfg(any(test, feature = "test-support"))]
    Synthetic(Arc<SyntheticHostProfile>),
    Native(Arc<NativeCodexHostProfile>),
    ClaudeDocker(Arc<claude_docker::ClaudeDockerHostProfile>),
}
impl Profile {
    fn root(&self) -> &RootGuard {
        match self {
            #[cfg(any(test, feature = "test-support"))]
            Self::Synthetic(p) => &p.root,
            Self::Native(p) => &p.root,
            Self::ClaudeDocker(p) => &p.root,
        }
    }
}
struct Physical {
    handle: Option<SessionHandle>,
    observer: Option<aq_session_host::terminal_output::OutputObserver>,
    pending_output: Vec<aq_session_host::terminal_output::ControlEvent>,
    output_taken_without_sequencer: bool,
    stopped: Option<StoppedProof>,
    transport: Option<crate::native::adapter::responses_transport::ScopedResponsesTransport>,
    /// Spec 0006 agent channel; closed with the host, removed after Stopped.
    channel: Option<aq_agent_channel::AgentChannel>,
    /// The attempt's transcript source in the node's observation store; dropping it
    /// drains and releases the source.
    observation: Option<aq_observation_store::node::SourceHandle>,
    outcome: Outcome,
}
impl Physical {
    fn ensure_complete_output_available(&self) -> Result<()> {
        ensure!(
            !self.output_taken_without_sequencer,
            "native output was consumed before sequencer attachment"
        );
        Ok(())
    }
    fn poll_unsequenced_output(
        &mut self,
    ) -> Result<Vec<aq_session_host::terminal_output::ControlEvent>> {
        let result = self.poll_output();
        if result.as_ref().map_or(true, |events| !events.is_empty()) {
            self.output_taken_without_sequencer = true;
        }
        result
    }
    fn poll_output(&mut self) -> Result<Vec<aq_session_host::terminal_output::ControlEvent>> {
        self.poll_control_output()
    }
    fn poll_control_output(
        &mut self,
    ) -> Result<Vec<aq_session_host::terminal_output::ControlEvent>> {
        if !self.pending_output.is_empty() {
            return Ok(std::mem::take(&mut self.pending_output));
        }
        self.observer
            .as_mut()
            .context("native observer absent")?
            .poll()
    }
}
#[derive(Clone, Copy)]
enum Outcome {
    Pending,
    Running,
    Stopped,
    Unknown,
}
/// A local real Running receipt; it is neither remote Started ACK nor renewal.
pub struct LocalPhaseHost {
    entry: Arc<Entry>,
    owner: Arc<Mutex<Owned>>,
}
pub struct HeldLaunchHistory {
    entry: Arc<Entry>,
}
pub struct LocalStoppedHistory {
    entry: Arc<Entry>,
}
pub enum NativeHostLaunchResult {
    Running(LocalPhaseHost),
    StoppedBeforeStarted(LocalStoppedHistory),
    Unknown(HeldLaunchHistory),
}
pub enum NativeStopOutcome {
    StoppedPendingJournal(Box<HostObservation>),
    Unknown,
}
mod host;
pub use aq_handoff_note::driver_exit_success;
pub use host::{claude_driver_dir, wipe_claude_driver_secrets};
mod entry;
impl InputPipeline {
    pub fn phase_host_history(&self, id: Uuid) -> Result<Option<NativeHostHistory>> {
        let entry = self.launch.entry(id)?;
        Ok(lock(&entry.history)?.clone())
    }

    #[cfg(test)]
    pub(crate) fn test_hold_bootstrap(&self) -> Result<tokio::sync::OwnedSemaphorePermit> {
        self.launch
            .bootstrap
            .clone()
            .try_acquire_owned()
            .context("hold test bootstrap")
    }
    #[cfg(test)]
    pub(crate) fn test_hold_writer(&self) -> Result<tokio::sync::OwnedSemaphorePermit> {
        self.storage_slots
            .clone()
            .try_acquire_owned()
            .context("hold test writer")
    }
    #[cfg(test)]
    pub(crate) fn test_fault_native(
        &self,
        stage: Checkpoint,
        fault: crate::native::journal::WriteFault,
    ) -> Result<()> {
        *lock(&self.launch.fault)? = Some((stage, fault));
        Ok(())
    }
    #[cfg(test)]
    pub(crate) fn test_pause_native(
        &self,
        stage: Checkpoint,
        pause: &crate::native::journal::WritePause,
    ) -> Result<()> {
        *lock(&self.launch.hook)? = Some((stage, pause.clone_gate()));
        Ok(())
    }

    pub fn launch_phase_native(
        self: &Arc<Self>,
        execution: &DurablePhaseExecution,
        profile: Arc<NativeCodexHostProfile>,
    ) -> impl std::future::Future<Output = Result<NativeHostLaunchResult>> + Send + 'static {
        self.launch_profile(execution, Profile::Native(profile), None)
    }
    pub fn launch_phase_native_result(
        self: &Arc<Self>,
        execution: &DurablePhaseExecution,
        profile: Arc<NativeCodexHostProfile>,
        channel: crate::native::result::VerifiedResultChannel,
    ) -> impl std::future::Future<Output = Result<NativeHostLaunchResult>> + Send + 'static {
        self.launch_profile(execution, Profile::Native(profile), Some(channel))
    }
    /// MS1: the session host runs the docker-local Claude driver with the
    /// attempt's actual result channel.
    pub fn launch_phase_claude_docker_result(
        self: &Arc<Self>,
        execution: &DurablePhaseExecution,
        profile: Arc<claude_docker::ClaudeDockerHostProfile>,
        channel: crate::native::result::VerifiedResultChannel,
    ) -> impl std::future::Future<Output = Result<NativeHostLaunchResult>> + Send + 'static {
        self.launch_profile(execution, Profile::ClaudeDocker(profile), Some(channel))
    }
    #[cfg(any(test, feature = "test-support"))]
    pub fn launch_phase_synthetic(
        self: &Arc<Self>,
        execution: &DurablePhaseExecution,
        profile: SyntheticHostProfile,
    ) -> impl std::future::Future<Output = Result<NativeHostLaunchResult>> + Send + 'static {
        self.launch_profile(execution, Profile::Synthetic(Arc::new(profile)), None)
    }
    fn launch_profile(
        self: &Arc<Self>,
        execution: &DurablePhaseExecution,
        profile: Profile,
        channel: Option<crate::native::result::VerifiedResultChannel>,
    ) -> impl std::future::Future<Output = Result<NativeHostLaunchResult>> + Send + 'static {
        let claimed = self
            .launch_scope(execution)
            .and_then(|scope| {
                if let Some(channel) = &channel {
                    channel.check_pipeline(self, &scope)?;
                }
                self.launch.claim_profile(scope, profile, channel)
            })
            .map(|entry| ClaimGuard {
                registry: self.launch.clone(),
                entry,
                admitted: false,
            });
        self.launch_claimed(claimed)
    }
    fn launch_claimed(
        self: &Arc<Self>,
        claimed: Result<ClaimGuard>,
    ) -> impl std::future::Future<Output = Result<NativeHostLaunchResult>> + Send + 'static {
        let pipeline = self.clone();
        async move {
            let mut claim = claimed?;
            let entry = claim.entry.clone();
            let host_slot = pipeline
                .launch
                .bootstrap
                .clone()
                .try_acquire_owned()
                .context("native bootstrap lane overloaded")?;
            let writer = pipeline
                .storage_slots
                .clone()
                .try_acquire_owned()
                .context("native bootstrap writer unavailable")?;
            let owner = pipeline.owned.clone();
            let registry = pipeline.launch.clone();
            let returned = entry.clone();
            claim.admitted = true;
            let result = tokio::task::spawn_blocking(move || {
                let _host_slot = host_slot;
                let _writer = writer;
                let result = bootstrap(&owner, &registry, &entry);
                if let Err(error) = &result {
                    tracing::warn!(attempt_id = %entry.id, "native host bootstrap refused: {error:#}");
                    entry.retain_bootstrap_failure(&owner);
                }
                result
            })
            .await
            .context("native bootstrap job failed")?;
            returned.launch_result(&pipeline.owned, result).await
        }
    }
    pub fn stop_phase_host(
        &self,
        id: Uuid,
    ) -> impl std::future::Future<Output = Result<NativeStopOutcome>> + Send + 'static {
        let entry = self.launch.fence(id);
        let slots = self.launch.stop_slots.clone();
        async move {
            let Some(entry) = entry? else {
                return Ok(NativeStopOutcome::Unknown);
            };
            let permit = slots
                .try_acquire_owned()
                .context("native stop lane overloaded")?;
            let stopping = entry.clone();
            let outcome = tokio::task::spawn_blocking(move || {
                let _permit = permit;
                stopping.stop_now().unwrap_or(NativeStopOutcome::Unknown)
            })
            .await
            .context("native stop worker failed")?;
            entry.finish_stopped_transport().await?;
            Ok(outcome)
        }
    }
    pub async fn persist_phase_host_uncertain(&self, id: Uuid) -> Result<()> {
        let entry = self.launch.entry(id)?;
        ensure!(
            entry.stop.load(Ordering::SeqCst),
            "native unknown requires stop fence"
        );
        self.storage(move |owner| {
            owner
                .journal
                .native_uncertain(id, LaunchFailure::StopUncertain)?;
            entry.snapshot(&owner.journal)
        })
        .await
    }
    pub async fn persist_phase_host_stop(&self, id: Uuid) -> Result<()> {
        let entry = self.launch.entry(id)?;
        let proof = lock(&entry.physical)?
            .stopped
            .clone()
            .context("actual stopped proof absent")?;
        self.storage(move |owner| {
            owner.journal.native_stopped(id, &proof)?;
            entry.snapshot(&owner.journal)
        })
        .await
    }
    pub async fn report_phase_started(
        &self,
        host: &LocalPhaseHost,
    ) -> Result<aq_admission::started::Receipt> {
        ensure!(
            Arc::ptr_eq(&self.owned, &host.owner)
                && Arc::ptr_eq(&self.launch.entry(host.entry.id)?, &host.entry),
            "native Started foreign owner"
        );
        let history = lock(&host.entry.history)?
            .clone()
            .context("native Started history absent")?;
        if let Some(receipt) = history.started_receipt {
            return Ok(receipt);
        }
        ensure!(
            !host.entry.stop.load(Ordering::SeqCst),
            "native Started safety fenced"
        );
        let request = history.running.context("actual Running absent")?.request;
        let receipt = self.client.started(&request).await?;
        let saved = receipt.clone();
        let entry = host.entry.clone();
        self.storage(move |owner| {
            owner.journal.native_started_receipt(entry.id, saved)?;
            entry.snapshot(&owner.journal)
        })
        .await?;
        Ok(receipt)
    }
    pub fn poll_phase_output(
        &self,
        host: &LocalPhaseHost,
    ) -> Result<Vec<aq_session_host::terminal_output::ControlEvent>> {
        ensure!(
            Arc::ptr_eq(&self.owned, &host.owner)
                && Arc::ptr_eq(&self.launch.entry(host.entry.id)?, &host.entry),
            "native output foreign owner"
        );
        let outputs = lock(&self.launch.output)?;
        ensure!(
            !outputs.attached(host.entry.id),
            "native output already has an exclusive drain owner"
        );
        let mut state = lock(&host.entry.physical)?;
        state.poll_unsequenced_output()
    }
}
fn bootstrap(owner: &Arc<Mutex<Owned>>, _registry: &Registry, entry: &Arc<Entry>) -> Result<()> {
    match entry.profile.as_ref().context("native profile absent")? {
        Profile::Native(profile) => native::bootstrap(owner, entry, profile),
        Profile::ClaudeDocker(profile) => claude_docker::bootstrap(owner, entry, profile),
        #[cfg(any(test, feature = "test-support"))]
        Profile::Synthetic(_) => bootstrap_synthetic(owner, _registry, entry),
    }
}
/// What a launcher pins of its host profile in a launch intent: the root (with its device and
/// inode), the profile, the executable, helper, tmux and renderer, and the columns and rows.
pub(super) struct HostPins {
    pub pid_context: &'static str,
    pub root: (std::path::PathBuf, (u64, u64)),
    pub profile_digest: Digest,
    pub programs: [aq_session_protocol::ArtifactRef; 4],
    pub size: (u16, u16),
}
/// The durable launch intent of `a`'s granted execution on a host profile (every launcher's).
pub(super) fn launch_intent(
    scope: &LaunchExecutionScope,
    revision: u64,
    a: &crate::native::journal::Attempt,
    pins: HostPins,
) -> Result<NativeLaunchIntent> {
    let accepted = a.phase_scope()?;
    let grant = accepted.grant;
    let [executable, helper, tmux, renderer] = pins.programs;
    let pid_namespace = std::fs::read_link("/proc/self/ns/pid")?;
    Ok(NativeLaunchIntent {
        execution_digest: scope.digest.clone(),
        binding: grant.binding.clone(),
        grant_id: grant.grant_id,
        logical_guardian_id: accepted.guardian_id,
        launch_id: accepted.launch_id,
        reservation_id: accepted.reservation_id,
        runtime: a.reservation.runtime.clone(),
        terminal: aq_session_protocol::TerminalIdentity {
            session: grant.binding.session.clone(),
            terminal_id: Uuid::new_v4(),
            epoch: 1,
        },
        host_root: pins.root.0,
        root_device: pins.root.1.0,
        root_inode: pins.root.1.1,
        pid_namespace: pid_namespace.to_str().context(pins.pid_context)?.into(),
        profile_digest: pins.profile_digest,
        executable,
        helper,
        tmux,
        renderer,
        columns: pins.size.0,
        rows: pins.size.1,
        deadline_ns: scope.deadline,
        hard_deadline_ns: scope.hard,
        created_ns: boot_ns()?,
        created_revision: revision,
        started_operation_id: Uuid::new_v4(),
        stopped_operation_id: Uuid::new_v4(),
    })
}
#[cfg(any(test, feature = "test-support"))]
mod synthetic_bootstrap;
#[cfg(any(test, feature = "test-support"))]
use synthetic_bootstrap::bootstrap_synthetic;

#[cfg(test)]
mod scoped_transport_scenarios;

mod frozen_terminal_reports {
    use super::*;
    crate::native::phase_profile_shared::report_terminal! { aq_phase_terminal_protocol, crate::native::journal::phase_terminal::PhaseTerminalHistory, phase_terminal_history, terminal, report_phase_terminal_started, report_phase_terminal_stopped, phase_terminal_started, phase_terminal_stopped, phase_started_receipt, phase_stopped_receipt }
}
mod refresh_terminal_reports {
    use super::*;
    crate::native::phase_profile_shared::report_terminal! { aq_phase_terminal_protocol::refresh_v1, crate::native::journal::refresh_terminal::PhaseTerminalHistory, refresh_terminal_history, refresh_terminal, report_refresh_terminal_started, report_refresh_terminal_stopped, refresh_terminal_started, refresh_terminal_stopped, refresh_started_receipt, refresh_stopped_receipt }
}
