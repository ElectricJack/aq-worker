//! FORMAT12 candidate parking history. Serialized facts never mint live owners.
use super::*;
use crate::continuation::history::MAX_HISTORY_BYTES;
#[cfg(feature = "test-support")]
use crate::continuation::history::{
    ParkingHistory, ParkingIntent, ParkingSourceHistory, ParkingStopAnchor,
};
#[cfg(feature = "test-support")]
use aq_session_host::StoppedProof;

pub(super) fn validate(attempt: &Attempt, revision: u64, ids: &mut BTreeSet<Uuid>) -> Result<()> {
    let Some(history) = &attempt.parking else {
        return Ok(());
    };
    history.validate()?;
    let intent = &history.intent;
    let host = attempt
        .native_host
        .as_ref()
        .context("parking without native host intent")?;
    let source = attempt
        .source
        .as_ref()
        .context("parking without native source")?;
    let created = source
        .operations
        .get("create")
        .and_then(|op| op.result.as_ref())
        .context("parking without original workspace")?;
    let workspace: crate::source::Workspace = serde_json::from_value(created.clone())?;
    let execution = attempt.phase_scope()?;
    ensure!(
        intent.suspension.binding
            == aq_admission::transport::lifecycle_binding(
                &attempt.reservation.offer.offered.offer
            )?
            && intent.execution_record_digest == execution.digest
            && intent.host_intent_digest == host.intent.digest()?
            && intent.execution_spec_digest == host.intent.binding.execution_spec_digest
            && intent.source == *source.binding()?
            && canonical_digest(&intent.workspace)? == canonical_digest(&workspace)?
            && intent.managed_root == source.root
            && intent.reserved_disk_bytes == attempt.reservation.disk_bytes
            && intent.recorded_revision > host.intent.created_revision
            && intent.recorded_revision <= revision
            && !attempt
                .reservation
                .offer
                .offered
                .offer
                .manifest
                .terminal
                .input_enabled,
        "parking exact original execution/profile/source association"
    );
    ensure!(
        intent
            .workspace_limit_bytes
            .checked_mul(2)
            .and_then(|n| n.checked_add(intent.artifact_limit_bytes))
            .is_some_and(|n| n <= intent.reserved_disk_bytes),
        "parking source limits exceed original reservation"
    );
    for id in [
        intent.operations.terminal_fence,
        intent.operations.checkpoint,
        intent.operations.export,
        intent.operations.transfer,
        intent.operations.upload_begin,
        intent.operations.upload_finalize,
        intent.operations.report,
    ] {
        ensure!(ids.insert(id), "parking operation identity reused");
    }
    if let Some(anchor) = &history.stop {
        let stopped = host
            .stopped
            .as_ref()
            .context("parking anchor without native stopped history")?;
        ensure!(
            anchor.proof_digest == stopped.proof_digest
                && anchor.host_stopped_revision == stopped.recorded_revision
                && anchor.recorded_revision >= anchor.host_stopped_revision
                && anchor.recorded_revision > intent.recorded_revision
                && anchor.recorded_revision <= revision,
            "parking exact native stopped anchor"
        );
    }
    if let Some(pending) = history
        .source
        .upload
        .as_ref()
        .and_then(|upload| upload.pending.as_ref())
    {
        ensure!(
            ids.insert(pending.operation_id),
            "parking upload operation identity reused"
        );
    }
    Ok(())
}

/// One checked accounting projection for validation and available-capacity reads.
/// Full original disk stays charged; no parking path retires or evicts storage.
pub(super) fn charge(attempt: &Attempt) -> Result<Limits> {
    let released = if let Some(history) = &attempt.parking {
        if history.settled_source_claim()? {
            let anchor = history
                .stop
                .as_ref()
                .context("settled parking stop absent")?;
            let host = attempt
                .native_host
                .as_ref()
                .context("settled parking host absent")?;
            let stopped = host
                .stopped
                .as_ref()
                .context("settled parking native stop absent")?;
            ensure!(
                anchor.proof_digest == stopped.proof_digest
                    && anchor.host_stopped_revision == stopped.recorded_revision
                    && history.intent.host_intent_digest == host.intent.digest()?
                    && history.intent.execution_record_digest == attempt.phase_scope()?.digest,
                "capacity requires exact committed stopped/execution anchor"
            );
            attempt.source.as_ref().is_some_and(|source| {
                source.pending.is_none()
                    && source.complete()
                    && source.operations.len() == 5
                    && source
                        .operations
                        .values()
                        .all(|operation| operation.result.is_some())
            })
        } else {
            false
        }
    } else {
        false
    };
    let resources = &attempt.reservation.offer.offered.offer.manifest.resources;
    Ok(Limits {
        slots: u32::from(!released),
        memory_bytes: if released { 0 } else { resources.memory_bytes },
        cpu_millis: if released { 0 } else { resources.cpu_millis },
        disk_bytes: attempt.reservation.disk_bytes,
    })
}
pub(super) fn remaining_reserve(state: &State) -> Result<usize> {
    state
        .attempts
        .values()
        .filter_map(|attempt| attempt.parking.as_ref())
        .try_fold(0usize, |total, history| {
            let used = canonical_bytes(history)?.len();
            ensure!(used <= MAX_HISTORY_BYTES, "parking history reserved bound");
            total
                .checked_add(MAX_HISTORY_BYTES - used)
                .context("parking reserve overflow")
        })
}
#[cfg(feature = "test-support")]
fn actual_anchor(attempt: &Attempt, proof: &StoppedProof) -> Result<()> {
    let stopped = attempt
        .native_host
        .as_ref()
        .and_then(|host| host.stopped.as_ref())
        .context("actual stopped history absent")?;
    ensure!(
        stopped.proof_digest == canonical_digest(proof)?
            && canonical_digest(&stopped.observation)? == canonical_digest(proof.observation())?
            && proof.observation().phase == aq_session_host::HostPhase::Stopped
            && proof.observation().kernel_wait_echild
            && proof.observation().reaped_count > 0,
        "parking requires exact opaque ECHILD proof"
    );
    Ok(())
}

