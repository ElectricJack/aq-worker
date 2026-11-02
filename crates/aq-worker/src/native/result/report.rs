//! Exact B1 report intent and history-only response-loss reconciliation.
use super::*;
use aq_admission::{
    Label, result_record,
    transport::{ResultReport, WorkerIdentity},
};
impl InputPipeline {
    pub async fn record_result_report(
        &self,
        stopped: &DurableStoppedResult,
    ) -> Result<result_record::Receipt> {
        let id = stopped.attempt_id();
        let previous = self.result_snapshot(stopped).await?;
        let c = stopped.custody.clone();
        self.storage(move |owner| {
            let a = owner
                .journal
                .state()
                .attempts
                .get(&id)
                .context("result attempt absent")?;
            let h = a.result.as_ref().context("result history absent")?;
            if h.report.is_some() {
                return Ok(());
            }
            ensure!(
                h.digest()? == previous.digest()?,
                "result report CAS changed"
            );
            let claim = h
                .claim
                .as_ref()
                .context("explicit result claim required")?
                .submission()?;
            let retained = h
                .source
                .receipt
                .as_ref()
                .context("central source retention required")?;
            let stop = h.stop.as_ref().context("actual result stop required")?;
            let (evidence, _) = crate::native::journal::result_terminal_stop(a)?;
            let aq_session_protocol::SessionIdentity::Remote {
                node_id, boot_id, ..
            } = c.binding.launch.session
            else {
                anyhow::bail!("result worker identity absent")
            };
            let report = ResultReport {
                schema_version: 1,
                operation_id: c.binding.operation_id,
                identity: WorkerIdentity { node_id, boot_id },
                binding: c.binding.launch.clone(),
                report_id: c.binding.report_id,
                supersedes: None,
                outcome: stop.outcome,
                stop: evidence,
                source_receipt_ids: vec![retained.receipt_id],
                // This producer owns no complete effect ledger. Empty is an
                // honest known-ID claim, never proof of effect quiescence.
                pending_effect_ids: vec![],
                conclusion: Label::new(claim.conclusion.as_str())?,
            };
            result_record::validate_report(&report)?;
            let bytes = serde_json::to_vec(&report)?;
            ensure!(
                bytes.len() <= MAX_REPORT_BYTES,
                "result report exceeds protected envelope; original scope retained"
            );
            let digest = result_record::report_digest(&report)?;
            let expected = h.digest()?;
            owner.journal.update_result(id, &expected, |h, revision| {
                h.report = Some(ReportIntent {
                    request: report,
                    bytes,
                    digest,
                    recorded_revision: revision,
                });
                Ok(())
            })?;
            Ok(())
        })
        .await?;
        stopped.verify(self)?;
        let receipt = self.replay_result_report(id).await?;
        let history = self.result_snapshot(stopped).await?;
        if let Some(ack) = history.acknowledgment()? {
            if stopped.custody.storage.write_ack(&ack).is_err() {
                tracing::warn!(attempt_id=%id,"B1 result retained; local acknowledgement projection unavailable");
            }
        }
        Ok(receipt)
    }
    /// Replays ONLY the exact saved report, with current machine authentication.
    /// Reopening a journal can call this history method, but cannot create a
    /// result channel, execute, capture source, replace IDs or alter the body.
    pub async fn replay_result_report(&self, id: Uuid) -> Result<result_record::Receipt> {
        let history = self
            .storage(move |owner| {
                Ok(owner
                    .journal
                    .state()
                    .attempts
                    .get(&id)
                    .and_then(|a| a.result.clone())
                    .context("result history absent")?)
            })
            .await?;
        history.validate()?;
        if let Some(receipt) = history.receipt {
            return Ok(receipt);
        }
        let intent = history
            .report
            .as_ref()
            .context("no retained result report to reconcile")?;
        ensure!(
            intent.bytes == serde_json::to_vec(&intent.request)?,
            "result request bytes changed"
        );
        let receipt = self
            .client
            .report(
                &intent.request,
                &history.scope.binding.expected_outcome,
                &history
                    .stop
                    .as_ref()
                    .context("result actual stop absent")?
                    .stopped_receipt,
            )
            .await?;
        let expected = intent.digest.clone();
        let saved = receipt.clone();
        self.storage(move |owner| owner.journal.retain_result_receipt(id, &expected, saved))
            .await?;
        Ok(receipt)
    }
}
