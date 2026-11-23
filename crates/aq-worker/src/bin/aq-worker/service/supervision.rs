use super::*;
use std::collections::BTreeMap;
use std::sync::atomic::AtomicUsize;
use tokio::task::{JoinHandle, JoinSet};

/// D2, proof (b): before any slot starts a process, check that the unit's cgroup holds only
/// this invocation; then a lost guardian older than this process is known dead. The proof is
/// named when such a slot is held (the stopped contract cannot carry it yet).
fn attest_prior_invocation() {
    let attested = aq_node_service::invocation::prior_invocation_gone().and_then(|gone| {
        if let Some((invocation, unit)) = &gone {
            let own = aq_session_host::ownership::process_evidence(std::process::id())?;
            aq_session_host::lost_guardian::attest_prior_invocation_gone(
                own.start_ticks,
                invocation.clone(),
                unit.clone(),
            )?;
        }
        Ok(gone)
    });
    match attested {
        Ok(Some((invocation, unit))) => tracing::info!(
            %invocation, %unit,
            "the unit's cgroup holds no process of a previous invocation"
        ),
        Ok(None) => tracing::info!(
            "not under systemd, or the unit's cgroup holds other processes: no teardown proof"
        ),
        Err(e) => tracing::warn!("previous invocation check: {e:#}"),
    }
}

fn watch_shutdown() -> (JoinHandle<()>, watch::Receiver<bool>) {
    let (tx, rx) = watch::channel(false);
    let watcher = tokio::spawn(async move {
        crate::shutdown_signal().await;
        let _ = tx.send(true);
    });
    (watcher, rx)
}
fn machine_reporter(
    layout: &Layout,
    link: Arc<ServerLink>,
    mut stop: watch::Receiver<bool>,
) -> Option<JoinHandle<()>> {
    // Only slot 1 reports; the server groups the sibling slots by the machine key.
    aq_machine_load::sampler_from_file(&layout.machine_json(), layout.state_root.clone()).map(
        |(sampler, interval)| {
            tokio::spawn(aq_machine_load::report_loop(
                sampler,
                interval,
                move |report| {
                    let link = link.clone();
                    async move {
                        link.machine_report(&report)
                            .await
                            .map_err(|e| e.to_string())
                    }
                },
                async move {
                    while !*stop.borrow() {
                        if stop.changed().await.is_err() {
                            break;
                        }
                    }
                },
            ))
        },
    )
}

type NodeView = (u32, Layout, Arc<ServerLink>);
#[derive(Default)]
struct Nodes {
    manual_pause: BTreeMap<Uuid, watch::Receiver<bool>>,
    manual_pause_tx: BTreeMap<Uuid, watch::Sender<bool>>,
    native_active: BTreeMap<Uuid, Arc<AtomicUsize>>,
    sessions: BTreeMap<Uuid, Arc<NodeSession>>,
    views: Vec<NodeView>,
}
impl Nodes {
    fn load(layout: &Layout, config: &ServiceConfig) -> Result<Self> {
        let mut nodes = Self::default();
        let mut node_indices = layout.enrolled_nodes();
        for node in 1..=config.nodes {
            if !node_indices.contains(&node) {
                node_indices.push(node);
            }
        }
        for node in node_indices {
            nodes.register(node, layout.for_node(node), config.nodes)?;
        }
        nodes.restore_manual(&config.state_root);
        Ok(nodes)
    }
    fn register(&mut self, node: u32, view: Layout, active_nodes: u32) -> Result<()> {
        let enrollment = EnrollmentFile::load(&view.slot_enrollment(1))?;
        let id = enrollment.node_id;
        ensure!(
            !self.sessions.contains_key(&id),
            "two machine node entries share one node identity"
        );
        let (tx, rx) = watch::channel(false);
        self.manual_pause.insert(id, rx);
        self.manual_pause_tx.insert(id, tx);
        self.native_active
            .insert(id, Arc::new(std::sync::atomic::AtomicUsize::new(0)));
        let session = Arc::new(NodeSession::pending());
        if node > active_nodes {
            session.retire();
        }
        self.sessions.insert(id, session);
        self.views.push((
            node,
            view,
            Arc::new(ServerLink::from_enrollment(&enrollment)?),
        ));

        Ok(())
    }
    fn restore_manual(&self, state_root: &Path) {
        let Ok(entries) = std::fs::read_dir(state_root.join("manual-agents")) else {
            return;
        };
        for entry in entries.flatten() {
            if let Some((node, agent)) = active_agent(&entry.path())
                && let Some(session) = self.sessions.get(&node)
            {
                session.restore_manual(agent);
            }
        }
    }
    fn start_manual(
        &mut self,
        config: &ServiceConfig,
        layout: &Layout,
        rx: &watch::Receiver<bool>,
    ) -> Option<JoinHandle<Result<()>>> {
        config.manual.as_ref()?;
        Some(tokio::spawn(crate::manual::run(
            config.clone(),
            layout.clone(),
            rx.clone(),
            std::mem::take(&mut self.manual_pause_tx),
            self.native_active.clone(),
            self.sessions.clone(),
        )))
    }
    fn start_native(
        &mut self,
        config: &ServiceConfig,
        conversation_root: &Path,
    ) -> Result<(Arc<NativeSlotBackend>, Vec<ObservationNode>)> {
        let mut native_backend = NativeSlotBackend::new(config.native_template.clone());
        native_backend.conversation_root = Some(conversation_root.to_owned());
        native_backend.manual_pause = std::mem::take(&mut self.manual_pause);
        native_backend.native_active = std::mem::take(&mut self.native_active);
        native_backend.transcript_bound = Some(aq_handoff_note::TranscriptBound {
            max_bytes: config.observations.transcript_max_bytes,
            max_files: config.observations.transcript_max_files,
        });
        // One observation service per node, for the life of this process (its boot).
        let mut enrolled = Vec::new();
        for (_, view, _) in &self.views {
            let worker = WorkerConfig::load(&view.slot_enrollment(1))?;
            let session = self.sessions[&worker.node_id].clone();
            enrolled.push((worker, session));
        }
        let (observation_nodes, observations) = start_node_observations(config, enrolled);
        native_backend.observations = observations;
        native_backend.sessions = std::mem::take(&mut self.sessions);

        Ok((Arc::new(native_backend), observation_nodes))
    }
}

