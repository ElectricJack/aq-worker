//! The slot supervisor: one polling loop per slot, the control poll, handoff
//! markers and the graceful stop.
use crate::{
    EnrollmentFile, Layout, LinkError, RepositoryBinding, ServerLink, ServiceConfig,
    config::{AqGitConfig, RetryConfig, STOP_MARGIN},
    enrollment_file::write_private,
    git::AqGit,
};
use anyhow::{Context, Result};
use aq_protocol::{ControlResponse, HandoffNotice, HandoffReason};
use std::{
    collections::HashSet,
    os::unix::fs::OpenOptionsExt,
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
    time::Duration,
};
use tokio::{sync::watch, task::JoinSet};
use uuid::Uuid;

#[derive(Debug, Clone)]
pub struct SlotContext {
    pub slot: u32,
    pub node_id: Uuid,
    pub enrollment: PathBuf,
    pub state_dir: PathBuf,
    /// The session host root (`Layout::host_root`) and aq-git's socket, in the slot's runtime
    /// directory.
    pub host_root: PathBuf,
    pub git_socket: PathBuf,
    /// What is left of the pause after a failed attempt. The backend comes up and reports
    /// (heartbeat, inventory) at once, but asks for no offer until it has passed
    /// (`polling::poll_until_offer`), so a waiting slot never shows an expired report.
    pub hold: Duration,
}
#[derive(Debug, PartialEq, Eq)]
pub enum AttemptOutcome {
    AttemptStopped,
    Paused,
    ShutdownBeforeAdmission,
}
/// Written into a slot's `current` when its attempt ended on a refusal the server answered.
const SERVER_REFUSED: &str = "server-refused";
const PREPARE_UNKNOWN: &str = "prepare_disposition_unknown";
#[derive(Debug, PartialEq, Eq)]
pub enum RecoverOutcome {
    Clean,
    Reported,
    Salvaged,
    /// Its host was lost: the server failed it and retries the task (D2 ruling C). Its unsaved
    /// work was not salvaged; its sandbox is kept for `release-sandbox`. The slot is free.
    Lost,
    Held(String),
}
#[async_trait::async_trait]
pub trait SlotBackend: Send + Sync + 'static {
    /// Runs independently of slot recovery, including when all slots are held.
    async fn node_control(&self, _ctx: SlotContext, _reply: &ControlResponse) -> Result<()> {
        Ok(())
    }

    /// Resolves when the node has room for one more queued attempt (at once by default). The
    /// slot waits on it before it creates a state directory or starts aq-git for an attempt.
    async fn ready(&self, _node_id: Uuid) {}
    async fn run_attempt(
        &self,
        ctx: SlotContext,
        shutdown: watch::Receiver<bool>,
        pause: watch::Receiver<bool>,
    ) -> Result<AttemptOutcome>;
    async fn recover(&self, ctx: SlotContext) -> Result<RecoverOutcome>;
    fn retained_attempts(&self, ctx: &SlotContext) -> Result<Vec<Uuid>>;
    /// Destroys the leftover stopped sandboxes of the slot's claimed attempts (their work is in
    /// the worktree): the driver can be killed between its claim and the node's destroy.
    async fn release_claimed(&self, ctx: SlotContext) -> Result<usize>;
}
#[async_trait::async_trait]
pub trait ControlSource: Send + Sync + 'static {
    async fn control(&self) -> Result<ControlResponse>;
    /// Posts the notice for an attempt of `slot`, under that slot's node credential.
    async fn handoff(&self, slot: u32, notice: &HandoffNotice) -> Result<()>;
}
/// The machine's server calls. The control poll uses slot 1's credential (the
/// server groups the slots by machine key). A handoff notice uses its own slot's: the server
/// refuses a notice for an attempt the posting node does not own.
pub struct MachineLink {
    primary: Arc<ServerLink>,
    enrollments: Vec<PathBuf>,
    single_node: bool,
}
impl MachineLink {
    pub fn new(primary: Arc<ServerLink>, layout: &Layout, slots: u32) -> Self {
        let enrollments = (1..=slots).map(|s| layout.slot_enrollment(s)).collect();
        Self {
            primary,
            enrollments,
            single_node: layout.node.is_some(),
        }
    }
    /// The link of `slot`'s node, from its enrollment file.
    pub fn slot(&self, slot: u32) -> Result<ServerLink> {
        let path = (slot.checked_sub(1))
            .and_then(|i| self.enrollments.get(i as usize))
            .with_context(|| format!("no slot {slot}"))?;
        ServerLink::from_enrollment(&EnrollmentFile::load(path)?)
    }
}
#[async_trait::async_trait]
impl ControlSource for MachineLink {
    async fn control(&self) -> Result<ControlResponse> {
        self.primary.control().await
    }
    async fn handoff(&self, slot: u32, notice: &HandoffNotice) -> Result<()> {
        if slot == 1 || self.single_node {
            return self.primary.handoff(notice).await;
        }
        self.slot(slot)?.handoff(notice).await
    }
}
#[async_trait::async_trait]
pub trait GitFactory: Send + Sync {
    async fn start(&self, state_dir: &Path, socket: &Path) -> Result<Box<dyn GitChild>>;
}
#[async_trait::async_trait]
pub trait GitChild: Send + Sync {
    async fn register(&self, repos: &[RepositoryBinding]) -> Result<()>;
    async fn stop(self: Box<Self>) -> Result<()>;
}
/// Starts the real aq-git child.
pub struct AqGitFactory(pub AqGitConfig);
#[async_trait::async_trait]
impl GitFactory for AqGitFactory {
    async fn start(&self, state_dir: &Path, socket: &Path) -> Result<Box<dyn GitChild>> {
        Ok(Box::new(AqGit::start(&self.0, state_dir, socket).await?))
    }
}
#[async_trait::async_trait]
impl GitChild for AqGit {
    async fn register(&self, repos: &[RepositoryBinding]) -> Result<()> {
        AqGit::register(self, repos).await
    }
    async fn stop(self: Box<Self>) -> Result<()> {
        AqGit::stop(*self).await
    }
}

