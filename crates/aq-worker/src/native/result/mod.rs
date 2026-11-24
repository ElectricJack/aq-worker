//! Execution-owned local result intake. Serialized history is never an owner.
//! ```compile_fail
//! let _: aq_worker::native::result::VerifiedResultChannel = serde_json::from_str("{}").unwrap();
//! ```
use super::*;
use aq_agent_result_local as wire;
use aq_session_protocol::{Digest, LaunchBinding};
use execution::LaunchExecutionScope;
use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
    sync::Weak,
};
mod creation;
pub mod history;
mod pins;
mod report;
mod roots;
mod salvage;
mod source;
mod stopped;
use aq_agent_client::completion::storage;
use creation::{ChannelIntent, RecordedChannel};
use history::*;
pub use pins::ResultInstallationPins;
use salvage::{Live, ResultJournal, StoppedOwner};
pub use salvage::{SalvageRoots, finish_salvage, salvage_roots};
use source::snapshot;
pub(crate) use source::transfer_bound;
pub use stopped::{DurableStoppedResult, ResultStopCause};
use storage::ChannelStorage;
pub(in crate::native) fn retained_submission(
    scope: &ScopeRecord,
) -> Result<Option<wire::Submission>> {
    let StorageStage::Ready { pins, .. } = &scope.storage else {
        return Ok(None);
    };
    storage::retained_submission(&scope.root, pins, &scope.binding)
}
#[derive(Default)]
pub(super) struct Registry {
    entries: Mutex<BTreeMap<Uuid, Arc<Custody>>>,
    namespace: Mutex<Option<storage::Namespace>>,
}
impl Drop for Registry {
    fn drop(&mut self) {
        let entries = self
            .entries
            .get_mut()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        for entry in entries.values() {
            entry.storage.release_presence();
        }
    }
}
impl Registry {
    fn entries(&self) -> Result<std::sync::MutexGuard<'_, BTreeMap<Uuid, Arc<Custody>>>> {
        self.entries
            .lock()
            .map_err(|_| anyhow::anyhow!("result registry poisoned"))
    }
    fn custody(&self, id: Uuid) -> Result<Option<Arc<Custody>>> {
        Ok(self.entries()?.get(&id).cloned())
    }
}
struct Custody {
    pipeline: Weak<InputPipeline>,
    scope: LaunchExecutionScope,
    storage: ChannelStorage,
    roots: Arc<roots::SourceRoots>,
    binding: wire::Binding,
    pins: ResultInstallationPins,
    presence: wire::Presence,
    definition: super::super::adapter::result_profile::ResultAdapterDefinition,
    claimed: std::sync::atomic::AtomicBool,
}
/// Issued only after original retained execution, selected Definition bytes,
/// private directory custody and journal fsync/readback have all been joined.
/// No Deserialize, Clone, reopen, public constructor or authority trait.
pub struct VerifiedResultChannel {
    custody: Arc<Custody>,
}
impl VerifiedResultChannel {
    pub(super) fn check_pipeline(
        &self,
        pipeline: &InputPipeline,
        scope: &LaunchExecutionScope,
    ) -> Result<()> {
        let c = &self.custody;
        ensure!(
            c.pipeline
                .upgrade()
                .is_some_and(|p| Arc::ptr_eq(&p.owned, &pipeline.owned))
                && c.scope.id == scope.id
                && c.scope.inputs.same_owner(&scope.inputs),
            "result launch belongs to another actual owner"
        );
        self.verify_for(
            &c.binding.launch,
            &scope.digest,
            &c.binding.expected_outcome,
            &scope.inputs.workspace().workspace().path,
            &c.pins,
        )
    }
    pub fn root(&self) -> &Path {
        self.custody.storage.root()
    }
    pub fn submissions(&self) -> &Path {
        self.custody.storage.submissions()
    }
    pub fn binding(&self) -> &wire::Binding {
        &self.custody.binding
    }
    pub fn verify_for(
        &self,
        binding: &LaunchBinding,
        digest: &Digest,
        outcome: &ArtifactRef,
        workspace: &Path,
        pins: &ResultInstallationPins,
    ) -> Result<()> {
        let c = &self.custody;
        ensure!(
            &c.binding.launch == binding
                && &c.binding.execution_record_digest == digest
                && &c.binding.expected_outcome == outcome
                && c.scope.inputs.workspace().workspace().path == workspace
                && &c.pins == pins,
            "result channel differs from actual physical execution inputs"
        );
        self.verify_retained()
    }
    fn verify_retained(&self) -> Result<()> {
        let c = &self.custody;
        let pipeline = c
            .pipeline
            .upgrade()
            .context("result pipeline owner dropped")?;
        ensure!(
            pipeline
                .results
                .entries()?
                .get(&c.scope.id)
                .is_some_and(|other| Arc::ptr_eq(other, &self.custody)),
            "result owner not retained"
        );
        c.roots.verify()?;
        c.storage.verify_control(&c.binding, &c.presence)?;
        ensure!(c.storage.intake_open()?, "result intake closed");
        let owner = pipeline
            .owned
            .lock()
            .map_err(|_| anyhow::anyhow!("result original owner poisoned"))?;
        // Pre-launch mapping still checks the actual original clean input owner.
        c.scope.check(&owner)?;
        let a = owner.journal.state().result_attempt(c.scope.id)?;
        let h = a.result.as_ref().context("result history absent")?;
        ensure!(
            h.claim.is_none()
                && !h.intake_closed
                && h.scope.binding == c.binding
                && matches!(&h.scope.storage,StorageStage::Ready{pins,..} if pins==c.storage.pins()),
            "result journal differs from retained storage"
        );
        c.definition
            .validate_manifest(&a.reservation.offer.offered.offer.manifest)?;
        Ok(())
    }
}
impl InputPipeline {
    /// Explicit opt-in, before host construction. No path or caller-provided
    /// artifact metadata can replace the actual retained input owner.
    pub async fn create_result_channel_in_namespace(
        self: &Arc<Self>,
        execution: &DurablePhaseExecution,
        namespace: PathBuf,
        pins: ResultInstallationPins,
    ) -> Result<VerifiedResultChannel> {
        self.check_phase_execution(execution).await?;
        let root = {
            let mut retained = self
                .results
                .namespace
                .lock()
                .map_err(|_| anyhow::anyhow!("result namespace poisoned"))?;
            if retained.is_none() {
                *retained = Some(storage::Namespace::create(&namespace)?);
            }
            retained
                .as_ref()
                .unwrap()
                .attempt(&namespace, execution.history().attempt_id())?
        };
        self.create_result_channel(execution, root, pins).await
    }
    pub async fn create_result_channel(
        self: &Arc<Self>,
        execution: &DurablePhaseExecution,
        root: PathBuf,
        pins: ResultInstallationPins,
    ) -> Result<VerifiedResultChannel> {
        let scope = self.launch_scope(execution)?;
        let id = scope.id;
        pins.validate()?;
        ensure!(
            self.results.entries()?.get(&id).is_none(),
            "result owner already exists"
        );
        let intent = ChannelIntent {
            inputs: scope.inputs.clone(),
            scope: self.launch_scope(execution)?,
            pins: pins.clone(),
            root: root.clone(),
        };
        let RecordedChannel {
            binding,
            presence,
            definition,
            workspace,
            roots,
        } = self.storage(move |owner| intent.record(owner)).await?;
        let storage = ChannelStorage::create(&root, &workspace)?;
        self.record_ready_storage(id, storage.pins().clone())
            .await?;
        // Publication happens after Ready fsync. A cut before publication holds
        // capacity and IDs but creates no live result scope on reopen.
        storage.publish(&binding, &presence)?;
        let custody = Arc::new(Custody {
            pipeline: Arc::downgrade(self),
            scope,
            storage,
            roots: Arc::new(roots),
            binding,
            pins,
            presence,
            definition,
            claimed: std::sync::atomic::AtomicBool::new(false),
        });
        let mut entries = self.results.entries()?;
        ensure!(
            entries.len() < 64 && !entries.contains_key(&id),
            "result registry capacity/reuse"
        );
        entries.insert(id, custody.clone());
        Ok(VerifiedResultChannel { custody })
    }
}
impl InputPipeline {
    async fn record_ready_storage(&self, id: Uuid, directory_pins: StoragePins) -> Result<()> {
        self.storage(move |owner| {
            let h = owner.journal.state().attempts[&id].result.as_ref().unwrap();
            let digest = h.digest()?;
            owner.journal.update_result(id, &digest, |h, revision| {
                ensure!(
                    matches!(h.scope.storage, StorageStage::Creating),
                    "result storage already ready"
                );
                h.scope.storage = StorageStage::Ready {
                    pins: directory_pins,
                    recorded_revision: revision,
                };
                Ok(())
            })?;
            Ok(())
        })
        .await
    }
    /// Poll one fixed local claim through the original live execution owner.
    /// Receipt projection follows journal fsync; publication alone is Pending.
    pub async fn poll_result_claim(&self, id: Uuid) -> Result<bool> {
        let custody = self
            .results
            .custody(id)?
            .context("result channel not owned")?;
        ensure!(
            custody
                .pipeline
                .upgrade()
                .is_some_and(|p| Arc::ptr_eq(&p.owned, &self.owned)),
            "foreign result pipeline"
        );
        if custody.claimed.load(std::sync::atomic::Ordering::SeqCst) {
            return Ok(true);
        }
        let Some(bytes) = custody.storage.read_submission()? else {
            return Ok(false);
        };
        custody
            .storage
            .verify_control(&custody.binding, &custody.presence)?;
        let submission: wire::Submission = wire::decode(&bytes)?;
        ensure!(
            submission.binding == custody.binding && wire::encode(&submission)? == bytes,
            "changed/noncanonical result submission"
        );
        let history = self.record_claim(&custody, bytes, submission).await?;
        // Closing admission is a safety action; acknowledgement is projected
        // from the fsynced record and cannot be upgraded by client JSON.
        custody
            .claimed
            .store(true, std::sync::atomic::Ordering::SeqCst);
        custody.storage.close_intake();
        if let Some(ack) = history.acknowledgment()?
            && custody.storage.write_ack(&ack).is_err()
        {
            tracing::warn!(attempt_id=%id,"durable result retained; local acknowledgement projection unavailable");
        }
        Ok(true)
    }
    async fn record_claim(
        &self,
        custody: &Arc<Custody>,
        bytes: Vec<u8>,
        submission: wire::Submission,
    ) -> Result<ResultHistory> {
        let id = custody.scope.id;
        let bytes_digest = Digest::of(&bytes);
        let submission_digest = submission.digest()?;
        let scope_digest = custody.scope.digest.clone();
        let deadline = custody.scope.deadline;
        let hard = custody.scope.hard;
        let launch = self.launch.clone();
        let kept = custody.clone();
        self.storage(move |owner| {
            let old = owner.journal.state().result_history(id)?;
            if let Some(claim) = &old.claim {
                ensure!(
                    claim.bytes == bytes
                        && claim.bytes_digest == bytes_digest
                        && claim.submission_digest == submission_digest,
                    "result claim conflict"
                );
                return Ok(old.clone());
            }
            ensure!(!old.intake_closed, "result intake closed");
            kept.storage.verify_control(&kept.binding, &kept.presence)?;
            ensure!(kept.storage.intake_open()?, "result intake owner absent");
            // Deliberately do not revalidate the initial clean worktree after
            // tools have edited it. Actual host ownership and execution clocks
            // remain independently checked under the journal owner.
            owner
                .journal
                .check_native_execution(id, &scope_digest, (deadline, hard))?;
            launch.result_current(&kept.scope)?;
            let digest = old.digest()?;
            owner.journal.update_result(id, &digest, |h, revision| {
                h.claim = Some(ClaimRecord {
                    bytes,
                    bytes_digest,
                    submission_digest,
                    recorded_revision: revision,
                });
                h.intake_closed = true;
                Ok(())
            })
        })
        .await
    }
    pub async fn close_result_intake(&self, id: Uuid) -> Result<()> {
        let custody = self.results.custody(id)?;
        let Some(custody) = custody else {
            return Ok(());
        };
        custody.storage.close_intake();
        self.storage(move |owner| {
            let h = owner.journal.state().attempts[&id]
                .result
                .as_ref()
                .context("result history absent")?;
            if h.claim.is_none() {
                custody.storage.release_presence();
            }
            if h.intake_closed {
                return Ok(());
            }
            let digest = h.digest()?;
            owner.journal.update_result(id, &digest, |h, _| {
                h.intake_closed = true;
                Ok(())
            })?;
            Ok(())
        })
        .await
    }
}
#[cfg(test)]
pub(crate) mod tests;

