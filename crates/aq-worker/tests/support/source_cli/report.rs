use super::*;
impl Peer {
    pub(super) fn result(&self, value: Value) -> Response {
        let r: ResultReport = serde_json::from_value(value.clone()).unwrap();
        r.validate().unwrap();
        let mut operations = self.operations.lock().unwrap();
        if let Some((old, response)) = operations.get(&r.report_id) {
            assert_eq!(old, &value);
            return json_response(response);
        }
        assert_eq!(
            r.supersedes_report_id,
            self.report.lock().unwrap().as_ref().map(|r| r.report_id)
        );
        let (disposition, lost_message) = if self.cancel {
            assert_eq!(r.stop_certainty, StopCertainty::NotStarted);
            assert_eq!(r.outcome, ProcessOutcome::Cancelled);
            assert!(r.artifacts.is_empty() && r.pending_operation_ids.is_empty());
            (
                ResultDisposition::RetainedCancelled,
                "synthetic intermediary lost committed nonstart response",
            )
        } else {
            self.assert_artifact_report(&r);
            (
                ResultDisposition::Accepted,
                "synthetic intermediary lost committed result response",
            )
        };
        let response = json!(ResultResponse {
            report_id: r.report_id,
            disposition,
            retention_release: true
        });
        operations.insert(r.report_id, (value, response));
        *self.report.lock().unwrap() = Some(r);
        self.report_lost.store(true, Ordering::SeqCst);
        (StatusCode::BAD_GATEWAY, lost_message).into_response()
    }

    fn assert_artifact_report(&self, r: &ResultReport) {
        let upload = self.upload.lock().unwrap();
        let (m, status) = upload.as_ref().unwrap();
        let receipt = status.receipt.as_ref().unwrap();
        assert_eq!(r.artifacts.len(), 1);
        let a = &r.artifacts[0];
        assert_eq!(a.artifact_id, m.artifact_id);
        assert_eq!(a.digest, m.manifest_digest);
        assert_eq!(a.bytes, m.manifest_bytes + m.bundle_bytes);
        assert_eq!(a.central_receipt, Some(receipt.receipt_id.to_string()));
        assert_eq!(r.stop_certainty, StopCertainty::ConfirmedStopped);
        assert!(r.pending_operation_ids.is_empty());
    }
}