pub const CONTROL_INTERVAL: Duration = Duration::from_secs(5);
pub const BACKOFF_START: Duration = Duration::from_secs(30);
pub const BACKOFF_MAX: Duration = Duration::from_secs(300);
const REVOKED_INTERVAL: Duration = Duration::from_secs(60);
const FORCE_WAIT: Duration = Duration::from_secs(20);
/// Bound on one handoff notice post during the stop (the posts also end with the grace).
const HANDOFF_POST_WAIT: Duration = Duration::from_secs(4);
// systemd's TimeoutStopSec is the grace plus STOP_MARGIN: the forced stop must fit in the
// margin. A post started just before the grace ends is the most the handoff can add.
const _: () = assert!(FORCE_WAIT.as_secs() + HANDOFF_POST_WAIT.as_secs() < STOP_MARGIN.as_secs());
pub use handoff::{SERVICE_STOP_MARKER, mark_handoff, recorded_handoff, running_attempts};

/// Why polling is paused; slots poll only while no flag is set.
const REVOKED: usize = 0;
const STOPPING: usize = 1;
/// Set from the start until the first successful control reply, whatever it carries: no slot
/// polls for work before the server has answered this machine once.
const AWAITING_FIRST_CONTROL_REPLY: usize = 2;
struct Pause {
    tx: watch::Sender<bool>,
    flags: Mutex<[bool; 3]>,
}
impl Pause {
    /// Notifies the slots only when the combined value changes: every notification wakes a
    /// slot's idle wait, which then polls again at once.
    fn set(&self, which: usize, on: bool) {
        let mut flags = self.flags.lock().unwrap();
        flags[which] = on;
        let paused = flags.iter().any(|f| *f);
        self.tx
            .send_if_modified(|v| std::mem::replace(v, paused) != paused);
    }
}
#[derive(Default)]
struct ControlState {
    unauthorized: u32,
    revoked: bool,
    /// Consecutive refusals by an edge in front of AQ (Cloudflare Access).
    edge: u32,
}
impl ControlState {
    fn interval(&self) -> Duration {
        if self.revoked {
            REVOKED_INTERVAL
        } else if self.edge > 0 {
            (CONTROL_INTERVAL * 2u32.saturating_pow(self.edge.min(16))).min(BACKOFF_MAX)
        } else {
            CONTROL_INTERVAL
        }
    }
}

