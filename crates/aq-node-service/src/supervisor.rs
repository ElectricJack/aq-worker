//! The slot supervisor: one polling loop per slot, the control poll, handoff
//! markers, the renderer switch, measurement renewal and the graceful stop.
use crate::{
    EnrollmentFile, Layout, LinkError, RepositoryBinding, ServerLink, ServiceConfig,
    config::{AqGitConfig, STOP_MARGIN},
    enrollment_file::write_private,
    git::AqGit,
    measure::measure_lock,
};
use anyhow::{Context, Result};
use aq_protocol::{
    ArtifactPin, ControlResponse, HandoffNotice, HandoffReason, MeasurementSubmission,
    QualificationPin,
};
use std::{
    collections::HashSet,
    os::unix::fs::OpenOptionsExt,
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
    time::Duration,
};
use tokio::{
    sync::watch,
    task::{JoinHandle, JoinSet},
    time::Instant,
};
use uuid::Uuid;

#[derive(Debug, Clone)]
pub struct SlotContext {
    pub slot: u32,
    pub node_id: Uuid,
    pub enrollment: PathBuf,
    pub state_dir: PathBuf,
    pub renderer: Option<ArtifactPin>,
}
#[derive(Debug, PartialEq, Eq)]
pub enum AttemptOutcome {
    AttemptStopped,
    Paused,
    ShutdownBeforeAdmission,
}
#[derive(Debug, PartialEq, Eq)]
pub enum RecoverOutcome {
    Clean,
    Reported,
    Salvaged,
    Held(String),
}
#[async_trait::async_trait]
pub trait SlotBackend: Send + Sync + 'static {
    async fn run_attempt(
        &self,
        ctx: SlotContext,
        shutdown: watch::Receiver<bool>,
        pause: watch::Receiver<bool>,
    ) -> Result<AttemptOutcome>;
    async fn recover(&self, ctx: SlotContext) -> Result<RecoverOutcome>;
    fn retained_attempts(&self, ctx: &SlotContext) -> Result<Vec<Uuid>>;
    /// Runs one measurement. When `cancel` turns true it stops, releases its sandbox and
    /// returns an error.
    async fn measure(
        &self,
        token_file: &Path,
        model: &str,
        cancel: watch::Receiver<bool>,
    ) -> Result<MeasurementSubmission>;
}
#[async_trait::async_trait]
pub trait ControlSource: Send + Sync + 'static {
    async fn control(&self) -> Result<ControlResponse>;
    async fn handoff(&self, notice: &HandoffNotice) -> Result<()>;
    async fn submit_measurement(&self, s: &MeasurementSubmission) -> Result<QualificationPin>;
}
#[async_trait::async_trait]
impl ControlSource for ServerLink {
    async fn control(&self) -> Result<ControlResponse> {
        ServerLink::control(self).await
    }
    async fn handoff(&self, notice: &HandoffNotice) -> Result<()> {
        ServerLink::handoff(self, notice).await
    }
    async fn submit_measurement(&self, s: &MeasurementSubmission) -> Result<QualificationPin> {
        ServerLink::submit_measurement(self, s).await
    }
}
#[async_trait::async_trait]
pub trait GitFactory: Send + Sync {
    async fn start(&self, state_dir: &Path) -> Result<Box<dyn GitChild>>;
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
    async fn start(&self, state_dir: &Path) -> Result<Box<dyn GitChild>> {
        Ok(Box::new(AqGit::start(&self.0, state_dir).await?))
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
pub const QUIET_WINDOW: Duration = Duration::from_secs(30);
pub const BACKOFF_START: Duration = Duration::from_secs(30);
pub const BACKOFF_MAX: Duration = Duration::from_secs(300);
const REVOKED_INTERVAL: Duration = Duration::from_secs(60);
/// After a failed measurement: this long, doubled after each further failure while the
/// machine has no pin, up to `MEASURE_RETRY_MAX`.
const MEASURE_RETRY: Duration = Duration::from_secs(600);
const MEASURE_RETRY_MAX: Duration = Duration::from_secs(7200);
const FORCE_WAIT: Duration = Duration::from_secs(20);
/// How long the stop waits for a cancelled measurement to release its sandbox.
const MEASURE_STOP_WAIT: Duration = Duration::from_secs(5);
// systemd's TimeoutStopSec is the grace plus STOP_MARGIN: the forced stop and the measurement
// release must fit in the margin.
const _: () = assert!(FORCE_WAIT.as_secs() + MEASURE_STOP_WAIT.as_secs() < STOP_MARGIN.as_secs());
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
/// searching every slot's current/host/driver-*/driver.json. True when written or already present.
pub fn mark_handoff(
    layout: &Layout,
    slots: u32,
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
/// Attempt ids of drivers that are running: driver.json present, exit.json absent.
pub fn running_attempts(layout: &Layout, slots: u32) -> Result<Vec<Uuid>> {
    Ok(driver_dirs(layout, slots)
        .into_iter()
        .filter(|(d, _)| !d.join("exit.json").exists())
        .map(|(_, id)| id)
        .collect())
}
fn driver_dirs(layout: &Layout, slots: u32) -> Vec<(PathBuf, Uuid)> {
    (1..=slots)
        .flat_map(|slot| driver_dirs_in(&layout.current(slot).join("host")))
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
/// driver directory under `state_dir/host`, else the slot's service-stop marker; None when
/// neither exists.
pub fn recorded_handoff(state_dir: &Path, attempt: Uuid) -> Option<HandoffReason> {
    let driver = driver_dirs_in(&state_dir.join("host"))
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
const QUIET: usize = 0;
const REVOKED: usize = 1;
const STOPPING: usize = 2;
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
#[derive(Default)]
struct Measuring {
    busy: bool,
    /// No new measurement before this.
    not_before: Option<Instant>,
    failures: u32,
}

pub struct Supervisor<B: SlotBackend, C: ControlSource> {
    config: ServiceConfig,
    layout: Layout,
    backend: Arc<B>,
    control: Arc<C>,
    git: Arc<dyn GitFactory>,
}
struct Shared<B: SlotBackend, C: ControlSource> {
    sup: Supervisor<B, C>,
    pause: Pause,
    stopping: watch::Sender<bool>,
    force: watch::Sender<bool>,
    renderer: Mutex<Option<ArtifactPin>>,
    latest: Mutex<Option<QualificationPin>>,
    measuring: Mutex<Measuring>,
    measurement: Mutex<Option<JoinHandle<()>>>,
    /// Slots inside `attempt` (their `current` belongs to a live attempt).
    active: Mutex<HashSet<u32>>,
    started: (Instant, u64),
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
        }
    }
    /// Supervises every slot until `shutdown`. It returns only then: a slot that cannot run
    /// (held, or no enrollment) never ends the service, which would restart it in a loop.
    pub async fn run(self, mut shutdown: watch::Receiver<bool>) -> Result<()> {
        let wall = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)?
            .as_millis() as u64;
        let renderer = std::fs::read(self.layout.renderer_pin())
            .ok()
            .and_then(|b| serde_json::from_slice(&b).ok());
        let shared = Arc::new(Shared {
            pause: Pause {
                tx: watch::channel(false).0,
                flags: Mutex::new([false; 3]),
            },
            stopping: watch::channel(false).0,
            force: watch::channel(false).0,
            renderer: Mutex::new(renderer),
            latest: Mutex::new(None),
            measuring: Mutex::default(),
            measurement: Mutex::new(None),
            active: Mutex::default(),
            started: (Instant::now(), wall),
            sup: self,
        });
        let control_task = tokio::spawn(shared.clone().control_loop());
        let mut loops = JoinSet::new();
        for slot in 1..=shared.sup.config.slots {
            loops.spawn(shared.clone().slot_loop(slot));
        }
        let signalled_first = tokio::select! {
            () = drain(&mut loops) => false,
            () = signalled(&mut shutdown) => true,
        };
        if !signalled_first {
            tracing::warn!(
                "no slot is running; the service stays up (control poll, measurement) until it is stopped"
            );
            signalled(&mut shutdown).await;
        }
        shared.stopping.send_replace(true);
        shared.pause.set(STOPPING, true);
        let grace = tokio::time::sleep(Duration::from_millis(shared.sup.config.shutdown_grace_ms));
        tokio::pin!(grace);
        let mut posted = HashSet::new();
        while !loops.is_empty() {
            shared.hand_off_running(&mut posted).await;
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
        let measurement = shared.measurement.lock().unwrap().take();
        if let Some(mut task) = measurement
            && tokio::time::timeout(MEASURE_STOP_WAIT, &mut task)
                .await
                .is_err()
        {
            tracing::error!(
                "the measurement did not release its sandbox in time; the next start sweeps it"
            );
            task.abort();
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
async fn drain(loops: &mut JoinSet<()>) {
    while loops.join_next().await.is_some() {}
}
/// Waits `backoff` (false when the service stops first), then doubles it up to `BACKOFF_MAX`.
async fn back_off(stopping: &mut watch::Receiver<bool>, backoff: &mut Duration) -> bool {
    let waited = tokio::select! {
        () = tokio::time::sleep(*backoff) => true,
        _ = stopping.wait_for(|v| *v) => false,
    };
    *backoff = (*backoff * 2).min(BACKOFF_MAX);
    waited
}

impl<B: SlotBackend, C: ControlSource> Shared<B, C> {
    fn now_ms(&self) -> u64 {
        self.started.1 + self.started.0.elapsed().as_millis() as u64
    }
    fn ctx(&self, slot: u32, node_id: Uuid, state_dir: PathBuf) -> SlotContext {
        SlotContext {
            slot,
            node_id,
            enrollment: self.sup.layout.slot_enrollment(slot),
            state_dir,
            renderer: self.renderer.lock().unwrap().clone(),
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
    /// held for the owner; an error is retried by the caller with backoff.
    async fn settle(&self, slot: u32, node_id: Uuid) -> Result<bool> {
        let layout = &self.sup.layout;
        let current = layout.current(slot);
        let ctx = self.ctx(slot, node_id, current.clone());
        if current.join("journal").exists() {
            if let RecoverOutcome::Held(why) = self.recover(&ctx).await? {
                tracing::error!(slot, "slot held until the owner releases it: {why}");
                return Ok(false);
            }
            self.archive(&ctx);
        } else if current.exists() {
            self.archive(&ctx);
        }
        let _ = layout.prune_empty(slot);
        Ok(true)
    }
    async fn recover(&self, ctx: &SlotContext) -> Result<RecoverOutcome> {
        let git = self.sup.git.start(&ctx.state_dir).await?;
        let out = async {
            git.register(&self.sup.config.repositories).await?;
            self.sup.backend.recover(ctx.clone()).await
        }
        .await;
        let _ = git.stop().await;
        out
    }
    async fn slot_loop(self: Arc<Self>, slot: u32) {
        let mut backoff = BACKOFF_START;
        let mut stopping = self.stopping.subscribe();
        let mut pause = self.pause.tx.subscribe();
        let enrollment = self.sup.layout.slot_enrollment(slot);
        let node_id = loop {
            match EnrollmentFile::load(&enrollment) {
                Ok(f) => break f.node_id,
                Err(e) => tracing::error!(slot, "slot waits for its enrollment: {e:#}"),
            }
            if !back_off(&mut stopping, &mut backoff).await {
                return;
            }
        };
        loop {
            if *stopping.borrow() {
                return;
            }
            match self.settle(slot, node_id).await {
                Ok(true) => {}
                Ok(false) => {
                    // Held: nothing more runs here, but the service stays up.
                    let _ = stopping.wait_for(|v| *v).await;
                    return;
                }
                Err(e) => {
                    tracing::warn!(
                        slot,
                        "recovery failed, retrying in {}s: {e:#}",
                        backoff.as_secs()
                    );
                    if !back_off(&mut stopping, &mut backoff).await {
                        return;
                    }
                    continue;
                }
            }
            loop {
                if *stopping.borrow() {
                    return;
                }
                if !*pause.borrow() {
                    break;
                }
                tokio::select! { _ = pause.changed() => {}, _ = stopping.changed() => {} }
            }
            match self.attempt(slot, node_id).await {
                Ok(AttemptOutcome::AttemptStopped) => backoff = BACKOFF_START,
                Ok(AttemptOutcome::Paused) => {}
                Ok(AttemptOutcome::ShutdownBeforeAdmission) => return,
                Err(e) => {
                    tracing::warn!(
                        slot,
                        "attempt failed, retrying in {}s: {e:#}",
                        backoff.as_secs()
                    );
                    if !back_off(&mut stopping, &mut backoff).await {
                        return;
                    }
                }
            }
        }
    }
    async fn attempt(&self, slot: u32, node_id: Uuid) -> Result<AttemptOutcome> {
        let dir = self.sup.layout.fresh_current(slot)?;
        let ctx = self.ctx(slot, node_id, dir.clone());
        self.active.lock().unwrap().insert(slot);
        let result = async {
            let git = self.sup.git.start(&dir).await?;
            let run = async {
                git.register(&self.sup.config.repositories).await?;
                self.sup
                    .backend
                    .run_attempt(
                        ctx.clone(),
                        self.force.subscribe(),
                        self.pause.tx.subscribe(),
                    )
                    .await
            }
            .await;
            if let Err(e) = git.stop().await {
                tracing::warn!(slot, "aq-git stop: {e:#}");
            }
            run
        }
        .await;
        self.active.lock().unwrap().remove(&slot);
        // An attempt that failed after admission keeps its state for recovery (handoff
        // notice and salvage); only a finished or never-admitted attempt is archived.
        let admitted = || {
            self.sup
                .backend
                .retained_attempts(&ctx)
                .map_or(true, |a| !a.is_empty())
        };
        if result.is_err() && admitted() {
            tracing::warn!(slot, "the failed attempt stays in current for recovery");
        } else {
            self.archive(&ctx);
        }
        result
    }
    async fn hand_off_running(&self, posted: &mut HashSet<Uuid>) {
        let layout = &self.sup.layout;
        let slots = self.sup.config.slots;
        let active: Vec<u32> = self.active.lock().unwrap().iter().copied().collect();
        for slot in active {
            let current = layout.current(slot);
            if let Err(e) = write_marker(
                &current.join(SERVICE_STOP_MARKER),
                HandoffReason::ServiceStop,
            ) {
                tracing::warn!(slot, "service-stop marker: {e:#}");
            }
        }
        for id in running_attempts(layout, slots).unwrap_or_default() {
            if posted.insert(id) {
                let notice = HandoffNotice {
                    schema_version: 1,
                    attempt_id: id,
                    reason: HandoffReason::ServiceStop,
                };
                if let Err(e) = self.sup.control.handoff(&notice).await {
                    tracing::warn!(%id, "handoff notice: {e:#}");
                }
            }
            if let Err(e) = mark_handoff(layout, slots, id, HandoffReason::ServiceStop) {
                tracing::warn!(%id, "handoff marker: {e:#}");
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
                self.apply(resp);
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
    fn apply(self: &Arc<Self>, resp: ControlResponse) {
        let layout = &self.sup.layout;
        for id in resp.handoff {
            if let Err(e) =
                mark_handoff(layout, self.sup.config.slots, id, HandoffReason::OwnerStop)
            {
                tracing::warn!(%id, "handoff marker: {e:#}");
            }
        }
        *self.latest.lock().unwrap() = resp.qualification.clone();
        if let Some(pin) = &resp.qualification {
            self.switch_renderer(pin);
        }
        self.maybe_measure();
    }
    /// A new renderer takes effect at `effective_at_ms`. The slots pause in the quiet window
    /// before it and on every switch, including the first pin after a start, so a poll that
    /// is already running cannot take an offer under the old renderer. The next tick that
    /// sees the active pin resumes them.
    fn switch_renderer(&self, pin: &QualificationPin) {
        let now = self.now_ms();
        let mut active = self.renderer.lock().unwrap();
        if active.as_ref() == Some(&pin.renderer) {
            self.pause.set(QUIET, false);
        } else if now >= pin.effective_at_ms {
            let saved = serde_json::to_vec(&pin.renderer)
                .map_err(Into::into)
                .and_then(|b| write_private(&self.sup.layout.renderer_pin(), &b));
            if let Err(e) = saved {
                tracing::warn!("renderer pin not saved: {e:#}");
            }
            *active = Some(pin.renderer.clone());
            self.pause.set(QUIET, true);
        } else if now + QUIET_WINDOW.as_millis() as u64 >= pin.effective_at_ms {
            self.pause.set(QUIET, true);
        }
    }
    fn maybe_measure(self: &Arc<Self>) {
        let m = &self.sup.config.measurement;
        let (true, Some(file), Some(model)) = (m.enabled, m.token_file.clone(), m.model.clone())
        else {
            return;
        };
        let now = self.now_ms();
        let pinned = self.latest.lock().unwrap().clone();
        let due = pinned
            .as_ref()
            .is_none_or(|p| p.expires_at_ms.saturating_sub(now) <= m.renew_before_ms);
        let lock = {
            let state = self.measuring.lock().unwrap();
            if !due || state.busy || state.not_before.is_some_and(|t| Instant::now() < t) {
                return;
            }
            // `aq-worker service measure` (the install script) may hold it: skip this tick.
            match measure_lock(&self.sup.layout.measure_lock(), false) {
                Ok(Some(lock)) => lock,
                Ok(None) => return,
                Err(e) => {
                    tracing::warn!("measurement lock: {e:#}");
                    return;
                }
            }
        };
        self.measuring.lock().unwrap().busy = true;
        let shared = self.clone();
        let task = tokio::spawn(async move {
            let _lock = lock;
            let cancel = shared.stopping.subscribe();
            let done = async {
                let submission = shared.sup.backend.measure(&file, &model, cancel).await?;
                shared.sup.control.submit_measurement(&submission).await
            }
            .await;
            let mut state = shared.measuring.lock().unwrap();
            let wait = match done {
                Ok(pin) => {
                    tracing::info!(
                        expires_at_ms = pin.expires_at_ms,
                        "qualification measurement submitted"
                    );
                    *shared.latest.lock().unwrap() = Some(pin);
                    state.failures = 0;
                    MEASURE_RETRY
                }
                Err(e) => {
                    state.failures += 1;
                    let wait = if pinned.is_none() {
                        MEASURE_RETRY
                            .saturating_mul(2u32.saturating_pow(state.failures.min(16) - 1))
                            .min(MEASURE_RETRY_MAX)
                    } else {
                        MEASURE_RETRY
                    };
                    tracing::warn!(
                        "qualification measurement failed, next try in {} min: {e:#}",
                        wait.as_secs() / 60
                    );
                    wait
                }
            };
            (state.busy, state.not_before) = (false, Some(Instant::now() + wait));
        });
        *self.measurement.lock().unwrap() = Some(task);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::MeasurementConfig;
    use aq_protocol::DesiredState;
    use std::collections::{HashMap, VecDeque};
    use tokio::time::sleep;

    enum Step {
        Outcome(AttemptOutcome),
        Error,
        /// Fails after admission: the journal holds the attempt.
        ErrorAdmitted,
        /// Waits for the forced shutdown.
        Hang,
        /// Keeps the pause receiver, returns Paused when polling pauses.
        Observe,
    }
    #[derive(Default)]
    struct FakeBackend {
        script: Mutex<HashMap<u32, VecDeque<Step>>>,
        recovery: Mutex<HashMap<u32, VecDeque<Result<RecoverOutcome, ()>>>>,
        calls: Mutex<Vec<String>>,
        run_times: Mutex<Vec<Instant>>,
        renderers: Mutex<Vec<Option<ArtifactPin>>>,
        observed: Mutex<Option<watch::Receiver<bool>>>,
        released: Mutex<Option<Instant>>,
        retained_empty: bool,
        measured: Mutex<Vec<Instant>>,
        /// Measurements fail; or wait for the cancel and record the release.
        measure_fails: bool,
        measure_hangs: bool,
        measure_released: Mutex<bool>,
    }
    #[async_trait::async_trait]
    impl SlotBackend for FakeBackend {
        async fn run_attempt(
            &self,
            ctx: SlotContext,
            mut shutdown: watch::Receiver<bool>,
            mut pause: watch::Receiver<bool>,
        ) -> Result<AttemptOutcome> {
            self.calls.lock().unwrap().push(format!("run:{}", ctx.slot));
            self.run_times.lock().unwrap().push(Instant::now());
            self.renderers.lock().unwrap().push(ctx.renderer.clone());
            let step = self
                .script
                .lock()
                .unwrap()
                .get_mut(&ctx.slot)
                .and_then(VecDeque::pop_front);
            match step {
                None => Ok(AttemptOutcome::ShutdownBeforeAdmission),
                Some(Step::Outcome(o)) => Ok(o),
                Some(Step::Error) => anyhow::bail!("boom"),
                Some(Step::ErrorAdmitted) => {
                    std::fs::write(ctx.state_dir.join("journal"), b"j").unwrap();
                    anyhow::bail!("source transfer failed")
                }
                Some(Step::Hang) => {
                    let _ = shutdown.wait_for(|v| *v).await;
                    *self.released.lock().unwrap() = Some(Instant::now());
                    Ok(AttemptOutcome::ShutdownBeforeAdmission)
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
        fn retained_attempts(&self, ctx: &SlotContext) -> Result<Vec<Uuid>> {
            let journal = ctx.state_dir.join("journal").exists();
            Ok(if self.retained_empty && !journal {
                vec![]
            } else {
                vec![Uuid::nil()]
            })
        }
        async fn measure(
            &self,
            _: &Path,
            _: &str,
            mut cancel: watch::Receiver<bool>,
        ) -> Result<MeasurementSubmission> {
            self.measured.lock().unwrap().push(Instant::now());
            if self.measure_hangs {
                let _ = cancel.wait_for(|v| *v).await;
                *self.measure_released.lock().unwrap() = true;
                anyhow::bail!("cancelled");
            }
            anyhow::ensure!(!self.measure_fails, "no sandbox");
            Ok(MeasurementSubmission {
                schema_version: 1,
                calls: vec![],
            })
        }
    }
    struct FakeControl {
        /// An error is (status, whether the body is AQ's error schema).
        replies: Mutex<VecDeque<Result<ControlResponse, (u16, bool)>>>,
        polls: Mutex<Vec<Instant>>,
        handoffs: Mutex<Vec<HandoffNotice>>,
        submissions: Mutex<u32>,
        renewed: QualificationPin,
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
        async fn handoff(&self, n: &HandoffNotice) -> Result<()> {
            self.handoffs.lock().unwrap().push(n.clone());
            Ok(())
        }
        async fn submit_measurement(&self, _: &MeasurementSubmission) -> Result<QualificationPin> {
            *self.submissions.lock().unwrap() += 1;
            Ok(self.renewed.clone())
        }
    }
    struct FakeGit;
    #[async_trait::async_trait]
    impl GitFactory for FakeGit {
        async fn start(&self, _: &Path) -> Result<Box<dyn GitChild>> {
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
            submissions: Mutex::default(),
            renewed: pin("1", 0, wall() + 86_400_000),
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
            config_dir: dir.path().join("cfg"),
            state_root: dir.path().join("state"),
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
            slots,
            native_template: "/n.json".into(),
            state_root: layout.state_root.clone(),
            aq_git: AqGitConfig {
                binary: "/g".into(),
                git_executable: "/usr/bin/git".into(),
            },
            repositories: vec![],
            measurement: MeasurementConfig::default(),
            shutdown_grace_ms: 10_000,
        };
        Rig {
            _dir: dir,
            layout,
            config,
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
        let layout = Layout {
            config_dir: rig.layout.config_dir.clone(),
            state_root: rig.layout.state_root.clone(),
        };
        let sup = Supervisor::new(
            rig.config.clone(),
            layout,
            backend.clone(),
            control.clone(),
            Arc::new(FakeGit),
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
            .current(slot)
            .join(format!("host/driver-{}", Uuid::new_v4()));
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
        let state = rig.layout.current(1);
        assert_eq!(recorded_handoff(&state, a), None);
        assert!(mark_handoff(&rig.layout, 1, a, HandoffReason::ServiceStop).unwrap());
        assert!(da.join("handoff").exists());
        assert_eq!(
            recorded_handoff(&state, a),
            Some(HandoffReason::ServiceStop)
        );
        assert_eq!(recorded_handoff(&state, b), None);
        assert_eq!(recorded_handoff(&state, Uuid::from_u128(3)), None);
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
            !rig.layout
                .current(1)
                .join("host")
                .read_dir()
                .unwrap()
                .any(|_| true),
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
    async fn renderer_switch_pauses_polling_in_quiet_window() {
        let rig = rig(1);
        let backend = Arc::new(FakeBackend::default());
        script(&backend, 1, vec![Step::Observe, Step::Hang]);
        let effective = wall() + 40_000;
        let (handle, tx) = start(
            &rig,
            &backend,
            &control(vec![reply(
                vec![],
                Some(pin("2", effective, effective + 86_400_000)),
            )]),
        );
        sleep(Duration::from_secs(7)).await;
        let paused = backend.observed.lock().unwrap().clone().unwrap();
        assert!(!*paused.borrow(), "before the quiet window");
        sleep(Duration::from_secs(8)).await;
        assert!(*paused.borrow(), "inside the quiet window");
        assert_eq!(backend.calls.lock().unwrap().len(), 1);
        sleep(Duration::from_secs(35)).await;
        assert!(!*paused.borrow(), "after the switch");
        assert_eq!(
            backend
                .renderers
                .lock()
                .unwrap()
                .last()
                .unwrap()
                .as_ref()
                .unwrap()
                .version,
            "2"
        );
        assert!(backend.renderers.lock().unwrap()[0].is_none());
        let saved: ArtifactPin =
            serde_json::from_slice(&std::fs::read(rig.layout.renderer_pin()).unwrap()).unwrap();
        assert_eq!(saved.version, "2");
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
                .map(|n| (n.attempt_id, n.reason))
                .collect::<Vec<_>>(),
            [(attempt, HandoffReason::ServiceStop)]
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
    async fn measurement_runs_when_pin_near_expiry() {
        let mut rig = rig(1);
        rig.config.measurement = MeasurementConfig {
            enabled: true,
            token_file: Some("/tmp/token".into()),
            model: Some("m".into()),
            renew_before_ms: 14_400_000,
        };
        let backend = Arc::new(FakeBackend::default());
        script(&backend, 1, vec![Step::Hang]);
        let now = wall();
        let old = pin("1", now - 1000, now + 3 * 3_600_000);
        let renewed = pin("1", now - 1000, now + 24 * 3_600_000);
        let ctl = control(vec![reply(vec![], Some(old)), reply(vec![], Some(renewed))]);
        let (handle, tx) = start(&rig, &backend, &ctl);
        sleep(Duration::from_secs(12)).await;
        assert_eq!(
            (
                backend.measured.lock().unwrap().len(),
                *ctl.submissions.lock().unwrap()
            ),
            (1, 1)
        );
        sleep(Duration::from_secs(1200)).await;
        assert_eq!(backend.measured.lock().unwrap().len(), 1);
        stop(handle, tx).await;
    }

    #[tokio::test(start_paused = true)]
    async fn revoked_credential_pauses_all_slots() {
        let rig = rig(1);
        let backend = Arc::new(FakeBackend::default());
        script(&backend, 1, vec![Step::Observe, Step::Hang]);
        let ctl = control(vec![Err((401, true))]);
        let (handle, tx) = start(&rig, &backend, &ctl);
        sleep(Duration::from_secs(11)).await;
        assert_eq!(ctl.polls.lock().unwrap().len(), 3);
        assert!(*backend.observed.lock().unwrap().clone().unwrap().borrow());
        sleep(Duration::from_secs(49)).await;
        assert_eq!(
            ctl.polls.lock().unwrap().len(),
            3,
            "the next poll waits 60 s"
        );
        sleep(Duration::from_secs(11)).await;
        assert_eq!(ctl.polls.lock().unwrap().len(), 4);
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
    fn measuring(rig: &mut Rig) {
        rig.config.measurement = MeasurementConfig {
            enabled: true,
            token_file: Some("/tmp/token".into()),
            model: Some("m".into()),
            renew_before_ms: 14_400_000,
        };
    }

    /// Review #1: an attempt that fails after admission keeps current/journal, and the next
    /// round recovers it (handoff notice and salvage) before new work.
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
        assert!(rig.layout.current(1).join("journal").exists());
        assert_eq!(count(rig.layout.archive_dir(1)), 0, "not archived");
        sleep(Duration::from_secs(30)).await;
        assert_eq!(
            *backend.calls.lock().unwrap(),
            ["run:1", "recover:1", "run:1"]
        );
        assert_eq!(
            count(rig.layout.archive_dir(1)),
            1,
            "archived after recovery"
        );
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
        let backend = Arc::new(FakeBackend::default());
        let outcomes = [Err(()), Ok(RecoverOutcome::Salvaged)];
        backend.recovery.lock().unwrap().insert(1, outcomes.into());
        script(&backend, 1, vec![Step::Hang]);
        let (handle, tx) = start(&rig, &backend, &control(vec![reply(vec![], None)]));
        sleep(Duration::from_secs(29)).await;
        assert_eq!(*backend.calls.lock().unwrap(), ["recover:1"]);
        sleep(Duration::from_secs(2)).await;
        assert_eq!(
            *backend.calls.lock().unwrap(),
            ["recover:1", "recover:1", "run:1"]
        );
        stop(handle, tx).await;
    }

    /// Review #4: the first pin after a start switches the renderer; the running poll is
    /// paused, and the next attempt runs under the new renderer.
    #[tokio::test(start_paused = true)]
    async fn first_pin_pauses_a_running_poll() {
        let rig = rig(1);
        let backend = Arc::new(FakeBackend::default());
        script(&backend, 1, vec![Step::Observe, Step::Hang]);
        let now = wall();
        let pinned = reply(vec![], Some(pin("2", now - 1000, now + 86_400_000)));
        let (handle, tx) = start(&rig, &backend, &control(vec![reply(vec![], None), pinned]));
        sleep(Duration::from_secs(1)).await;
        assert_eq!(*backend.renderers.lock().unwrap(), [None]);
        sleep(Duration::from_secs(5)).await;
        let paused = backend.observed.lock().unwrap().clone().unwrap();
        assert!(
            *paused.borrow(),
            "the switch pauses the poll that runs under no pin"
        );
        assert_eq!(backend.calls.lock().unwrap().len(), 1);
        sleep(Duration::from_secs(5)).await;
        let renderers = backend.renderers.lock().unwrap().clone();
        assert_eq!(renderers.len(), 2);
        assert_eq!(renderers[1].as_ref().unwrap().version, "2");
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
        pause.set(QUIET, false);
        assert!(!rx.has_changed().unwrap());
        pause.set(QUIET, true);
        assert!(rx.has_changed().unwrap() && *rx.borrow_and_update());
        pause.set(QUIET, true);
        pause.set(REVOKED, true);
        pause.set(QUIET, false);
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
            let ctl = control(vec![Err((status, aq))]);
            let (handle, tx) = start(&rig, &backend, &ctl);
            sleep(Duration::from_secs(1000)).await;
            assert!(
                !*backend.observed.lock().unwrap().clone().unwrap().borrow(),
                "{status}"
            );
            let times = ctl.polls.lock().unwrap().clone();
            let gaps: Vec<u64> = times.windows(2).map(|w| (w[1] - w[0]).as_secs()).collect();
            assert_eq!(gaps[..6], [10, 20, 40, 80, 160, 300], "{status}");
            stop(handle, tx).await;
        }
        let rig = rig(1);
        let backend = Arc::new(FakeBackend::default());
        script(&backend, 1, vec![Step::Observe, Step::Hang]);
        let (handle, tx) = start(&rig, &backend, &control(vec![Err((403, true))]));
        sleep(Duration::from_secs(11)).await;
        assert!(*backend.observed.lock().unwrap().clone().unwrap().borrow());
        stop(handle, tx).await;
    }

    /// Review 9b: without a pin, failed measurements back off from 10 min to 2 h; and the
    /// service skips its measurement while `service measure` holds the lock.
    #[tokio::test(start_paused = true)]
    async fn measurement_backs_off_without_a_pin_and_yields_to_the_lock() {
        let mut rig = rig(1);
        measuring(&mut rig);
        let held = measure_lock(&rig.layout.measure_lock(), false)
            .unwrap()
            .unwrap();
        let backend = Arc::new(FakeBackend {
            measure_fails: true,
            ..Default::default()
        });
        script(&backend, 1, vec![Step::Hang]);
        let (handle, tx) = start(&rig, &backend, &control(vec![reply(vec![], None)]));
        sleep(Duration::from_secs(60)).await;
        assert!(
            backend.measured.lock().unwrap().is_empty(),
            "the lock is held"
        );
        drop(held);
        sleep(Duration::from_secs(400 * 60)).await;
        let times = backend.measured.lock().unwrap().clone();
        let gaps: Vec<u64> = times
            .windows(2)
            .map(|w| (w[1] - w[0]).as_secs() / 60)
            .collect();
        assert_eq!(gaps, [10, 20, 40, 80, 120, 120]);
        stop(handle, tx).await;
    }

    /// Review #7: a stop cancels a running measurement and waits for its release.
    #[tokio::test(start_paused = true)]
    async fn shutdown_cancels_the_measurement_and_waits_for_its_release() {
        let mut rig = rig(1);
        measuring(&mut rig);
        let backend = Arc::new(FakeBackend {
            measure_hangs: true,
            ..Default::default()
        });
        script(&backend, 1, vec![Step::Hang]);
        let (handle, tx) = start(&rig, &backend, &control(vec![reply(vec![], None)]));
        sleep(Duration::from_secs(10)).await;
        assert_eq!(backend.measured.lock().unwrap().len(), 1);
        stop(handle, tx).await;
        assert!(*backend.measure_released.lock().unwrap());
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
        let current = rig.layout.current(1);
        assert_eq!(recorded_handoff(&current, Uuid::from_u128(9)), None);
        tx.send_replace(true);
        sleep(Duration::from_secs(1)).await;
        assert_eq!(
            recorded_handoff(&current, Uuid::from_u128(9)),
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
}
