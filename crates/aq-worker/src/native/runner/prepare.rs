use super::*;
use crate::native::preparation::{PhaseExecutionCommit, ScopedPresenceProducer};
use aq_admission::{ArtifactKind, startup::NativeOffer};
pub(crate) struct Preparation<'a> {
    pub(crate) pipeline: &'a Arc<InputPipeline>,
    pub(crate) inventory: &'a ScopedPresenceProducer,
    pub(crate) config: &'a NativeConfig,
}
impl Preparation<'_> {
    pub(crate) async fn collect(
        &self,
        offer: &NativeOffer,
        deadline: u64,
        git: GitClient,
    ) -> Result<DurablePhaseExecution> {
        match &self.config.negotiation {
            ConfiguredPreparation::Frozen(n) => self.collect_frozen(n, offer, deadline, git).await,
            ConfiguredPreparation::Refresh(n) => {
                self.collect_refresh(n, offer, deadline, git).await
            }
        }
    }
}
macro_rules! collect_profile {
    ($collect:ident,$negotiation:ty,$options:path,$accept:ident,$artifacts:ident,$material:ident,$artifact:ident,$hydrate:ident,$prepare:ident,$execute:ident) => {
        impl Preparation<'_> {
            async fn $collect(
                &self,
                negotiation: &$negotiation,
                offer: &NativeOffer,
                deadline: u64,
                git: GitClient,
            ) -> Result<DurablePhaseExecution> {
                let pipeline = self.pipeline;
                let config = self.config;
                let id = offer.offered.offer.binding.attempt_id;
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
                            && d.artifact.digest
                                == offer.offered.offer.binding.execution_spec_digest
                    })
                    .context("native original typed spec absent")?;
                let spec = pipeline
                    .$artifact(id, preparation.clone(), descriptor.artifact.clone())
                    .await?;
                let workspace = pipeline
                    .$hydrate(id, preparation.clone(), spec, git)
                    .await?;
                let wait = config.check_backend_admission().await.err();
                aq_node_service::layout::report_admission_wait(&config.state_dir, wait.clone());
                wait.map_or(Ok(()), Err).context("native grant withheld")?;
                let _inventory = pipeline.inventory.gate.lock().await;
                // The execution request needs a live report, not a new one: a third of the TTL.
                let (ttl, need) = (config.inventory_ttl_ms, config.inventory_ttl_ms * 333_333);
                node_inventory(pipeline, self.inventory, ttl, need).await?;
                let inputs = pipeline
                    .$prepare(id, preparation, artifacts, workspace)
                    .await?;
                match pipeline.$execute(inputs, &accepted).await? {
                    PhaseExecutionCommit::Usable(grant) => Ok(grant),
                    PhaseExecutionCommit::HistoryOnly(_) => {
                        anyhow::bail!("native execution is history only; no launch allowed")
                    }
                }
            }
        }
    };
}
collect_profile! {collect_frozen,aq_admission::preparation_v2::Negotiation,crate::native::preparation::PhasePreparationOptions,accept_phase,artifacts_phase,material_phase,artifact_phase,hydrate_source_phase,prepare_inputs_phase,request_phase_execution}
collect_profile! {collect_refresh,aq_admission::preparation_refresh_v1::Negotiation,crate::native::preparation::RefreshPreparationOptions,accept_refresh,artifacts_refresh,material_refresh,artifact_refresh,hydrate_source_refresh,prepare_inputs_refresh,request_refresh_execution}
