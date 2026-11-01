macro_rules! runner_prepare {
    ($wire:path,$http:ident,$options:ident,$prepared:ident,$accept:ident,$propose:ident,$begin:ident,$observe:ident,$record:ident,$check:ident) => {
        // Production orchestrator helpers. Preserve intent before each network effect.
        use super::*;
        use crate::native::http::$http::{AcceptedPreparation, ProposedPreparation};
        use aq_admission::startup::NativeOffer;
        use aq_session_protocol::Capability;
        use phase::{AcceptRequest, Negotiation};
        use $wire as phase;
        pub struct $options {
            pub negotiation: Negotiation,
            pub capabilities: Vec<Capability>,
            pub source_profile: ArtifactRef,
            pub maximum_preparation_ms: u64,
        }
        pub struct $prepared {
            pub(crate) proposed: Arc<ProposedPreparation>,
            pub(crate) request: AcceptRequest,
            pub(crate) observed: Arc<AcceptedPreparation>,
        }
        impl $prepared {
            pub fn observed(&self) -> &Arc<AcceptedPreparation> {
                &self.observed
            }
        }
        impl InputPipeline {
            pub async fn $accept(
                &self,
                id: Uuid,
                offer: &NativeOffer,
                options: $options,
                deadline_ns: u64,
            ) -> Result<$prepared> {
                let identity = self.snapshot().await?.identity;
                let proposed = Arc::new(
                    self.client
                        .$propose(&identity, offer, &options.negotiation, deadline_ns)
                        .await?,
                );
                let intent = proposed.clone();
                let request = self
                    .storage(move |owner| {
                        owner.journal.$begin(
                            id,
                            &intent,
                            options.capabilities,
                            options.source_profile,
                            options.maximum_preparation_ms,
                        )
                    })
                    .await?;
                let observed = Arc::new(self.client.$observe(&proposed, &request, None).await?);
                let held = observed.clone();
                self.storage(move |owner| {
                    owner.journal.$record(id, &held)?;
                    owner.journal.$check(id, &held)?;
                    Ok(())
                })
                .await?;
                Ok($prepared {
                    proposed,
                    request,
                    observed,
                })
            }
        }
    };
}
pub(crate) use runner_prepare;
