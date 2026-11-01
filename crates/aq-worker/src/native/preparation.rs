//! Bounded current input collection, never Prepare or execution permission.
#[path = "hydration.rs"]
mod hydration;
use super::{
    ArtifactAssembly, VerifiedArtifact,
    http::{NativeClient, boot_ns, control::Preparation},
    journal::{Journal, State},
    materials::{InstalledMaterial, MaterialStore},
};
use anyhow::{Context, Result, ensure};
use aq_admission::MaterialReceipt;
use aq_session_protocol::ArtifactRef;
pub use hydration::HydratedWorkspace;
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
}
impl InputPipeline {
    pub fn new(journal: Journal, materials: MaterialStore, client: NativeClient) -> Self {
        Self {
            owned: Arc::new(Mutex::new(Owned { journal, materials })),
            storage_slots: Arc::new(Semaphore::new(1)),
            bulk_slots: Arc::new(Semaphore::new(1)),
            client,
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
    pub async fn snapshot(&self) -> Result<State> {
        self.storage(|owner| Ok(owner.journal.state().clone()))
            .await
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
