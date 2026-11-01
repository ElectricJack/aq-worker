//! Actual input join. No journal metadata can construct this process-local owner.
use super::*;
use aq_admission::transport::Prepare;
use aq_session_host::RenderedStartup;
pub struct PreparedInputs {
    request: Prepare,
    startup: RenderedStartup,
    workspace: HydratedWorkspace,
    artifacts: Vec<VerifiedArtifact>,
    materials: Vec<InstalledMaterial>,
    retained_inline_bytes: u64,
    startup_bytes: u64,
}
impl PreparedInputs {
    pub(super) fn retained_inline_bytes(&self) -> u64 {
        self.retained_inline_bytes
    }
    pub(super) fn startup_bytes(&self) -> u64 {
        self.startup_bytes
    }
    pub(super) fn verify_current(&self, journal: &Journal) -> Result<()> {
        let attempt = journal
            .state()
            .attempts
            .get(&self.request.binding.attempt_id)
            .context("retained input attempt absent")?;
        let prepared = attempt
            .prepared
            .as_ref()
            .context("retained Prepared intent absent")?;
        ensure!(
            serde_json::to_value(&self.request)? == serde_json::to_value(&prepared.request)?,
            "retained input Prepared request changed"
        );
        ensure!(
            self.materials.len() == self.request.materials.len(),
            "retained material count changed"
        );
        for material in &self.materials {
            let receipt = material.receipt()?;
            ensure!(
                self.request
                    .materials
                    .iter()
                    .any(|r| serde_json::to_value(r).ok() == serde_json::to_value(&receipt).ok()),
                "retained material owner changed"
            );
            drop(material.read_private()?);
        }
        self.workspace.revalidate(journal)?;
        Ok(())
    }
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
pub struct PreparedPhaseInputs {
    retained_inline_bytes: u64,
    startup_bytes: u64,
    request: aq_admission::preparation_v2::PrepareRequest,
    startup: RenderedStartup,
    workspace: HydratedWorkspace,
    artifacts: Vec<VerifiedArtifact>,
    materials: Vec<InstalledMaterial>,
}
impl PreparedPhaseInputs {
    pub(super) fn retained_inline_bytes(&self) -> u64 {
        self.retained_inline_bytes
    }
    pub(super) fn startup_bytes(&self) -> u64 {
        self.startup_bytes
    }
    pub(super) fn verify_execution_inputs(
        &self,
        journal: &Journal,
        store: &MaterialStore,
        deadline: u64,
    ) -> Result<()> {
        let a = journal
            .state()
            .attempts
            .get(&self.request.original.binding.attempt_id)
            .context("phase retained attempt absent")?;
        let p = a
            .phase_prepared
            .as_ref()
            .context("phase retained Prepare absent")?;
        ensure!(
            serde_json::to_value(&self.request)? == serde_json::to_value(&p.request)?,
            "phase retained Prepare changed"
        );
        ensure!(
            self.materials.len() == self.request.original.materials.len(),
            "phase retained material count"
        );
        for material in &self.materials {
            let r = material.receipt()?;
            ensure!(
                self.request
                    .original
                    .materials
                    .iter()
                    .any(|v| serde_json::to_value(v).ok() == serde_json::to_value(&r).ok()),
                "phase retained material pin"
            );
            drop(material.read_private()?);
            // Verify the current private installation namespace and actual raw
            // record against the retained immutable receipt, without replacing
            // the first owner bundle or extending retrieval/preparation clocks.
            drop(store.reopen_pinned(&r)?.read_private()?);
        }
        self.workspace.revalidate_execution(journal, deadline)
    }
    pub fn request(&self) -> &aq_admission::preparation_v2::PrepareRequest {
        &self.request
    }
    pub fn startup(&self) -> &RenderedStartup {
        &self.startup
    }
    pub fn workspace(&self) -> &HydratedWorkspace {
        &self.workspace
    }
    pub fn artifacts(&self) -> &[VerifiedArtifact] {
        &self.artifacts
    }
    pub fn materials(&self) -> &[InstalledMaterial] {
        &self.materials
    }
}
enum JoinedRequest {
    Legacy(Prepare),
    Phase(aq_admission::preparation_v2::PrepareRequest),
}
struct JoinedInputs {
    request: JoinedRequest,
    startup: RenderedStartup,
    workspace: HydratedWorkspace,
    artifacts: Vec<VerifiedArtifact>,
    materials: Vec<InstalledMaterial>,
    retained_inline_bytes: u64,
    startup_bytes: u64,
}
impl InputPipeline {
    pub async fn prepare_inputs(
        &self,
        id: Uuid,
        preparation: Arc<Preparation>,
        artifacts: Vec<VerifiedArtifact>,
        workspace: HydratedWorkspace,
        renderer: ArtifactRef,
    ) -> Result<PreparedInputs> {
        let p = self
            .join_inputs(
                id,
                InputContext::Legacy(preparation),
                artifacts,
                workspace,
                renderer,
            )
            .await?;
        let JoinedRequest::Legacy(request) = p.request else {
            anyhow::bail!("legacy join protocol mismatch")
        };
        Ok(PreparedInputs {
            request,
            startup: p.startup,
            workspace: p.workspace,
            artifacts: p.artifacts,
            materials: p.materials,
            retained_inline_bytes: p.retained_inline_bytes,
            startup_bytes: p.startup_bytes,
        })
    }
    pub async fn prepare_inputs_phase(
        &self,
        id: Uuid,
        preparation: Arc<crate::native::http::preparation_v2::AcceptedPreparation>,
        artifacts: Vec<VerifiedArtifact>,
        workspace: HydratedWorkspace,
        renderer: ArtifactRef,
    ) -> Result<PreparedPhaseInputs> {
        let p = self
            .join_inputs(
                id,
                InputContext::Phase(preparation),
                artifacts,
                workspace,
                renderer,
            )
            .await?;
        let JoinedRequest::Phase(request) = p.request else {
            anyhow::bail!("phase join protocol mismatch")
        };
        Ok(PreparedPhaseInputs {
            retained_inline_bytes: p.retained_inline_bytes,
            startup_bytes: p.startup_bytes,
            request,
            startup: p.startup,
            workspace: p.workspace,
            artifacts: p.artifacts,
            materials: p.materials,
        })
    }
    async fn join_inputs(
        &self,
        id: Uuid,
        preparation: InputContext,
        mut artifacts: Vec<VerifiedArtifact>,
        workspace: HydratedWorkspace,
        renderer: ArtifactRef,
    ) -> Result<JoinedInputs> {
        // Keep the same finite bulk lane across validation and durable commit.
        let _bulk = self
            .bulk_slots
            .clone()
            .try_acquire_owned()
            .context("native Prepare bulk lane overloaded")?;
        self.storage(move |owner| {
            preparation.check(&owner.journal, id)?;
            let a = owner
                .journal
                .state()
                .attempts
                .get(&id)
                .context("Prepare attempt absent")?;
            let offer = &a.reservation.offer;
            ensure!(
                workspace.attempt_id() == id && workspace.binding() == preparation.binding(),
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
                    total <= crate::native::MAX_INLINE_SET_BYTES
                        && identities.insert(input.receipt().artifact.digest),
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
            let startup_bytes = payload.bytes().len() as u64;
            let retained_inline_bytes =
                artifacts.iter().try_fold(startup_bytes, |sum, input| {
                    sum.checked_add(input.bytes().len() as u64)
                        .context("retained input byte overflow")
                })?;
            ensure!(
                retained_inline_bytes <= crate::native::MAX_INLINE_SET_BYTES,
                "retained input byte bound"
            );
            let startup = crate::native::prepare_startup(offer, &renderer, descriptor, payload)?;
            let mut materials = Vec::new();
            let mut receipts = Vec::new();
            for lease in preparation.materials() {
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
            preparation.check(&owner.journal, id)?;
            let request = match &preparation {
                InputContext::Legacy(p) => {
                    JoinedRequest::Legacy(owner.journal.record_prepare(id, p, receipts)?)
                }
                InputContext::Phase(p) => {
                    JoinedRequest::Phase(owner.journal.record_phase_prepare(id, p, receipts)?)
                }
            };
            ensure!(
                boot_ns()? < workspace.deadline_ns(),
                "Prepare source deadline expired after fsync"
            );
            Ok(JoinedInputs {
                request,
                startup,
                workspace,
                artifacts,
                materials,
                retained_inline_bytes,
                startup_bytes,
            })
        })
        .await
    }
}
