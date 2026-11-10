//! One fresh native attempt. Configuration and executor readiness precede all
//! network admission. Retained journal/resources are never released on exit.
mod claude_docker;
mod codex;
mod config;
pub mod sandbox;
pub use claude_docker::{
    ClaudeDockerExecutor, ClaudeDockerSettings, InstalledExecutor, destroy_claimed_sandboxes,
    release_claimed_sandboxes, release_preserved_sandbox,
};
#[cfg(all(test, feature = "test-support"))]
pub(crate) use codex::drive_running;
pub use codex::{CodexExecutor, CodexSettings, ExecutorConfig};
#[cfg(all(test, feature = "test-support"))]
pub(crate) use codex::{PreStop, drive_running_with_pre_stop};
mod heartbeat;
mod prepare;
#[cfg(test)]
pub(crate) use prepare::collect as test_collect;
mod profile;
mod recovery;
pub use recovery::{
    ReconcileDisposition, SalvageDisposition, claimed_attempts, reconcile_native,
    retained_attempts, salvage_interrupted,
};
pub(super) mod publish_output;
use super::{
    http::NativeClient,
    journal::{Journal, Limits, RuntimeIdentity},
    materials::MaterialStore,
    preparation::{DurablePhaseExecution, InputPipeline, QualifiedPresenceProfile},
};
use crate::{config::WorkerConfig, source::GitClient};
use anyhow::{Context, Result, ensure};
use aq_admission::{
    Resources,
    transport::{Poll, Register},
};
pub use config::{ConfiguredPreparation, NativeConfig};
use std::{
    future::Future,
    os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt},
    pin::Pin,
    sync::Arc,
};
use tokio::sync::watch;
use uuid::Uuid;

pub type ExecutionFuture<'a> = Pin<Box<dyn Future<Output = Result<()>> + Send + 'a>>;
/// Trusted installed implementation. This seam is not an inbound protocol or
/// executable selector. A peer used in tests never makes a CLI runtime eligible.
pub trait NativeExecutor: Send {
    /// Verify actual installed runtime/qualification support before registration,
    /// journal publication or offer polling. Private pins alone are insufficient.
    fn preflight(&mut self, config: &NativeConfig) -> Result<()>;
    /// Consume the real durable execution owner. Implementations own their host
    /// stop/cleanup and exact lifecycle/output reporting, including on errors.
    /// A shutdown signal must request cleanup, not drop the running host future.
    fn execute<'a>(
        &'a mut self,
        pipeline: Arc<InputPipeline>,
        grant: DurablePhaseExecution,
        shutdown: watch::Receiver<bool>,
    ) -> ExecutionFuture<'a>;
}
/// Until the concrete production host adapter qualifies, ordinary CLI admission
/// refuses here before any journal, registration, inventory or poll mutation.
pub struct UnavailableExecutor;
impl NativeExecutor for UnavailableExecutor {
    fn preflight(&mut self, _: &NativeConfig) -> Result<()> {
        anyhow::bail!("no qualified native executor installed; no admission attempted")
    }
    fn execute<'a>(
        &'a mut self,
        _: Arc<InputPipeline>,
        _: DurablePhaseExecution,
        _: watch::Receiver<bool>,
    ) -> ExecutionFuture<'a> {
        Box::pin(async { anyhow::bail!("native executor unavailable") })
    }
}
#[derive(Debug, PartialEq, Eq)]
pub enum RunDisposition {
    ShutdownBeforeAdmission,
    NoOffer,
    AttemptStopped,
    PollingPaused,
}
pub use aq_node_service::polling::{IDLE_POLL_INTERVAL, PollLoop, poll_until_offer};

