//! Immutable received initial authority history, never a deserialized live permit.
use super::*;
use crate::native::http::control::Granted;
use aq_admission::{control::InitialGrantDelivery, transport::Prepare};
const CLOCK: &str = "linux_clock_boottime";
#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReceivedInitialGrant {
    pub delivery: InitialGrantDelivery,
    pub delivery_digest: Digest,
    pub prepare_digest: Digest,
    pub runtime: RuntimeIdentity,
    pub kernel_boot_id: Uuid,
    pub clock_source: String,
    pub sent_ns: u64,
    pub received_ns: u64,
    pub deadline_ns: u64,
    pub hard_deadline_ns: u64,
    /// Sampled BEFORE serialization/fsync. Not a durable post-fsync timestamp.
    pub observed_before_write_ns: u64,
    pub record_revision: u64,
}
pub(super) fn validate(a: &Attempt, revision: u64) -> Result<()> {
    let Some(record) = &a.initial_grant else {
        return Ok(());
    };
    let prepared = a
        .prepared
        .as_ref()
        .context("grant without Prepared intent")?;
    let request = &prepared.request;
    let plan = &prepared.preparation.delivery.original;
    record.delivery.validate_for(
        request,
        plan,
        &a.reservation.offer,
        record.delivery.delivery.nonce,
    )?;
    ensure!(
        record.prepare_digest == prepared.request_digest
            && record.delivery_digest == canonical_digest(&record.delivery)?
            && same(&record.runtime, &a.reservation.runtime)?
            && !record.kernel_boot_id.is_nil()
            && record.clock_source == CLOCK
            && record.record_revision
                >= request
                    .journal_revision
                    .checked_add(2)
                    .context("received grant revision overflow")?
            && record.record_revision <= revision
            && canonical_bytes(&record.delivery)?.len() <= aq_admission::control::MAX_REPLY_BYTES,
        "received grant history association/bound"
    );
    let hard = record
        .delivery
        .execution_deadline_ms
        .checked_sub(record.delivery.delivery.clock.server_time_ms)
        .and_then(|ms| ms.checked_mul(1_000_000))
        .and_then(|ns| record.sent_ns.checked_add(ns))
        .context("received grant hard clock overflow")?;
    let maximum = record.delivery.delivery.client_deadline_ns(
        record.sent_ns,
        record.received_ns,
        Some(
            prepared
                .preparation
                .deadline_ns
                .min(record.hard_deadline_ns),
        ),
    )?;
    ensure!(
        record.sent_ns >= prepared.prepared_at_ns
            && record.received_ns >= record.sent_ns
            && record.received_ns < record.deadline_ns
            && record.deadline_ns <= maximum
            && record.hard_deadline_ns <= hard
            && record.received_ns < record.hard_deadline_ns
            && record.observed_before_write_ns >= record.received_ns,
        "received grant historical clocks"
    );
    Ok(())
}
fn same_initial(a: &InitialGrantDelivery, b: &InitialGrantDelivery) -> Result<bool> {
    Ok(a.schema_version == b.schema_version
        && a.operation_id == b.operation_id
        && same(&a.original, &b.original)?
        && a.versions == b.versions
        && a.execution_deadline_ms == b.execution_deadline_ms)
}
impl Journal {
    /// Called only with actual process-local input and network observations.
    /// Expiry does not discard once-usable received evidence or release holds.
    pub(crate) fn record_initial_grant(
        &mut self,
        id: Uuid,
        request: &Prepare,
        observed: &Granted,
    ) -> Result<ReceivedInitialGrant> {
        let now = self.live()?;
        ensure!(
            observed.renewal().is_none(),
            "initial owner refuses renewal observation"
        );
        let a = self
            .state
            .attempts
            .get(&id)
            .context("received grant attempt absent")?;
        let prepared = a
            .prepared
            .as_ref()
            .context("received grant Prepared intent absent")?;
        ensure!(
            same(request, &prepared.request)?
                && observed.initial_prepare_digest() == &prepared.request_digest,
            "received grant differs from actual Prepared request"
        );
        observed.initial().validate_for(
            request,
            &prepared.preparation.delivery.original,
            &a.reservation.offer,
            observed.initial().delivery.nonce,
        )?;
        if let Some(first) = &a.initial_grant {
            ensure!(
                same_initial(&first.delivery, observed.initial())?,
                "changed initial grant replay"
            );
            return Ok(first.clone());
        }
        let mut next = self.next()?;
        let record = ReceivedInitialGrant {
            delivery: observed.initial().clone(),
            delivery_digest: canonical_digest(observed.initial())?,
            prepare_digest: prepared.request_digest.clone(),
            runtime: a.reservation.runtime.clone(),
            kernel_boot_id: self.kernel_boot_id,
            clock_source: CLOCK.into(),
            sent_ns: observed.initial_sent_ns(),
            received_ns: observed.initial_received_ns(),
            deadline_ns: observed.deadline_ns(),
            hard_deadline_ns: observed.hard_deadline_ns(),
            observed_before_write_ns: now,
            record_revision: next.revision,
        };
        next.attempts.get_mut(&id).unwrap().initial_grant = Some(record.clone());
        self.persist(next)?;
        Ok(record)
    }
}

impl ReceivedInitialGrant {
    /// Local record identity, NOT the full worker journal checkpoint digest.
    pub fn digest(&self) -> Result<Digest> {
        canonical_digest(self)
    }
}
impl Journal {
    pub(crate) fn check_initial_grant(
        &self,
        id: Uuid,
        digest: &Digest,
        deadline: u64,
        hard: u64,
    ) -> Result<u64> {
        let now = self.live()?;
        let record = self
            .state
            .attempts
            .get(&id)
            .and_then(|a| a.initial_grant.as_ref())
            .context("initial grant history absent")?;
        ensure!(
            &record.digest()? == digest
                && deadline <= record.deadline_ns
                && hard <= record.hard_deadline_ns
                && now < deadline.min(hard),
            "durable initial grant expired or inconsistent"
        );
        Ok(now)
    }
}
