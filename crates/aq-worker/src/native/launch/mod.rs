//! Owned native host launch and test-support synthetic process fixtures.
//! ```compile_fail
//! let _: aq_worker::native::launch::LocalPhaseHost = serde_json::from_str("{}").unwrap();
//! ```
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
mod native;
mod output_relay;
mod root;
#[cfg(any(test, feature = "test-support"))]
mod synthetic;
use crate::native::journal::launch::{LaunchFailure, NativeHostHistory, NativeLaunchIntent};
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
    profile: Option<Profile>,
    history: Mutex<Option<NativeHostHistory>>,
    stop: AtomicBool,
    physical: Mutex<Physical>,
}
enum Profile {
    #[cfg(any(test, feature = "test-support"))]
    Synthetic(Arc<SyntheticHostProfile>),
    Native(Arc<NativeCodexHostProfile>),
}
impl Profile {
    fn root(&self) -> &RootGuard {
        match self {
            #[cfg(any(test, feature = "test-support"))]
            Self::Synthetic(p) => &p.root,
            Self::Native(p) => &p.root,
        }
    }
}
struct Physical {
    handle: Option<SessionHandle>,
    observer: Option<aq_session_host::terminal_output::OutputObserver>,
    pending_output: Vec<aq_session_host::terminal_output::ControlEvent>,
    stopped: Option<StoppedProof>,
    outcome: Outcome,
}
impl Physical {
    fn poll_output(&mut self) -> Result<Vec<aq_session_host::terminal_output::ControlEvent>> {
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
                        profile: None,
                        history: Mutex::new(Some(h.clone())),
                        stop: AtomicBool::new(true),
                        physical: Mutex::new(Physical {
                            handle: None,
                            observer: None,
                            pending_output: Vec::new(),
                            stopped: None,
                            outcome: Outcome::Unknown,
                        }),
                    }),
                );
            }
        }
        Self {
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
        Ok(entry)
    }
    #[cfg(any(test, feature = "test-support"))]
    fn claim(
        &self,
        scope: LaunchExecutionScope,
        profile: SyntheticHostProfile,
    ) -> Result<Arc<Entry>> {
        self.claim_profile(scope, Profile::Synthetic(Arc::new(profile)))
    }
    fn claim_profile(&self, scope: LaunchExecutionScope, profile: Profile) -> Result<Arc<Entry>> {
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
            profile: Some(profile),
            history: Mutex::new(None),
            stop: AtomicBool::new(false),
            physical: Mutex::new(Physical {
                handle: None,
                observer: None,
                pending_output: Vec::new(),
                stopped: None,
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
            .and_then(|scope| self.launch.claim_profile(scope, Profile::Native(profile)))
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
            tokio::task::spawn_blocking(move || {
                let _permit = permit;
                Ok(entry.stop_now().unwrap_or(NativeStopOutcome::Unknown))
            })
            .await
            .context("native stop worker failed")?
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
    /// Durable facts/readback only; valid after restart, never a live host permit.
    pub async fn phase_terminal_history(
        &self,
        id: Uuid,
    ) -> Result<Option<crate::native::journal::phase_terminal::PhaseTerminalHistory>> {
        let state = self.snapshot().await?;
        Ok(state
            .attempts
            .get(&id)
            .and_then(|a| a.native_host.as_ref())
            .and_then(|h| h.terminal.clone()))
    }
    pub async fn report_phase_terminal_started(
        &self,
        id: Uuid,
    ) -> Result<aq_phase_terminal_protocol::StartedReceipt> {
        let h = self
            .phase_terminal_history(id)
            .await?
            .context("phase Started outbox absent")?;
        if let Some(receipt) = h.started.receipt {
            return Ok(receipt);
        }
        let receipt = self
            .client
            .phase_terminal_started(&h.started.request)
            .await?;
        let saved = receipt.clone();
        self.storage(move |owner| owner.journal.phase_started_receipt(id, saved))
            .await?;
        Ok(receipt)
    }
    pub async fn report_phase_terminal_stopped(
        &self,
        id: Uuid,
    ) -> Result<aq_phase_terminal_protocol::StoppedReceipt> {
        let h = self
            .phase_terminal_history(id)
            .await?
            .and_then(|h| h.stopped)
            .context("phase Stopped outbox absent")?;
        if let Some(receipt) = h.receipt {
            return Ok(receipt);
        }
        let receipt = self.client.phase_terminal_stopped(&h.request).await?;
        let saved = receipt.clone();
        self.storage(move |owner| owner.journal.phase_stopped_receipt(id, saved))
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
        state.poll_output()
    }
}
fn bootstrap(owner: &Arc<Mutex<Owned>>, _registry: &Registry, entry: &Arc<Entry>) -> Result<()> {
    match entry.profile.as_ref().context("native profile absent")? {
        Profile::Native(profile) => native::bootstrap(owner, entry, profile),
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
        let grant = &a.phase_execution.as_ref().unwrap().delivery.receipt.grant;
        let accepted = &a.phase_preparation.as_ref().unwrap().delivery.receipt.lease;
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
            reservation_id: accepted.hold.reservation.reservation_id,
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
    loop {
        let _events = observer.poll()?;
        if entry.stop.load(Ordering::SeqCst) {
            released.stop()?;
            anyhow::bail!("native host stopped before Running")
        }
        if let Some(handle) = released.poll_started(&mut authority)? {
            let mut state = lock(&entry.physical)?;
            state.handle = Some(handle);
            state.observer = Some(observer);
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
