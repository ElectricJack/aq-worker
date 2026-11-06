//! Actual stopped custody. A deserialized anchor cannot construct this owner.
use super::*;
#[derive(Clone, Copy, Debug)]
pub enum ResultStopCause {
    Natural,
    Cancelled,
    Expired,
    Failure,
    /// The worker ended a still-running session because its explicit in-session
    /// result claim was durably recorded. This is not a deadline expiry.
    ResultRecorded,
    /// MS1 docker-local: the node-side driver proved the harness exited 0 in
    /// its sandbox (backend runner exit) and the workspace was copied back,
    /// before submitting the claim. The only cause that reports Success.
    HarnessSucceeded,
}
impl ResultStopCause {
    /// Process outcome retained in the stop anchor. root_status is the tmux
    /// SERVER wait status and proves no harness exit code, so natural pane exit
    /// and a worker stop after a recorded result both remain Unknown; the
    /// recorded claim, not the process outcome, carries the completed work.
    pub fn process_outcome(self) -> aq_admission::transport::ProcessOutcome {
        use aq_admission::transport::ProcessOutcome;
        match self {
            Self::Cancelled => ProcessOutcome::Cancelled,
            Self::Expired => ProcessOutcome::Expired,
            // A failed execution is reported as failed, never as unknown.
            Self::Failure => ProcessOutcome::Failed,
            Self::HarnessSucceeded => ProcessOutcome::Success,
            Self::Natural | Self::ResultRecorded => ProcessOutcome::Unknown,
        }
    }
}
/// Retains the actual opaque kernel StoppedProof and original result/input owner.
/// No Clone/Deserialize/reopen. It grants source capture only, never execution.
pub struct DurableStoppedResult {
    pub(super) custody: Arc<Custody>,
    pub(super) proof: aq_session_host::StoppedProof,
}
impl DurableStoppedResult {
    pub fn attempt_id(&self) -> Uuid {
        self.custody.scope.id
    }
    pub(super) fn verify(&self, pipeline: &InputPipeline) -> Result<()> {
        let c = &self.custody;
        ensure!(
            c.pipeline
                .upgrade()
                .is_some_and(|p| Arc::ptr_eq(&p.owned, &pipeline.owned)),
            "result stopped owner dropped/foreign"
        );
        c.roots.verify()?;
        c.storage.verify()?;
        ensure!(
            self.proof.observation().kernel_wait_echild,
            "result stopped writers uncertain"
        );
        let a = pipeline
            .owned
            .lock()
            .map_err(|_| anyhow::anyhow!("result original owner poisoned"))?;
        let h = a
            .journal
            .state()
            .attempts
            .get(&c.scope.id)
            .and_then(|a| a.result.as_ref())
            .context("result stopped history absent")?;
        let digest = crate::native::journal::canonical_digest(&self.proof)?;
        ensure!(
            h.scope.binding == c.binding
                && h.stop.as_ref().is_some_and(|s| s.proof_digest == digest),
            "result stop anchor differs from actual retained proof"
        );
        Ok(())
    }
}
impl InputPipeline {
    pub async fn retain_stopped_result(
        &self,
        id: Uuid,
        cause: ResultStopCause,
    ) -> Result<Option<DurableStoppedResult>> {
        let custody = self
            .results
            .entries
            .lock()
            .map_err(|_| anyhow::anyhow!("result registry poisoned"))?
            .get(&id)
            .cloned();
        let Some(custody) = custody else {
            return Ok(None);
        };
        self.close_result_intake(id).await?;
        custody.roots.verify()?;
        let proof = self.launch.result_stopped(&custody.scope)?;
        let proof_digest = crate::native::journal::canonical_digest(&proof)?;
        let kept = proof.clone();
        let c = custody.clone();
        let h = self
            .storage(move |owner| {
                let a = owner
                    .journal
                    .state()
                    .attempts
                    .get(&id)
                    .context("result attempt absent")?;
                let old = a.result.as_ref().context("result history absent")?;
                if old.claim.is_none() {
                    return Ok(None);
                }
                let stopped = a
                    .native_host
                    .as_ref()
                    .and_then(|h| h.stopped.as_ref())
                    .context("result physical stop not durable")?;
                ensure!(
                    proof_digest == stopped.proof_digest && kept.observation().kernel_wait_echild,
                    "result actual stopped proof differs"
                );
                let (_, receipt) = crate::native::journal::result_terminal_stop(a)?;
                if matches!(cause, ResultStopCause::Expired) {
                    ensure!(boot_ns()? >= c.scope.deadline, "result premature expiry");
                }
                let outcome = cause.process_outcome();
                if let Some(stop) = &old.stop {
                    ensure!(
                        stop.proof_digest == proof_digest && stop.stopped_receipt == receipt,
                        "changed result stop replay"
                    );
                    return Ok(Some(old.clone()));
                }
                let digest = old.digest()?;
                let native_revision = stopped.recorded_revision;
                Ok(Some(owner.journal.update_result(
                    id,
                    &digest,
                    |h, revision| {
                        h.stop = Some(StopAnchor {
                            proof_digest,
                            native_stopped_revision: native_revision,
                            recorded_revision: revision,
                            stopped_receipt: receipt,
                            outcome,
                        });
                        Ok(())
                    },
                )?))
            })
            .await?;
        let Some(h) = h else { return Ok(None) };
        if let Some(ack) = h.acknowledgment()?
            && custody.storage.write_ack(&ack).is_err()
        {
            tracing::warn!(attempt_id=%id,"stopped result retained; local acknowledgement projection unavailable");
        }
        Ok(Some(DurableStoppedResult { custody, proof }))
    }
}

#[cfg(test)]
mod outcome_tests {
    use super::ResultStopCause;
    use aq_admission::transport::ProcessOutcome;
    #[test]
    fn only_a_proven_harness_success_reports_success_and_failure_reports_failed() {
        assert!(matches!(
            ResultStopCause::HarnessSucceeded.process_outcome(),
            ProcessOutcome::Success
        ));
        assert!(matches!(
            ResultStopCause::Failure.process_outcome(),
            ProcessOutcome::Failed
        ));
        for unproven in [ResultStopCause::Natural, ResultStopCause::ResultRecorded] {
            assert!(matches!(
                unproven.process_outcome(),
                ProcessOutcome::Unknown
            ));
        }
    }
}
