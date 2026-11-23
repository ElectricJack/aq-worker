use super::*;
use crate::native::preparation::ScopedPresenceProducer;
use aq_admission::transport::WorkerIdentity;
use aq_node_service::session::{InFlight, NodeSession};
use aq_worker_protocol::git_author::AttemptOffer;
type NodeSlot = (Arc<NodeSession>, u32);
type Pause = (watch::Receiver<bool>, std::time::Duration);
pub(super) struct Polling {
    shutdown: watch::Receiver<bool>,
    pause: Option<Pause>,
    node: Option<NodeSlot>,
}
impl Polling {
    pub(super) fn new(
        shutdown: watch::Receiver<bool>,
        pause: Option<Pause>,
        node: Option<NodeSlot>,
    ) -> Self {
        Self {
            shutdown,
            pause,
            node,
        }
    }
}
pub(super) async fn run<E: NativeExecutor>(
    worker: &WorkerConfig,
    mut config: NativeConfig,
    executor: &mut E,
    polling: Polling,
) -> Result<RunDisposition> {
    let Polling {
        shutdown,
        pause,
        node,
    } = polling;
    config.validate()?;
    config.capacity.slots = 1;
    // The service keeps one heartbeat per node, also against a server without node slots.
    let serviced = node.is_some();
    let node = node.filter(|(n, _)| n.aware());
    // No network or state-directory publication precedes this implementation gate.
    executor.preflight(&config)?;
    if *shutdown.borrow() {
        return Ok(RunDisposition::ShutdownBeforeAdmission);
    }
    let admission = Admission::open(worker, config, shutdown, (node, serviced)).await?;
    admission.dispatch(executor, pause).await
}
struct Admission {
    config: NativeConfig,
    git: GitClient,
    disk: u64,
    client: NativeClient,
    heartbeat: Option<heartbeat::Heartbeat>,
    shutdown: watch::Receiver<bool>,
    identity: WorkerIdentity,
    pipeline: Arc<InputPipeline>,
    producer: ScopedPresenceProducer,
    held: std::fs::File,
}
type Found = (AttemptOffer, u64, InFlight);
impl Admission {
    async fn open(
        worker: &WorkerConfig,
        config: NativeConfig,
        shutdown: watch::Receiver<bool>,
        service: (Option<NodeSlot>, bool),
    ) -> Result<Self> {
        let (node, serviced) = service;
        let profile = config.presence_profile()?;
        let git = config.git_client(worker)?;
        let disk = git.native_reservation_bytes()?;
        ensure!(
            disk <= config.capacity.disk_bytes,
            "native Git reservation exceeds configured capacity"
        );
        let mut client = NativeClient::from_config(worker)?
            .with_git_author_support(git.supports_checkpoint_author());
        if let Some((node, slot)) = &node {
            client = client.for_node_slot(*slot, node.registered_slots());
        }
        let (held, journal) = open_scope(&config, worker.node_id, &node)?;
        let (heartbeat, shutdown) = if serviced {
            (None, shutdown)
        } else {
            let (heartbeat, shutdown) = heartbeat::Heartbeat::start(worker, shutdown).await?;
            (Some(heartbeat), shutdown)
        };
        let identity = journal.state().identity.clone();
        std::fs::DirBuilder::new()
            .mode(0o700)
            .create(config.state_dir.join("materials"))?;
        held.sync_all()?;
        let materials =
            MaterialStore::open(&config.state_dir.join("materials"), identity.clone(), 32)?;
        let mut pipeline = InputPipeline::new(journal, materials, client.clone());
        if let Some((node, _)) = &node {
            pipeline = pipeline.with_inventory(node.inventory.clone());
        }
        let pipeline = Arc::new(pipeline);
        check_root(&config, &held)?;
        if node.is_none() {
            client
                .register(&Register {
                    schema_version: 1,
                    operation_id: Uuid::new_v4(),
                    identity: identity.clone(),
                    runtime: config.runtime.runtime.clone(),
                    capabilities: config.capabilities.clone(),
                    features: config.features.clone(),
                })
                .await?;
        }
        let producer = pipeline
            .scoped_presence(profile, config.harness_qualifications.clone(), 0)
            .await?;
        Ok(Self {
            config,
            git,
            disk,
            client,
            heartbeat,
            shutdown,
            identity,
            pipeline,
            producer,
            held,
        })
    }
    async fn execute<E: NativeExecutor>(
        self,
        executor: &mut E,
        found: Found,
    ) -> Result<RunDisposition> {
        let Self {
            config,
            git,
            disk,
            shutdown,
            pipeline,
            producer,
            held,
            heartbeat,
            ..
        } = self;
        let (offer, offer_deadline, in_flight) = found;
        check_root(&config, &held)?;
        if *shutdown.borrow() {
            return Ok(RunDisposition::ShutdownBeforeAdmission);
        };
        let maximum = crate::native::http::boot_ns()?
            .checked_add(
                offer
                    .offer
                    .offered
                    .reservation_ttl_ms
                    .checked_mul(1_000_000)
                    .context("native reservation overflow")?,
            )
            .context("native reservation overflow")?;
        let deadline = maximum.min(offer_deadline);
        let original = offer.offer.clone();
        let id = pipeline
            .reserve(offer, config.runtime.clone(), deadline, disk)
            .await?;
        let grant = prepare::Preparation {
            pipeline: &pipeline,
            inventory: &producer,
            config: &config,
        }
        .collect(&original, deadline, git)
        .await?;
        drop(in_flight);
        check_root(&config, &held)?;
        if let Some(h) = &heartbeat {
            h.check()?;
        }
        let prepared_state = pipeline.snapshot().await?;
        ensure!(
            profile::TerminalProfile::for_execution(
                prepared_state
                    .attempts
                    .get(&id)
                    .context("native prepared attempt absent")?
            )? == config.negotiation.profile(),
            "native collected profile differs from explicit configuration"
        );
        pipeline.check_phase_execution(&grant).await?;
        // Never select/cancel this future on shutdown: the executor owns physical stop
        // and durable evidence. Its guardian enforces the original execution caps.
        executor.execute(pipeline.clone(), grant, shutdown).await?;
        check_completed(&config, &pipeline, id).await?;
        if let Some(h) = &heartbeat {
            h.check()?;
        }
        Ok(RunDisposition::AttemptStopped)
    }
}
async fn check_completed(config: &NativeConfig, pipeline: &InputPipeline, id: Uuid) -> Result<()> {
    let state = pipeline.snapshot().await?;
    let host = state
        .attempts
        .get(&id)
        .and_then(|a| a.native_host.as_ref())
        .context("native executor returned without retained host history")?;
    ensure!(
        host.stopped.is_some(),
        "native executor returned without retained physical stop"
    );
    ensure!(
        config
            .negotiation
            .profile()
            .complete(state.attempts.get(&id).unwrap())?,
        "native executor missing matching durable lifecycle acknowledgments"
    );
    Ok(())
}
fn open_scope(
    config: &NativeConfig,
    node_id: Uuid,
    node: &Option<NodeSlot>,
) -> Result<(std::fs::File, Journal)> {
    let held = std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .open(&config.state_dir)?;
    fs2::FileExt::try_lock_exclusive(&held).context("native state scope already owned")?;
    check_root(config, &held)?;
    let journal = if let Some((node, _)) = node {
        Journal::create_identity(
            &config.state_dir.join("journal"),
            aq_admission::transport::WorkerIdentity {
                node_id,
                boot_id: node.boot_id,
            },
            config.capacity.clone(),
        )?
    } else {
        open_journal(config, node_id)?
    };
    ensure!(
        !journal.recovery_required(),
        "native history cannot admit new work; use reconcile-native to report retained outcome"
    );
    Ok((held, journal))
}
mod polling;