fn counted_startup(offer: &aq_admission::startup::NativeOffer) -> bool {
    let startup = wire::startup_artifact();
    let manifest = &offer.offered.offer.manifest;
    manifest.modules.contains(&startup)
        && manifest.context.references.iter().any(|r| {
            r.artifact == startup
                && r.delivery == aq_session_protocol::ContextDelivery::Startup
                && r.required
                && r.bytes == wire::STARTUP_TEXT.len() as u64
        })
        && offer
            .startup
            .startup_inputs
            .iter()
            .any(|r| r.artifact == startup && r.bytes == wire::STARTUP_TEXT.len() as u64)
}
impl InputPipeline {
    pub async fn result_needs_reconciliation(
        &self,
        id: Uuid,
        code: wire::RecoveryCode,
    ) -> Result<()> {
        let custody = self.results.custody(id)?;
        let Some(custody) = custody else {
            return Ok(());
        };
        custody.storage.close_intake();
        let h = self
            .storage(move |owner| {
                let h = owner.journal.state().result_history(id)?;
                if h.claim.is_none() {
                    return Ok(h.clone());
                }
                let digest = h.digest()?;
                owner.journal.update_result(id, &digest, |h, _| {
                    if h.receipt.is_none() {
                        h.failure = Some(code);
                    }
                    Ok(())
                })
            })
            .await?;
        if let Some(ack) = h.acknowledgment()?
            && custody.storage.write_ack(&ack).is_err()
        {
            tracing::warn!(attempt_id=%id,"result recovery retained; local projection unavailable");
        }
        Ok(())
    }
}
