use super::*;
use crate::native::preparation::{PhaseExecutionCommit, PhasePreparationOptions};
use aq_admission::{ArtifactKind, startup::NativeOffer};
pub(super) async fn collect(
    pipeline: &Arc<InputPipeline>,
    _client: &NativeClient,
    config: &NativeConfig,
    id: Uuid,
    offer: &NativeOffer,
    deadline: u64,
    git: GitClient,
) -> Result<DurablePhaseExecution> {
    let accepted = pipeline
        .accept_phase(
            id,
            offer,
            PhasePreparationOptions {
                negotiation: config.negotiation.clone(),
                capabilities: config.capabilities.clone(),
                source_profile: config.source_profile.clone(),
                maximum_preparation_ms: config.maximum_preparation_ms,
            },
            deadline,
        )
        .await?;
    let preparation = accepted.observed().clone();
    let artifacts = pipeline.artifacts_phase(id, preparation.clone()).await?;
    for lease in &preparation.delivery().receipt.lease.materials {
        pipeline
            .material_phase(id, preparation.clone(), lease.lease_id)
            .await?;
    }
    let descriptor = offer
        .offered
        .artifacts
        .iter()
        .find(|d| {
            d.kind == ArtifactKind::Composition
                && d.artifact.id.as_str() == "aq.profiled-execution-spec"
                && d.artifact.digest == offer.offered.offer.binding.execution_spec_digest
        })
        .context("native original typed spec absent")?;
    let spec = pipeline
        .artifact_phase(id, preparation.clone(), descriptor.artifact.clone())
        .await?;
    let workspace = pipeline
        .hydrate_source_phase(id, preparation.clone(), spec, git)
        .await?;
    let inputs = pipeline
        .prepare_inputs_phase(
            id,
            preparation,
            artifacts,
            workspace,
            config.renderer.clone(),
        )
        .await?;
    match pipeline.request_phase_execution(inputs, &accepted).await? {
        PhaseExecutionCommit::Usable(grant) => Ok(grant),
        PhaseExecutionCommit::HistoryOnly(_) => {
            anyhow::bail!("native execution is history only; no launch allowed")
        }
    }
}
