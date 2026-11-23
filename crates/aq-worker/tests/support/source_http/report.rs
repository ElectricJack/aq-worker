use super::*;
impl PeerState {
    pub(super) fn report(&mut self, c: &mut TcpStream, body: &[u8]) {
        let report: ResultReport = serde_json::from_slice(body).unwrap();
        report.validate().unwrap();
        if let Some((old, response)) = self.operations.get(&report.report_id) {
            if old != body {
                refuse(c);
            } else {
                respond(c, response, false);
            }
            return;
        }
        if report.supersedes_report_id != self.report.as_ref().map(|r| r.report_id) {
            refuse(c);
            return;
        }
        if !self.has_retained_artifact(&report) {
            refuse(c);
            return;
        }
        assert!(report.pending_operation_ids.is_empty());
        let response = json!(ResultResponse {
            report_id: report.report_id,
            disposition: ResultDisposition::Accepted,
            retention_release: true
        });
        self.operations
            .insert(report.report_id, (body.to_vec(), response.clone()));
        self.report = Some(report);
        self.report_lost = true;
        respond(c, &response, true);
    }

    fn has_retained_artifact(&self, report: &ResultReport) -> bool {
        let m = self.manifest.as_ref().unwrap();
        let receipt = self.status.as_ref().unwrap().receipt.as_ref().unwrap();
        assert_eq!(report.attempt, m.attempt);
        assert_eq!(report.resolved_spec_digest, m.resolved_spec_digest);
        assert_eq!(report.stop_certainty, StopCertainty::ConfirmedStopped);
        assert_eq!(report.artifacts.len(), 1);
        let a = &report.artifacts[0];
        assert_eq!(a.artifact_id, m.artifact_id);
        assert_eq!(a.digest, m.manifest_digest);
        assert_eq!(a.bytes, m.manifest_bytes + m.bundle_bytes);
        a.central_receipt == Some(receipt.receipt_id.to_string())
    }
}
