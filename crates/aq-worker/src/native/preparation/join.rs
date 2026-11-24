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
mod frozen_inputs {
    use super::*;
    crate::native::phase_profile_shared::prepared_inputs! { aq_admission::preparation_v2, PreparedPhaseInputs, phase_prepared }
}
mod refresh_inputs {
    use super::*;
    crate::native::phase_profile_shared::prepared_inputs! { aq_admission::preparation_refresh_v1, PreparedRefreshInputs, refresh_prepared }
}
pub use frozen_inputs::PreparedPhaseInputs;
pub use refresh_inputs::PreparedRefreshInputs;

enum JoinedRequest {
    Legacy(Prepare),
    Phase(aq_admission::preparation_v2::PrepareRequest),
    Refresh(aq_admission::preparation_refresh_v1::PrepareRequest),
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
// The three public protocols join the same physical inputs and return their own request type.
macro_rules! prepare_entry {
    ($name:ident, $accepted:ty, $variant:ident, $prepared:ident, $error:literal) => {
        pub async fn $name(
            &self,
            id: Uuid,
            preparation: Arc<$accepted>,
            artifacts: Vec<VerifiedArtifact>,
            workspace: HydratedWorkspace,
        ) -> Result<$prepared> {
            let attempt = InputAttempt {
                id,
                preparation: InputContext::$variant(preparation),
            };
            let p = self.join_inputs(attempt, artifacts, workspace).await?;
            let JoinedRequest::$variant(request) = p.request else {
                anyhow::bail!($error)
            };
            Ok($prepared {
                request,
                startup: p.startup,
                workspace: p.workspace,
                artifacts: p.artifacts,
                materials: p.materials,
                retained_inline_bytes: p.retained_inline_bytes,
                startup_bytes: p.startup_bytes,
            })
        }
    };
}
impl InputPipeline {
    prepare_entry!(
        prepare_inputs,
        Preparation,
        Legacy,
        PreparedInputs,
        "legacy join protocol mismatch"
    );
    prepare_entry!(
        prepare_inputs_phase,
        Phase,
        Phase,
        PreparedPhaseInputs,
        "phase join protocol mismatch"
    );
    prepare_entry!(
        prepare_inputs_refresh,
        Refresh,
        Refresh,
        PreparedRefreshInputs,
        "phase join protocol mismatch"
    );
    async fn join_inputs(
        &self,
        InputAttempt { id, preparation }: InputAttempt,
        mut artifacts: Vec<VerifiedArtifact>,
        workspace: HydratedWorkspace,
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
            validate_inline(a, &artifacts)?;
            let (startup, retained_inline_bytes, startup_bytes) =
                render_startup(offer, &mut artifacts)?;
            let (materials, receipts) =
                open_materials(a, &owner.materials, preparation.materials())?;
            workspace.revalidate(&owner.journal)?;
            preparation.check(&owner.journal, id)?;
            let request = match &preparation {
                InputContext::Legacy(p) => {
                    JoinedRequest::Legacy(owner.journal.record_prepare(id, p, receipts)?)
                }
                InputContext::Phase(p) => {
                    JoinedRequest::Phase(owner.journal.record_phase_prepare(id, p, receipts)?)
                }
                InputContext::Refresh(p) => {
                    JoinedRequest::Refresh(owner.journal.record_refresh_prepare(id, p, receipts)?)
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

fn validate_inline(
    a: &crate::native::journal::Attempt,
    artifacts: &[VerifiedArtifact],
) -> Result<()> {
    let offer = &a.reservation.offer;
    ensure!(
        artifacts.len() == offer.offered.artifacts.len(),
        "Prepare missing/extra inline input"
    );
    let mut total = 0u64;
    let mut identities = std::collections::BTreeSet::new();
    for input in artifacts {
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
    Ok(())
}

fn render_startup(
    offer: &aq_admission::startup::NativeOffer,
    artifacts: &mut Vec<VerifiedArtifact>,
) -> Result<(RenderedStartup, u64, u64)> {
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
    let retained_inline_bytes = artifacts.iter().try_fold(startup_bytes, |sum, input| {
        sum.checked_add(input.bytes().len() as u64)
            .context("retained input byte overflow")
    })?;
    ensure!(
        retained_inline_bytes <= crate::native::MAX_INLINE_SET_BYTES,
        "retained input byte bound"
    );
    let startup = crate::native::prepare_startup(offer, descriptor, payload)?;
    Ok((startup, retained_inline_bytes, startup_bytes))
}

fn open_materials(
    a: &crate::native::journal::Attempt,
    store: &MaterialStore,
    leases: &[aq_admission::MaterialLease],
) -> Result<(Vec<InstalledMaterial>, Vec<MaterialReceipt>)> {
    let mut materials = Vec::new();
    let mut receipts = Vec::new();
    for lease in leases {
        let pin = a
            .material_downloads
            .get(&lease.lease_id)
            .context("Prepare material intent absent")?
            .receipt()?
            .context("Prepare installation pin absent")?;
        let installed = store.reopen_pinned(&pin)?;
        let receipt = installed.receipt()?;
        receipt.validate_against(lease, receipt.received_at_ms)?;
        materials.push(installed);
        receipts.push(receipt);
    }
    Ok((materials, receipts))
}