pub async fn run_once<E: NativeExecutor>(
    worker: &WorkerConfig,
    config: NativeConfig,
    executor: &mut E,
    shutdown: watch::Receiver<bool>,
) -> Result<RunDisposition> {
    run_admitted(worker, config, executor, shutdown, None).await
}
/// Same admission path as `run_once`, renewing the acknowledged inventory near expiry, then
/// polls (limit 1) until an offer, `pause` (`PollingPaused`) or `shutdown`.
pub async fn run_until_attempt<E: NativeExecutor>(
    worker: &WorkerConfig,
    config: NativeConfig,
    executor: &mut E,
    shutdown: watch::Receiver<bool>,
    pause: watch::Receiver<bool>,
) -> Result<RunDisposition> {
    run_admitted(worker, config, executor, shutdown, Some(pause)).await
}
async fn run_admitted<E: NativeExecutor>(
    worker: &WorkerConfig,
    config: NativeConfig,
    executor: &mut E,
    shutdown: watch::Receiver<bool>,
    pause: Option<watch::Receiver<bool>>,
) -> Result<RunDisposition> {
    config.validate()?;
    // No network or state-directory publication precedes this implementation gate.
    executor.preflight(&config)?;
    if *shutdown.borrow() {
        return Ok(RunDisposition::ShutdownBeforeAdmission);
    }
    let profile = config.presence_profile()?;
    let git = GitClient::new(&config.git_socket, &config.git_root)?;
    let disk = git.native_reservation_bytes()?;
    ensure!(
        disk <= config.capacity.disk_bytes,
        "native Git reservation exceeds configured capacity"
    );
    let client = NativeClient::from_config(worker)?;
    let held = std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .open(&config.state_dir)?;
    fs2::FileExt::try_lock_exclusive(&held).context("native state scope already owned")?;
    check_root(&config, &held)?;
    let journal = open_journal(&config, worker.node_id)?;
    ensure!(
        !journal.recovery_required(),
        "native history cannot admit new work; use reconcile-native to report retained outcome"
    );
    let (heartbeat, shutdown) = heartbeat::Heartbeat::start(worker, shutdown).await?;
    let identity = journal.state().identity.clone();
    std::fs::DirBuilder::new()
        .mode(0o700)
        .create(config.state_dir.join("materials"))?;
    held.sync_all()?;
    let materials = MaterialStore::open(&config.state_dir.join("materials"), identity.clone(), 32)?;
    let pipeline = Arc::new(InputPipeline::new(journal, materials, client.clone()));
    check_root(&config, &held)?;
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
    let producer = pipeline
        .scoped_presence(profile, config.harness_qualifications.clone(), 0)
        .await?;
    let mut poll_shutdown = shutdown.clone();
    // Preserve the acknowledged snapshot while idle polling. Replacing it on
    // every poll invalidates offers assembled from that same snapshot.
    let inventory_cache = tokio::sync::Mutex::new(None::<(u64, u64)>);
    let poll_once = || async {
        heartbeat.check()?;
        let now = super::http::boot_ns()?;
        let mut cached = inventory_cache.lock().await;
        let sequence = if let Some((sequence, deadline)) = *cached
            && deadline.saturating_sub(now) > 5_000_000_000
        {
            sequence
        } else {
            let inventory = producer.begin_inventory(config.inventory_ttl_ms)?;
            let acknowledged = producer.publish_inventory(&inventory).await?;
            let deadline = acknowledged.deadline_ns();
            ensure!(
                super::http::boot_ns()? < deadline,
                "native inventory expired before poll"
            );
            let sequence = inventory.request().sequence;
            pipeline.record_inventory_sequence(sequence);
            *cached = Some((sequence, deadline));
            sequence
        };
        drop(cached);
        if *shutdown.borrow() {
            return Ok(None);
        }
        let available = pipeline.available().await?;
        let mut page = client
            .poll(&Poll {
                schema_version: 1,
                operation_id: Uuid::new_v4(),
                identity: identity.clone(),
                inventory_sequence: sequence,
                available: Resources {
                    slots: available.slots,
                    memory_bytes: available.memory_bytes,
                    cpu_millis: available.cpu_millis,
                    disk_bytes: available.disk_bytes,
                },
                limit: 1,
                after_offer: None,
            })
            .await?;
        ensure!(
            page.page.offers.len() <= 1 && page.page.offers.len() == page.offer_deadlines_ns.len(),
            "native poll cardinality"
        );
        Ok(page
            .page
            .offers
            .pop()
            .map(|offer| (offer, page.offer_deadlines_ns[0])))
    };
    let (offer, offer_deadline) = match pause {
        None => match poll_once().await? {
            Some(found) => found,
            None if *poll_shutdown.borrow() => return Ok(RunDisposition::ShutdownBeforeAdmission),
            None => return Ok(RunDisposition::NoOffer),
        },
        Some(mut pause) => {
            match poll_until_offer(
                poll_once,
                IDLE_POLL_INTERVAL,
                &mut poll_shutdown,
                &mut pause,
            )
            .await?
            {
                PollLoop::Offer(found) => found,
                PollLoop::Paused => return Ok(RunDisposition::PollingPaused),
                PollLoop::Shutdown => return Ok(RunDisposition::ShutdownBeforeAdmission),
            }
        }
    };
    check_root(&config, &held)?;
    if *shutdown.borrow() {
        return Ok(RunDisposition::ShutdownBeforeAdmission);
    };
    let maximum = super::http::boot_ns()?
        .checked_add(
            offer
                .offered
                .reservation_ttl_ms
                .checked_mul(1_000_000)
                .context("native reservation overflow")?,
        )
        .context("native reservation overflow")?;
    let deadline = maximum.min(offer_deadline);
    let id = pipeline
        .reserve(offer.clone(), config.runtime.clone(), deadline, disk)
        .await?;
    let grant = prepare::collect(&pipeline, &client, &config, id, &offer, deadline, git).await?;
    check_root(&config, &held)?;
    heartbeat.check()?;
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
    heartbeat.check()?;
    Ok(RunDisposition::AttemptStopped)
}
fn check_root(config: &NativeConfig, held: &std::fs::File) -> Result<()> {
    let a = held.metadata()?;
    let b = std::fs::symlink_metadata(&config.state_dir)?;
    ensure!(
        b.is_dir()
            && a.dev() == b.dev()
            && a.ino() == b.ino()
            && b.uid() == unsafe { libc::geteuid() }
            && b.mode() & 0o7777 == 0o700,
        "native state directory changed"
    );
    Ok(())
}

#[cfg(test)]
pub(crate) mod tests;

#[cfg(test)]
#[path = "runner/config_tests.rs"]
mod config_tests;
#[cfg(test)]
#[path = "runner/profile_tests.rs"]
mod profile_tests;

fn open_journal(config: &NativeConfig, node: Uuid) -> Result<Journal> {
    let root = config.state_dir.join("journal");
    match std::fs::symlink_metadata(&root) {
        Ok(_) => Journal::open(&root, node, config.capacity.clone()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            ensure!(
                std::fs::symlink_metadata(config.state_dir.join("materials"))
                    .is_err_and(|e| e.kind() == std::io::ErrorKind::NotFound),
                "orphan native materials require reconciliation"
            );
            Journal::create(&root, node, config.capacity.clone())
        }
        Err(error) => Err(error.into()),
    }
}
