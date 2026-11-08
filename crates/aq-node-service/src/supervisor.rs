//! The slot supervisor: one polling loop per slot, the control poll, handoff
//! markers, the renderer switch, measurement renewal and the graceful stop.
use crate::{
    EnrollmentFile, Layout, LinkError, RepositoryBinding, ServerLink, ServiceConfig,
    config::AqGitConfig, enrollment_file::write_private, git::AqGit,
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
use tokio::{sync::watch, task::JoinSet, time::Instant};
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
    async fn measure(&self, token_file: &Path, model: &str) -> Result<MeasurementSubmission>;
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
const MEASURE_RETRY: Duration = Duration::from_secs(600);
const FORCE_WAIT: Duration = Duration::from_secs(20);

/// Writes <driver dir>/handoff (create-new, 0600) for the driver whose driver.json names `attempt`,
/// searching every slot's current/host/driver-*/driver.json. True when written or already present.
pub fn mark_handoff(
    layout: &Layout,
    slots: u32,
    attempt: Uuid,
    reason: HandoffReason,
) -> Result<bool> {
    for (dir, id) in driver_dirs(layout, slots) {
        if id != attempt {
            continue;
        }
        let body = serde_json::json!({"schema_version": 1, "reason": reason}).to_string();
        let marker = dir.join(aq_handoff_note::HANDOFF_MARKER);
        return match std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&marker)
        {
            Ok(mut f) => std::io::Write::write_all(&mut f, body.as_bytes())
                .map(|()| true)
                .context("write handoff marker"),
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => Ok(true),
            Err(e) => Err(e).context("create handoff marker"),
        };
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
    let mut found = Vec::new();
    for slot in 1..=slots {
        let Ok(entries) = std::fs::read_dir(layout.current(slot).join("host")) else {
            continue;
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
    }
    found
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
    fn set(&self, which: usize, on: bool) {
        let mut flags = self.flags.lock().unwrap();
        flags[which] = on;
        self.tx.send_replace(flags.iter().any(|f| *f));
    }
}
#[derive(Default)]
struct ControlState {
    unauthorized: u32,
    revoked: bool,
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
    measuring: Mutex<(bool, Option<Instant>)>,
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
            measuring: Mutex::new((false, None)),
            started: (Instant::now(), wall),
            sup: self,
        });
        let enabled = shared.startup().await;
        let control_task = tokio::spawn(shared.clone().control_loop());
        let mut loops = JoinSet::new();
        for (slot, node_id) in enabled {
            loops.spawn(shared.clone().slot_loop(slot, node_id));
        }
        let finished = async { while loops.join_next().await.is_some() {} };
        tokio::pin!(finished);
        tokio::select! {
            () = &mut finished => {}
            () = signalled(&mut shutdown) => {
                shared.stopping.send_replace(true);
                shared.pause.set(STOPPING, true);
                let grace = tokio::time::sleep(Duration::from_millis(shared.sup.config.shutdown_grace_ms));
                tokio::pin!(grace);
                let mut posted = HashSet::new();
                loop {
                    shared.hand_off_running(&mut posted).await;
                    tokio::select! {
                        () = &mut finished => break,
                        () = &mut grace => {
                            shared.force.send_replace(true);
                            if tokio::time::timeout(FORCE_WAIT, &mut finished).await.is_err() {
                                tracing::error!("slots did not stop after the forced shutdown");
                            }
                            break;
                        }
                        () = tokio::time::sleep(CONTROL_INTERVAL) => {}
                    }
                }
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
    /// Recovers each slot's leftover state; returns the slots that may poll.
    async fn startup(&self) -> Vec<(u32, Uuid)> {
        let mut enabled = Vec::new();
        for slot in 1..=self.sup.config.slots {
            let layout = &self.sup.layout;
            let node_id = match EnrollmentFile::load(&layout.slot_enrollment(slot)) {
                Ok(f) => f.node_id,
                Err(e) => {
                    tracing::error!(slot, "slot disabled, enrollment unreadable: {e:#}");
                    continue;
                }
            };
            let current = layout.current(slot);
            let ctx = self.ctx(slot, node_id, current.clone());
            if current.join("journal").exists() {
                match self.recover(&ctx).await {
                    Ok(RecoverOutcome::Held(why)) => {
                        tracing::error!(slot, "slot disabled, recovery held: {why}");
                        continue;
                    }
                    Ok(_) => self.archive(&ctx),
                    Err(e) => {
                        tracing::error!(slot, "slot disabled, recovery failed: {e:#}");
                        continue;
                    }
                }
            } else if current.exists() {
                self.archive(&ctx);
            }
            let _ = layout.prune_empty(slot);
            enabled.push((slot, node_id));
        }
        enabled
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
    async fn slot_loop(self: Arc<Self>, slot: u32, node_id: Uuid) {
        let mut backoff = BACKOFF_START;
        let mut stopping = self.stopping.subscribe();
        let mut pause = self.pause.tx.subscribe();
        loop {
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
                    tokio::select! {
                        () = tokio::time::sleep(backoff) => {}
                        _ = stopping.wait_for(|v| *v) => return,
                    }
                    backoff = (backoff * 2).min(BACKOFF_MAX);
                }
            }
        }
    }
    async fn attempt(&self, slot: u32, node_id: Uuid) -> Result<AttemptOutcome> {
        let dir = self.sup.layout.fresh_current(slot)?;
        let ctx = self.ctx(slot, node_id, dir.clone());
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
        self.archive(&ctx);
        result
    }
    async fn hand_off_running(&self, posted: &mut HashSet<Uuid>) {
        let layout = &self.sup.layout;
        let slots = self.sup.config.slots;
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
            tokio::time::sleep(if state.revoked {
                REVOKED_INTERVAL
            } else {
                CONTROL_INTERVAL
            })
            .await;
        }
    }
    async fn tick(self: &Arc<Self>, state: &mut ControlState) {
        match self.sup.control.control().await {
            Ok(resp) => {
                state.unauthorized = 0;
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
                    .is_some_and(|l| l.status == 401) =>
            {
                state.unauthorized += 1;
                if state.unauthorized >= 3 && !state.revoked {
                    state.revoked = true;
                    self.pause.set(REVOKED, true);
                    tracing::error!(
                        "the server refused this machine's credential (revoked from the Swarm page?); all slots are paused. Uninstall the service or enroll the machine again"
                    );
                }
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
            self.pause.set(QUIET, false);
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
        let due = self
            .latest
            .lock()
            .unwrap()
            .as_ref()
            .is_none_or(|p| p.expires_at_ms.saturating_sub(now) <= m.renew_before_ms);
        {
            let mut busy = self.measuring.lock().unwrap();
            if !due || busy.0 || busy.1.is_some_and(|t| t.elapsed() < MEASURE_RETRY) {
                return;
            }
            *busy = (true, Some(Instant::now()));
        }
        let shared = self.clone();
        tokio::spawn(async move {
            let done = async {
                let submission = shared.sup.backend.measure(&file, &model).await?;
                shared.sup.control.submit_measurement(&submission).await
            }
            .await;
            match done {
                Ok(pin) => {
                    tracing::info!(
                        expires_at_ms = pin.expires_at_ms,
                        "qualification measurement submitted"
                    );
                    *shared.latest.lock().unwrap() = Some(pin);
                }
                Err(e) => tracing::warn!("qualification measurement failed: {e:#}"),
            }
            shared.measuring.lock().unwrap().0 = false;
        });
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
        /// Waits for the forced shutdown.
        Hang,
        /// Keeps the pause receiver, returns Paused when polling pauses.
        Observe,
    }
    #[derive(Default)]
    struct FakeBackend {
        script: Mutex<HashMap<u32, VecDeque<Step>>>,
        recovery: Mutex<HashMap<u32, RecoverOutcome>>,
        calls: Mutex<Vec<String>>,
        run_times: Mutex<Vec<Instant>>,
        renderers: Mutex<Vec<Option<ArtifactPin>>>,
        observed: Mutex<Option<watch::Receiver<bool>>>,
        released: Mutex<Option<Instant>>,
        retained_empty: bool,
        measured: Mutex<u32>,
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
            Ok(self
                .recovery
                .lock()
                .unwrap()
                .remove(&ctx.slot)
                .unwrap_or(RecoverOutcome::Clean))
        }
        fn retained_attempts(&self, _: &SlotContext) -> Result<Vec<Uuid>> {
            Ok(if self.retained_empty {
                vec![]
            } else {
                vec![Uuid::nil()]
            })
        }
        async fn measure(&self, _: &Path, _: &str) -> Result<MeasurementSubmission> {
            *self.measured.lock().unwrap() += 1;
            Ok(MeasurementSubmission {
                schema_version: 1,
                calls: vec![],
            })
        }
    }
    struct FakeControl {
        replies: Mutex<VecDeque<Result<ControlResponse, u16>>>,
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
            reply.map_err(|status| LinkError { status, code: None }.into())
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
    fn control(replies: Vec<Result<ControlResponse, u16>>) -> Arc<FakeControl> {
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
    ) -> Result<ControlResponse, u16> {
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
        let (handle, _tx) = start(&rig, &backend, &control(vec![reply(vec![], None)]));
        handle.await.unwrap().unwrap();
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
        let (handle, _tx) = start(&rig, &backend, &control(vec![reply(vec![], None)]));
        handle.await.unwrap().unwrap();
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
        backend
            .recovery
            .lock()
            .unwrap()
            .insert(1, RecoverOutcome::Held("sandbox_absent".into()));
        let (handle, _tx) = start(&rig, &backend, &control(vec![reply(vec![], None)]));
        handle.await.unwrap().unwrap();
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
        sleep(Duration::from_secs(30)).await;
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
                *backend.measured.lock().unwrap(),
                *ctl.submissions.lock().unwrap()
            ),
            (1, 1)
        );
        sleep(Duration::from_secs(1200)).await;
        assert_eq!(*backend.measured.lock().unwrap(), 1);
        stop(handle, tx).await;
    }

    #[tokio::test(start_paused = true)]
    async fn revoked_credential_pauses_all_slots() {
        let rig = rig(1);
        let backend = Arc::new(FakeBackend::default());
        script(&backend, 1, vec![Step::Observe, Step::Hang]);
        let ctl = control(vec![Err(401)]);
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
}
