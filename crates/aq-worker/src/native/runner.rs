//! Readiness precedes admission; retained journal/resources survive every exit.
mod admission;
mod claude_docker;
mod codex;
mod config;
pub mod sandbox;
#[cfg(feature = "project_environments")]
pub use claude_docker::offered_pins;
pub use claude_docker::{
    ClaudeDockerExecutor, ClaudeDockerSettings, CodexDockerSettings, InstalledExecutor,
    destroy_claimed_sandboxes, launch_pins, release_claimed_sandboxes, release_preserved_sandbox,
    write_back_login,
};
pub use codex::{CodexExecutor, CodexSettings, ExecutorConfig};
#[cfg(all(test, feature = "test-support"))]
pub(crate) use codex::{DriveOptions, PreStop, RunningHost};
mod heartbeat;
mod prepare;
#[cfg(test)]
pub(crate) use prepare::Preparation as TestPreparation;
mod profile;
mod recovery;
pub use recovery::{
    ReconcileDisposition, SalvageDisposition, claimed_attempts, lock_recovery_scope,
    reconcile_native, register_recorded_source, retained_attempts, salvage_interrupted,
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
    os::unix::fs::{DirBuilderExt, OpenOptionsExt},
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
    let polling = admission::Polling::new(shutdown, None, None);
    admission::run(worker, config, executor, polling).await
}
/// Poll until offer/pause/shutdown, renewing inventory near expiry; for `hold` it only reports.
pub async fn run_until_attempt<E: NativeExecutor>(
    worker: &WorkerConfig,
    config: NativeConfig,
    executor: &mut E,
    shutdown: watch::Receiver<bool>,
    pause: watch::Receiver<bool>,
    hold: std::time::Duration,
) -> Result<RunDisposition> {
    let polling = admission::Polling::new(shutdown, Some((pause, hold)), None);
    admission::run(worker, config, executor, polling).await
}
/// A slot in a node boot already registered by the service, independently of recovery.
pub async fn run_node_slot<E: NativeExecutor>(
    worker: &WorkerConfig,
    config: NativeConfig,
    executor: &mut E,
    shutdown: watch::Receiver<bool>,
    // The pause signal and, as in `run_until_attempt`, the hold during which it only reports.
    pause: (watch::Receiver<bool>, std::time::Duration),
    node: Arc<aq_node_service::session::NodeSession>,
    slot: u32,
) -> Result<RunDisposition> {
    let polling = admission::Polling::new(shutdown, Some(pause), Some((node, slot)));
    admission::run(worker, config, executor, polling).await
}
/// The node's one inventory stream (its gate held): every slot uses the sequence the node last
/// had acknowledged, and a new report is sent only when that one has `need_ns` or less left.
async fn node_inventory(
    pipeline: &InputPipeline,
    producer: &super::preparation::ScopedPresenceProducer,
    ttl_ms: u64,
    need_ns: u64,
) -> Result<u64> {
    if let Some(sequence) = pipeline.inventory.current(super::http::boot_ns()?, need_ns) {
        return Ok(sequence);
    }
    let inventory = producer.begin_inventory(ttl_ms)?;
    let deadline = producer.publish_inventory(&inventory).await?.deadline_ns();
    let (live, sequence) = (
        super::http::boot_ns()? < deadline,
        inventory.request().sequence,
    );
    ensure!(live, "native inventory expired before poll");
    pipeline.inventory.record(sequence, deadline);
    Ok(sequence)
}
fn check_root(config: &NativeConfig, held: &std::fs::File) -> Result<()> {
    aq_node_service::session::check_state_root(&config.state_dir, held)
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