pub struct Supervisor<B: SlotBackend, C: ControlSource> {
    config: ServiceConfig,
    layout: Layout,
    backend: Arc<B>,
    control: Arc<C>,
    git: Arc<dyn GitFactory>,
    retiring: bool,
}
struct Shared<B: SlotBackend, C: ControlSource> {
    sup: Supervisor<B, C>,
    pause: Pause,
    stopping: watch::Sender<bool>,
    force: watch::Sender<bool>,
    /// Slots inside `attempt` (their `current` belongs to a live attempt).
    active: Mutex<HashSet<u32>>,
    target: watch::Sender<u32>,
}
impl<B: SlotBackend, C: ControlSource> Supervisor<B, C> {
    pub fn new(
        config: ServiceConfig,
        layout: Layout,
        backend: Arc<B>,
        control: Arc<C>,
        git: Arc<dyn GitFactory>,
    ) -> Self {
        Self {
            config,
            layout,
            backend,
            control,
            git,
            retiring: false,
        }
    }
    /// Retire an explicitly removed node: recover its own attempts and admit nothing new.
    pub fn retiring(mut self, retiring: bool) -> Self {
        self.retiring = retiring;
        self
    }
    /// Supervises every slot until `shutdown`. It returns only then: a slot that cannot run
    /// (held, or no enrollment) never ends the service, which would restart it in a loop.
    pub async fn run(self, mut shutdown: watch::Receiver<bool>) -> Result<()> {
        let initial = if self.layout.node.is_some() {
            1
        } else {
            self.config.nodes
        };
        let shared = Arc::new(Shared {
            pause: Pause {
                tx: watch::channel(true).0,
                flags: Mutex::new([false, false, true]),
            },
            stopping: watch::channel(false).0,
            force: watch::channel(false).0,
            active: Mutex::default(),
            target: watch::channel(if self.retiring { 0 } else { initial }).0,
            sup: self,
        });
        let control_task = tokio::spawn(shared.clone().control_loop());
        let mut loops = shared.supervise_slots(initial, &mut shutdown).await;
        shared.stop_slots(&mut loops).await;
        control_task.abort();
        Ok(())
    }
}
impl<B: SlotBackend, C: ControlSource> Shared<B, C> {
    async fn supervise_slots(
        self: &Arc<Self>,
        initial: u32,
        shutdown: &mut watch::Receiver<bool>,
    ) -> JoinSet<(u32, bool)> {
        let mut loops = JoinSet::new();
        let mut spawned = HashSet::new();
        let mut target = self.target.subscribe();
        let mut initial_slots = self.sup.layout.existing_slots();
        initial_slots.extend(1..=initial);
        self.spawn_slots(initial_slots, &mut spawned, &mut loops);
        loop {
            tokio::select! {
                () = signalled(shutdown) => break,
                _ = target.changed() => {
                    self.spawn_slots(1..=*target.borrow_and_update(), &mut spawned, &mut loops);
                }
                Some(outcome) = loops.join_next(), if !loops.is_empty() => {
                    // Completed retired loops can be recreated by a later increase.
                    if self.sup.retiring && loops.is_empty() { break; }
                    self.respawn_retired(outcome, &mut spawned, &mut loops);
                }
            }
        }
        loops
    }

    fn respawn_retired(
        self: &Arc<Self>,
        outcome: Result<(u32, bool), tokio::task::JoinError>,
        spawned: &mut HashSet<u32>,
        loops: &mut JoinSet<(u32, bool)>,
    ) {
        if let Ok((slot, retired)) = outcome
            && retired
        {
            spawned.remove(&slot);
            if slot <= *self.target.borrow() {
                self.spawn_slot(slot, spawned, loops);
            }
        }
    }

