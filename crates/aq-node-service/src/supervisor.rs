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
/// Written in a slot's `current` when the service stops, so an attempt that had no driver
/// directory yet (still preparing) is still handed off as a service stop, not a crash.
pub const SERVICE_STOP_MARKER: &str = "service-stop";

fn write_marker(path: &Path, reason: HandoffReason) -> Result<()> {
    let body = serde_json::json!({"schema_version": 1, "reason": reason}).to_string();
    match std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(path)
    {
        Ok(mut f) => {
            std::io::Write::write_all(&mut f, body.as_bytes()).context("write handoff marker")
        }
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => Ok(()),
        Err(e) => Err(e).context("create handoff marker"),
    }
}
/// Writes <driver dir>/handoff (create-new, 0600) for the driver whose driver.json names `attempt`,
/// searching current/host/driver-*/driver.json of `slots`: the slots whose attempt is still
/// running (a kept-for-recovery or held `current` is not). True when written or already present.
pub fn mark_handoff(
    layout: &Layout,
    slots: &[u32],
    attempt: Uuid,
    reason: HandoffReason,
) -> Result<bool> {
    for (dir, id) in driver_dirs(layout, slots) {
        if id == attempt {
            return write_marker(&dir.join(aq_handoff_note::HANDOFF_MARKER), reason).map(|()| true);
        }
    }
    Ok(false)
}
/// The slot and attempt id of each driver running in `slots`: driver.json present, exit.json
/// absent.
pub fn running_attempts(layout: &Layout, slots: &[u32]) -> Result<Vec<(u32, Uuid)>> {
    Ok(slots
        .iter()
        .flat_map(|&slot| {
            let dirs = driver_dirs_in(&layout.host_root(slot));
            dirs.into_iter()
                .filter(|(d, _)| !d.join("exit.json").exists())
                .map(move |(_, id)| (slot, id))
        })
        .collect())
}
fn driver_dirs(layout: &Layout, slots: &[u32]) -> Vec<(PathBuf, Uuid)> {
    slots
        .iter()
        .flat_map(|slot| driver_dirs_in(&layout.host_root(*slot)))
        .collect()
}
fn driver_dirs_in(host: &Path) -> Vec<(PathBuf, Uuid)> {
    let mut found = Vec::new();
    let Ok(entries) = std::fs::read_dir(host) else {
        return found;
    };
    for entry in entries.flatten() {
        let dir = entry.path();
        if !entry.file_name().to_string_lossy().starts_with("driver-") {
            continue;
        }
        let Ok(bytes) = std::fs::read(dir.join("driver.json")) else {
            continue;
        };
        let id = serde_json::from_slice::<serde_json::Value>(&bytes)
            .ok()
            .and_then(|v| v["owner"]["attempt_id"].as_str()?.parse().ok());
        if let Some(id) = id {
            found.push((dir, id));
        }
    }
    found
}
/// The reason the service recorded for `attempt` before the cut: the handoff marker in its
/// driver directory under `host_root`, else the slot's service-stop marker in `state_dir`;
/// None when neither exists.
pub fn recorded_handoff(
    host_root: &Path,
    state_dir: &Path,
    attempt: Uuid,
) -> Option<HandoffReason> {
    let driver = driver_dirs_in(host_root)
        .into_iter()
        .find(|(_, id)| *id == attempt)
        .and_then(|(dir, _)| {
            aq_handoff_note::read_handoff_marker(&dir.join(aq_handoff_note::HANDOFF_MARKER))
        });
    let slot = state_dir.join(SERVICE_STOP_MARKER);
    driver.or_else(|| {
        slot.exists()
            .then(|| aq_handoff_note::read_handoff_marker(&slot))
            .flatten()
    })
}

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
        let mut loops = JoinSet::new();
        let mut spawned = HashSet::new();
        let mut target = shared.target.subscribe();
        let mut initial_slots = shared.sup.layout.existing_slots();
        initial_slots.extend(1..=initial);
        for slot in initial_slots {
            if spawned.insert(slot) {
                let shared = shared.clone();
                loops.spawn(async move {
                    let retired = shared.clone().slot_loop(slot).await;
                    (slot, retired)
                });
            }
        }
        loop {
            tokio::select! {
                () = signalled(&mut shutdown) => break,
                _ = target.changed() => {
                    for slot in 1..=*target.borrow_and_update() {
                        if spawned.insert(slot) {
                            let shared = shared.clone();
                            loops.spawn(async move { let retired = shared.clone().slot_loop(slot).await; (slot, retired) });
                        }
                    }
                }
                Some(outcome) = loops.join_next(), if !loops.is_empty() => {
                    // Completed retired loops can be recreated by a later increase.
                    if shared.sup.retiring && loops.is_empty() { break; }
                    if let Ok((slot, retired)) = outcome && retired {
                        spawned.remove(&slot);
                        if slot <= *shared.target.borrow() {
                            spawned.insert(slot);
                            let shared = shared.clone();
                            loops.spawn(async move { let retired = shared.clone().slot_loop(slot).await; (slot, retired) });
                        }
                    }
                }
            }
        }
        shared.stopping.send_replace(true);
        shared.pause.set(STOPPING, true);
        let grace = tokio::time::sleep(Duration::from_millis(shared.sup.config.shutdown_grace_ms));
        tokio::pin!(grace);
        let mut posted = HashSet::new();
        let grace_end = grace.deadline();
        while !loops.is_empty() {
            // Bounded by the grace, so the stop stays inside systemd's TimeoutStopSec.
            let _ = tokio::time::timeout_at(grace_end, shared.hand_off_running(&mut posted)).await;
            tokio::select! {
                () = drain(&mut loops) => {}
                () = &mut grace => {
                    shared.force.send_replace(true);
                    if tokio::time::timeout(FORCE_WAIT, drain(&mut loops)).await.is_err() {
                        tracing::error!("slots did not stop after the forced shutdown");
                    }
                    break;
                }
                () = tokio::time::sleep(CONTROL_INTERVAL) => {}
            }
        }
        control_task.abort();
        Ok(())
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
        let has = |s: &str| error.contains(s);
        if has("endpoint returned 401") || has("endpoint returned 403") {
            Self::Fault
        } else if has("endpoint returned 4") || has("endpoint returned 503") {
            Self::Refused
        } else if has("aq-git refused") || has("native source") {
            Self::Offer
        } else if has("transport failed") || has("error sending request") {
            Self::Unreachable
        } else {
            Self::Fault
        }
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
            let recovered = self.recover(&ctx).await;
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
                tracing::error!(slot, "slot held until the owner releases it: {why}");
                if let Err(e) = write_private(&layout.slot_held(slot), why.as_bytes()) {
                    tracing::warn!(slot, "holding reason not recorded: {e:#}");
                }
                return Ok(false);
            }
            // Which claims preserved their sandbox is read from the journal (durable), so this
            // holds after a reboot emptied the runtime dir and its driver markers.
            self.sup.backend.release_claimed(ctx.clone()).await?;
            self.archive(&ctx);
        } else if current.exists() {
            self.archive(&ctx);
        }
        let _ = layout.prune_empty(slot);
        let _ = std::fs::remove_file(layout.slot_held(slot));
        Ok(true)
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
    async fn slot_loop(self: Arc<Self>, slot: u32) -> bool {
        let retry = self.sup.config.retry.clone();
        let fault_max = Duration::from_millis(retry.fault_backoff_max_ms);
        let mut failures = Failures::new(&retry);
        // No offer is asked for before `resume`. The wait is spent inside the next attempt
        // (`SlotContext::hold`), which reports while it waits; `holds` is false once an attempt
        // failed during its hold (it cannot stay up), and the wait is then spent here.
        let mut resume = tokio::time::Instant::now();
        let mut holds = true;
        let mut stopping = self.stopping.subscribe();
        let mut pause = self.pause.tx.subscribe();
        let enrollment = self.sup.layout.slot_enrollment(slot);
        let node_id = loop {
            match EnrollmentFile::load(&enrollment) {
                Ok(f) => break f.node_id,
                Err(e) => tracing::error!(slot, "slot waits for its enrollment: {e:#}"),
            }
            if !back_off(&mut stopping, &mut failures.fault, fault_max).await {
                return false;
            }
        };
        failures = Failures::new(&retry);
        loop {
            if *stopping.borrow() {
                return false;
            }
            match self.settle(slot, node_id).await {
                Ok(true) => {}
                Ok(false) => {
                    // Held: nothing more runs here, but the service stays up.
                    let _ = stopping.wait_for(|v| *v).await;
                    return false;
                }
                Err(e) => {
                    tracing::warn!(
                        slot,
                        "recovery failed, retrying in {}s: {e:#}",
                        failures.fault.as_secs()
                    );
                    if !back_off(&mut stopping, &mut failures.fault, fault_max).await {
                        return false;
                    }
                    continue;
                }
            }
            loop {
                if slot > *self.target.borrow() {
                    return true;
                }
                if *stopping.borrow() {
                    return false;
                }
                let mut target = self.target.subscribe();
                let paused = *pause.borrow();
                let ready = async {
                    match paused {
                        true => std::future::pending().await,
                        false => self.sup.backend.ready(node_id).await,
                    }
                };
                tokio::select! { _ = ready => break, _ = pause.changed() => {}, _ = stopping.changed() => {}, _ = target.changed() => {} }
            }
            let left = |resume: tokio::time::Instant| {
                resume.saturating_duration_since(tokio::time::Instant::now())
            };
            if !holds && !wait(&mut stopping, left(resume)).await {
                return false;
            }
            match self.attempt(slot, node_id, left(resume)).await {
                Ok(AttemptOutcome::AttemptStopped) => {
                    (failures, holds) = (Failures::new(&retry), true);
                }
                Ok(AttemptOutcome::Paused) => holds = true,
                Ok(AttemptOutcome::ShutdownBeforeAdmission) => return false,
                Err(e) if !left(resume).is_zero() => {
                    holds = false;
                    tracing::warn!(
                        slot,
                        "the slot could not stay up while it waited; it waits {}s without reporting: {e:#}",
                        left(resume).as_secs()
                    );
                }
                Err(e) => {
                    let text = format!("{e:#}");
                    let kind = FailureKind::of(&text);
                    let delay = failures.pause(kind, &retry);
                    tracing::warn!(
                        slot,
                        ?kind,
                        "attempt failed, retrying in {}s: {text}",
                        delay.as_secs()
                    );
                    // The server answered (a refusal, not a lost reply): recovery reads this.
                    if ["returned 4", "returned 503"]
                        .iter()
                        .any(|s| text.contains(&format!("native endpoint {s}")))
                    {
                        let _ = write_private(
                            &self.sup.layout.current(slot).join(SERVER_REFUSED),
                            text.as_bytes(),
                        );
                    }
                    resume = tokio::time::Instant::now() + delay;
                }
            }
        }
    }
    async fn attempt(&self, slot: u32, node_id: Uuid, hold: Duration) -> Result<AttemptOutcome> {
        let dir = self.sup.layout.fresh_current(slot)?;
        let ctx = SlotContext {
            hold,
            ..self.ctx(slot, node_id, dir.clone())
        };
        self.active.lock().unwrap().insert(slot);
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
        let admitted = || {
            self.sup
                .backend
                .retained_attempts(&ctx)
                .map_or(true, |a| !a.is_empty())
        };
        if (result.is_err() || released.is_err()) && admitted() {
            tracing::warn!(slot, "the attempt stays in current for recovery");
        } else {
            self.archive(&ctx);
        }
        result
    }
    /// Slots inside `attempt`: only their drivers are live. A failed attempt kept for recovery
    /// and a held slot's attempt ended before this stop, so they are never marked by it.
    fn active_slots(&self) -> Vec<u32> {
        self.active.lock().unwrap().iter().copied().collect()
    }
    /// Marks and announces the running attempts. Each notice post is bounded by
    /// `HANDOFF_POST_WAIT`; the caller bounds the whole call by the grace.
    async fn hand_off_running(&self, posted: &mut HashSet<Uuid>) {
        let layout = &self.sup.layout;
        let slots = self.active_slots();
        for &slot in &slots {
            let current = layout.current(slot);
            if let Err(e) = write_marker(
                &current.join(SERVICE_STOP_MARKER),
                HandoffReason::ServiceStop,
            ) {
                tracing::warn!(slot, "service-stop marker: {e:#}");
            }
        }
        // Every marker first: the driver acts on it, the notice only informs, so a hung
        // server can never delay another slot's marker.
        let running = running_attempts(layout, &slots).unwrap_or_default();
        for &(slot, id) in &running {
            if let Err(e) = mark_handoff(layout, &[slot], id, HandoffReason::ServiceStop) {
                tracing::warn!(%id, "handoff marker: {e:#}");
            }
        }
        for (slot, id) in running {
            if posted.insert(id) {
                let notice = HandoffNotice {
                    schema_version: 1,
                    attempt_id: id,
                    reason: HandoffReason::ServiceStop,
                };
                let post = self.sup.control.handoff(slot, &notice);
                let sent = tokio::time::timeout(HANDOFF_POST_WAIT, post);
                match sent.await {
                    Ok(Ok(())) => {}
                    Ok(Err(e)) => tracing::warn!(%id, "handoff notice: {e:#}"),
                    Err(_) => tracing::warn!(%id, "handoff notice timed out"),
                }
            }
        }
    }
    async fn control_loop(self: Arc<Self>) {
        let mut state = ControlState::default();
        loop {
            self.tick(&mut state).await;
            tokio::time::sleep(state.interval()).await;
        }
    }
    async fn tick(self: &Arc<Self>, state: &mut ControlState) {
        match self.sup.control.control().await {
            Ok(resp) => {
                (state.unauthorized, state.edge) = (0, 0);
                if std::mem::take(&mut state.revoked) {
                    tracing::info!(
                        "the server accepts this machine's credential again; slots resume"
                    );
                    self.pause.set(REVOKED, false);
                }
                if let Err(e) = self.apply(resp).await {
                    tracing::warn!("node control: {e:#}");
                }
            }
            Err(e)
                if e.downcast_ref::<LinkError>()
                    .is_some_and(LinkError::revoked) =>
            {
                (state.unauthorized, state.edge) = (state.unauthorized + 1, 0);
                if state.unauthorized >= 3 && !state.revoked {
                    state.revoked = true;
                    self.pause.set(REVOKED, true);
                    tracing::error!(
                        "the server refused this machine's credential (revoked from the Swarm page?); all slots are paused. Uninstall the service or enroll the machine again"
                    );
                }
            }
            Err(e)
                if e.downcast_ref::<LinkError>()
                    .is_some_and(LinkError::edge_refused) =>
            {
                (state.unauthorized, state.edge) = (0, state.edge + 1);
                tracing::error!(
                    "{e:#} without an AQ answer: an edge in front of the server (Cloudflare Access?) refused this machine's service token. Check the Access application and the token; this is not a revoke, so slots are not paused. Next control poll in {}s",
                    state.interval().as_secs()
                );
            }
            Err(e) => {
                state.unauthorized = 0;
                tracing::warn!("control poll failed: {e:#}");
            }
        }
    }
    /// The qualification pin a reply may still carry is ignored: the worker runs whatever
    /// model an offer names, under the template's renderer.
    async fn apply(&self, resp: ControlResponse) -> Result<()> {
        let layout = &self.sup.layout;
        if layout.node.is_some() {
            let slots = if self.sup.retiring {
                0
            } else {
                resp.node_slots()
            };
            layout.check_slots(slots)?;
            let enrollment = EnrollmentFile::load(&layout.slot_enrollment(1))?;
            // Explicit node retirement never registers a new boot over old claims.
            if !self.sup.retiring {
                self.sup
                    .backend
                    .node_control(self.ctx(1, enrollment.node_id, layout.current(1)), &resp)
                    .await?;
            }
            self.target
                .send_if_modified(|v| std::mem::replace(v, slots) != slots);
        }
        for id in resp.handoff {
            if let Err(e) = mark_handoff(layout, &self.active_slots(), id, HandoffReason::OwnerStop)
            {
                tracing::warn!(%id, "handoff marker: {e:#}");
            }
        }
        self.pause.set(AWAITING_FIRST_CONTROL_REPLY, false);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use aq_protocol::{ArtifactPin, DesiredState, QualificationPin};
    use std::collections::{HashMap, VecDeque};
    use std::sync::atomic::Ordering::SeqCst;
    use tokio::time::{Instant, sleep};

    enum Step {
        Outcome(AttemptOutcome),
        Error,
        /// Fails with this text once it polls (after its hold).
        Fail(&'static str),
        /// Fails as it starts, before it could wait or report.
        Down,
        /// Fails after admission: the journal holds the attempt.
        ErrorAdmitted,
        /// Finishes an admitted attempt (its journal holds it).
        Finished,
        /// Waits for the forced shutdown.
        Hang,
        Until(watch::Receiver<bool>),
        /// Keeps the pause receiver, returns Paused when polling pauses.
        Observe,
        /// Admitted and running (journal and driver.json written) until the task is aborted.
        Running,
    }
    #[derive(Default)]
    struct FakeBackend {
        script: Mutex<HashMap<u32, VecDeque<Step>>>,
        contexts: Mutex<Vec<SlotContext>>,
        controls: Mutex<Vec<Uuid>>,
        recovery: Mutex<HashMap<u32, VecDeque<Result<RecoverOutcome, ()>>>>,
        calls: Mutex<Vec<String>>,
        run_times: Mutex<Vec<Instant>>,
        /// When each attempt was started (it reports from then on) and the hold it was given.
        starts: Mutex<Vec<(Instant, Duration)>>,
        observed: Mutex<Option<watch::Receiver<bool>>>,
        released: Mutex<Option<Instant>>,
        retained_empty: bool,
        /// release_claimed fails this many times; the slots it was called for.
        release_failures: Mutex<u32>,
        releases: Mutex<Vec<u32>>,
        /// As the real backend: one session per node, shared by its slots.
        session: Arc<crate::session::NodeSession>,
        /// The boot, operation id and slot count of each register sent.
        registrations: Mutex<Vec<(Uuid, Uuid, u32)>>,
        /// Each inventory report: its slot, its sequence in the node's stream, and when.
        reports: Mutex<Vec<(u32, u64, Instant)>>,
        /// While it holds `false` the node has no room: `ready` waits.
        room: Option<watch::Receiver<bool>>,
    }
    impl FakeBackend {
        /// Publishes as a slot's poll does: under the node's gate, with the node's next sequence.
        async fn report(&self, slot: u32) {
            let inventory = &self.session.inventory;
            let _gate = inventory.gate.lock().await;
            let sequence = inventory.next(0).unwrap();
            (inventory.acknowledged).store(sequence, std::sync::atomic::Ordering::SeqCst);
            (self.reports.lock().unwrap()).push((slot, sequence, Instant::now()));
        }
    }
    #[async_trait::async_trait]
    impl SlotBackend for FakeBackend {
        async fn node_control(&self, ctx: SlotContext, reply: &ControlResponse) -> Result<()> {
            self.controls.lock().unwrap().push(ctx.node_id);
            self.session.update(reply)?;
            let session = &self.session;
            session
                .register_current(|operation, slots| async move {
                    let sent = (session.boot_id, operation, slots);
                    self.registrations.lock().unwrap().push(sent);
                    Ok(())
                })
                .await
        }
        async fn ready(&self, _: Uuid) {
            if let Some(mut room) = self.room.clone() {
                let _ = room.wait_for(|v| *v).await;
            }
        }
        async fn run_attempt(
            &self,
            ctx: SlotContext,
            mut shutdown: watch::Receiver<bool>,
            mut pause: watch::Receiver<bool>,
        ) -> Result<AttemptOutcome> {
            self.contexts.lock().unwrap().push(ctx.clone());
            self.calls.lock().unwrap().push(format!("run:{}", ctx.slot));
            (self.starts.lock().unwrap()).push((Instant::now(), ctx.hold));
            {
                let mut script = self.script.lock().unwrap();
                let steps = script.get_mut(&ctx.slot);
                if let Some(steps) = steps.filter(|s| matches!(s.front(), Some(Step::Down))) {
                    steps.pop_front();
                    anyhow::bail!("docker down");
                }
            }
            // As the real backend: it is up and reports during the hold, and polls after it.
            let reporting = async {
                loop {
                    self.report(ctx.slot).await;
                    sleep(crate::polling::IDLE_POLL_INTERVAL).await;
                }
            };
            // In the real order: it reports, then honours a pause, then asks for an offer.
            tokio::select! {
                biased;
                () = reporting => {}
                _ = pause.wait_for(|v| *v) => return Ok(AttemptOutcome::Paused),
                () = sleep(ctx.hold) => {}
            }
            self.run_times.lock().unwrap().push(Instant::now());
            let step = self
                .script
                .lock()
                .unwrap()
                .get_mut(&ctx.slot)
                .and_then(VecDeque::pop_front);
            match step {
                None => Ok(AttemptOutcome::ShutdownBeforeAdmission),
                Some(Step::Outcome(o)) => Ok(o),
                Some(Step::Error) | Some(Step::Down) => anyhow::bail!("boom"),
                Some(Step::Fail(text)) => anyhow::bail!(text),
                Some(Step::Finished) => {
                    std::fs::write(ctx.state_dir.join("journal"), b"j").unwrap();
                    Ok(AttemptOutcome::AttemptStopped)
                }
                Some(Step::ErrorAdmitted) => {
                    std::fs::write(ctx.state_dir.join("journal"), b"j").unwrap();
                    // Its driver never wrote exit.json (the pane was cut).
                    let dir = ctx.host_root.join("driver-1");
                    std::fs::create_dir_all(&dir).unwrap();
                    let owner = serde_json::json!({"owner": {"attempt_id": Uuid::from_u128(42)}});
                    std::fs::write(dir.join("driver.json"), owner.to_string()).unwrap();
                    anyhow::bail!("source transfer failed")
                }
                Some(Step::Until(mut done)) => {
                    let _ = done.wait_for(|v| *v).await;
                    Ok(AttemptOutcome::AttemptStopped)
                }
                Some(Step::Hang) => {
                    let _ = shutdown.wait_for(|v| *v).await;
                    *self.released.lock().unwrap() = Some(Instant::now());
                    Ok(AttemptOutcome::ShutdownBeforeAdmission)
                }
                Some(Step::Running) => {
                    std::fs::write(ctx.state_dir.join("journal"), b"j").unwrap();
                    let dir = ctx.host_root.join("driver-1");
                    std::fs::create_dir_all(&dir).unwrap();
                    let owner = serde_json::json!({"owner": {"attempt_id": Uuid::from_u128(43)}});
                    std::fs::write(dir.join("driver.json"), owner.to_string()).unwrap();
                    std::future::pending().await
                }
                Some(Step::Observe) => {
                    *self.observed.lock().unwrap() = Some(pause.clone());
                    let _ = pause.wait_for(|v| *v).await;
                    Ok(AttemptOutcome::Paused)
                }
            }
        }
        async fn recover(&self, ctx: SlotContext) -> Result<RecoverOutcome> {
            self.calls
                .lock()
                .unwrap()
                .push(format!("recover:{}", ctx.slot));
            let next = (self.recovery.lock().unwrap())
                .get_mut(&ctx.slot)
                .and_then(VecDeque::pop_front);
            match next.unwrap_or(Ok(RecoverOutcome::Clean)) {
                Ok(outcome) => Ok(outcome),
                Err(()) => anyhow::bail!("server unreachable"),
            }
        }
        async fn release_claimed(&self, ctx: SlotContext) -> Result<usize> {
            self.releases.lock().unwrap().push(ctx.slot);
            let mut failures = self.release_failures.lock().unwrap();
            if *failures > 0 {
                *failures -= 1;
                anyhow::bail!("docker down");
            }
            Ok(0)
        }
        fn retained_attempts(&self, ctx: &SlotContext) -> Result<Vec<Uuid>> {
            let journal = ctx.state_dir.join("journal").exists();
            Ok(if self.retained_empty && !journal {
                vec![]
            } else {
                vec![Uuid::nil()]
            })
        }
    }
    struct FakeControl {
        /// An error is (status, whether the body is AQ's error schema).
        replies: Mutex<VecDeque<Result<ControlResponse, (u16, bool)>>>,
        polls: Mutex<Vec<Instant>>,
        /// Each notice with the slot it was posted for.
        handoffs: Mutex<Vec<(u32, HandoffNotice)>>,
        hang_handoffs: Mutex<bool>,
    }
    #[async_trait::async_trait]
    impl ControlSource for FakeControl {
        async fn control(&self) -> Result<ControlResponse> {
            self.polls.lock().unwrap().push(Instant::now());
            let mut q = self.replies.lock().unwrap();
            let reply = if q.len() > 1 {
                q.pop_front().unwrap()
            } else {
                q.front().unwrap().clone()
            };
            reply.map_err(|(status, aq)| {
                let code = aq.then(|| "unauthorized".to_owned());
                LinkError { status, code }.into()
            })
        }
        async fn handoff(&self, slot: u32, n: &HandoffNotice) -> Result<()> {
            self.handoffs.lock().unwrap().push((slot, n.clone()));
            let hang = *self.hang_handoffs.lock().unwrap();
            if hang {
                std::future::pending::<()>().await;
            }
            Ok(())
        }
    }
    struct FakeGit;
    #[async_trait::async_trait]
    impl GitFactory for FakeGit {
        async fn start(&self, _: &Path, socket: &Path) -> Result<Box<dyn GitChild>> {
            assert!(
                socket.parent().unwrap().is_dir(),
                "the slot runtime dir exists"
            );
            Ok(Box::new(FakeGit))
        }
    }
    #[async_trait::async_trait]
    impl GitChild for FakeGit {
        async fn register(&self, _: &[RepositoryBinding]) -> Result<()> {
            Ok(())
        }
        async fn stop(self: Box<Self>) -> Result<()> {
            Ok(())
        }
    }

    fn pin(renderer: &str, effective: u64, expires: u64) -> QualificationPin {
        QualificationPin {
            renderer: ArtifactPin {
                id: "r".into(),
                version: renderer.into(),
                digest: "0".repeat(64),
            },
            effective_at_ms: effective,
            expires_at_ms: expires,
        }
    }
    fn control(replies: Vec<Result<ControlResponse, (u16, bool)>>) -> Arc<FakeControl> {
        Arc::new(FakeControl {
            replies: Mutex::new(replies.into()),
            polls: Mutex::default(),
            handoffs: Mutex::default(),
            hang_handoffs: Mutex::default(),
        })
    }
    fn reply(
        handoff: Vec<Uuid>,
        qualification: Option<QualificationPin>,
    ) -> Result<ControlResponse, (u16, bool)> {
        Ok(ControlResponse {
            schema_version: 1,
            machine_key: "desk".into(),
            desired: DesiredState::Running,
            generation: 1,
            handoff,
            qualification,
            slots: None,
        })
    }
    fn wall() -> u64 {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_millis() as u64
    }
    struct Rig {
        _dir: tempfile::TempDir,
        layout: Layout,
        config: ServiceConfig,
    }
    fn rig(slots: u32) -> Rig {
        let dir = tempfile::tempdir().unwrap();
        let layout = Layout {
            node: None,
            config_dir: dir.path().join("cfg"),
            state_root: dir.path().join("state"),
            runtime_root: dir.path().join("run/t"),
        };
        for slot in 1..=slots {
            EnrollmentFile {
                server_url: "https://aq.example.com".into(),
                node_id: Uuid::from_u128(slot as u128),
                credential: "c".into(),
                cf_client_id: None,
                cf_client_secret: None,
            }
            .save(&layout.slot_enrollment(slot))
            .unwrap();
        }
        let config = ServiceConfig {
            schema_version: 1,
            profile: "t".into(),
            machine_key: "desk".into(),
            nodes: slots,
            native_template: "/n.json".into(),
            state_root: layout.state_root.clone(),
            runtime_root: Some(layout.runtime_root.clone()),
            aq_git: AqGitConfig {
                binary: "/g".into(),
                git_executable: "/usr/bin/git".into(),
            },
            repositories: vec![],
            manual: None,
            shutdown_grace_ms: 10_000,
            retry: Default::default(),
            observations: Default::default(),
        };
        Rig {
            _dir: dir,
            layout,
            config,
        }
    }
    fn slots_reply(slots: u32) -> Result<ControlResponse, (u16, bool)> {
        let mut r = reply(vec![], None).unwrap();
        r.schema_version = 2;
        r.slots = Some(slots);
        Ok(r)
    }
    async fn spin() {
        for _ in 0..100 {
            tokio::task::yield_now().await;
        }
    }
    #[tokio::test(start_paused = true)]
    async fn node_slots_grow_and_shrink_without_interrupting_running_attempts() {
        let mut rig = rig(1);
        rig.layout.node = Some(1);
        let backend = Arc::new(FakeBackend::default());
        let (done, rx) = watch::channel(false);
        for slot in 1..=3 {
            script(&backend, slot, vec![Step::Until(rx.clone())]);
        }
        let ctl = control(vec![slots_reply(1)]);
        let (handle, stop_tx) = start(&rig, &backend, &ctl);
        spin().await;
        assert_eq!(backend.contexts.lock().unwrap().len(), 1);
        *ctl.replies.lock().unwrap() = vec![slots_reply(3)].into();
        tokio::time::advance(CONTROL_INTERVAL).await;
        spin().await;
        assert_eq!(backend.contexts.lock().unwrap().len(), 3);
        let registered = || backend.registrations.lock().unwrap().clone();
        let counts = |sent: &[(Uuid, Uuid, u32)]| sent.iter().map(|r| r.2).collect::<Vec<_>>();
        assert_eq!(
            counts(&registered()),
            [1, 3],
            "the raised count is registered"
        );
        *ctl.replies.lock().unwrap() = vec![slots_reply(1)].into();
        tokio::time::advance(CONTROL_INTERVAL).await;
        spin().await;
        assert!(
            backend
                .contexts
                .lock()
                .unwrap()
                .iter()
                .all(|c| c.state_dir.exists()),
            "shrink keeps running attempts"
        );
        let sent = registered();
        assert_eq!(counts(&sent), [1, 3], "a lowered count registers nothing");
        assert!(
            sent[0].0 == sent[1].0 && sent[0].1 != sent[1].1,
            "one boot, a fresh operation"
        );
        done.send_replace(true);
        spin().await;
        let contexts = backend.contexts.lock().unwrap().clone();
        assert_eq!(contexts.iter().filter(|c| c.slot > 1).count(), 2);
        assert!(
            contexts.iter().all(|c| c.node_id == Uuid::from_u128(1)
                && c.enrollment == rig.layout.slot_enrollment(1))
        );
        stop(handle, stop_tx).await;
    }
    /// A slot the node has no room for (manual sessions hold its capacity) waits for the permit:
    /// no state directory, no aq-git and no attempt until there is room, however long it takes.
    #[tokio::test(start_paused = true)]
    async fn a_slot_without_room_creates_nothing_until_the_node_has_room() {
        let mut rig = rig(1);
        rig.layout.node = Some(1);
        let (room, rx) = watch::channel(false);
        let backend = Arc::new(FakeBackend {
            room: Some(rx),
            ..FakeBackend::default()
        });
        let (done, until) = watch::channel(false);
        for slot in 1..=2 {
            script(&backend, slot, vec![Step::Until(until.clone())]);
        }
        let (handle, stop_tx) = start(&rig, &backend, &control(vec![slots_reply(2)]));
        spin().await;
        tokio::time::advance(Duration::from_secs(60)).await;
        spin().await;
        assert!(backend.contexts.lock().unwrap().is_empty());
        assert!(!(backend.calls.lock().unwrap().iter()).any(|c| c.starts_with("run")));
        for slot in 1..=2 {
            assert!(
                !rig.layout.current(slot).exists(),
                "slot {slot} created its state"
            );
        }
        room.send_replace(true);
        spin().await;
        assert_eq!(backend.contexts.lock().unwrap().len(), 2);
        done.send_replace(true);
        stop_tx.send_replace(true);
        handle.await.unwrap().unwrap();
    }
    #[tokio::test(start_paused = true)]
    async fn two_nodes_each_run_three_independent_attempts() {
        let mut rig = rig(2);
        let backend = Arc::new(FakeBackend::default());
        let (done, rx) = watch::channel(false);
        for slot in 1..=3 {
            script(
                &backend,
                slot,
                vec![Step::Until(rx.clone()), Step::Until(rx.clone())],
            );
        }
        rig.layout.node = Some(1);
        let (a, a_stop) = start(&rig, &backend, &control(vec![slots_reply(3)]));
        rig.layout.node = Some(2);
        let (b, b_stop) = start(&rig, &backend, &control(vec![slots_reply(3)]));
        spin().await;
        let contexts = backend.contexts.lock().unwrap().clone();
        assert_eq!(contexts.len(), 6);
        let identities: HashSet<_> = contexts.iter().map(|c| (c.node_id, c.slot)).collect();
        assert_eq!(identities.len(), 6);
        assert_eq!(
            contexts
                .iter()
                .map(|c| c.git_socket.clone())
                .collect::<HashSet<_>>()
                .len(),
            6
        );
        for ctx in &contexts {
            assert_eq!(
                EnrollmentFile::load(&ctx.enrollment).unwrap().node_id,
                ctx.node_id
            );
        }
        done.send_replace(true);
        spin().await;
        stop(a, a_stop).await;
        stop(b, b_stop).await;
    }
    #[tokio::test(start_paused = true)]
    async fn held_slot_does_not_block_sibling_or_node_registration() {
        let mut rig = rig(1);
        rig.layout.node = Some(1);
        let backend = Arc::new(FakeBackend::default());
        std::fs::create_dir_all(rig.layout.current(1)).unwrap();
        std::fs::write(rig.layout.current(1).join("journal"), "old").unwrap();
        backend
            .recovery
            .lock()
            .unwrap()
            .insert(1, vec![Ok(RecoverOutcome::Held("unknown".into()))].into());
        script(&backend, 2, vec![Step::Observe]);
        let (handle, stop_tx) = start(&rig, &backend, &control(vec![slots_reply(2)]));
        spin().await;
        assert!(!backend.controls.lock().unwrap().is_empty());
        assert_eq!(
            backend
                .contexts
                .lock()
                .unwrap()
                .iter()
                .map(|c| c.slot)
                .collect::<Vec<_>>(),
            vec![2]
        );
        assert!(rig.layout.slot_held(1).exists());
        stop(handle, stop_tx).await;
    }
    #[tokio::test(start_paused = true)]
    async fn lowering_machine_node_count_retires_six_nodes_using_their_own_identity() {
        let mut rig = rig(8);
        rig.config.nodes = 2;
        let backend = Arc::new(FakeBackend::default());
        let mut handles = Vec::new();
        for node in 3..=8 {
            let view = rig.layout.for_node(node);
            std::fs::create_dir_all(view.current(1)).unwrap();
            std::fs::write(view.current(1).join("journal"), "old").unwrap();
            let sup = Supervisor::new(
                rig.config.clone(),
                view,
                backend.clone(),
                control(vec![slots_reply(3)]),
                Arc::new(FakeGit),
            )
            .retiring(true);
            handles.push(tokio::spawn(sup.run(watch::channel(false).1)));
        }
        spin().await;
        for handle in handles {
            handle.await.unwrap().unwrap();
        }
        assert!(backend.contexts.lock().unwrap().is_empty());
        assert!(
            backend.controls.lock().unwrap().is_empty(),
            "retired nodes never register new boots"
        );
        assert_eq!(
            backend
                .calls
                .lock()
                .unwrap()
                .iter()
                .filter(|c| c.starts_with("recover:"))
                .count(),
            6
        );
        assert_eq!(rig.layout.enrolled_nodes(), (1..=8).collect::<Vec<_>>());
        for node in 3..=8 {
            assert!(!rig.layout.for_node(node).current(1).exists());
        }
    }
    fn script(backend: &FakeBackend, slot: u32, steps: Vec<Step>) {
        backend.script.lock().unwrap().insert(slot, steps.into());
    }
    type Handle = tokio::task::JoinHandle<Result<()>>;
    fn start(
        rig: &Rig,
        backend: &Arc<FakeBackend>,
        control: &Arc<FakeControl>,
    ) -> (Handle, watch::Sender<bool>) {
        start_with(rig, backend, control, Arc::new(FakeGit))
    }
    fn start_with(
        rig: &Rig,
        backend: &Arc<FakeBackend>,
        control: &Arc<FakeControl>,
        git: Arc<dyn GitFactory>,
    ) -> (Handle, watch::Sender<bool>) {
        let mut layout = Layout::for_service(&rig.layout.config_dir, &rig.config);
        layout.node = rig.layout.node;
        let sup = Supervisor::new(
            rig.config.clone(),
            layout,
            backend.clone(),
            control.clone(),
            git,
        );
        let (tx, rx) = watch::channel(false);
        (tokio::spawn(sup.run(rx)), tx)
    }
    async fn stop(handle: Handle, tx: watch::Sender<bool>) {
        tx.send_replace(true);
        handle.await.unwrap().unwrap();
    }
    fn count(dir: PathBuf) -> usize {
        std::fs::read_dir(dir).map(|d| d.count()).unwrap_or(0)
    }
    fn driver(rig: &Rig, slot: u32, attempt: Uuid) -> PathBuf {
        let dir = rig
            .layout
            .host_root(slot)
            .join(format!("driver-{}", Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("driver.json"),
            serde_json::json!({"owner": {"attempt_id": attempt}}).to_string(),
        )
        .unwrap();
        dir
    }

    #[test]
    fn recorded_handoff_reads_the_marker_of_the_attempt() {
        let rig = rig(1);
        let (a, b) = (Uuid::from_u128(1), Uuid::from_u128(2));
        let (da, _db) = (driver(&rig, 1, a), driver(&rig, 1, b));
        let (host, state) = (rig.layout.host_root(1), rig.layout.current(1));
        assert_eq!(recorded_handoff(&host, &state, a), None);
        assert!(mark_handoff(&rig.layout, &[1], a, HandoffReason::ServiceStop).unwrap());
        assert!(da.join("handoff").exists());
        assert_eq!(
            recorded_handoff(&host, &state, a),
            Some(HandoffReason::ServiceStop)
        );
        assert_eq!(recorded_handoff(&host, &state, b), None);
        assert_eq!(recorded_handoff(&host, &state, Uuid::from_u128(3)), None);
    }

    #[tokio::test(start_paused = true)]
    async fn starts_one_loop_per_slot_and_archives_each_attempt() {
        let rig = rig(2);
        let backend = Arc::new(FakeBackend::default());
        for slot in 1..=2 {
            script(
                &backend,
                slot,
                vec![
                    Step::Outcome(AttemptOutcome::AttemptStopped),
                    Step::Outcome(AttemptOutcome::ShutdownBeforeAdmission),
                ],
            );
        }
        let (handle, tx) = start(&rig, &backend, &control(vec![reply(vec![], None)]));
        sleep(Duration::from_secs(60)).await;
        assert!(!handle.is_finished(), "the service outlives its slot loops");
        stop(handle, tx).await;
        for slot in 1..=2 {
            assert_eq!(count(rig.layout.archive_dir(slot)), 2);
            assert!(!rig.layout.current(slot).exists());
        }
    }

    #[tokio::test(start_paused = true)]
    async fn recovers_existing_current_before_new_work() {
        let rig = rig(1);
        std::fs::create_dir_all(rig.layout.current(1)).unwrap();
        std::fs::write(rig.layout.current(1).join("journal"), b"j").unwrap();
        let backend = Arc::new(FakeBackend::default());
        script(
            &backend,
            1,
            vec![Step::Outcome(AttemptOutcome::ShutdownBeforeAdmission)],
        );
        let (handle, tx) = start(&rig, &backend, &control(vec![reply(vec![], None)]));
        sleep(Duration::from_secs(1)).await;
        stop(handle, tx).await;
        assert_eq!(*backend.calls.lock().unwrap(), ["recover:1", "run:1"]);
    }

    #[tokio::test(start_paused = true)]
    async fn held_recovery_disables_only_that_slot() {
        let rig = rig(2);
        for slot in 1..=2 {
            std::fs::create_dir_all(rig.layout.current(slot)).unwrap();
            std::fs::write(rig.layout.current(slot).join("journal"), b"j").unwrap();
        }
        let backend = Arc::new(FakeBackend::default());
        backend.recovery.lock().unwrap().insert(
            1,
            [Ok(RecoverOutcome::Held("sandbox_absent".into()))].into(),
        );
        let (handle, tx) = start(&rig, &backend, &control(vec![reply(vec![], None)]));
        sleep(Duration::from_secs(1)).await;
        stop(handle, tx).await;
        let calls = backend.calls.lock().unwrap().clone();
        assert!(
            calls.contains(&"run:2".to_string()) && !calls.contains(&"run:1".to_string()),
            "{calls:?}"
        );
        assert!(
            rig.layout.current(1).join("journal").exists(),
            "a held slot keeps its state"
        );
        // R1: the holding reason is recorded for `service status`, only for the held slot.
        assert_eq!(rig.layout.held_reason(1).as_deref(), Some("sandbox_absent"));
        assert_eq!(rig.layout.held_reason(2), None);
    }

    /// R1: an attempt that failed in preparation stays for recovery; recovery settles it at once
    /// (the backend reports it never started), the slot is archived, comes up again (it polls
    /// after its pause), and a holding reason left by an earlier hold is cleared.
    #[tokio::test(start_paused = true)]
    async fn a_failed_preparation_is_settled_and_the_slot_polls_again() {
        let rig = rig(1);
        let backend = Arc::new(FakeBackend {
            retained_empty: true,
            ..Default::default()
        });
        write_private(&rig.layout.slot_held(1), b"journal_inconsistent: old").unwrap();
        script(&backend, 1, vec![Step::ErrorAdmitted, Step::Hang]);
        let settled = [Ok(RecoverOutcome::Reported)];
        backend.recovery.lock().unwrap().insert(1, settled.into());
        let (handle, tx) = start(&rig, &backend, &control(vec![reply(vec![], None)]));
        sleep(Duration::from_secs(1)).await;
        assert_eq!(
            rig.layout.held_reason(1),
            None,
            "cleared when the slot settles"
        );
        assert_eq!(
            *backend.calls.lock().unwrap(),
            ["run:1", "recover:1", "run:1"],
            "recovered once, then up again"
        );
        assert_eq!(count(rig.layout.archive_dir(1)), 1, "settled and archived");
        assert_eq!(backend.run_times.lock().unwrap().len(), 1, "no poll yet");
        sleep(Duration::from_secs(30)).await;
        assert_eq!(backend.run_times.lock().unwrap().len(), 2, "the next poll");
        assert_eq!(rig.layout.held_reason(1), None);
        stop(handle, tx).await;
    }

    #[tokio::test(start_paused = true)]
    async fn handoff_marker_written_when_driver_dir_appears_later() {
        let rig = rig(1);
        let backend = Arc::new(FakeBackend::default());
        script(&backend, 1, vec![Step::Hang]);
        let attempt = Uuid::from_u128(77);
        let (handle, tx) = start(&rig, &backend, &control(vec![reply(vec![attempt], None)]));
        sleep(Duration::from_secs(1)).await;
        assert!(
            !rig.layout.host_root(1).read_dir().unwrap().any(|_| true),
            "no driver dir yet"
        );
        let dir = driver(&rig, 1, attempt);
        assert!(!dir.join("handoff").exists());
        sleep(Duration::from_secs(5)).await;
        let marker: serde_json::Value =
            serde_json::from_slice(&std::fs::read(dir.join("handoff")).unwrap()).unwrap();
        assert_eq!(
            marker,
            serde_json::json!({"schema_version": 1, "reason": "owner_stop"})
        );
        stop(handle, tx).await;
    }

    #[tokio::test(start_paused = true)]
    async fn failing_slot_backs_off_and_prunes_empty_dirs() {
        let rig = rig(1);
        let backend = Arc::new(FakeBackend {
            retained_empty: true,
            ..Default::default()
        });
        script(
            &backend,
            1,
            vec![
                Step::Error,
                Step::Error,
                Step::Error,
                Step::Error,
                Step::Hang,
            ],
        );
        let (handle, tx) = start(&rig, &backend, &control(vec![reply(vec![], None)]));
        sleep(Duration::from_secs(40)).await;
        assert_eq!(
            count(rig.layout.archive_dir(1)),
            0,
            "empty archives are pruned"
        );
        sleep(Duration::from_secs(500)).await;
        let times = backend.run_times.lock().unwrap().clone();
        let gaps: Vec<u64> = times.windows(2).map(|w| (w[1] - w[0]).as_secs()).collect();
        assert_eq!(gaps, [30, 60, 120, 240]);
        assert_eq!(count(rig.layout.archive_dir(1)), 0);
        stop(handle, tx).await;
    }

    #[tokio::test(start_paused = true)]
    async fn shutdown_hands_off_running_attempts_then_waits() {
        let rig = rig(1);
        let backend = Arc::new(FakeBackend::default());
        script(&backend, 1, vec![Step::Hang]);
        let ctl = control(vec![reply(vec![], None)]);
        let (handle, tx) = start(&rig, &backend, &ctl);
        sleep(Duration::from_secs(1)).await;
        let attempt = Uuid::from_u128(5);
        let dir = driver(&rig, 1, attempt);
        let signalled_at = Instant::now();
        tx.send_replace(true);
        sleep(Duration::from_secs(1)).await;
        assert_eq!(
            ctl.handoffs
                .lock()
                .unwrap()
                .iter()
                .map(|(slot, n)| (*slot, n.attempt_id, n.reason))
                .collect::<Vec<_>>(),
            [(1, attempt, HandoffReason::ServiceStop)]
        );
        let marker: serde_json::Value =
            serde_json::from_slice(&std::fs::read(dir.join("handoff")).unwrap()).unwrap();
        assert_eq!(marker["reason"], "service_stop");
        assert!(
            backend.released.lock().unwrap().is_none(),
            "the slot keeps running through the grace period"
        );
        handle.await.unwrap().unwrap();
        assert!(
            backend.released.lock().unwrap().unwrap() - signalled_at >= Duration::from_secs(10)
        );
    }

    #[tokio::test(start_paused = true)]
    async fn revoked_credential_pauses_all_slots() {
        let rig = rig(1);
        let backend = Arc::new(FakeBackend::default());
        script(&backend, 1, vec![Step::Observe, Step::Hang]);
        let ctl = control(vec![reply(vec![], None), Err((401, true))]);
        let (handle, tx) = start(&rig, &backend, &ctl);
        sleep(Duration::from_secs(16)).await;
        assert_eq!(ctl.polls.lock().unwrap().len(), 4);
        assert!(*backend.observed.lock().unwrap().clone().unwrap().borrow());
        sleep(Duration::from_secs(49)).await;
        assert_eq!(
            ctl.polls.lock().unwrap().len(),
            4,
            "the next poll waits 60 s"
        );
        sleep(Duration::from_secs(11)).await;
        assert_eq!(ctl.polls.lock().unwrap().len(), 5);
        assert_eq!(
            backend.calls.lock().unwrap().len(),
            1,
            "no attempt starts while paused"
        );
        stop(handle, tx).await;
    }

    fn journal(rig: &Rig, slot: u32) {
        std::fs::create_dir_all(rig.layout.current(slot)).unwrap();
        std::fs::write(rig.layout.current(slot).join("journal"), b"j").unwrap();
    }

    /// Review #1: an attempt that fails after admission keeps current/journal, and the next
    /// round recovers it (handoff notice and salvage) before new work, which waits out the
    /// back-off of the failure.
    #[tokio::test(start_paused = true)]
    async fn failed_admitted_attempt_stays_for_recovery() {
        let rig = rig(1);
        let backend = Arc::new(FakeBackend {
            retained_empty: true,
            ..Default::default()
        });
        script(&backend, 1, vec![Step::ErrorAdmitted, Step::Hang]);
        let (handle, tx) = start(&rig, &backend, &control(vec![reply(vec![], None)]));
        sleep(Duration::from_secs(1)).await;
        assert_eq!(
            *backend.calls.lock().unwrap(),
            ["run:1", "recover:1", "run:1"]
        );
        assert_eq!(
            count(rig.layout.archive_dir(1)),
            1,
            "archived after recovery"
        );
        let starts = backend.starts.lock().unwrap().clone();
        assert_eq!(starts[1].1, BACKOFF_START, "new work waits the back-off");
        assert_eq!(backend.run_times.lock().unwrap().len(), 1);
        stop(handle, tx).await;
    }

    /// Review #2: with every slot held the service does not return (systemd would restart it
    /// every 10 s); it keeps polling control and never recovers the held slots again.
    #[tokio::test(start_paused = true)]
    async fn all_slots_held_keeps_the_service_running() {
        let rig = rig(2);
        let backend = Arc::new(FakeBackend::default());
        for slot in 1..=2 {
            journal(&rig, slot);
            let held = Ok(RecoverOutcome::Held("sandbox_absent".into()));
            backend.recovery.lock().unwrap().insert(slot, [held].into());
        }
        let ctl = control(vec![reply(vec![], None)]);
        let (handle, tx) = start(&rig, &backend, &ctl);
        sleep(Duration::from_secs(3600)).await;
        assert!(!handle.is_finished());
        let mut calls = backend.calls.lock().unwrap().clone();
        calls.sort();
        assert_eq!(calls, ["recover:1", "recover:2"]);
        assert!(
            ctl.polls.lock().unwrap().len() > 700,
            "control keeps polling"
        );
        assert!(ctl.handoffs.lock().unwrap().is_empty());
        stop(handle, tx).await;
    }

    /// Review #3: a recovery error other than Held is retried with backoff; the slot then runs.
    #[tokio::test(start_paused = true)]
    async fn transient_recovery_error_retries_then_runs() {
        let rig = rig(1);
        journal(&rig, 1);
        // R1: a reason left by an earlier hold is not shown while recovery retries.
        write_private(&rig.layout.slot_held(1), b"journal_unreadable: node: old").unwrap();
        let backend = Arc::new(FakeBackend::default());
        let outcomes = [Err(()), Ok(RecoverOutcome::Salvaged)];
        backend.recovery.lock().unwrap().insert(1, outcomes.into());
        script(&backend, 1, vec![Step::Hang]);
        let (handle, tx) = start(&rig, &backend, &control(vec![reply(vec![], None)]));
        sleep(Duration::from_secs(29)).await;
        assert_eq!(*backend.calls.lock().unwrap(), ["recover:1"]);
        assert_eq!(rig.layout.held_reason(1), None, "retrying, not held");
        sleep(Duration::from_secs(2)).await;
        assert_eq!(
            *backend.calls.lock().unwrap(),
            ["recover:1", "recover:1", "run:1"]
        );
        stop(handle, tx).await;
    }

    /// Review M1: setting a flag to its current value wakes nobody.
    #[test]
    fn pause_notifies_only_on_change() {
        let pause = Pause {
            tx: watch::channel(false).0,
            flags: Mutex::new([false; 3]),
        };
        let mut rx = pause.tx.subscribe();
        pause.set(STOPPING, false);
        assert!(!rx.has_changed().unwrap());
        pause.set(STOPPING, true);
        assert!(rx.has_changed().unwrap() && *rx.borrow_and_update());
        pause.set(STOPPING, true);
        pause.set(REVOKED, true);
        pause.set(STOPPING, false);
        assert!(!rx.has_changed().unwrap(), "still paused by REVOKED");
        pause.set(REVOKED, false);
        assert!(rx.has_changed().unwrap() && !*rx.borrow_and_update());
    }

    /// Review M5: an edge's redirect or bare 403 backs the control poll off without pausing;
    /// AQ's own 403 is a revoke.
    #[tokio::test(start_paused = true)]
    async fn edge_refusal_backs_off_without_pausing() {
        for (status, aq) in [(302, false), (403, false)] {
            let rig = rig(1);
            let backend = Arc::new(FakeBackend::default());
            script(&backend, 1, vec![Step::Observe, Step::Hang]);
            let ctl = control(vec![reply(vec![], None), Err((status, aq))]);
            let (handle, tx) = start(&rig, &backend, &ctl);
            sleep(Duration::from_secs(1000)).await;
            assert!(
                !*backend.observed.lock().unwrap().clone().unwrap().borrow(),
                "{status}"
            );
            let times = ctl.polls.lock().unwrap().clone();
            let gaps: Vec<u64> = times.windows(2).map(|w| (w[1] - w[0]).as_secs()).collect();
            assert_eq!(gaps[..7], [5, 10, 20, 40, 80, 160, 300], "{status}");
            stop(handle, tx).await;
        }
        let rig = rig(1);
        let backend = Arc::new(FakeBackend::default());
        script(&backend, 1, vec![Step::Observe, Step::Hang]);
        let ctl = control(vec![reply(vec![], None), Err((403, true))]);
        let (handle, tx) = start(&rig, &backend, &ctl);
        sleep(Duration::from_secs(16)).await;
        assert!(*backend.observed.lock().unwrap().clone().unwrap().borrow());
        stop(handle, tx).await;
    }

    /// Review M4: a SIGTERM while an attempt prepares (no driver.json yet) marks the slot, so
    /// the restarted node reports the attempt as a service stop, not a crash.
    #[tokio::test(start_paused = true)]
    async fn stop_during_preparation_marks_the_slot_as_service_stop() {
        let rig = rig(1);
        let backend = Arc::new(FakeBackend::default());
        script(&backend, 1, vec![Step::Hang]);
        let (handle, tx) = start(&rig, &backend, &control(vec![reply(vec![], None)]));
        sleep(Duration::from_secs(1)).await;
        let (host, current) = (rig.layout.host_root(1), rig.layout.current(1));
        assert_eq!(recorded_handoff(&host, &current, Uuid::from_u128(9)), None);
        tx.send_replace(true);
        sleep(Duration::from_secs(1)).await;
        assert_eq!(
            recorded_handoff(&host, &current, Uuid::from_u128(9)),
            Some(HandoffReason::ServiceStop)
        );
        handle.await.unwrap().unwrap();
    }

    /// A held slot's state is not marked as a service stop: it was not interrupted by this stop.
    #[tokio::test(start_paused = true)]
    async fn held_slot_is_not_marked_on_stop() {
        let rig = rig(1);
        journal(&rig, 1);
        let backend = Arc::new(FakeBackend::default());
        let held = Ok(RecoverOutcome::Held("sandbox_absent".into()));
        backend.recovery.lock().unwrap().insert(1, [held].into());
        let (handle, tx) = start(&rig, &backend, &control(vec![reply(vec![], None)]));
        sleep(Duration::from_secs(1)).await;
        stop(handle, tx).await;
        assert!(!rig.layout.current(1).join(SERVICE_STOP_MARKER).exists());
    }

    /// Round 2, A: claimed sandboxes are released after every attempt; when that fails the
    /// attempt stays in current and the next round's settle releases it before archiving.
    #[tokio::test(start_paused = true)]
    async fn claimed_sandboxes_are_released_after_the_attempt_or_by_settle() {
        let rig = rig(1);
        let backend = Arc::new(FakeBackend::default());
        *backend.release_failures.lock().unwrap() = 1;
        script(&backend, 1, vec![Step::ErrorAdmitted, Step::Hang]);
        let (handle, tx) = start(&rig, &backend, &control(vec![reply(vec![], None)]));
        sleep(Duration::from_secs(1)).await;
        assert_eq!(
            *backend.releases.lock().unwrap(),
            [1, 1],
            "after the attempt (failed), then by settle before it archives"
        );
        assert_eq!(
            *backend.calls.lock().unwrap(),
            ["run:1", "recover:1", "run:1"]
        );
        assert_eq!(count(rig.layout.archive_dir(1)), 1);
        stop(handle, tx).await;
        // A finished attempt whose release fails is kept too.
        let rig = self::rig(1);
        let backend = Arc::new(FakeBackend::default());
        *backend.release_failures.lock().unwrap() = 1;
        script(&backend, 1, vec![Step::Finished, Step::Hang]);
        let (handle, tx) = start(&rig, &backend, &control(vec![reply(vec![], None)]));
        sleep(Duration::from_secs(1)).await;
        // Kept, then settled at once (a finished attempt resets the backoff).
        assert_eq!(
            *backend.calls.lock().unwrap(),
            ["run:1", "recover:1", "run:1"]
        );
        assert_eq!(*backend.releases.lock().unwrap(), [1, 1]);
        assert_eq!(count(rig.layout.archive_dir(1)), 1);
        stop(handle, tx).await;
    }

    /// Round 2, C: a stop during the recovery backoff does not mark the attempt that already
    /// failed: it was not stopped by the service, so its crash stays charged.
    #[tokio::test(start_paused = true)]
    async fn stop_during_recovery_backoff_does_not_mark_the_failed_attempt() {
        let rig = rig(1);
        let backend = Arc::new(FakeBackend::default());
        script(&backend, 1, vec![Step::ErrorAdmitted, Step::Hang]);
        backend.recovery.lock().unwrap().insert(1, [Err(())].into());
        let ctl = control(vec![reply(vec![Uuid::from_u128(42)], None)]);
        let (handle, tx) = start(&rig, &backend, &ctl);
        sleep(Duration::from_secs(6)).await;
        stop(handle, tx).await;
        let (host, current) = (rig.layout.host_root(1), rig.layout.current(1));
        assert!(current.join("journal").exists());
        assert!(host.join("driver-1/driver.json").exists());
        assert!(
            !host.join("driver-1/handoff").exists(),
            "no owner or service marker"
        );
        assert!(!current.join(SERVICE_STOP_MARKER).exists());
        assert!(ctl.handoffs.lock().unwrap().is_empty());
        assert_eq!(recorded_handoff(&host, &current, Uuid::from_u128(42)), None);
    }

    /// Round 2, F: a notice post that never answers cannot hold the stop past the grace plus
    /// the stop margin (systemd's TimeoutStopSec).
    #[tokio::test(start_paused = true)]
    async fn a_hanging_handoff_post_stays_inside_the_stop_budget() {
        let rig = rig(1);
        let backend = Arc::new(FakeBackend::default());
        script(&backend, 1, vec![Step::Hang]);
        let ctl = control(vec![reply(vec![], None)]);
        *ctl.hang_handoffs.lock().unwrap() = true;
        let (handle, tx) = start(&rig, &backend, &ctl);
        sleep(Duration::from_secs(1)).await;
        driver(&rig, 1, Uuid::from_u128(5));
        let signalled = Instant::now();
        stop(handle, tx).await;
        let budget = Duration::from_millis(rig.config.shutdown_grace_ms) + STOP_MARGIN;
        assert!(signalled.elapsed() < budget, "{:?}", signalled.elapsed());
        assert!(
            backend.released.lock().unwrap().is_some(),
            "the slot was forced"
        );
    }

    /// Round 3: every running attempt is marked before any notice is posted, so a hung post
    /// for one slot does not delay another slot's marker.
    #[tokio::test(start_paused = true)]
    async fn every_stop_marker_precedes_the_first_post() {
        let rig = rig(2);
        let backend = Arc::new(FakeBackend::default());
        for slot in 1..=2 {
            script(&backend, slot, vec![Step::Hang]);
        }
        let ctl = control(vec![reply(vec![], None)]);
        *ctl.hang_handoffs.lock().unwrap() = true;
        let (handle, tx) = start(&rig, &backend, &ctl);
        sleep(Duration::from_secs(1)).await;
        let dirs = [
            driver(&rig, 1, Uuid::from_u128(1)),
            driver(&rig, 2, Uuid::from_u128(2)),
        ];
        tx.send_replace(true);
        sleep(Duration::from_millis(100)).await;
        assert_eq!(
            ctl.handoffs.lock().unwrap().len(),
            1,
            "the first post hangs"
        );
        assert!(
            dirs.iter().all(|d| d.join("handoff").exists()),
            "both marked"
        );
        handle.await.unwrap().unwrap();
    }

    /// A qualification pin in the control reply (current, switching, not yet effective) or
    /// none at all changes nothing: the slots poll at once and are never paused by it.
    #[tokio::test(start_paused = true)]
    async fn a_qualification_pin_in_the_control_reply_is_ignored() {
        let now = wall();
        let soon = now + 20_000;
        for replies in [
            vec![reply(vec![], None)],
            vec![reply(vec![], Some(pin("1", now - 1000, now + 3_600_000)))],
            vec![
                reply(vec![], Some(pin("1", now - 1000, now + 86_400_000))),
                reply(vec![], Some(pin("2", soon, soon + 86_400_000))),
            ],
            vec![reply(vec![], Some(pin("2", soon, soon + 86_400_000)))],
        ] {
            let rig = rig(1);
            let backend = Arc::new(FakeBackend::default());
            script(&backend, 1, vec![Step::Observe]);
            let ctl = control(replies);
            let (handle, tx) = start(&rig, &backend, &ctl);
            sleep(Duration::from_secs(1)).await;
            assert_eq!(*backend.calls.lock().unwrap(), ["run:1"], "polls at once");
            let paused = backend.observed.lock().unwrap().clone().unwrap();
            for _ in 0..90 {
                sleep(Duration::from_secs(1)).await;
                assert!(!*paused.borrow(), "never paused");
            }
            sleep(Duration::from_secs(3600)).await;
            assert_eq!(*backend.calls.lock().unwrap(), ["run:1"]);
            stop(handle, tx).await;
        }
    }

    /// No slot polls before the first successful control reply; a failed control poll does
    /// not count.
    #[tokio::test(start_paused = true)]
    async fn slots_wait_for_the_first_control_reply() {
        let rig = rig(1);
        let backend = Arc::new(FakeBackend::default());
        script(&backend, 1, vec![Step::Hang]);
        let ctl = control(vec![Err((500, false)), reply(vec![], None)]);
        let (handle, tx) = start(&rig, &backend, &ctl);
        sleep(Duration::from_secs(4)).await;
        assert!(backend.calls.lock().unwrap().is_empty(), "no control reply");
        sleep(Duration::from_secs(2)).await;
        assert_eq!(*backend.calls.lock().unwrap(), ["run:1"]);
        stop(handle, tx).await;
    }

    /// D4: each slot's stop notice is posted for its own slot.
    #[tokio::test(start_paused = true)]
    async fn each_stop_notice_is_posted_for_its_own_slot() {
        let rig = rig(2);
        let backend = Arc::new(FakeBackend::default());
        for slot in 1..=2 {
            script(&backend, slot, vec![Step::Hang]);
        }
        let ctl = control(vec![reply(vec![], None)]);
        let (handle, tx) = start(&rig, &backend, &ctl);
        sleep(Duration::from_secs(1)).await;
        let (a1, a2) = (Uuid::from_u128(1), Uuid::from_u128(2));
        driver(&rig, 1, a1);
        driver(&rig, 2, a2);
        tx.send_replace(true);
        sleep(Duration::from_secs(1)).await;
        let mut posted: Vec<_> = (ctl.handoffs.lock().unwrap().iter())
            .map(|(slot, n)| (*slot, n.attempt_id))
            .collect();
        posted.sort();
        assert_eq!(posted, [(1, a1), (2, a2)]);
        handle.await.unwrap().unwrap();
    }

    /// D4: `MachineLink` posts a slot's notice with that slot's node credential, and the
    /// control poll with slot 1's.
    #[tokio::test]
    async fn machine_link_posts_each_notice_with_its_slots_credential() {
        use axum::{Json, Router, http::HeaderMap, routing::post};
        let seen = Arc::new(Mutex::new(Vec::<(String, String)>::new()));
        let record = |route: &'static str, seen: Arc<Mutex<Vec<(String, String)>>>| {
            post(
                move |h: HeaderMap, Json(body): Json<serde_json::Value>| async move {
                    let bearer = h["authorization"].to_str().unwrap().to_owned();
                    seen.lock().unwrap().push((route.to_owned(), bearer));
                    Json(match route {
                        // The node-slots contract: the reply carries the request's schema
                        // version and, at version 2, the node's slot count.
                        "control" => serde_json::json!({"schema_version": body["schema_version"],
                        "machine_key": "desk", "desired": "running", "generation": 1,
                        "handoff": [], "slots": 2}),
                        _ => body,
                    })
                },
            )
        };
        let app = Router::new()
            .route("/v1/worker/handoffs", record("handoffs", seen.clone()))
            .route("/v1/worker/control", record("control", seen.clone()));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let origin = format!("http://{}", listener.local_addr().unwrap());
        let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let rig = rig(2);
        for slot in 1..=2 {
            EnrollmentFile {
                server_url: origin.clone(),
                node_id: Uuid::from_u128(slot as u128),
                credential: format!("aqn_slot{slot}"),
                cf_client_id: None,
                cf_client_secret: None,
            }
            .save(&rig.layout.slot_enrollment(slot))
            .unwrap();
        }
        let first = EnrollmentFile::load(&rig.layout.slot_enrollment(1)).unwrap();
        let primary = Arc::new(ServerLink::from_enrollment(&first).unwrap());
        let link = MachineLink::new(primary, &rig.layout, 2);
        let notice = |id| HandoffNotice {
            schema_version: 1,
            attempt_id: Uuid::from_u128(id),
            reason: HandoffReason::ServiceStop,
        };
        link.handoff(2, &notice(2)).await.unwrap();
        link.handoff(1, &notice(1)).await.unwrap();
        let reply = link.control().await.unwrap();
        assert_eq!(
            (reply.schema_version, reply.node_slots()),
            (aq_protocol::NODE_SLOTS_SCHEMA_VERSION, 2)
        );
        assert!(link.handoff(3, &notice(3)).await.is_err(), "no slot 3");
        server.abort();
        let seen = seen.lock().unwrap().clone();
        let expected = [
            ("handoffs", "Bearer aqn_slot2"),
            ("handoffs", "Bearer aqn_slot1"),
            ("control", "Bearer aqn_slot1"),
        ];
        assert_eq!(seen, expected.map(|(r, b)| (r.to_owned(), b.to_owned())));
    }

    /// D2: the service is killed while an attempt runs (no stop marker, no archive, its aq-git
    /// killed and its socket left behind). The restarted service starts aq-git again on the
    /// slot's state, recovers (salvages) the attempt, archives it and takes new work.
    #[tokio::test]
    async fn a_killed_service_salvages_the_running_attempt_on_restart() {
        let mut rig = rig(1);
        let scripts = tempfile::tempdir().unwrap();
        rig.config.aq_git = crate::git::tests::fake_aq_git(scripts.path(), None);
        let git = || Arc::new(AqGitFactory(rig.config.aq_git.clone()));
        async fn until(what: &str, done: impl Fn() -> bool) {
            for _ in 0..400 {
                if done() {
                    return;
                }
                sleep(Duration::from_millis(25)).await;
            }
            panic!("timed out waiting for {what}");
        }
        let backend = Arc::new(FakeBackend::default());
        script(&backend, 1, vec![Step::Running]);
        let ctl = control(vec![reply(vec![], None)]);
        let (handle, _tx) = start_with(&rig, &backend, &ctl, git());
        let socket = rig.layout.git_socket(1);
        until("the attempt", || backend.calls.lock().unwrap().len() == 1).await;
        until("its driver", || {
            rig.layout.host_root(1).join("driver-1").exists()
        })
        .await;
        assert!(std::os::unix::net::UnixStream::connect(&socket).is_ok());
        // SIGKILL: nothing of the graceful stop runs; the aq-git child dies with the service.
        handle.abort();
        let _ = handle.await;
        until("aq-git to die", || {
            std::os::unix::net::UnixStream::connect(&socket).is_err()
        })
        .await;
        assert!(socket.exists(), "the killed aq-git left its socket");
        assert!(rig.layout.current(1).join("journal").exists());

        let backend = Arc::new(FakeBackend::default());
        let salvaged = [Ok(RecoverOutcome::Salvaged)];
        backend.recovery.lock().unwrap().insert(1, salvaged.into());
        script(&backend, 1, vec![Step::Hang]);
        let (handle, tx) = start_with(&rig, &backend, &ctl, git());
        until("recovery and new work", || {
            backend.calls.lock().unwrap().len() == 2
        })
        .await;
        assert_eq!(*backend.calls.lock().unwrap(), ["recover:1", "run:1"]);
        assert_eq!(
            count(rig.layout.archive_dir(1)),
            1,
            "archived after salvage"
        );
        assert!(
            !rig.layout.host_root(1).join("driver-1").exists(),
            "a fresh runtime dir"
        );
        tx.send_replace(true);
        handle.await.unwrap().unwrap();
    }

    /// After a reboot the runtime dir (the host root and its driver markers) is gone: settle
    /// still releases the claimed sandboxes (the backend reads which are preserved from the
    /// journal), then archives.
    #[tokio::test(start_paused = true)]
    async fn settle_releases_claimed_sandboxes_after_the_runtime_dir_is_wiped() {
        let rig = rig(1);
        journal(&rig, 1);
        assert!(
            !rig.layout.host_root(1).exists(),
            "no runtime dir (after a reboot)"
        );
        let backend = Arc::new(FakeBackend::default());
        let salvaged = [Ok(RecoverOutcome::Salvaged)];
        backend.recovery.lock().unwrap().insert(1, salvaged.into());
        script(&backend, 1, vec![Step::Hang]);
        let (handle, tx) = start(&rig, &backend, &control(vec![reply(vec![], None)]));
        sleep(Duration::from_secs(1)).await;
        assert_eq!(*backend.calls.lock().unwrap(), ["recover:1", "run:1"]);
        assert_eq!(
            backend.releases.lock().unwrap()[0],
            1,
            "released before the archive"
        );
        assert_eq!(count(rig.layout.archive_dir(1)), 1);
        stop(handle, tx).await;
    }

    /// D2 ruling C: a lost attempt released to the server frees its slot: it is archived (its
    /// record kept) and the slot takes new work.
    #[tokio::test(start_paused = true)]
    async fn a_released_lost_attempt_frees_the_slot() {
        let rig = rig(1);
        journal(&rig, 1);
        let backend = Arc::new(FakeBackend::default());
        backend
            .recovery
            .lock()
            .unwrap()
            .insert(1, [Ok(RecoverOutcome::Lost)].into());
        script(&backend, 1, vec![Step::Hang]);
        let (handle, tx) = start(&rig, &backend, &control(vec![reply(vec![], None)]));
        sleep(Duration::from_secs(1)).await;
        assert_eq!(*backend.calls.lock().unwrap(), ["recover:1", "run:1"]);
        assert_eq!(count(rig.layout.archive_dir(1)), 1, "kept in the archive");
        stop(handle, tx).await;
    }
    const REFUSED: &str = "native endpoint returned 409 Conflict";
    fn secs(times: &[Instant], since: Instant) -> Vec<u64> {
        times.iter().map(|t| (*t - since).as_secs()).collect()
    }
    /// The laptop's loop: an offer the server refused (409) cost the slot 30 s to 300 s with no
    /// report. Now the slot is settled and up again at once, reports while it waits the short
    /// pause, and takes the next offer; the control poll keeps its schedule throughout.
    #[tokio::test(start_paused = true)]
    async fn a_refused_offer_frees_the_slot_after_the_short_pause() {
        let rig = rig(1);
        let backend = Arc::new(FakeBackend {
            retained_empty: true,
            ..Default::default()
        });
        script(&backend, 1, vec![Step::Fail(REFUSED), Step::Hang]);
        let ctl = control(vec![reply(vec![], None)]);
        let began = Instant::now();
        let (handle, tx) = start(&rig, &backend, &ctl);
        sleep(Duration::from_secs(12)).await;
        let starts = backend.starts.lock().unwrap().clone();
        let pause = Duration::from_millis(rig.config.retry.offer_pause_ms);
        assert_eq!(pause, Duration::from_secs(3), "the default");
        assert_eq!(
            starts.iter().map(|s| s.1).collect::<Vec<_>>(),
            [Duration::ZERO, pause],
            "up again at once, asking for no offer during the pause"
        );
        assert_eq!(starts[1].0, starts[0].0, "it reports from the failure on");
        let polled = backend.run_times.lock().unwrap().clone();
        assert_eq!(
            secs(&polled, starts[0].0),
            [0, 3],
            "the next offer is taken"
        );
        assert_eq!(secs(&ctl.polls.lock().unwrap(), began), [0, 5, 10]);
        assert_eq!(count(rig.layout.archive_dir(1)), 0, "nothing was admitted");
        stop(handle, tx).await;
    }
    /// Both branches at once: a node with two agent slots, where the offer of slot 1 is refused
    /// (409) while slot 2 runs an attempt. Slot 2 is not interrupted, the node is registered
    /// once under one boot, its control poll and its single inventory stream go on (slot 1
    /// reports into it while it waits), and slot 1 takes the next offer after the short pause.
    #[tokio::test(start_paused = true)]
    async fn a_refused_offer_on_one_slot_leaves_its_sibling_and_the_node_as_they_were() {
        let mut rig = rig(1);
        rig.layout.node = Some(1);
        let backend = Arc::new(FakeBackend {
            retained_empty: true,
            ..Default::default()
        });
        let (done, rx) = watch::channel(false);
        script(
            &backend,
            1,
            vec![Step::Fail(REFUSED), Step::Until(rx.clone())],
        );
        script(&backend, 2, vec![Step::Until(rx)]);
        let ctl = control(vec![slots_reply(2)]);
        let began = Instant::now();
        let (handle, stop_tx) = start(&rig, &backend, &ctl);
        sleep(Duration::from_secs(12)).await;
        let contexts = backend.contexts.lock().unwrap().clone();
        let starts = backend.starts.lock().unwrap().clone();
        let of = |slot: u32| -> Vec<(u64, u64)> {
            let mine = contexts.iter().zip(&starts).filter(|(c, _)| c.slot == slot);
            mine.map(|(_, (at, hold))| ((*at - began).as_secs(), hold.as_secs()))
                .collect()
        };
        assert_eq!(of(1), [(0, 0), (0, 3)], "up at once, holding the 3 s");
        assert_eq!(of(2), [(0, 0)], "the sibling was started once");
        let running = contexts.iter().find(|c| c.slot == 2).unwrap();
        assert!(running.state_dir.exists(), "and still runs its attempt");
        assert_ne!(running.state_dir, contexts[0].state_dir);
        assert_ne!(running.git_socket, contexts[0].git_socket);
        assert!(
            contexts.iter().all(|c| c.node_id == Uuid::from_u128(1)
                && c.enrollment == rig.layout.slot_enrollment(1)),
            "one node identity"
        );
        let polled = backend.run_times.lock().unwrap().clone();
        assert_eq!(
            secs(&polled, began),
            [0, 0, 3],
            "slot 1 takes an offer after the pause"
        );
        let session = &backend.session;
        assert_eq!(
            *backend.registrations.lock().unwrap(),
            [(session.boot_id, session.registration_id, 2)],
            "registered once, under one boot"
        );
        assert_eq!(secs(&ctl.polls.lock().unwrap(), began), [0, 5, 10]);
        assert_eq!(backend.controls.lock().unwrap().len(), 3);
        let reports = backend.reports.lock().unwrap().clone();
        let sequences: Vec<u64> = reports.iter().map(|r| r.1).collect();
        assert_eq!(
            sequences,
            (1..=reports.len() as u64).collect::<Vec<_>>(),
            "one inventory stream for the node"
        );
        let waiting: Vec<u64> = (reports.iter().filter(|r| r.0 == 1))
            .map(|r| (r.2 - began).as_secs())
            .collect();
        assert_eq!(waiting, [0, 0], "slot 1 reported again as it began to wait");
        assert_eq!(session.inventory.acknowledged.load(SeqCst), 3);
        done.send_replace(true);
        spin().await;
        stop(handle, stop_tx).await;
    }
    /// Repeats of the same kind double the pause up to the configured ceiling; another kind
    /// starts at the short pause again, and so does the first failure after a finished attempt.
    #[tokio::test(start_paused = true)]
    async fn repeated_refusals_stop_at_the_configured_ceiling_and_reset() {
        let mut rig = rig(1);
        rig.config.retry.offer_pause_max_ms = 20_000;
        let backend = Arc::new(FakeBackend {
            retained_empty: true,
            ..Default::default()
        });
        let source = "aq-git refused: \"repository_not_found\" \"Repository is not registered\"";
        let mut steps: Vec<Step> = (0..6).map(|_| Step::Fail(REFUSED)).collect();
        steps.extend([Step::Fail(source), Step::Fail(source), Step::Finished]);
        steps.extend([Step::Fail(REFUSED), Step::Hang]);
        script(&backend, 1, steps);
        let (handle, tx) = start(&rig, &backend, &control(vec![reply(vec![], None)]));
        sleep(Duration::from_secs(300)).await;
        let times = backend.run_times.lock().unwrap().clone();
        let gaps: Vec<u64> = times.windows(2).map(|w| (w[1] - w[0]).as_secs()).collect();
        assert_eq!(gaps, [3, 6, 12, 20, 20, 20, 3, 6, 0, 3]);
        stop(handle, tx).await;
    }
    /// A fault of this machine keeps the long back-off, and the slot reports during it. A
    /// slot that cannot even stay up (it fails as it starts) waits here instead, without
    /// reporting, and is not started again before the back-off has passed.
    #[tokio::test(start_paused = true)]
    async fn a_local_fault_backs_off_long_and_a_slot_that_cannot_stay_up_waits_outside() {
        let rig = rig(2);
        let backend = Arc::new(FakeBackend {
            retained_empty: true,
            ..Default::default()
        });
        let sandbox = "Claude driver host did not establish Running";
        script(&backend, 1, vec![Step::Fail(sandbox), Step::Hang]);
        script(
            &backend,
            2,
            vec![Step::Error, Step::Down, Step::Error, Step::Hang],
        );
        let ctl = control(vec![reply(vec![], None)]);
        let began = Instant::now();
        let (handle, tx) = start(&rig, &backend, &ctl);
        sleep(Duration::from_secs(100)).await;
        let calls = backend.calls.lock().unwrap().clone();
        let starts = backend.starts.lock().unwrap().clone();
        let of = |slot: &str| -> Vec<(u64, u64)> {
            let mine = calls.iter().zip(&starts).filter(|(c, _)| c.ends_with(slot));
            mine.map(|(_, (at, hold))| ((*at - began).as_secs(), hold.as_secs()))
                .collect()
        };
        assert_eq!(of(":1"), [(0, 0), (0, 30)], "up and reporting for the 30 s");
        assert_eq!(of(":2"), [(0, 0), (0, 30), (30, 0), (90, 0)]);
        let polls = secs(&ctl.polls.lock().unwrap(), began);
        assert_eq!(polls, (0..20).map(|n| n * 5).collect::<Vec<u64>>());
        stop(handle, tx).await;
    }
    /// The texts are the ones the service logged on the laptop and the desktop.
    #[test]
    fn failures_are_classified_from_their_text() {
        use FailureKind::*;
        for (text, kind) in [
            (REFUSED, Refused),
            ("native endpoint returned 503 Service Unavailable", Refused),
            (
                "source transfer endpoint returned 503 Service Unavailable",
                Refused,
            ),
            ("native endpoint returned 413 Payload Too Large", Refused),
            (
                "aq-git refused: \"repository_not_found\" \"Repository is not registered\"",
                Offer,
            ),
            ("native source bulk lane overloaded", Offer),
            (
                "native control transport failed; disposition unknown",
                Unreachable,
            ),
            ("native endpoint returned 401 Unauthorized", Fault),
            ("native endpoint returned 500 Internal Server Error", Fault),
            ("Claude driver host did not establish Running", Fault),
            ("No space left on device (os error 28)", Fault),
        ] {
            assert_eq!(FailureKind::of(text), kind, "{text}");
        }
    }
    /// The repositories list only pre-registers: what it names is registered with the slot's
    /// aq-git before the attempt, and without a list the attempt runs all the same (a
    /// repository is then registered from the offer that names it).
    #[tokio::test(start_paused = true)]
    async fn listed_repositories_are_registered_before_the_attempt_and_none_are_needed() {
        struct Recording(Arc<Mutex<Vec<Vec<RepositoryBinding>>>>);
        #[async_trait::async_trait]
        impl GitFactory for Recording {
            async fn start(&self, _: &Path, _: &Path) -> Result<Box<dyn GitChild>> {
                Ok(Box::new(Recording(self.0.clone())))
            }
        }
        #[async_trait::async_trait]
        impl GitChild for Recording {
            async fn register(&self, repos: &[RepositoryBinding]) -> Result<()> {
                self.0.lock().unwrap().push(repos.to_vec());
                Ok(())
            }
            async fn stop(self: Box<Self>) -> Result<()> {
                Ok(())
            }
        }
        let listed = RepositoryBinding {
            repository_id: Uuid::from_u128(7),
            identity: "o/r".into(),
            binding_generation: 1,
            object_format: "sha1".into(),
        };
        for repositories in [vec![], vec![listed]] {
            let mut rig = rig(1);
            rig.config.repositories = repositories.clone();
            let backend = Arc::new(FakeBackend::default());
            script(&backend, 1, vec![Step::Hang]);
            let seen = Arc::new(Mutex::new(Vec::new()));
            let git = Arc::new(Recording(seen.clone()));
            let ctl = control(vec![reply(vec![], None)]);
            let (handle, tx) = start_with(&rig, &backend, &ctl, git);
            sleep(Duration::from_secs(1)).await;
            assert_eq!(*seen.lock().unwrap(), [repositories]);
            assert_eq!(*backend.calls.lock().unwrap(), ["run:1"]);
            stop(handle, tx).await;
        }
    }
}
