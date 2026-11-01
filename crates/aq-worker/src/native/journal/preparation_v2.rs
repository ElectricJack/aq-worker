//! Separate durable no-launch phase provenance. Disk history grants no permission.
use super::*;
use crate::native::http::preparation_v2::validate_original_delivery;
use crate::native::http::preparation_v2::{AcceptedPreparation, ProposedPreparation};
use aq_admission::{
    Resources,
    preparation_v2::{AcceptRequest, AcceptedDelivery, Proposal, ResourceHold},
};

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PhaseAcceptance {
    pub proposal: Proposal,
    pub proposal_digest: Digest,
    pub request: AcceptRequest,
    pub request_digest: Digest,
    /// Captured before the atomic protocol choice/Accept intent publication.
    pub created_ns: u64,
    pub original_deadline_ns: u64,
}
#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReceivedPreparation {
    pub delivery: AcceptedDelivery,
    pub receipt_digest: Digest,
    pub sent_ns: u64,
    pub received_ns: u64,
    pub deadline_ns: u64,
    pub observed_before_write_ns: u64,
    pub record_revision: u64,
}
fn resources(r: &Reservation) -> Resources {
    let needed = &r.offer.offered.offer.manifest.resources;
    Resources {
        slots: 1,
        memory_bytes: needed.memory_bytes,
        cpu_millis: needed.cpu_millis,
        disk_bytes: r.disk_bytes,
    }
}
pub(super) fn validate(a: &Attempt, identity: &WorkerIdentity, revision: u64) -> Result<()> {
    let Some(p) = &a.phase_acceptance else {
        ensure!(
            a.phase_preparation.is_none(),
            "phase receipt without intent"
        );
        return Ok(());
    };
    let r = &a.reservation;
    p.request.validate_for(&p.proposal)?;
    validate_original_delivery(&p.proposal.original, &r.offer)?;
    ensure!(
        p.proposal_digest == p.proposal.digest()?
            && p.request_digest == p.request.digest()?
            && p.request.original.identity == *identity
            && a.acceptance
                .as_ref()
                .is_some_and(|v| same(v, &p.request.original).unwrap_or(false))
            && same(&p.request.hold.resources, &resources(r))?
            && p.request.hold.source_disk_bytes == r.disk_bytes
            && p.created_ns >= r.reserved_at_ns
            && p.created_ns < p.original_deadline_ns
            && p.original_deadline_ns <= r.deadline_ns
            && p.request.original.reservation_remaining_ms
                <= (p.original_deadline_ns - p.created_ns) / 1_000_000,
        "phase intent differs from exact reservation/proposal"
    );
    // Input adoption is a later owner. This slice cannot authorize any legacy
    // input, Prepare, or execution path by laundering the new lease into a plan.
    ensure!(
        a.preparation.is_none()
            && a.downloads.is_empty()
            && a.material_downloads.is_empty()
            && a.source.is_none()
            && a.prepared.is_none()
            && a.initial_grant.is_none(),
        "phase attempt has legacy observations"
    );
    if let Some(record) = &a.phase_preparation {
        record
            .delivery
            .validate_for(&p.request, &p.proposal, record.delivery.delivery.nonce)?;
        let sample = record.delivery.sample(
            &p.request,
            &p.proposal,
            record.delivery.delivery.nonce,
            record.sent_ns,
            record.received_ns,
            None,
        )?;
        ensure!(
            record.receipt_digest == record.delivery.receipt.digest()?
                && record.sent_ns >= p.created_ns
                && record.sent_ns < p.original_deadline_ns
                && record.received_ns >= record.sent_ns
                && record.received_ns < record.deadline_ns
                && record.deadline_ns <= sample.deadline_ns()
                && record.observed_before_write_ns >= record.received_ns
                && record.record_revision
                    >= p.request
                        .original
                        .journal_revision
                        .checked_add(2)
                        .context("phase revision overflow")?
                && record.record_revision <= revision,
            "phase received history association/clock"
        );
    }
    Ok(())
}
impl Journal {
    /// One atomic publication selects the new protocol and retains exact request
    /// bytes. No legacy-only intermediate intent exists at any fsync cut.
    pub fn begin_phase_accept(
        &mut self,
        id: Uuid,
        proposed: &ProposedPreparation,
        advertised_capabilities: Vec<Capability>,
        source_profile: ArtifactRef,
        maximum_preparation_ms: u64,
    ) -> Result<AcceptRequest> {
        let now = self.live()?;
        let a = self
            .state
            .attempts
            .get(&id)
            .context("phase attempt absent")?;
        let r = &a.reservation;
        validate_original_delivery(&proposed.proposal().original, &r.offer)?;
        ensure!(
            proposed.original_deadline_ns() <= r.deadline_ns,
            "phase proposal differs from reserved cap"
        );
        let cap = proposed.original_deadline_ns();
        let acceptance = ProfiledAcceptance {
            binding: r.offer.offered.offer.binding.clone(),
            advertised_capabilities,
        };
        acceptance.validate_against(&r.offer.offered.offer)?;
        if let Some(old) = &a.phase_acceptance {
            ensure!(
                old.proposal_digest == proposed.proposal().digest()?
                    && same(&old.request.original.acceptance, &acceptance)?
                    && old.request.hold.source_profile == source_profile
                    && old.request.hold.maximum_preparation_ms == maximum_preparation_ms,
                "phase intent retry changed"
            );
            // Return historical exact intent even after proposal expiry. Only an
            // actual still-live Accepted observation can permit transport replay.
            return Ok(old.request.clone());
        }
        ensure!(
            a.acceptance.is_none() && now < cap,
            "legacy intent or expired phase proposal"
        );
        let original = Accept {
            schema_version: 1,
            operation_id: r.accept_operation_id,
            identity: self.state.identity.clone(),
            acceptance,
            reservation: NoLaunchReservation {
                reservation_id: r.reservation_id,
                reservation_operation_id: r.reservation_operation_id,
                reservation_journal_digest: canonical_digest(r)?,
            },
            guardian_id: r.guardian_id,
            launch_id: r.launch_id,
            journal_revision: self.state.revision,
            journal_digest: canonical_digest(&self.state)?,
            reservation_remaining_ms: (cap - now) / 1_000_000,
        };
        let request = AcceptRequest {
            schema_version: 2,
            negotiation: proposed.proposal().negotiation.clone(),
            proposal_digest: proposed.proposal().digest()?,
            hold: ResourceHold {
                reservation: original.reservation.clone(),
                resources: resources(r),
                source_profile,
                source_disk_bytes: r.disk_bytes,
                maximum_preparation_ms,
            },
            original,
        };
        request.validate_for(proposed.proposal())?;
        let record = PhaseAcceptance {
            proposal: proposed.proposal().clone(),
            proposal_digest: proposed.proposal().digest()?,
            request: request.clone(),
            request_digest: request.digest()?,
            created_ns: now,
            original_deadline_ns: cap,
        };
        let mut next = self.next()?;
        let a = next.attempts.get_mut(&id).unwrap();
        a.acceptance_digest = Some(canonical_digest(&request.original)?);
        a.acceptance = Some(request.original.clone());
        a.phase_acceptance = Some(record);
        self.persist(next)?;
        Ok(request)
    }
    /// Retain once-usable actual received evidence even if the lease expires
    /// during persistence. The return value is history, never a live permit.
    pub fn record_phase_preparation(
        &mut self,
        id: Uuid,
        observed: &AcceptedPreparation,
    ) -> Result<ReceivedPreparation> {
        let now = self.live()?;
        let a = self
            .state
            .attempts
            .get(&id)
            .context("phase attempt absent")?;
        let p = a
            .phase_acceptance
            .as_ref()
            .context("phase intent not durable")?;
        ensure!(
            observed.request_digest() == &p.request_digest
                && observed.proposal_digest() == &p.proposal_digest,
            "phase receipt differs from durable request"
        );
        observed.delivery().validate_for(
            &p.request,
            &p.proposal,
            observed.delivery().delivery.nonce,
        )?;
        if let Some(first) = &a.phase_preparation {
            ensure!(
                same(&first.delivery.receipt, &observed.delivery().receipt)?,
                "phase receipt changed on replay"
            );
            return Ok(first.clone());
        }
        let mut next = self.next()?;
        let (first, sent_ns, received_ns, deadline_ns) = observed.first_transmission();
        let record = ReceivedPreparation {
            delivery: first.clone(),
            receipt_digest: first.receipt.digest()?,
            sent_ns,
            received_ns,
            deadline_ns,
            observed_before_write_ns: now,
            record_revision: next.revision,
        };
        next.attempts.get_mut(&id).unwrap().phase_preparation = Some(record.clone());
        self.persist(next)?;
        Ok(record)
    }
    /// Check the actual current invocation and opaque network observation against
    /// immutable fsynced history. Input/host effects require their own owners.
    pub fn check_phase_preparation(&self, id: Uuid, observed: &AcceptedPreparation) -> Result<u64> {
        let now = self.live()?;
        let a = self
            .state
            .attempts
            .get(&id)
            .context("phase attempt absent")?;
        let p = a.phase_acceptance.as_ref().context("phase intent absent")?;
        let record = a
            .phase_preparation
            .as_ref()
            .context("phase receipt not durable")?;
        ensure!(
            observed.request_digest() == &p.request_digest
                && observed.proposal_digest() == &p.proposal_digest
                && same(&record.delivery.receipt, &observed.delivery().receipt)?,
            "phase current receipt differs"
        );
        let cap = record.deadline_ns.min(observed.deadline_ns());
        ensure!(now < cap, "durable preparation expired");
        Ok(cap)
    }
}
