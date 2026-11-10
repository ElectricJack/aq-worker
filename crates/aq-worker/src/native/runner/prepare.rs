use super::*;
use crate::native::preparation::{PhaseExecutionCommit, ScopedPresenceProducer};
use aq_admission::{ArtifactKind, startup::NativeOffer};
pub(crate) async fn collect(
    pipeline: &Arc<InputPipeline>,
    inventory: &ScopedPresenceProducer,
    config: &NativeConfig,
    id: Uuid,
    offer: &NativeOffer,
    deadline: u64,
    git: GitClient,
) -> Result<DurablePhaseExecution> {
    match &config.negotiation {
        ConfiguredPreparation::Frozen(n) => {
            collect_frozen(pipeline, config, n, id, offer, deadline, (git, inventory)).await
        }
        ConfiguredPreparation::Refresh(n) => {
            collect_refresh(pipeline, config, n, id, offer, deadline, (git, inventory)).await
        }
    }
}
macro_rules! collect_profile {
    ($collect:ident,$negotiation:ty,$options:path,$accept:ident,$artifacts:ident,$material:ident,$artifact:ident,$hydrate:ident,$prepare:ident,$execute:ident) => {
        async fn $collect(
            pipeline: &Arc<InputPipeline>,
            config: &NativeConfig,
            negotiation: &$negotiation,
            id: Uuid,
            offer: &NativeOffer,
            deadline: u64,
            sources: (GitClient, &ScopedPresenceProducer),
        ) -> Result<DurablePhaseExecution> {
            let (git, inventory) = sources;
            type Options = $options;
            let accepted = pipeline
                .$accept(
                    id,
                    offer,
                    Options {
                        negotiation: negotiation.clone(),
                        capabilities: config.capabilities.clone(),
                        source_profile: config.source_profile.clone(),
                        maximum_preparation_ms: config.maximum_preparation_ms,
                    },
                    deadline,
                )
                .await?;
            let preparation = accepted.observed().clone();
            let artifacts = pipeline.$artifacts(id, preparation.clone()).await?;
            for lease in &preparation.delivery().receipt.lease.materials {
                pipeline
                    .$material(id, preparation.clone(), lease.lease_id)
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
                .$artifact(id, preparation.clone(), descriptor.artifact.clone())
                .await?;
            let workspace = pipeline
                .$hydrate(id, preparation.clone(), spec, git)
                .await?;
            let refreshed = inventory.begin_inventory(config.inventory_ttl_ms)?;
            inventory.publish_inventory(&refreshed).await?;
            pipeline.record_inventory_sequence(refreshed.request().sequence);
            let inputs = pipeline
                .$prepare(
                    id,
                    preparation,
                    artifacts,
                    workspace,
                    config.renderer.clone(),
                )
                .await?;
            match pipeline.$execute(inputs, &accepted).await? {
                PhaseExecutionCommit::Usable(grant) => Ok(grant),
                PhaseExecutionCommit::HistoryOnly(_) => {
                    anyhow::bail!("native execution is history only; no launch allowed")
                }
            }
        }
    };
}
collect_profile! {collect_frozen,aq_admission::preparation_v2::Negotiation,crate::native::preparation::PhasePreparationOptions,accept_phase,artifacts_phase,material_phase,artifact_phase,hydrate_source_phase,prepare_inputs_phase,request_phase_execution}
collect_profile! {collect_refresh,aq_admission::preparation_refresh_v1::Negotiation,crate::native::preparation::RefreshPreparationOptions,accept_refresh,artifacts_refresh,material_refresh,artifact_refresh,hydrate_source_refresh,prepare_inputs_refresh,request_refresh_execution}
