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
pub mod history;
mod pins;
mod report;
mod roots;
mod source;
mod stopped;
mod storage;
use history::*;
pub use pins::ResultInstallationPins;
pub use stopped::{DurableStoppedResult, ResultStopCause};
use storage::ChannelStorage;
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
struct Custody {
    pipeline: Weak<InputPipeline>,
    scope: LaunchExecutionScope,
    storage: ChannelStorage,
    roots: roots::SourceRoots,
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
        let pipeline = c
            .pipeline
            .upgrade()
            .context("result pipeline owner dropped")?;
        ensure!(
            pipeline
                .results
                .entries
                .lock()
                .map_err(|_| anyhow::anyhow!("result registry poisoned"))?
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
        let a = owner
            .journal
            .state()
            .attempts
            .get(&c.scope.id)
            .context("result attempt absent")?;
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
            self.results
                .entries
                .lock()
                .map_err(|_| anyhow::anyhow!("result registry poisoned"))?
                .get(&id)
                .is_none(),
            "result owner already exists"
        );
        let inputs = scope.inputs.clone();
        let owned_scope = self.launch_scope(execution)?;
        let pins_for_journal = pins.clone();
        let root_for_journal = root.clone();
        let (binding, presence, definition, workspace, roots) = self
            .storage(move |owner| {
                owned_scope.check(owner)?;
                let a = owner
                    .journal
                    .state()
                    .attempts
                    .get(&id)
                    .context("result attempt absent")?;
                let m = &a.reservation.offer.offered.offer.manifest;
                let retained = inputs
                    .artifacts()
                    .iter()
                    .find(|v| v.receipt().artifact == m.harness.adapter)
                    .context("selected result Definition bytes absent")?;
                let definition =
                    super::super::adapter::result_profile::ResultAdapterDefinition::decode(
                        retained.bytes(),
                    )?;
                definition.validate_retained(
                    &m.harness.adapter,
                    &a.reservation.offer.offered.artifacts,
                    retained,
                )?;
                definition.validate_manifest(m)?;
                let expected = ResultInstallationPins {
                    adapter: m.harness.adapter.clone(),
                    contract: definition.contract.clone(),
                    client_manifest: definition.client_manifest.clone(),
                    client_executable: definition.client_executable.clone(),
                    environment: definition.environment.clone(),
                    recipe: definition.recipe.clone(),
                    permission_digest: definition.permission_digest.clone(),
                    reserved_client_bytes: definition.reserved_client_bytes,
                };
                ensure!(
                    expected == pins_for_journal
                        && definition.runtime == a.reservation.runtime.runtime,
                    "result client pins not selected by original grant"
                );
                let source = a.source.as_ref().context("result source owner absent")?;
                // Instructions must already have been counted and rendered centrally.
                let startup = wire::startup_artifact();
                ensure!(
                    counted_startup(&a.reservation.offer)
                        && inputs
                            .artifacts()
                            .iter()
                            .any(|v| v.receipt().artifact == startup
                                && v.bytes() == wire::STARTUP_TEXT.as_bytes()),
                    "result reporting instructions were not centrally counted"
                );
                let binding = wire::Binding {
                    schema_version: 1,
                    launch: a.reservation.offer.offered.offer.binding.clone(),
                    execution_record_digest: owned_scope.digest.clone(),
                    expected_outcome: source.spec.outcome_artifact.clone(),
                    operation_id: Uuid::new_v4(),
                    report_id: Uuid::new_v4(),
                    nonce: Uuid::new_v4(),
                };
                let presence = wire::Presence {
                    schema_version: 1,
                    binding_digest: binding.digest()?,
                    kernel_boot_id: owner.journal.state().kernel_boot_id,
                    original_execution_deadline_boot_ns: owned_scope.deadline,
                    original_hard_deadline_boot_ns: owned_scope.hard,
                };
                let workspace = inputs.workspace().workspace().clone();
                let git = inputs.workspace().git();
                let roots = roots::SourceRoots::new(git.root(), &workspace.path)?;
                let (git_pin, workspace_pin) = roots.pins()?;
                let history = ResultHistory {
                    schema_version: 1,
                    scope: ScopeRecord {
                        binding: binding.clone(),
                        contract: wire::artifact(),
                        installation: pins_for_journal,
                        root: root_for_journal,
                        storage: StorageStage::Creating,
                        git_pin,
                        workspace_pin,
                        runtime: a.reservation.runtime.clone(),
                        kernel_boot_id: presence.kernel_boot_id,
                        original_deadline_ns: owned_scope.deadline,
                        original_hard_deadline_ns: owned_scope.hard,
                        created_ns: boot_ns()?,
                        recorded_revision: 1,
                        source: source.binding()?.clone(),
                        workspace: workspace.clone(),
                        managed_root: git.root().into(),
                        reserved_disk_bytes: a.reservation.disk_bytes,
                        workspace_limit_bytes: git.workspace_byte_limit(),
                        artifact_limit_bytes: git.transfer_byte_limit(),
                        operations: SourceOperations {
                            checkpoint: Uuid::new_v4(),
                            export: Uuid::new_v4(),
                            transfer: Uuid::new_v4(),
                            upload_begin: Uuid::new_v4(),
                            upload_finalize: Uuid::new_v4(),
                        },
                    },
                    intake_closed: false,
                    claim: None,
                    stop: None,
                    source: SourceHistory::default(),
                    report: None,
                    receipt: None,
                    failure: None,
                };
                owner.journal.begin_result(id, history)?;
                Ok((binding, presence, definition, workspace, roots))
            })
            .await?;
        let storage = ChannelStorage::create(&root, &workspace.path)?;
        let directory_pins = storage.pins().clone();
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
        .await?;
        // Publication happens after Ready fsync. A cut before publication holds
        // capacity and IDs but creates no live result scope on reopen.
        storage.publish(&binding, &presence)?;
        let custody = Arc::new(Custody {
            pipeline: Arc::downgrade(self),
            scope,
            storage,
            roots,
            binding,
            pins,
            presence,
            definition,
            claimed: std::sync::atomic::AtomicBool::new(false),
        });
        let mut entries = self
            .results
            .entries
            .lock()
            .map_err(|_| anyhow::anyhow!("result registry poisoned"))?;
        ensure!(
            entries.len() < 64 && !entries.contains_key(&id),
            "result registry capacity/reuse"
        );
        entries.insert(id, custody.clone());
        Ok(VerifiedResultChannel { custody })
    }
}
impl InputPipeline {
    /// Poll one fixed local claim through the original live execution owner.
    /// Receipt projection follows journal fsync; publication alone is Pending.
    pub async fn poll_result_claim(&self, id: Uuid) -> Result<bool> {
        let custody = self
            .results
            .entries
            .lock()
            .map_err(|_| anyhow::anyhow!("result registry poisoned"))?
            .get(&id)
            .cloned()
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
        let bytes_digest = Digest::of(&bytes);
        let submission_digest = submission.digest()?;
        let scope_digest = custody.scope.digest.clone();
        let deadline = custody.scope.deadline;
        let hard = custody.scope.hard;
        let launch = self.launch.clone();
        let kept = custody.clone();
        let history = self
            .storage(move |owner| {
                let old = owner
                    .journal
                    .state()
                    .attempts
                    .get(&id)
                    .and_then(|a| a.result.as_ref())
                    .context("result history absent")?;
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
                    .check_native_execution(id, &scope_digest, deadline, hard)?;
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
            .await?;
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
    pub async fn close_result_intake(&self, id: Uuid) -> Result<()> {
        let custody = self
            .results
            .entries
            .lock()
            .map_err(|_| anyhow::anyhow!("result registry poisoned"))?
            .get(&id)
            .cloned();
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
mod tests;

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
        let custody = self
            .results
            .entries
            .lock()
            .map_err(|_| anyhow::anyhow!("result registry poisoned"))?
            .get(&id)
            .cloned();
        let Some(custody) = custody else {
            return Ok(());
        };
        custody.storage.close_intake();
        let h = self
            .storage(move |owner| {
                let h = owner
                    .journal
                    .state()
                    .attempts
                    .get(&id)
                    .and_then(|a| a.result.as_ref())
                    .context("result history absent")?;
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
