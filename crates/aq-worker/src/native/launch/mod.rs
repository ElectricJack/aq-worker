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
pub mod claude_docker;
mod control_relay;
mod native;
mod output_relay;
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
struct ClaimGuard {
    registry: Arc<Registry>,
    entry: Arc<Entry>,
    admitted: bool,
}
impl Drop for ClaimGuard {
    fn drop(&mut self) {
        if !self.admitted
            && let Ok(mut state) = self.registry.entries.lock()
            && !state.fenced.contains(&self.entry.id)
            && state
                .entries
                .get(&self.entry.id)
                .is_some_and(|e| Arc::ptr_eq(e, &self.entry))
        {
            state.entries.remove(&self.entry.id);
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
impl Registry {
    pub(super) fn new(state: &State) -> Self {
        #[cfg(test)]
        let hook = Arc::new(Mutex::new(None));
        #[cfg(test)]
        let fault = Arc::new(Mutex::new(None));
        let mut entries = BTreeMap::new();
        for (id, a) in &state.attempts {
            if let Some(h) = &a.native_host {
                entries.insert(
                    *id,
                    Arc::new(Entry {
                        #[cfg(test)]
                        hook: hook.clone(),
                        #[cfg(test)]
                        fault: fault.clone(),
                        id: *id,
                        scope: None,
                        result_channel: None,
                        profile: None,
                        history: Mutex::new(Some(h.clone())),
                        stop: AtomicBool::new(true),
                        physical: Mutex::new(Physical {
                            handle: None,
                            observer: None,
                            pending_output: Vec::new(),
                            output_taken_without_sequencer: false,
                            stopped: None,
                            transport: None,
                            channel: None,
                            outcome: Outcome::Unknown,
                        }),
                    }),
                );
            }
        }
        Self {
            control: Mutex::new(None),
            #[cfg(test)]
            hook,
            #[cfg(test)]
            fault,
            output: Arc::new(Mutex::new(output_relay::Outputs::default())),
            admitted: Mutex::new(state.attempts.keys().copied().collect()),
            entries: Mutex::new(RegistryEntries {
                entries,
                fenced: BTreeSet::new(),
            }),
            bootstrap: Arc::new(Semaphore::new(1)),
            stop_slots: Arc::new(Semaphore::new(1)),
            #[cfg(any(test, feature = "test-support"))]
            pool: aq_session_host::observed::SealedElfPool::new(16 * 1024 * 1024, 4)
                .expect("fixed bounded ELF pool"),
        }
    }
    pub(super) fn admit(&self, id: Uuid) -> Result<()> {
        let mut admitted = lock(&self.admitted)?;
        ensure!(
            admitted.contains(&id) || admitted.len() < 64,
            "native registry admission capacity"
        );
        admitted.insert(id);
        Ok(())
    }
    pub(super) fn result_stopped(
        &self,
        scope: &LaunchExecutionScope,
    ) -> Result<aq_session_host::StoppedProof> {
        let e = self.entry(scope.id)?;
        let actual = e
            .scope
            .as_ref()
            .context("reopened host cannot capture result")?;
        ensure!(
            actual.digest == scope.digest && actual.inputs.same_owner(&scope.inputs),
            "result stopped host owner differs"
        );
        let p = lock(&e.physical)?;
        let proof = p
            .stopped
            .as_ref()
            .context("result requires actual retained StoppedProof")?;
        ensure!(
            matches!(p.outcome, Outcome::Stopped) && proof.observation().kernel_wait_echild,
            "result writers not proven stopped"
        );
        Ok(proof.clone())
    }
    pub(super) fn result_current(&self, scope: &LaunchExecutionScope) -> Result<()> {
        let e = self.entry(scope.id)?;
        let actual = e
            .scope
            .as_ref()
            .context("reopened host has no result authority")?;
        ensure!(
            actual.digest == scope.digest
                && actual.inputs.same_owner(&scope.inputs)
                && !e.stop.load(Ordering::SeqCst),
            "result host owner differs or stopped"
        );
        let physical = lock(&e.physical)?;
        ensure!(
            physical.stopped.is_none() && matches!(physical.outcome, Outcome::Running),
            "result host not live"
        );
        let observation = physical
            .handle
            .as_ref()
            .context("result actual host absent")?
            .observe()?;
        ensure!(
            observation.phase == aq_session_host::HostPhase::Running,
            "result process no longer running"
        );
        Ok(())
    }
    fn entry(&self, id: Uuid) -> Result<Arc<Entry>> {
        lock(&self.entries)?
            .entries
            .get(&id)
            .cloned()
            .context("native host attempt absent")
    }
    fn fence(&self, id: Uuid) -> Result<Option<Arc<Entry>>> {
        ensure!(
            lock(&self.admitted)?.contains(&id),
            "native stop not admitted"
        );
        let mut state = lock(&self.entries)?;
        state.fenced.insert(id);
        let entry = state.entries.get(&id).cloned();
        if let Some(e) = &entry {
            e.stop.store(true, Ordering::SeqCst);
        }
        drop(state);
        if let Some(e) = &entry {
            e.cancel_transport();
        }
        Ok(entry)
    }
    #[cfg(any(test, feature = "test-support"))]
    fn claim(
        &self,
        scope: LaunchExecutionScope,
        profile: SyntheticHostProfile,
    ) -> Result<Arc<Entry>> {
        self.claim_profile(scope, Profile::Synthetic(Arc::new(profile)), None)
    }
    fn claim_profile(
        &self,
        scope: LaunchExecutionScope,
        profile: Profile,
        result_channel: Option<crate::native::result::VerifiedResultChannel>,
    ) -> Result<Arc<Entry>> {
        ensure!(
            lock(&self.admitted)?.contains(&scope.id),
            "native host not admitted"
        );
        let mut state = lock(&self.entries)?;
        ensure!(
            !state.entries.contains_key(&scope.id) && !state.fenced.contains(&scope.id),
            "native host already claimed; no relaunch"
        );
        let e = Arc::new(Entry {
            #[cfg(test)]
            hook: self.hook.clone(),
            #[cfg(test)]
            fault: self.fault.clone(),
            id: scope.id,
            scope: Some(Arc::new(scope)),
            result_channel: result_channel.map(Arc::new),
            profile: Some(profile),
            history: Mutex::new(None),
            stop: AtomicBool::new(false),
            physical: Mutex::new(Physical {
                handle: None,
                observer: None,
                pending_output: Vec::new(),
                output_taken_without_sequencer: false,
                stopped: None,
                transport: None,
                channel: None,
                outcome: Outcome::Pending,
            }),
        });
        state.entries.insert(e.id, e.clone());
        Ok(e)
    }
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
impl LocalPhaseHost {
    pub fn attempt_id(&self) -> Uuid {
        self.entry.id
    }
    pub fn observe(&self) -> Result<HostObservation> {
        lock(&self.entry.physical)?
            .handle
            .as_ref()
            .context("native physical handle absent")?
            .observe()
    }
    /// Extend the host guardian's local permit (never server authority) up to
    /// `until_boot_ms`, which the caller bounds by the server grant. Used by
    /// non-interactive hosts whose local permit is capped at MAX_PERMIT_MS.
    pub fn renew_local(&self, sequence: u64, until_boot_ms: u64) -> Result<()> {
        let intent = lock(&self.entry.history)?
            .as_ref()
            .map(|h| h.intent.clone())
            .context("native launch intent absent")?;
        let now = boot_ns()? / 1_000_000;
        let permit = ExecutionPermit {
            session: intent.binding.session.clone(),
            manifest_digest: intent.binding.launch_manifest_digest.clone(),
            sequence,
            issued_boot_ms: now,
            expires_boot_ms: until_boot_ms
                .min(now + aq_session_host::MAX_PERMIT_MS - 5_000)
                .min(intent.hard_deadline_ns.min(intent.deadline_ns) / 1_000_000),
        };
        lock(&self.entry.physical)?
            .handle
            .as_ref()
            .context("native physical handle absent")?
            .renew(&permit)
    }
    /// MS1 docker-local: the driver's own exit record (`exit.json` in its
    /// private directory), written before it submits the claim. None when the
    /// host is not a Claude docker-local host or the record is absent.
    pub fn claude_driver_succeeded(&self) -> Option<bool> {
        driver_exit_success(&self.claude_driver_dir()?.join("exit.json"))
    }
    /// MS1 docker-local: the driver's private directory, if this is a Claude
    /// docker-local host.
    pub fn claude_driver_dir(&self) -> Option<std::path::PathBuf> {
        if !matches!(self.entry.profile, Some(Profile::ClaudeDocker(_))) {
            return None;
        }
        let intent = lock(&self.entry.history).ok()?.as_ref()?.intent.clone();
        Some(claude_driver_dir(&intent.host_root, intent.launch_id))
    }
}
/// `<host_root>/driver-<launch_id>`: the private directory of one launch.
pub fn claude_driver_dir(host_root: &std::path::Path, launch_id: Uuid) -> std::path::PathBuf {
    host_root.join(format!("driver-{launch_id}"))
}
/// The driver's exit record. Missing, unreadable or malformed is None
/// (Unknown): only an explicit `{"success": true}` can report Success.
pub fn driver_exit_success(path: &std::path::Path) -> Option<bool> {
    let bytes = std::fs::read(path).ok()?;
    let value: serde_json::Value = serde_json::from_slice(&bytes).ok()?;
    value["success"].as_bool()
}
/// MS1 review S3a: after the host stopped, the launch's leased token file is
/// removed whatever happened to the driver (it normally deleted it itself).
pub fn wipe_claude_driver_secrets(dir: &std::path::Path) {
    match std::fs::remove_file(dir.join("token")) {
        Ok(()) => tracing::warn!(dir = %dir.display(), "removed a leftover leased token file"),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => tracing::warn!(dir = %dir.display(), "leased token file not removed: {e}"),
    }
}
impl LocalPhaseHost {
    pub fn started_request(&self) -> Result<aq_admission::started::Request> {
        lock(&self.entry.history)?
            .as_ref()
            .and_then(|h| h.running.as_ref())
            .map(|r| r.request.clone())
            .context("native Running history absent")
    }
}
impl HeldLaunchHistory {
    pub fn attempt_id(&self) -> Uuid {
        self.entry.id
    }
}
impl LocalStoppedHistory {
    pub fn attempt_id(&self) -> Uuid {
        self.entry.id
    }
}
impl Entry {
    fn cancel_transport(&self) {
        // Safety cancellation remains available if another owner panicked
        // while holding this mutex. It can revoke IO, never authorize effects.
        let physical = self
            .physical
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some(transport) = &physical.transport {
            transport.cancel();
        }
        if let Some(channel) = &physical.channel {
            channel.close();
        }
    }
    #[cfg(test)]
    fn hook(&self, stage: Checkpoint, journal: &mut Journal) -> Result<()> {
        let mut fault = lock(&self.fault)?;
        if fault.as_ref().is_some_and(|(s, _)| *s == stage) {
            journal.inject_fault(fault.take().unwrap().1);
        }
        drop(fault);
        let mut hook = lock(&self.hook)?;
        if hook.as_ref().is_some_and(|(s, _)| *s == stage) {
            journal.set_pause_gate(hook.take().unwrap().1);
        }
        Ok(())
    }

    fn current(&self, owner: &Owned) -> Result<()> {
        ensure!(
            !self.stop.load(Ordering::SeqCst),
            "native host safety stop fenced"
        );
        self.profile
            .as_ref()
            .context("recovery cannot authorize host")?
            .root()
            .verify()?;
        self.scope
            .as_ref()
            .context("recovery has no execution scope")?
            .check(owner)?;
        self.profile
            .as_ref()
            .context("recovery has no profile")?
            .root()
            .verify()?;
        ensure!(
            !self.stop.load(Ordering::SeqCst),
            "native host stopped during current checks"
        );
        Ok(())
    }
    fn snapshot(&self, journal: &Journal) -> Result<()> {
        *lock(&self.history)? = Some(
            journal
                .state()
                .attempts
                .get(&self.id)
                .and_then(|a| a.native_host.clone())
                .context("native host history absent")?,
        );
        Ok(())
    }
    fn stop_now(&self) -> Result<NativeStopOutcome> {
        self.stop.store(true, Ordering::SeqCst);
        self.cancel_transport();
        let h = lock(&self.history)?
            .clone()
            .context("native host has no durable intent")?;
        let intent = h
            .host_intent
            .as_ref()
            .context("native host has no physical configuration")?;
        let root = RootGuard::open(
            &h.intent.host_root,
            Some((h.intent.root_device, h.intent.root_inode)),
        )?;
        root.verify()?;
        let mut handle = SessionHost::recover(&h.intent.host_root, intent)?;
        handle.wait_stopped(Duration::from_secs(2))?;
        let proof = handle.stopped_proof()?;
        let observation = proof.observation().clone();
        if handle.cleanup_private_inputs(&proof).is_err() {
            tracing::warn!(attempt_id=%self.id,"stopped private runtime data retained after cleanup refusal");
        }
        let mut state = lock(&self.physical)?;
        state.stopped = Some(proof);
        state.handle = Some(handle);
        state.outcome = Outcome::Stopped;
        Ok(NativeStopOutcome::StoppedPendingJournal(Box::new(
            observation,
        )))
    }
    async fn finish_stopped_transport(&self) -> Result<()> {
        let transport = {
            let mut physical = lock(&self.physical)?;
            if physical.stopped.is_none() {
                return Ok(());
            }
            drop(physical.channel.take()); // removes the socket directory
            physical.transport.take()
        };
        if let Some(transport) = transport {
            // Cancellation of this wait still aborts the serving task. This
            // closes local IO; it proves no rollback of upstream effects.
            let stopped = tokio::time::timeout(Duration::from_secs(1), transport.stop()).await;
            if !matches!(stopped, Ok(Ok(()))) {
                tracing::warn!(attempt_id=%self.id,"stopped transport task ended without a clean acknowledgement");
            }
        }
        Ok(())
    }
}
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
        let claimed = self
            .launch_scope(execution)
            .and_then(|scope| {
                self.launch
                    .claim_profile(scope, Profile::Native(profile), None)
            })
            .map(|entry| ClaimGuard {
                registry: self.launch.clone(),
                entry,
                admitted: false,
            });
        self.launch_claimed(claimed)
    }
    pub fn launch_phase_native_result(
        self: &Arc<Self>,
        execution: &DurablePhaseExecution,
        profile: Arc<NativeCodexHostProfile>,
        channel: crate::native::result::VerifiedResultChannel,
    ) -> impl std::future::Future<Output = Result<NativeHostLaunchResult>> + Send + 'static {
        let claimed = self
            .launch_scope(execution)
            .and_then(|scope| {
                channel.check_pipeline(self, &scope)?;
                self.launch
                    .claim_profile(scope, Profile::Native(profile), Some(channel))
            })
            .map(|entry| ClaimGuard {
                registry: self.launch.clone(),
                entry,
                admitted: false,
            });
        self.launch_claimed(claimed)
    }
    /// MS1: the session host runs the docker-local Claude driver with the
    /// attempt's actual result channel.
    pub fn launch_phase_claude_docker_result(
        self: &Arc<Self>,
        execution: &DurablePhaseExecution,
        profile: Arc<claude_docker::ClaudeDockerHostProfile>,
        channel: crate::native::result::VerifiedResultChannel,
    ) -> impl std::future::Future<Output = Result<NativeHostLaunchResult>> + Send + 'static {
        let claimed = self
            .launch_scope(execution)
            .and_then(|scope| {
                channel.check_pipeline(self, &scope)?;
                self.launch
                    .claim_profile(scope, Profile::ClaudeDocker(profile), Some(channel))
            })
            .map(|entry| ClaimGuard {
                registry: self.launch.clone(),
                entry,
                admitted: false,
            });
        self.launch_claimed(claimed)
    }
    #[cfg(any(test, feature = "test-support"))]
    pub fn launch_phase_synthetic(
        self: &Arc<Self>,
        execution: &DurablePhaseExecution,
        profile: SyntheticHostProfile,
    ) -> impl std::future::Future<Output = Result<NativeHostLaunchResult>> + Send + 'static {
        let claimed = self
            .launch_scope(execution)
            .and_then(|scope| self.launch.claim(scope, profile))
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
                }
                if result.is_err() {
                    let _ = entry.stop_now();
                    if let Ok(mut owned) = lock(&owner) {
                        let _ = owned.journal.native_update(entry.id, |h, _, _| {
                            h.uncertain = Some(LaunchFailure::ExecUnobserved);
                            Ok(())
                        });
                        let proof = lock(&entry.physical)
                            .ok()
                            .and_then(|state| state.stopped.clone());
                        if let Some(proof) = proof {
                            let _ = owned.journal.native_stopped(entry.id, &proof);
                        }
                        let _ = entry.snapshot(&owned.journal);
                    }
                }
                result
            })
            .await
            .context("native bootstrap job failed")?;
            match result {
                Ok(()) => {
                    let outcome = lock(&returned.physical)?.outcome;
                    Ok(match outcome {
                        Outcome::Running => NativeHostLaunchResult::Running(LocalPhaseHost {
                            entry: returned,
                            owner: pipeline.owned.clone(),
                        }),
                        Outcome::Stopped => {
                            NativeHostLaunchResult::StoppedBeforeStarted(LocalStoppedHistory {
                                entry: returned,
                            })
                        }
                        _ => NativeHostLaunchResult::Unknown(HeldLaunchHistory { entry: returned }),
                    })
                }
                Err(_) => {
                    let _ = returned.stop_now();
                    returned.finish_stopped_transport().await?;
                    let stopped = lock(&returned.history)?
                        .as_ref()
                        .is_some_and(|h| h.stopped.is_some() && h.running.is_none());
                    lock(&returned.physical)?.outcome = if stopped {
                        Outcome::Stopped
                    } else {
                        Outcome::Unknown
                    };
                    if stopped {
                        Ok(NativeHostLaunchResult::StoppedBeforeStarted(
                            LocalStoppedHistory { entry: returned },
                        ))
                    } else {
                        Ok(NativeHostLaunchResult::Unknown(HeldLaunchHistory {
                            entry: returned,
                        }))
                    }
                }
            }
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
#[cfg(any(test, feature = "test-support"))]
fn bootstrap_synthetic(
    owner: &Arc<Mutex<Owned>>,
    registry: &Registry,
    entry: &Arc<Entry>,
) -> Result<()> {
    let scope = entry.scope.as_ref().context("native launch scope absent")?;
    let Profile::Synthetic(profile) = entry.profile.as_ref().context("native profile absent")?
    else {
        anyhow::bail!("synthetic profile required")
    };
    {
        let mut owner = lock(owner)?;
        entry.current(&owner)?;
        let state = owner.journal.state();
        let a = &state.attempts[&entry.id];
        let accepted = a.phase_scope()?;
        let grant = accepted.grant;
        ensure!(
            a.reservation.offer.startup.payload_bytes <= 4096,
            "synthetic startup ceiling"
        );
        let (device, inode) = profile.root.identity();
        let intent = NativeLaunchIntent {
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
            host_root: profile.config.root.clone(),
            root_device: device,
            root_inode: inode,
            pid_namespace: std::fs::read_link("/proc/self/ns/pid")?
                .to_str()
                .context("PID namespace text")?
                .into(),
            profile_digest: profile.digest.clone(),
            executable: profile.config.program.artifact.clone(),
            helper: profile.config.helper.artifact.clone(),
            tmux: profile.config.tmux.artifact.clone(),
            renderer: profile.config.renderer.clone(),
            columns: profile.config.columns,
            rows: profile.config.rows,
            deadline_ns: scope.deadline,
            hard_deadline_ns: scope.hard,
            created_ns: boot_ns()?,
            created_revision: state.revision,
            started_operation_id: Uuid::new_v4(),
            stopped_operation_id: Uuid::new_v4(),
        };
        #[cfg(test)]
        entry.hook(Checkpoint::Outer, &mut owner.journal)?;
        owner.journal.begin_native_launch(entry.id, intent)?;
        entry.snapshot(&owner.journal)?;
        entry.current(&owner)?;
    }
    profile.verify_installation()?;
    {
        let owner = lock(owner)?;
        entry.current(&owner)?;
    }
    let h = lock(&entry.history)?.clone().unwrap();
    let i = &h.intent;
    let elf = registry.pool.load_verified(
        &profile.config.program.path,
        &profile.config.program.artifact,
    )?;
    let startup = aq_session_host::observed::ObservedStartup::new(
        scope.inputs.startup().clone(),
        i.terminal.clone(),
        elf,
        aq_session_host::observed::ObservationProfile::small_elf(
            profile.config.renderer.clone(),
            i.columns,
            i.rows,
        )?,
        scope.hard / 1_000_000,
    )?;
    let permit = ExecutionPermit {
        session: i.binding.session.clone(),
        manifest_digest: i.binding.launch_manifest_digest.clone(),
        sequence: 1,
        issued_boot_ms: boot_ns()? / 1_000_000,
        expires_boot_ms: scope.deadline.min(scope.hard) / 1_000_000,
    };
    let credentials = {
        let owner = lock(owner)?;
        entry.current(&owner)?;
        owner.journal.state().attempts[&entry.id]
            .reservation
            .offer
            .offered
            .offer
            .manifest
            .credentials
            .iter()
            .map(|c| c.credential_profile.clone())
            .collect()
    };
    let local = aq_session_host::LocalEnvironment {
        resource_policy: aq_session_host::ResourcePolicy::CallerAdmission(ArtifactRef {
            id: aq_session_protocol::Text::new("aq.synthetic-admission-only")?,
            version: aq_session_protocol::Text::new("1")?,
            digest: Digest::of(b"synthetic host admission; no hard resource enforcement"),
        }),
        workspace: scope.inputs.workspace().workspace().path.clone(),
        helper_executable: profile.config.helper.path.clone(),
        tmux_executable: profile.config.tmux.path.clone(),
        tmux_version: "tmux 3.4".into(),
        credential_profiles: credentials,
        context_material: vec![],
    };
    let adapter = Adapter {
        profile: profile.clone(),
        launch_id: i.launch_id,
    };
    let mut authority = authority::Authority {
        owner: owner.clone(),
        entry: entry.clone(),
    };
    let pending = SessionHost::prepare_observed(
        &profile.config.root,
        i.launch_id,
        &startup,
        permit,
        &local,
        &adapter,
        &mut authority,
    )?;
    let (mut observer, baseline) = pending.open_baseline_observer()?;
    let mut released = pending.release_after_baseline(&baseline, &mut authority)?;
    let until = Instant::now() + Duration::from_secs(3);
    let mut bootstrap_output = Vec::new();
    let mut bootstrap_bytes = 0;
    loop {
        let events = observer.poll()?;
        bootstrap_bytes += events
            .iter()
            .map(|e| match e {
                aq_session_host::terminal_output::ControlEvent::Bytes(bytes) => bytes.len(),
                _ => 0,
            })
            .sum::<usize>();
        ensure!(
            bootstrap_bytes <= 65536 && bootstrap_output.len() + events.len() <= 64,
            "native bootstrap output bound"
        );
        bootstrap_output.extend(events);
        if entry.stop.load(Ordering::SeqCst) {
            released.stop()?;
            anyhow::bail!("native host stopped before Running")
        }
        if let Some(handle) = released.poll_started(&mut authority)? {
            let mut state = lock(&entry.physical)?;
            state.handle = Some(handle);
            state.observer = Some(observer);
            state.pending_output = bootstrap_output;
            state.outcome = Outcome::Running;
            return Ok(());
        }
        ensure!(
            boot_ns()? < scope.deadline.min(scope.hard) && Instant::now() < until,
            "native positive exec observation expired"
        );
        std::thread::sleep(Duration::from_millis(2));
    }
}

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

#[cfg(test)]
mod driver_dir_tests {
    #[test]
    fn leftover_token_is_wiped_and_a_missing_one_is_fine() {
        let dir = tempfile::tempdir().unwrap();
        let token = dir.path().join("token");
        std::fs::write(&token, b"x").unwrap();
        std::fs::write(dir.path().join("exit.json"), b"{}").unwrap();
        super::wipe_claude_driver_secrets(dir.path());
        assert!(!token.exists());
        assert!(
            dir.path().join("exit.json").exists(),
            "non-secret records stay"
        );
        super::wipe_claude_driver_secrets(dir.path());
    }
}