fn active_agent(entry: &Path) -> Option<(Uuid, Uuid)> {
    let bytes = std::fs::read(entry.join("state.json")).ok()?;
    let agent: serde_json::Value = serde_json::from_slice(&bytes).ok()?;
    match agent["status"].as_str() {
        Some("stopped") => return None,
        Some("failed") if agent["sandbox"].is_null() => return None,
        _ => {}
    }
    Some((
        agent["node_id"].as_str()?.parse().ok()?,
        agent["id"].as_str()?.parse().ok()?,
    ))
}

struct Supervisors<'a> {
    config: &'a ServiceConfig,
    conversations: PathBuf,
    shutdown: watch::Receiver<bool>,
    backend: Arc<NativeSlotBackend>,
    git: Arc<AqGitFactory>,
    tasks: JoinSet<Result<()>>,
}
impl Supervisors<'_> {
    fn spawn(&mut self, (node, view, node_link): NodeView) -> Result<()> {
        let control = Arc::new(MachineLink::new(node_link.clone(), &view, 1));
        let (node_stop, node_rx) = watch::channel(false);
        let mut machine_stop = self.shutdown.clone();
        let forward = node_stop.clone();
        let forwarding = tokio::spawn(async move {
            let _ = machine_stop.wait_for(|v| *v).await;
            forward.send_replace(true);
        });
        let worker = WorkerConfig::load(&view.slot_enrollment(1))?;
        let supervisor_rx = node_rx.clone();
        let conversations = tokio::spawn(upload_attempt_conversations(
            self.conversations.join(worker.node_id.to_string()),
            node_link,
            node_rx.clone(),
        ));
        let heartbeat = tokio::spawn(node_heartbeat(worker, node_stop, node_rx));
        let sup = Supervisor::new(
            self.config.clone(),
            view,
            self.backend.clone(),
            control,
            self.git.clone(),
        )
        .retiring(node > self.config.nodes);
        // Each node is isolated; a refused sibling never stops this supervisor.
        self.tasks.spawn(async move {
            // heartbeat sends stop on fatal refusal; machine stop is forwarded above.
            let result = sup.run(supervisor_rx).await;
            forwarding.abort();
            heartbeat.abort();
            conversations.abort();
            result
        });
        Ok(())
    }
    async fn finish(mut self) -> Result<()> {
        let mut result = Ok(());
        while let Some(outcome) = self.tasks.join_next().await {
            match outcome {
                Ok(Ok(())) => {}
                Ok(Err(e)) => {
                    tracing::error!("node stopped: {e:#}");
                    result = Err(e);
                }
                Err(e) => {
                    result = Err(e.into());
                }
            }
        }
        result
    }
}
async fn stop_manual(manual: Option<JoinHandle<Result<()>>>) {
    if let Some(manual) = manual {
        match manual.await {
            Ok(Ok(())) => {}
            _ => tracing::error!(
                "manual session supervisor stopped with an error; sandboxes retain their expiring watchdog"
            ),
        }
    }
}
/// `aq-worker run --service-config`: supervise every slot until SIGTERM or ctrl-c.
pub(crate) async fn run_service(service_config: &Path) -> Result<()> {
    let config = ServiceConfig::load(service_config)?;
    let config_dir = service_config
        .parent()
        .context("service config has no directory")?;
    let layout = Layout::for_service(config_dir, &config);
    // Every socket lives under the runtime root (D1): create and check it before any slot.
    layout.prepare_runtime()?;
    attest_prior_invocation();
    let slot1 = EnrollmentFile::load(&layout.slot_enrollment(1))?;
    let link = Arc::new(ServerLink::from_enrollment(&slot1)?);
    let (watcher, rx) = watch_shutdown();
    let machine = machine_reporter(&layout, link, rx.clone());
    tracing::info!(profile = %config.profile, machine = %config.machine_key, nodes = config.nodes,
        runtime = %layout.runtime_root.display(), "worker service starting");
    let mut nodes = Nodes::load(&layout, &config)?;
    let manual = nodes.start_manual(&config, &layout, &rx);
    let conversations = config.state_root.join("attempt-conversations");
    let (backend, observations) = nodes.start_native(&config, &conversations)?;
    let mut supervisors = Supervisors {
        config: &config,
        conversations,
        shutdown: rx,
        backend,
        git: Arc::new(AqGitFactory(config.aq_git.clone())),
        tasks: JoinSet::new(),
    };
    for node in nodes.views {
        supervisors.spawn(node)?;
    }
    let result = supervisors.finish().await;
    stop_manual(manual).await;
    stop_observations(observations);
    watcher.abort();
    if let Some(machine) = machine {
        machine.abort();
    }
    result
}
