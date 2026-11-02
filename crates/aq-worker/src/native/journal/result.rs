//! Result history joins. None of these serialized facts reconstruct live custody.
use super::*;
use crate::native::result::history::{MAX_HISTORY_BYTES, ResultHistory, StorageStage};
pub(super) fn remaining_reserve(state: &State) -> Result<usize> {
    state
        .attempts
        .values()
        .filter_map(|a| a.result.as_ref())
        .try_fold(0usize, |n, h| {
            let used = canonical_bytes(h)?.len();
            ensure!(used <= MAX_HISTORY_BYTES, "result history bound");
            n.checked_add(MAX_HISTORY_BYTES - used)
                .context("result reserve overflow")
        })
}
pub(super) fn validate(a: &Attempt, state: &State, ids: &mut BTreeSet<Uuid>) -> Result<()> {
    let Some(h) = &a.result else { return Ok(()) };
    h.validate()?;
    let s = &h.scope;
    let execution = a.phase_scope()?;
    let offer = &a.reservation.offer.offered;
    let source = a.source.as_ref().context("result source owner absent")?;
    ensure!(
        s.binding.launch == offer.offer.binding
            && s.binding.execution_record_digest == execution.digest
            && s.binding.expected_outcome == source.spec.outcome_artifact
            && same(&s.runtime, &a.reservation.runtime)?
            && s.kernel_boot_id == execution.kernel_boot_id
            && s.original_deadline_ns == execution.deadline_ns
            && s.original_hard_deadline_ns == execution.hard_deadline_ns
            && s.created_ns >= execution.observed_before_write_ns
            && s.recorded_revision > execution.record_revision
            && s.recorded_revision < state.revision
            && s.source == *source.binding()?
            && s.managed_root == source.root
            && s.reserved_disk_bytes == a.reservation.disk_bytes,
        "result original execution/source association"
    );
    let manifest = &offer.offer.manifest;
    ensure!(
        s.installation.adapter == manifest.harness.adapter
            && s.installation.environment == manifest.harness.environment
            && s.installation.permission_digest == canonical_digest(&manifest.permissions)?,
        "result selected installation differs"
    );
    for id in [s.binding.operation_id, s.binding.report_id, s.binding.nonce]
        .into_iter()
        .chain(s.operations.all())
    {
        ensure!(ids.insert(id), "result global operation identity reused");
    }
    if let StorageStage::Ready {
        recorded_revision, ..
    } = &s.storage
    {
        ensure!(
            *recorded_revision < state.revision,
            "result storage revision"
        );
    }
    if let Some(c) = &h.claim {
        ensure!(
            c.recorded_revision < state.revision,
            "result claim revision"
        );
    }
    if let Some(stop) = &h.stop {
        let host = a
            .native_host
            .as_ref()
            .context("result stop without actual host")?;
        let stopped = host
            .stopped
            .as_ref()
            .context("result stop without physical proof")?;
        let (_, receipt) = terminal_stop(a)?;
        ensure!(
            stop.proof_digest == stopped.proof_digest
                && stop.native_stopped_revision == stopped.recorded_revision
                && stop.stopped_receipt == receipt
                && stop.recorded_revision < state.revision,
            "result actual stop anchor differs"
        );
    }
    if let Some(report) = &h.report {
        let (stop, _) = terminal_stop(a)?;
        ensure!(
            same(&report.request.stop, &stop)? && report.recorded_revision < state.revision,
            "result report original typed stop differs"
        );
    }
    if let Some(upload) = &h.source.upload {
        if let Some(p) = &upload.pending {
            ensure!(ids.insert(p.operation_id), "result upload operation reused");
        }
    }
    Ok(())
}
pub(crate) fn terminal_stop(
    a: &Attempt,
) -> Result<(aq_admission::transport::WorkerStopEvidence, ArtifactRef)> {
    let h = a.native_host.as_ref().context("result host absent")?;
    match (
        &a.phase_execution,
        &a.refresh_execution,
        &h.terminal,
        &h.refresh_terminal,
    ) {
        (Some(_), None, Some(t), None) => {
            let s = t
                .stopped
                .as_ref()
                .context("result stopped request absent")?;
            let r = s.receipt.as_ref().context("result stopped ACK absent")?;
            r.validate_for(&s.request)?;
            Ok((s.request.original.evidence.clone(), r.original.artifact()?))
        }
        (None, Some(_), None, Some(t)) => {
            let s = t
                .stopped
                .as_ref()
                .context("result refresh stopped request absent")?;
            let r = s
                .receipt
                .as_ref()
                .context("result refresh stopped ACK absent")?;
            r.validate_for(&s.request)?;
            Ok((s.request.original.evidence.clone(), r.original.artifact()?))
        }
        _ => anyhow::bail!("missing or mixed result terminal profile"),
    }
}
impl Journal {
    pub(crate) fn begin_result(&mut self, id: Uuid, mut history: ResultHistory) -> Result<()> {
        self.live()?;
        let a = self
            .state
            .attempts
            .get(&id)
            .context("result attempt absent")?;
        ensure!(
            a.result.is_none() && a.native_host.is_none(),
            "result channel must precede physical launch"
        );
        let execution = a.phase_scope()?;
        self.check_native_execution(
            id,
            &history.scope.binding.execution_record_digest,
            execution.deadline_ns,
            execution.hard_deadline_ns,
        )?;
        let mut next = self.next()?;
        history.scope.recorded_revision = self.state.revision;
        next.attempts.get_mut(&id).unwrap().result = Some(history);
        self.persist(next)
    }
    pub(crate) fn update_result(
        &mut self,
        id: Uuid,
        expected: &Digest,
        update: impl FnOnce(&mut ResultHistory, u64) -> Result<()>,
    ) -> Result<ResultHistory> {
        self.live()?;
        let old = self
            .state
            .attempts
            .get(&id)
            .and_then(|a| a.result.as_ref())
            .context("result history absent")?;
        ensure!(&old.digest()? == expected, "result CAS changed");
        let mut next = self.next()?;
        let h = next.attempts.get_mut(&id).unwrap().result.as_mut().unwrap();
        update(h, self.state.revision)?;
        let result = h.clone();
        self.persist(next)?;
        ensure!(
            self.state.attempts[&id].result.as_ref().unwrap().digest()? == result.digest()?,
            "result durable readback differs"
        );
        Ok(result)
    }
}
