macro_rules! journal_prepared {
    ($wire:path,$http:ident,$acceptance:ident,$preparation:ident,$prepared:ident,$origin:ident,$record:ident,$check:ident) => {
        // Durable phase Prepare intent, never execution authority.
        use super::*;
        use crate::native::http::$http::AcceptedPreparation;
        use aq_admission::{MaterialReceipt, transport::Prepare};
        use phase::PrepareRequest;
        use $wire as phase;
        #[derive(Clone, Serialize, Deserialize)]
        #[serde(deny_unknown_fields)]
        pub struct PhasePreparedIntent {
            pub request: PrepareRequest,
            pub request_digest: Digest,
            pub prepared_at_ns: u64,
            pub origin: InputOrigin,
        }
        pub(super) fn validate(
            a: &Attempt,
            identity: &WorkerIdentity,
            revision: u64,
            ids: &mut BTreeSet<Uuid>,
        ) -> Result<()> {
            let Some(p) = &a.$prepared else {
                return Ok(());
            };
            let accept = a
                .$acceptance
                .as_ref()
                .context("phase Prepare intent absent")?;
            let received = a
                .$preparation
                .as_ref()
                .context("phase Prepare receipt absent")?;
            p.request.validate_for(
                &received.delivery.receipt,
                &accept.request,
                &accept.proposal,
            )?;
            ensure!(
                matches!(p.origin, InputOrigin::$origin(_)),
                "phase Prepare legacy origin"
            );
            p.origin.validate(
                a,
                p.request.original.journal_revision,
                p.prepared_at_ns,
                revision,
            )?;
            ensure!(
                p.request.original.identity == *identity
                    && p.request_digest == canonical_digest(&p.request)?
                    && ids.insert(p.request.original.operation_id)
                    && same(&p.request.original.artifacts, &prepared::receipts(a)?)?,
                "phase Prepare identity/coverage"
            );
            let source = a.source.as_ref().context("phase Prepare source absent")?;
            ensure!(
                source.verified.is_some()
                    && source.complete()
                    && p.prepared_at_ns < source.preparation.deadline_ns(),
                "phase Prepare source incomplete"
            );
            for receipt in &p.request.original.materials {
                let saved = a
                    .material_downloads
                    .get(&receipt.lease_id)
                    .context("phase material intent absent")?
                    .receipt()?
                    .context("phase material pin absent")?;
                ensure!(same(receipt, &saved)?, "phase material receipt changed");
            }
            Ok(())
        }
        impl Journal {
            pub(crate) fn $record(
                &mut self,
                id: Uuid,
                observed: &AcceptedPreparation,
                materials: Vec<MaterialReceipt>,
            ) -> Result<PrepareRequest> {
                self.$check(id, observed)?;
                let a = self
                    .state
                    .attempts
                    .get(&id)
                    .context("phase Prepare attempt absent")?;
                let artifacts = prepared::receipts(a)?;
                if let Some(old) = &a.$prepared {
                    ensure!(
                        same(&old.request.original.artifacts, &artifacts)?
                            && same(&old.request.original.materials, &materials)?,
                        "phase Prepare retry changed"
                    );
                    return Ok(old.request.clone());
                }
                let accepted = a.$preparation.as_ref().context("phase accepted absent")?;
                let intent = a.$acceptance.as_ref().context("phase Accept absent")?;
                let l = &accepted.delivery.receipt.lease;
                let request = PrepareRequest {
                    schema_version: 2,
                    negotiation: accepted.delivery.receipt.negotiation.clone(),
                    accepted_receipt_digest: accepted.receipt_digest.clone(),
                    expected_preparation_revision: l.revision,
                    original: Prepare {
                        schema_version: 1,
                        operation_id: Uuid::new_v4(),
                        identity: self.state.identity.clone(),
                        binding: l.binding.clone(),
                        reservation: l.hold.reservation.clone(),
                        guardian_id: l.guardian_id,
                        launch_id: l.launch_id,
                        journal_revision: self.state.revision,
                        journal_digest: canonical_digest(&self.state)?,
                        artifacts,
                        materials,
                    },
                };
                request.validate_for(
                    &accepted.delivery.receipt,
                    &intent.request,
                    &intent.proposal,
                )?;
                let record = PhasePreparedIntent {
                    request_digest: canonical_digest(&request)?,
                    request: request.clone(),
                    prepared_at_ns: self.live()?,
                    origin: InputOrigin::capture(a)?,
                };
                let mut next = self.next()?;
                next.attempts.get_mut(&id).unwrap().$prepared = Some(record);
                self.persist(next)?;
                self.$check(id, observed)?;
                Ok(request)
            }
        }
    };
}
pub(crate) use journal_prepared;