    fn spawn_slots(
        self: &Arc<Self>,
        slots: impl IntoIterator<Item = u32>,
        spawned: &mut HashSet<u32>,
        loops: &mut JoinSet<(u32, bool)>,
    ) {
        for slot in slots {
            self.spawn_slot(slot, spawned, loops);
        }
    }

    fn spawn_slot(
        self: &Arc<Self>,
        slot: u32,
        spawned: &mut HashSet<u32>,
        loops: &mut JoinSet<(u32, bool)>,
    ) {
        if spawned.insert(slot) {
            let shared = self.clone();
            loops.spawn(async move { (slot, shared.slot_loop(slot).await) });
        }
    }

    async fn stop_slots(&self, loops: &mut JoinSet<(u32, bool)>) {
        self.stopping.send_replace(true);
        self.pause.set(STOPPING, true);
        let grace = tokio::time::sleep(Duration::from_millis(self.sup.config.shutdown_grace_ms));
        tokio::pin!(grace);
        let mut posted = HashSet::new();
        let grace_end = grace.deadline();
        while !loops.is_empty() {
            // Bounded by the grace, so the stop stays inside systemd's TimeoutStopSec.
            let _ = tokio::time::timeout_at(grace_end, self.hand_off_running(&mut posted)).await;
            tokio::select! {
                () = drain(loops) => {}
                () = &mut grace => {
                    self.force.send_replace(true);
                    if tokio::time::timeout(FORCE_WAIT, drain(loops)).await.is_err() {
                        tracing::error!("slots did not stop after the forced shutdown");
                    }
                    break;
                }
                () = tokio::time::sleep(CONTROL_INTERVAL) => {}
            }
        }
    }
}

async fn await_recovery_record(recorded: &Path, stopping: &mut watch::Receiver<bool>) -> bool {
    loop {
        tokio::select! {
            _ = stopping.wait_for(|v| *v) => return false,
            _ = tokio::time::sleep(CONTROL_INTERVAL) => {
                if std::fs::remove_file(recorded).is_ok() {
                    return true;
                }
            }
        }
    }
}

async fn signalled(rx: &mut watch::Receiver<bool>) {
    if rx.wait_for(|v| *v).await.is_err() {
        std::future::pending::<()>().await;
    }
}
/// Waits for every slot loop; cancel-safe, so it can be raced and called again.
async fn drain<T: 'static>(loops: &mut JoinSet<T>) {
    while loops.join_next().await.is_some() {}
}
/// Waits `backoff` (false when the service stops first), then doubles it up to `max`.
async fn back_off(
    stopping: &mut watch::Receiver<bool>,
    backoff: &mut Duration,
    max: Duration,
) -> bool {
    let waited = wait(stopping, *backoff).await;
    *backoff = (*backoff * 2).min(max);
    waited
}
/// Waits `time`; false when the service stops first.
async fn wait(stopping: &mut watch::Receiver<bool>, time: Duration) -> bool {
    tokio::select! {
        () = tokio::time::sleep(time) => true,
        _ = stopping.wait_for(|v| *v) => false,
    }
}

