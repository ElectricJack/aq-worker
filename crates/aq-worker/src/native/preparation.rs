//! Bounded current input collection, never Prepare or execution permission.
/// The Legacy, Phase and Refresh entry points of one input operation (`$context`).
macro_rules! input_entries {
    ($context:ident $args:tt -> $out:ty {$($name:ident: $kind:ident($prep:ty)),+}) => {
        $(input_entries!(@one $name, $kind, $prep, $context, $args, $out);)+
    };
    (@one $name:ident, $kind:ident, $prep:ty, $context:ident, ($($arg:ident: $ty:ty),*), $out:ty) => {
        pub async fn $name(&self, id: Uuid, preparation: Arc<$prep>, $($arg: $ty),*) -> Result<$out> {
            self.$context(id, InputContext::$kind(preparation), $($arg),*).await
        }
    };
}
type Phase = super::http::preparation_v2::AcceptedPreparation;
type Refresh = super::http::preparation_refresh_v1::AcceptedPreparation;
mod context;
mod runner_prepare;
mod runner_refresh;
pub(crate) use context::InputContext;
pub use runner_prepare::{PhasePreparation, PhasePreparationOptions};
pub use runner_refresh::{RefreshPreparation, RefreshPreparationOptions};
mod execution;
mod granted;
#[path = "launch/mod.rs"]
pub mod launch;
#[path = "result/mod.rs"]
pub mod result;
pub use execution::{DurablePhaseExecution, PhaseExecutionCommit, RetainedPhaseExecutionHistory};
#[path = "hydration.rs"]
pub(crate) mod hydration;
mod join;
use super::{
    ArtifactAssembly, VerifiedArtifact,
    http::{NativeClient, boot_ns, control::Preparation},
    journal::{Journal, State},
    materials::{InstalledMaterial, MaterialStore},
};
use anyhow::{Context, Result, ensure};
use aq_admission::MaterialReceipt;
use aq_session_protocol::ArtifactRef;
pub use granted::{
    AcknowledgedInventory, DurableInitialGrant, GrantCommit, InventoryOperation, PresenceOperation,
    QualifiedPresenceProfile, RetainedGrantHistory, RetainedInputStats, ScopedPresenceProducer,
};
pub use hydration::HydratedWorkspace;
pub use join::{PreparedInputs, PreparedPhaseInputs, PreparedRefreshInputs};
use std::sync::{Arc, Mutex};
use tokio::sync::Semaphore;
use uuid::Uuid;
struct Owned {
    journal: Journal,
    materials: MaterialStore,
}
/// One admitted storage job and one admitted bulk operation. A cancelled async
/// caller does not cancel an already-running fsync or release its lane permit.
/// Independent NativeClient renewal/control lanes are never acquired here.
pub struct InputPipeline {
    owned: Arc<Mutex<Owned>>,
    storage_slots: Arc<Semaphore>,
    bulk_slots: Arc<Semaphore>,
    client: NativeClient,
    retained: Arc<Mutex<granted::RetainedOwners>>,
    launch: Arc<launch::Registry>,
    results: result::Registry,
    scoped_presence_owned: std::sync::atomic::AtomicBool,
    /// Sequence of this node's last acknowledged inventory (0 = none yet);
    /// the server refuses a poll that names any other sequence.
    inventory_sequence: std::sync::atomic::AtomicU64,
}
impl InputPipeline {
    /// Reserve through the original live journal and install its process-local
    /// retained-input membership in the same storage job. Cancellation cannot
    /// cancel an in-flight fsync or publish only half of this handoff.
    pub async fn reserve(
        &self,
        offer: aq_admission::startup::NativeOffer,
        runtime: super::journal::RuntimeIdentity,
        deadline_ns: u64,
        disk_bytes: u64,
    ) -> Result<Uuid> {
        let retained = self.retained.clone();
        let launch = self.launch.clone();
        self.storage(move |owner| {
            let mut retained = retained
                .lock()
                .map_err(|_| anyhow::anyhow!("retained reservation owner poisoned"))?;
            let id =
                retained.reserve(&mut owner.journal, offer, runtime, deadline_ns, disk_bytes)?;
            // A registry refusal preserves the durable reservation and all
            // resource claims; it never permits a launch or frees capacity.
            launch.admit(id)?;
            Ok(id)
        })
        .await
    }
    /// Current capacity only; reopened or poisoned journals refuse polling.
    pub async fn available(&self) -> Result<super::journal::Limits> {
        self.storage(|owner| owner.journal.available()).await
    }
    pub fn new(journal: Journal, materials: MaterialStore, client: NativeClient) -> Self {
        let retained = granted::RetainedOwners::new(journal.state());
        let launch = Arc::new(launch::Registry::new(journal.state()));
        Self {
            owned: Arc::new(Mutex::new(Owned { journal, materials })),
            storage_slots: Arc::new(Semaphore::new(1)),
            bulk_slots: Arc::new(Semaphore::new(1)),
            client,
            retained: Arc::new(Mutex::new(retained)),
            launch,
            results: result::Registry::default(),
            scoped_presence_owned: std::sync::atomic::AtomicBool::new(false),
            inventory_sequence: std::sync::atomic::AtomicU64::new(0),
        }
    }
    async fn storage<T: Send + 'static>(
        &self,
        operation: impl FnOnce(&mut Owned) -> Result<T> + Send + 'static,
    ) -> Result<T> {
        let permit = self
            .storage_slots
            .clone()
            .try_acquire_owned()
            .context("native input storage lane overloaded")?;
        let owned = self.owned.clone();
        tokio::task::spawn_blocking(move || {
            let _permit = permit;
            let mut owner = owned
                .lock()
                .map_err(|_| anyhow::anyhow!("native input storage poisoned"))?;
            operation(&mut owner)
        })
        .await
        .context("native input storage worker failed")?
    }
    #[cfg(test)]
    pub(crate) async fn test_journal_fault(&self, fault: super::journal::WriteFault) -> Result<()> {
        self.storage(move |owner| {
            owner.journal.inject_fault(fault);
            Ok(())
        })
        .await
    }
    #[cfg(test)]
    pub(crate) async fn test_pause_grant_commit(
        &self,
        pause: &super::journal::WritePause,
    ) -> Result<()> {
        let gate = pause.clone_gate();
        self.storage(move |owner| {
            owner.journal.set_pause_gate(gate);
            Ok(())
        })
        .await
    }
    /// MS1/B3: read this attempt's pending cancellation from the node's own
    /// poll. A zero-capacity poll asks for no offers; the server answers
    /// cancellations first. Errors mean "unknown", never "not cancelled".
    /// Records the node's acknowledged inventory sequence for later polls.
    pub fn record_inventory_sequence(&self, sequence: u64) {
        self.inventory_sequence
            .store(sequence, std::sync::atomic::Ordering::SeqCst);
    }
    pub async fn poll_cancellation(&self, id: Uuid) -> Result<Option<Uuid>> {
        let inventory_sequence = self
            .inventory_sequence
            .load(std::sync::atomic::Ordering::SeqCst);
        anyhow::ensure!(
            inventory_sequence > 0,
            "no acknowledged inventory; cancellation unknown"
        );
        let identity = self.snapshot().await?.identity;
        let page = self
            .client
            .poll(&aq_admission::transport::Poll {
                schema_version: 1,
                operation_id: Uuid::new_v4(),
                identity,
                inventory_sequence,
                available: aq_admission::Resources {
                    slots: 0,
                    memory_bytes: 0,
                    cpu_millis: 0,
                    disk_bytes: 0,
                },
                limit: 1,
                after_offer: None,
            })
            .await?;
        Ok(page
            .page
            .cancellations
            .iter()
            .find(|c| c.binding.attempt_id == id)
            .map(|c| c.cancellation_id))
    }
    pub async fn snapshot(&self) -> Result<State> {
        self.storage(|owner| Ok(owner.journal.state().clone()))
            .await
    }
    // Collect all original inline inputs with one durable plan and completion.
    // Lost payloads are downloaded again under the original IDs and deadlines.
    input_entries! { artifacts_context() -> Vec<VerifiedArtifact> {
        artifacts: Legacy(Preparation), artifacts_phase: Phase(Phase), artifacts_refresh: Refresh(Refresh)
    } }
    async fn artifacts_context(
        &self,
        id: Uuid,
        preparation: InputContext,
    ) -> Result<Vec<VerifiedArtifact>> {
        let _bulk = self
            .bulk_slots
            .clone()
            .try_acquire_owned()
            .context("native input bulk lane overloaded")?;
        let observed = preparation.clone();
        let plans = self
            .storage(move |owner| owner.journal.begin_artifact_batch(id, &observed))
            .await?;
        let mut verified = Vec::with_capacity(plans.len());
        // Use only the client's existing two bulk slots. Structured futures
        // cannot leave detached downloads after cancellation or a failed pair.
        // Preserve original input order and publish no partial completion set.
        for pair in plans.chunks(2) {
            let first = self.planned_artifact(&pair[0], &preparation);
            if let Some(second) = pair.get(1) {
                let (first, second) =
                    tokio::try_join!(first, self.planned_artifact(second, &preparation),)?;
                verified.extend([first, second]);
            } else {
                verified.push(first.await?);
            }
        }
        self.storage(move |owner| {
            owner
                .journal
                .complete_artifact_batch(id, &preparation, &verified)?;
            Ok(verified)
        })
        .await
    }
    async fn planned_artifact(
        &self,
        plan: &super::journal::ArtifactDownload,
        preparation: &InputContext,
    ) -> Result<VerifiedArtifact> {
        let mut assembly = ArtifactAssembly::new(plan.descriptor.clone())?;
        for request in &plan.requests {
            let deadline = plan
                .preparation
                .deadline_ns()
                .min(preparation.deadline_ns());
            ensure!(boot_ns()? < deadline, "native artifact batch expired");
            let client = self.client.fetch_until(deadline);
            let response = client.artifact(request, &plan.descriptor).await?;
            assembly.append(response.offset, &response.bytes)?;
        }
        assembly.finish()
    }
    input_entries! { artifact_context(reference: ArtifactRef) -> VerifiedArtifact {
        artifact: Legacy(Preparation), artifact_phase: Phase(Phase), artifact_refresh: Refresh(Refresh)
    } }
    async fn artifact_context(
        &self,
        id: Uuid,
        preparation: InputContext,
        reference: ArtifactRef,
    ) -> Result<VerifiedArtifact> {
        let _bulk = self
            .bulk_slots
            .clone()
            .try_acquire_owned()
            .context("native input bulk lane overloaded")?;
        let observed = preparation.clone();
        let plan = self
            .storage(move |owner| {
                observed.check(&owner.journal, id)?;
                owner
                    .journal
                    .begin_artifact_context(id, &reference, &observed)
            })
            .await?;
        let mut assembly = ArtifactAssembly::new(plan.descriptor.clone())?;
        for request in &plan.requests {
            let deadline = plan
                .preparation
                .deadline_ns()
                .min(preparation.deadline_ns());
            ensure!(boot_ns()? < deadline, "native artifact preparation expired");
            let client = self.client.fetch_until(deadline);
            let response = client.artifact(request, &plan.descriptor).await?;
            assembly.append(response.offset, &response.bytes)?;
        }
        let verified = assembly.finish()?;
        self.storage(move |owner| {
            preparation.check(&owner.journal, id)?;
            owner
                .journal
                .complete_artifact_context(id, &verified, &preparation)?;
            Ok(verified)
        })
        .await
    }
    input_entries! { material_context(lease_id: Uuid) -> MaterialReceipt {
        material: Legacy(Preparation), material_phase: Phase(Phase), material_refresh: Refresh(Refresh)
    } }
    async fn material_context(
        &self,
        id: Uuid,
        preparation: InputContext,
        lease_id: Uuid,
    ) -> Result<MaterialReceipt> {
        let _bulk = self
            .bulk_slots
            .clone()
            .try_acquire_owned()
            .context("native input bulk lane overloaded")?;
        let observed = preparation.clone();
        let plan = self
            .storage(move |owner| {
                observed.check(&owner.journal, id)?;
                owner
                    .journal
                    .begin_material_context(id, lease_id, &observed)
            })
            .await?;
        let lease = plan
            .preparation
            .materials()
            .iter()
            .find(|l| l.lease_id == lease_id)
            .context("material lease not original")?;
        let deadline = plan
            .preparation
            .deadline_ns()
            .min(preparation.deadline_ns());
        ensure!(boot_ns()? < deadline, "native material preparation expired");
        let client = self.client.fetch_until(deadline);
        let private = client.material(&plan.request, lease).await?;
        self.storage(move |owner| {
            preparation.check(&owner.journal, id)?;
            let installed = owner.materials.install_context(private, &preparation)?;
            owner.journal.pin_installed_material(id, &installed)?;
            installed.receipt()
        })
        .await
    }
    /// Uses only the independently retained pin. Reading historical files creates
    /// no fresh network observation, lease, grant or process permit.
    pub async fn reopen_material(&self, id: Uuid, lease: Uuid) -> Result<InstalledMaterial> {
        self.storage(move |owner| {
            let expected = owner
                .journal
                .state()
                .attempts
                .get(&id)
                .context("unknown native attempt")?
                .material_downloads
                .get(&lease)
                .context("material intent absent")?
                .receipt()?
                .context("material pin not durable")?;
            owner.materials.reopen_pinned(&expected)
        })
        .await
    }
}
