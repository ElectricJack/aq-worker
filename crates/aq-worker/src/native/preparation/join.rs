//! Actual input join. No journal metadata can construct this process-local owner.
use super::*;
use aq_admission::transport::Prepare;
use aq_session_host::RenderedStartup;
const MAX_JOIN_BYTES: u64 = 8 * 1024 * 1024;
pub struct PreparedInputs {
    request: Prepare,
    startup: RenderedStartup,
    workspace: HydratedWorkspace,
    artifacts: Vec<VerifiedArtifact>,
    materials: Vec<InstalledMaterial>,
}
impl PreparedInputs {
    pub fn request(&self) -> &Prepare {
        &self.request
    }
    pub fn startup(&self) -> &RenderedStartup {
        &self.startup
    }
    pub fn workspace(&self) -> &HydratedWorkspace {
        &self.workspace
    }
    pub fn materials(&self) -> &[InstalledMaterial] {
        &self.materials
    }
    pub fn artifacts(&self) -> &[VerifiedArtifact] {
        &self.artifacts
    }
}
impl InputPipeline {
    pub async fn prepare_inputs(
        &self,
        id: Uuid,
        preparation: Arc<Preparation>,
        mut artifacts: Vec<VerifiedArtifact>,
        workspace: HydratedWorkspace,
        renderer: ArtifactRef,
    ) -> Result<PreparedInputs> {
        // Keep the same finite bulk lane across validation and durable commit.
        let _bulk = self
            .bulk_slots
            .clone()
            .try_acquire_owned()
            .context("native Prepare bulk lane overloaded")?;
        self.storage(move |owner| {
            owner.journal.check_preparation(id, &preparation)?;
            let a = owner
                .journal
                .state()
                .attempts
                .get(&id)
                .context("Prepare attempt absent")?;
            let offer = &a.reservation.offer;
            ensure!(
                workspace.attempt_id() == id && workspace.binding() == &preparation.plan().binding,
                "Prepare source owner mismatch"
            );
            ensure!(
                artifacts.len() == offer.offered.artifacts.len(),
                "Prepare missing/extra inline input"
            );
            let mut total = 0u64;
            let mut identities = std::collections::BTreeSet::new();
            for input in &artifacts {
                total = total
                    .checked_add(input.bytes().len() as u64)
                    .context("Prepare input bytes overflow")?;
                ensure!(
                    total <= MAX_JOIN_BYTES && identities.insert(input.receipt().artifact.digest),
                    "Prepare duplicate/oversized inline input"
                );
                let expected = offer
                    .offered
                    .artifacts
                    .iter()
                    .find(|d| d.artifact == input.descriptor.artifact)
                    .context("Prepare foreign inline input")?;
                ensure!(
                    serde_json::to_value(expected)? == serde_json::to_value(&input.descriptor)?,
                    "Prepare original descriptor mismatch"
                );
                let retained = a
                    .downloads
                    .values()
                    .find(|d| d.descriptor.artifact == expected.artifact)
                    .and_then(|d| d.receipt.as_ref())
                    .context("Prepare durable verification absent")?;
                ensure!(
                    serde_json::to_value(retained)? == serde_json::to_value(input.receipt())?,
                    "Prepare verification changed"
                );
            }
            let descriptor_id = offer.startup.artifact()?;
            let descriptor = artifacts.remove(
                artifacts
                    .iter()
                    .position(|d| d.receipt().artifact == descriptor_id)
                    .context("Prepare startup descriptor absent")?,
            );
            let payload = artifacts.remove(
                artifacts
                    .iter()
                    .position(|d| d.receipt().artifact == offer.startup.payload)
                    .context("Prepare startup payload absent")?,
            );
            let startup = crate::native::prepare_startup(offer, &renderer, descriptor, payload)?;
            let mut materials = Vec::new();
            let mut receipts = Vec::new();
            for lease in &preparation.plan().materials {
                let pin = a
                    .material_downloads
                    .get(&lease.lease_id)
                    .context("Prepare material intent absent")?
                    .receipt()?
                    .context("Prepare installation pin absent")?;
                let installed = owner.materials.reopen_pinned(&pin)?;
                let receipt = installed.receipt()?;
                receipt.validate_against(lease, receipt.received_at_ms)?;
                materials.push(installed);
                receipts.push(receipt);
            }
            workspace.revalidate(&owner.journal)?;
            owner.journal.check_preparation(id, &preparation)?;
            let request = owner.journal.record_prepare(id, &preparation, receipts)?;
            ensure!(
                boot_ns()? < workspace.deadline_ns(),
                "Prepare source deadline expired after fsync"
            );
            Ok(PreparedInputs {
                request,
                startup,
                workspace,
                artifacts,
                materials,
            })
        })
        .await
    }
}
