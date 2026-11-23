//! Deterministic worker lifecycle. Offers, authority, observations and final
//! disposition are separate journal facts; uncertainty always retains resources.
use crate::{
    clock::Clock,
    fixture::FixtureHandle,
    journal::{AttemptRecord, Journal},
};
use anyhow::{Context, Result, ensure};
use aq_worker_protocol::*;
use std::path::Path;
use uuid::Uuid;

mod directive;
mod launch;
mod reporting;
mod reservation;
pub struct Runtime<C: Clock> {
    pub journal: Journal,
    pub clock: C,
}
impl<C: Clock> Runtime<C> {
    pub fn new(journal: Journal, clock: C) -> Self {
        Self { journal, clock }
    }
    pub fn reserve(&mut self, offer: Offer) -> Result<AcceptRequest> {
        self.reserve_inner(offer, false, 0)
    }
    pub fn authorize(&mut self, grant: Grant, sent_at_ms: u64) -> Result<()> {
        grant.validate()?;
        let now = self.clock.now_ms();
        ensure!(sent_at_ms <= now, "grant send time in future");
        let deadline = sent_at_ms
            .checked_add(grant.valid_for_ms)
            .context("grant deadline overflow")?;
        self.journal.transaction(|s| {
            ensure!(
                !s.reconciliation_required && grant.identity == s.identity,
                "stale incarnation"
            );
            let record = s
                .attempts
                .get_mut(&grant.attempt.attempt_id)
                .context("unknown attempt")?;
            ensure!(
                record.phase == Phase::Reserved || record.phase == Phase::Authorized,
                "grant cannot revive terminal reservation"
            );
            ensure!(
                now < record.acceptance.reservation_deadline_ms && now < deadline,
                "expired local reservation/grant"
            );
            ensure!(
                grant.attempt == record.offer.attempt
                    && grant.offer_id == record.offer.offer_id
                    && grant.reservation_id == record.acceptance.reservation_id
                    && grant.reservation_deadline_ms == record.acceptance.reservation_deadline_ms
                    && grant.resolved_spec_digest == record.offer.spec.resolved_spec_digest,
                "grant binding mismatch"
            );
            ensure!(
                record
                    .grant
                    .as_ref()
                    .is_none_or(|g| grant.grant_sequence >= g.grant_sequence),
                "stale grant sequence"
            );
            record.grant = Some(grant);
            record.grant_deadline_ms = Some(deadline);
            record.acceptance_sent_at_ms = sent_at_ms;
            record.phase = Phase::Authorized;
            Ok(())
        })
    }
    pub fn renew(&mut self, grant: Grant, sent_at_ms: u64) -> Result<()> {
        grant.validate()?;
        let now = self.clock.now_ms();
        self.journal.transaction(|s| {
            ensure!(
                !s.reconciliation_required && grant.identity == s.identity,
                "stale incarnation"
            );
            let r = s
                .attempts
                .get_mut(&grant.attempt.attempt_id)
                .context("unknown attempt")?;
            ensure!(
                matches!(
                    r.phase,
                    Phase::Authorized | Phase::Preparing | Phase::Running
                ),
                "attempt cannot renew"
            );
            ensure!(
                now < r.grant_deadline_ms.context("missing authority")?,
                "expired authority cannot renew"
            );
            let previous = r.grant.as_ref().context("missing grant")?;
            ensure!(
                grant.attempt == previous.attempt
                    && grant.offer_id == previous.offer_id
                    && grant.reservation_id == previous.reservation_id
                    && grant.resolved_spec_digest == previous.resolved_spec_digest
                    && grant.reservation_deadline_ms == previous.reservation_deadline_ms
                    && grant.grant_sequence > previous.grant_sequence,
                "renewal binding/sequence mismatch"
            );
            let deadline = sent_at_ms
                .checked_add(grant.valid_for_ms)
                .context("grant deadline overflow")?;
            ensure!(sent_at_ms <= now && now < deadline, "late renewal");
            r.grant = Some(grant);
            r.grant_deadline_ms = Some(deadline);
            Ok(())
        })
    }
    /// Persist intent before spawn. A crash at/after this point never replays spawn.
    pub fn begin_launch(&mut self, attempt_id: Uuid) -> Result<Uuid> {
        let now = self.clock.now_ms();
        self.journal.transaction(|s| {
            ensure!(
                !s.reconciliation_required,
                "startup reconciliation required"
            );
            let r = s.attempts.get_mut(&attempt_id).context("unknown attempt")?;
            ensure!(
                r.phase == Phase::Authorized && r.launch_id.is_none(),
                "launch already attempted or unauthorized"
            );
            ensure!(
                r.offer.identity == s.identity
                    && now < r.grant_deadline_ms.context("missing authority")?
                    && now < r.acceptance.reservation_deadline_ms,
                "expired/stale launch authority"
            );
            ensure!(
                r.source.as_ref().is_none_or(|s| s.workspace.is_some()),
                "source workspace preparation required"
            );
            let launch_id = Uuid::new_v4();
            r.launch_id = Some(launch_id);
            r.ownership_profile = Some(crate::guardian::PROFILE.into());
            r.phase = Phase::LaunchUncertain;
            r.stop_certainty = StopCertainty::StopUncertain;
            Ok(launch_id)
        })
    }
    pub fn observation(&mut self, attempt_id: Uuid, detail: String) -> Result<Observation> {
        ensure!(detail.len() <= MAX_TEXT, "observation too large");
        self.journal.transaction(|s| {
            let r = s.attempts.get_mut(&attempt_id).context("unknown attempt")?;
            r.observation_sequence = r
                .observation_sequence
                .checked_add(1)
                .context("observation sequence overflow")?;
            let observation = Observation {
                identity: s.identity.clone(),
                operation_id: Uuid::new_v4(),
                attempt: r.offer.attempt.clone(),
                sequence: r.observation_sequence,
                phase: r.phase,
                stop_certainty: r.stop_certainty,
                detail,
            };
            ensure!(
                r.pending_observations.len() < MAX_BATCH,
                "observation outbound queue full"
            );
            r.pending_observations
                .insert(observation.sequence, observation.clone());
            Ok(observation)
        })
    }
    pub fn acknowledge_observation(&mut self, attempt_id: Uuid, ack: &Ack) -> Result<()> {
        self.journal.transaction(|s| {
            let r = s.attempts.get_mut(&attempt_id).context("unknown attempt")?;
            let sequence = r
                .pending_observations
                .iter()
                .find(|(_, o)| o.operation_id == ack.operation_id)
                .map(|(n, _)| *n)
                .context("unknown observation acknowledgement")?;
            r.pending_observations.remove(&sequence);
            Ok(())
        })
    }
    pub fn reconcile_request(&self) -> ReconcileRequest {
        ReconcileRequest {
            identity: self.journal.state().identity.clone(),
            operation_id: Uuid::new_v4(),
            attempts: self
                .journal
                .state()
                .attempts
                .values()
                .filter(|r| r.phase != Phase::Retired)
                .take(MAX_BATCH)
                .map(|r| ReconcileAttempt {
                    attempt: r.offer.attempt.clone(),
                    previous_boot_id: r.offer.identity.boot_id,
                    phase: r.phase,
                    launch_id: r.launch_id,
                    stop_certainty: r.stop_certainty,
                    report_id: r.report.as_ref().map(|x| x.report_id),
                })
                .collect(),
            cursor: None,
            max_items: MAX_BATCH as u32,
        }
    }
}
