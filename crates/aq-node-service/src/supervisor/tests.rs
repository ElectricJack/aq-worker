mod capacity;
mod control;
mod handoff;
mod recovery;
mod retry;
use super::*;
use aq_protocol::{ArtifactPin, DesiredState, QualificationPin};
use std::collections::{HashMap, VecDeque};
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
    async fn report_until_ready(
        &self,
        ctx: &SlotContext,
        pause: &mut watch::Receiver<bool>,
    ) -> Result<bool> {
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
            _ = pause.wait_for(|v| *v) => return Ok(false),
            () = sleep(ctx.hold) => {}
        }
        Ok(true)
    }

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
        if !self.report_until_ready(&ctx, &mut pause).await? {
            return Ok(AttemptOutcome::Paused);
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

fn journal(rig: &Rig, slot: u32) {
    std::fs::create_dir_all(rig.layout.current(slot)).unwrap();
    std::fs::write(rig.layout.current(slot).join("journal"), b"j").unwrap();
}

fn assert_recoverable_without_marker(current: &Path, host: &Path) {
    assert!(current.join("journal").exists());
    assert!(host.join("driver-1/driver.json").exists());
    assert!(
        !host.join("driver-1/handoff").exists(),
        "no owner or service marker"
    );
    assert!(!current.join(SERVICE_STOP_MARKER).exists());
}

const REFUSED: &str = "native endpoint returned 409 Conflict";
