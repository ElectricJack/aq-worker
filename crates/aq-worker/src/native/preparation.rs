//! Bounded current input collection, never Prepare or execution permission.
mod granted;
#[path = "hydration.rs"]
mod hydration;
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
pub use join::PreparedInputs;
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
    scoped_presence_owned: std::sync::atomic::AtomicBool,
}
impl InputPipeline {
    pub fn new(journal: Journal, materials: MaterialStore, client: NativeClient) -> Self {
        let retained = granted::RetainedOwners::new(journal.state());
        Self {
            owned: Arc::new(Mutex::new(Owned { journal, materials })),
            storage_slots: Arc::new(Semaphore::new(1)),
            bulk_slots: Arc::new(Semaphore::new(1)),
            client,
            retained: Arc::new(Mutex::new(retained)),
            scoped_presence_owned: std::sync::atomic::AtomicBool::new(false),
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
    pub async fn snapshot(&self) -> Result<State> {
        self.storage(|owner| Ok(owner.journal.state().clone()))
            .await
    }
    /// Collect all original inline inputs with one durable plan and completion.
    /// Lost payloads are downloaded again under the original IDs and deadlines.
    pub async fn artifacts(
        &self,
        id: Uuid,
        preparation: Arc<Preparation>,
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
        preparation: &Preparation,
    ) -> Result<VerifiedArtifact> {
        let mut assembly = ArtifactAssembly::new(plan.descriptor.clone())?;
        for request in &plan.requests {
            ensure!(
                boot_ns()? < plan.preparation.deadline_ns.min(preparation.deadline_ns()),
                "native artifact batch expired"
            );
            let response = self.client.artifact(request, &plan.descriptor).await?;
            assembly.append(response.offset, &response.bytes)?;
        }
        assembly.finish()
    }
    pub async fn artifact(
        &self,
        id: Uuid,
        preparation: Arc<Preparation>,
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
                owner.journal.check_preparation(id, &observed)?;
                owner.journal.begin_artifact_download(id, &reference)
            })
            .await?;
        let mut assembly = ArtifactAssembly::new(plan.descriptor.clone())?;
        for request in &plan.requests {
            ensure!(
                boot_ns()? < plan.preparation.deadline_ns.min(preparation.deadline_ns()),
                "native artifact preparation expired"
            );
            let response = self.client.artifact(request, &plan.descriptor).await?;
            assembly.append(response.offset, &response.bytes)?;
        }
        let verified = assembly.finish()?;
        self.storage(move |owner| {
            owner.journal.check_preparation(id, &preparation)?;
            owner.journal.complete_artifact_download(id, &verified)?;
            Ok(verified)
        })
        .await
    }
    pub async fn material(
        &self,
        id: Uuid,
        preparation: Arc<Preparation>,
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
                owner.journal.check_preparation(id, &observed)?;
                owner.journal.begin_material_download(id, lease_id)
            })
            .await?;
        let lease = plan
            .preparation
            .delivery
            .original
            .materials
            .iter()
            .find(|l| l.lease_id == lease_id)
            .context("material lease not original")?;
        ensure!(
            boot_ns()? < plan.preparation.deadline_ns.min(preparation.deadline_ns()),
            "native material preparation expired"
        );
        let private = self.client.material(&plan.request, lease).await?;
        self.storage(move |owner| {
            owner.journal.check_preparation(id, &preparation)?;
            let installed = owner.materials.install(private, &preparation)?;
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