#[cfg(feature = "test-support")]
impl Journal {
    /// Readback is still history. Only the physical owner holding the actual
    /// opaque proof may combine it with that proof to construct a stopped token.
    pub(crate) fn parking_readback(&self, id: Uuid) -> Result<ParkingHistory> {
        self.storage.check_owner()?;
        ensure!(!self.poisoned, "parking writer poisoned");
        let raw = self.storage.read(c"journal.json", MAX_BYTES)?;
        let snapshot: Snapshot = serde_json::from_slice(&raw)?;
        ensure!(
            snapshot.digest == canonical_digest(&snapshot.state)?
                && snapshot.digest == canonical_digest(&self.state)?,
            "parking committed snapshot differs from retained writer"
        );
        snapshot
            .state
            .attempts
            .get(&id)
            .and_then(|attempt| attempt.parking.clone())
            .context("parking committed readback absent")
    }
    pub(crate) fn begin_parking(
        &mut self,
        id: Uuid,
        intent: ParkingIntent,
    ) -> Result<ParkingHistory> {
        self.live()?;
        let attempt = self
            .state
            .attempts
            .get(&id)
            .context("parking attempt absent")?;
        if let Some(existing) = &attempt.parking {
            ensure!(
                existing.intent.digest()? == intent.digest()?,
                "parking intent changed"
            );
            return Ok(existing.clone());
        }
        let mut next = self.next()?;
        ensure!(
            intent.recorded_revision == next.revision,
            "parking intent revision"
        );
        next.attempts.get_mut(&id).unwrap().parking = Some(ParkingHistory {
            schema_version: 1,
            intent,
            stop: None,
            source: ParkingSourceHistory::default(),
            retention: None,
        });
        self.persist(next)?;
        self.parking_readback(id)
    }
    pub(crate) fn record_parking_stopped(
        &mut self,
        id: Uuid,
        intent_digest: &Digest,
        proof: &StoppedProof,
    ) -> Result<ParkingHistory> {
        // Safety/retention after execution expiry, never a new launch check.
        self.storage.check_owner()?;
        ensure!(!self.poisoned, "parking writer poisoned");
        self.native_stopped(id, proof)?;
        let attempt = self
            .state
            .attempts
            .get(&id)
            .context("parking stop attempt absent")?;
        actual_anchor(attempt, proof)?;
        let history = attempt.parking.as_ref().context("parking intent absent")?;
        ensure!(
            history.intent.digest()? == *intent_digest,
            "parking stop intent changed"
        );
        if history.stop.is_some() {
            return self.parking_readback(id);
        }
        let stopped = attempt
            .native_host
            .as_ref()
            .unwrap()
            .stopped
            .as_ref()
            .unwrap();
        let anchor = ParkingStopAnchor {
            proof_digest: stopped.proof_digest.clone(),
            host_stopped_revision: stopped.recorded_revision,
            recorded_revision: self
                .state
                .revision
                .checked_add(1)
                .context("parking revision overflow")?,
        };
        let mut next = self.next()?;
        next.attempts
            .get_mut(&id)
            .unwrap()
            .parking
            .as_mut()
            .unwrap()
            .stop = Some(anchor);
        self.persist(next)?;
        self.parking_readback(id)
    }
    pub(crate) fn parking_progress(
        &mut self,
        id: Uuid,
        proof: &StoppedProof,
        expected: &Digest,
        proposed: ParkingHistory,
    ) -> Result<ParkingHistory> {
        self.storage.check_owner()?;
        ensure!(!self.poisoned, "parking writer poisoned");
        let attempt = self
            .state
            .attempts
            .get(&id)
            .context("parking progress attempt absent")?;
        actual_anchor(attempt, proof)?;
        let previous = attempt
            .parking
            .as_ref()
            .context("parking progress absent")?;
        ensure!(
            canonical_digest(previous)? == *expected,
            "parking history compare-and-swap conflict"
        );
        previous.validate_advance(&proposed)?;
        if canonical_digest(previous)? == canonical_digest(&proposed)? {
            return Ok(previous.clone());
        }
        let mut next = self.next()?;
        next.attempts.get_mut(&id).unwrap().parking = Some(proposed);
        self.persist(next)?;
        Ok(self.state.attempts[&id].parking.as_ref().unwrap().clone())
    }
}
