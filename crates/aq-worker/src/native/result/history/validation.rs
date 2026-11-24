use super::*;

impl ResultHistory {
    pub fn validate(&self) -> Result<()> {
        self.validate_scope_identity()?;
        self.validate_scope_paths()?;
        self.validate_reservation()?;
        self.validate_scope_operations()?;
        self.validate_intake()?;
        ensure!(
            encoded(&self.scope)?.len() <= 16384
                && encoded(&self.source)?.len() <= MAX_SOURCE_HISTORY_BYTES,
            "result source envelope cannot spend stop/report reserve"
        );
        self.validate_source()?;
        if let Some(report) = &self.report {
            self.validate_report(report)?;
        }
        if let Some(receipt) = &self.receipt {
            self.validate_receipt(receipt)?;
        }
        ensure!(
            encoded(self)?.len() <= MAX_HISTORY_BYTES,
            "result history reserved envelope exhausted"
        );
        Ok(())
    }
    fn validate_intake(&self) -> Result<()> {
        let s = &self.scope;
        if let Some(claim) = &self.claim {
            self.validate_claim(claim)?;
        }
        ensure!(
            self.salvaged.is_none() || (self.claim.is_some() && self.stop.is_some()),
            "result salvage without claim and stop"
        );
        if let Some(stop) = &self.stop {
            ensure!(
                self.intake_closed
                    && stop.native_stopped_revision > 0
                    && stop.recorded_revision >= stop.native_stopped_revision
                    && stop.recorded_revision > s.recorded_revision,
                "result stopped anchor order"
            );
        }
        Ok(())
    }
    fn validate_claim(&self, claim: &ClaimRecord) -> Result<()> {
        let s = &self.scope;
        let value = claim.submission()?;
        ensure!(
            value.binding == s.binding
                && self.intake_closed
                && matches!(s.storage, StorageStage::Ready { recorded_revision, .. } if claim.recorded_revision > recorded_revision),
            "result claimed scope changed"
        );
        Ok(())
    }
    fn validate_report(&self, report: &ReportIntent) -> Result<()> {
        let s = &self.scope;
        let claim = self
            .claim
            .as_ref()
            .context("result report without explicit claim")?
            .submission()?;
        let stop = self
            .stop
            .as_ref()
            .context("result report without actual stop")?;
        let receipts = self.source.receipt_ids()?;
        result_record::validate_report(&report.request)?;
        ensure!(
            report.exact_bytes()?
                && (report.request.operation_id, report.request.report_id)
                    == (s.binding.operation_id, s.binding.report_id)
                && report.request.binding == s.binding.launch
                && report.request.supersedes.is_none()
                && (
                    &report.request.source_receipt_ids,
                    report.request.conclusion.as_str()
                ) == (&receipts, claim.conclusion.as_str())
                && serde_json::to_value(report.request.outcome)?
                    == serde_json::to_value(stop.outcome)?
                && report.recorded_revision > stop.recorded_revision,
            "result exact retained report changed"
        );
        Ok(())
    }
    fn validate_receipt(&self, receipt: &aq_admission::result_record::Receipt) -> Result<()> {
        let s = &self.scope;
        let report = self
            .report
            .as_ref()
            .context("result receipt without exact request")?;
        receipt.validate_for(&report.request)?;
        ensure!(
            receipt.expected_outcome == s.binding.expected_outcome
                && receipt.stopped_receipt == self.stop.as_ref().unwrap().stopped_receipt,
            "result B1 receipt owner pins changed"
        );
        Ok(())
    }
}

mod scope;

impl ReportIntent {
    fn exact_bytes(&self) -> Result<bool> {
        Ok(self.bytes == serde_json::to_vec(&self.request)?
            && self.digest == result_record::report_digest(&self.request)?
            && self.bytes.len() <= MAX_REPORT_BYTES)
    }
}