/// Why an attempt failed, read from its error text (the backend's errors cross the
/// `SlotBackend` boundary as text; `SERVER_REFUSED` is found the same way). It decides how long
/// the slot pauses before it asks for another offer (`RetryConfig`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FailureKind {
    /// The server answered with a refusal (409, another 4xx) or 503: nothing is wrong here.
    Refused,
    /// One offer could not be prepared (its source, a local aq-git refusal): another may work.
    Offer,
    /// The server was not reached.
    Unreachable,
    /// Everything else, which includes this machine's own faults (sandbox, disk, a refused
    /// credential): the long back-off.
    Fault,
}
impl FailureKind {
    pub fn of(error: &str) -> Self {
        for (kind, patterns) in [
            (
                Self::Fault,
                ["endpoint returned 401", "endpoint returned 403"],
            ),
            (
                Self::Refused,
                ["endpoint returned 4", "endpoint returned 503"],
            ),
            (Self::Offer, ["aq-git refused", "native source"]),
            (
                Self::Unreachable,
                ["transport failed", "error sending request"],
            ),
        ] {
            if patterns.iter().any(|pattern| error.contains(pattern)) {
                return kind;
            }
        }
        Self::Fault
    }
}
/// A slot's run of failed attempts since its last finished one.
struct Failures {
    last: Option<FailureKind>,
    /// How often `last` came in a row after its first time.
    repeats: u32,
    /// The next long back-off; also the wait after a failed recovery.
    fault: Duration,
}
impl Failures {
    fn new(retry: &RetryConfig) -> Self {
        Self {
            last: None,
            repeats: 0,
            fault: Duration::from_millis(retry.fault_backoff_start_ms),
        }
    }
    /// The pause after a failure of `kind`. A fault doubles from its start to its ceiling
    /// until an attempt finishes. Any other kind is the short pause, doubled for each
    /// repeat of the same kind up to the low ceiling.
    fn pause(&mut self, kind: FailureKind, retry: &RetryConfig) -> Duration {
        self.repeats = match self.last.replace(kind) {
            Some(last) if last == kind => self.repeats + 1,
            _ => 0,
        };
        if kind == FailureKind::Fault {
            let now = self.fault;
            self.fault = (now * 2).min(Duration::from_millis(retry.fault_backoff_max_ms));
            return now;
        }
        let pause = retry
            .offer_pause_ms
            .saturating_mul(1 << self.repeats.min(20));
        Duration::from_millis(pause.min(retry.offer_pause_max_ms))
    }
}

impl<B: SlotBackend, C: ControlSource> Shared<B, C> {
    fn ctx(&self, slot: u32, node_id: Uuid, state_dir: PathBuf) -> SlotContext {
        let layout = &self.sup.layout;
        SlotContext {
            slot,
            node_id,
            enrollment: layout.slot_enrollment(slot),
            state_dir,
            host_root: layout.host_root(slot),
            git_socket: layout.git_socket(slot),
            hold: Duration::ZERO,
        }
    }
    fn archive(&self, ctx: &SlotContext) {
        let empty = self
            .sup
            .backend
            .retained_attempts(ctx)
            .map(|a| a.is_empty())
            .unwrap_or_else(|e| {
                tracing::warn!(slot = ctx.slot, "{e:#}");
                false
            });
        let l = &self.sup.layout;
        if let Err(e) = l
            .archive_current(ctx.slot, empty)
            .and_then(|_| l.prune_empty(ctx.slot))
        {
            tracing::warn!(slot = ctx.slot, "archive failed: {e:#}");
        }
    }
    /// Recovers or archives what the last attempt left in `current`. False when recovery is
    /// held for the owner (its reason is recorded for `service status`); an error is retried
    /// by the caller with backoff.
    async fn settle(&self, slot: u32, node_id: Uuid) -> Result<bool> {
        let layout = &self.sup.layout;
        let current = layout.current(slot);
        let ctx = self.ctx(slot, node_id, current.clone());
        if current.join("journal").exists() {
            if !self.settle_journal(&ctx).await? {
                return Ok(false);
            }
            self.archive(&ctx);
        } else if current.exists() {
            self.archive(&ctx);
        }
        let _ = layout.prune_empty(slot);
        let _ = std::fs::remove_file(layout.slot_held(slot));
        Ok(true)
    }
    async fn settle_journal(&self, ctx: &SlotContext) -> Result<bool> {
        let slot = ctx.slot;
        let layout = &self.sup.layout;
        let current = &ctx.state_dir;
        let recovered = self.recover(ctx).await;
        if recovered.is_err() {
            // Retried with backoff, so not held: `service status` shows no stale reason.
            let _ = std::fs::remove_file(layout.slot_held(slot));
        }
        // A Prepare the server answered with a refusal has a known disposition: no grant.
        // Without this the slot was held for good after one refused Prepare.
        let refused = current.join(SERVER_REFUSED).exists();
        let recovered = match recovered? {
            RecoverOutcome::Held(why) if refused && why.starts_with(PREPARE_UNKNOWN) => {
                tracing::warn!(
                    slot,
                    "the server refused this attempt's Prepare; archived, slot released"
                );
                RecoverOutcome::Clean
            }
            other => other,
        };
        if let RecoverOutcome::Held(why) = recovered {
            self.mark_held(slot, &why);
            return Ok(false);
        }
        // Which claims preserved their sandbox is read from the journal (durable), so this
        // holds after a reboot emptied the runtime dir and its driver markers.
        self.sup.backend.release_claimed(ctx.clone()).await?;
        Ok(true)
    }

