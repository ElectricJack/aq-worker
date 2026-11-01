//! Production orchestrator helpers. Preserve intent before each network effect.
use super::*;
use crate::native::http::preparation_v2::{AcceptedPreparation, ProposedPreparation};
use aq_admission::{
    preparation_v2::{AcceptRequest, Negotiation},
    startup::NativeOffer,
};
use aq_session_protocol::Capability;
pub struct PhasePreparationOptions {
    pub negotiation: Negotiation,
    pub capabilities: Vec<Capability>,
    pub source_profile: ArtifactRef,
    pub maximum_preparation_ms: u64,
}
pub struct PhasePreparation {
    pub(crate) proposed: Arc<ProposedPreparation>,
    pub(crate) request: AcceptRequest,
    pub(crate) observed: Arc<AcceptedPreparation>,
}
impl PhasePreparation {
    pub fn observed(&self) -> &Arc<AcceptedPreparation> {
        &self.observed
    }
}
impl InputPipeline {
    pub async fn accept_phase(
        &self,
        id: Uuid,
        offer: &NativeOffer,
        options: PhasePreparationOptions,
        deadline_ns: u64,
    ) -> Result<PhasePreparation> {
        let identity = self.snapshot().await?.identity;
        let proposed = Arc::new(
            self.client
                .propose_preparation(&identity, offer, &options.negotiation, deadline_ns)
                .await?,
        );
        let intent = proposed.clone();
        let request = self
            .storage(move |owner| {
                owner.journal.begin_phase_accept(
                    id,
                    &intent,
                    options.capabilities,
                    options.source_profile,
                    options.maximum_preparation_ms,
                )
            })
            .await?;
        let observed = Arc::new(
            self.client
                .accept_preparation(&proposed, &request, None)
                .await?,
        );
        let held = observed.clone();
        self.storage(move |owner| {
            owner.journal.record_phase_preparation(id, &held)?;
            owner.journal.check_phase_preparation(id, &held)?;
            Ok(())
        })
        .await?;
        Ok(PhasePreparation {
            proposed,
            request,
            observed,
        })
    }
}