    fn mark_held(&self, slot: u32, why: &str) {
        tracing::error!(slot, "slot held until the owner releases it: {why}");
        if let Err(e) = write_private(&self.sup.layout.slot_held(slot), why.as_bytes()) {
            tracing::warn!(slot, "holding reason not recorded: {e:#}");
        }
    }

    async fn recover(&self, ctx: &SlotContext) -> Result<RecoverOutcome> {
        self.sup.layout.slot_runtime_for_recovery(ctx.slot)?;
        let git = self.sup.git.start(&ctx.state_dir, &ctx.git_socket).await?;
        let out = async {
            git.register(&self.sup.config.repositories).await?;
            self.sup.backend.recover(ctx.clone()).await
        }
        .await;
        let _ = git.stop().await;
        out
    }
    async fn attempt(&self, slot: u32, node_id: Uuid, hold: Duration) -> Result<AttemptOutcome> {
        let dir = self.sup.layout.fresh_current(slot)?;
        let ctx = SlotContext {
            hold,
            ..self.ctx(slot, node_id, dir.clone())
        };
        self.active.lock().unwrap().insert(slot);
        let (merger, slot_pause) = self.watch_slot_pause(slot);
        let result = async {
            let git = self.sup.git.start(&dir, &ctx.git_socket).await?;
            let run = async {
                git.register(&self.sup.config.repositories).await?;
                self.sup
                    .backend
                    .run_attempt(ctx.clone(), self.force.subscribe(), slot_pause)
                    .await
            }
            .await;
            if let Err(e) = git.stop().await {
                tracing::warn!(slot, "aq-git stop: {e:#}");
            }
            run
        }
        .await;
        merger.abort();
        self.active.lock().unwrap().remove(&slot);
        // A claimed attempt's sandbox the node did not destroy (a killed driver, a failed
        // transfer): on failure `current` stays, and `settle` releases it before archiving.
        let released = self.sup.backend.release_claimed(ctx.clone()).await;
        if let Err(e) = &released {
            tracing::warn!(slot, "claimed sandbox release: {e:#}");
        }
        // An attempt that failed after admission keeps its state for recovery (handoff
        // notice and salvage); only a finished or never-admitted attempt is archived.
        self.archive_finished(&ctx, result.is_err() || released.is_err());
        result
    }
    fn watch_slot_pause(&self, slot: u32) -> (tokio::task::JoinHandle<()>, watch::Receiver<bool>) {
        let mut global = self.pause.tx.subscribe();
        let mut target = self.target.subscribe();
        let (tx, slot_pause) = watch::channel(*global.borrow() || slot > *target.borrow());
        let merger = tokio::spawn(async move {
            loop {
                tx.send_replace(*global.borrow() || slot > *target.borrow());
                tokio::select! {
                    r = global.changed() => { if r.is_err() { break; } },
                    r = target.changed() => { if r.is_err() { break; } },
                }
            }
        });
        (merger, slot_pause)
    }

    fn archive_finished(&self, ctx: &SlotContext, uncertain: bool) {
        let slot = ctx.slot;
        let admitted = || {
            self.sup
                .backend
                .retained_attempts(ctx)
                .map_or(true, |a| !a.is_empty())
        };
        if uncertain && admitted() {
            tracing::warn!(slot, "the attempt stays in current for recovery");
        } else {
            self.archive(ctx);
        }
    }

    /// Slots inside `attempt`: only their drivers are live. A failed attempt kept for recovery
    /// and a held slot's attempt ended before this stop, so they are never marked by it.
    fn active_slots(&self) -> Vec<u32> {
        self.active.lock().unwrap().iter().copied().collect()
    }
}

mod control;
mod handoff;
mod slots;

#[cfg(test)]
mod tests;
