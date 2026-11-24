use super::*;
use aq_worker::journal::AttemptRecord;
impl Case {
    pub(super) fn assert_cancelled(
        &self,
        r: &AttemptRecord,
        output: &std::process::Output,
    ) -> std::path::PathBuf {
        let source = r.source.as_ref().unwrap();
        assert!(self.peer.directive_acked.load(Ordering::SeqCst));
        assert!(!output.status.success());
        assert!(
            String::from_utf8_lossy(&output.stderr)
                .contains("source preparation cancelled or expired")
        );
        assert!(!source.download.as_ref().unwrap().verified);
        assert_eq!(
            source.download.as_ref().unwrap().manifest_offset,
            source.download.as_ref().unwrap().manifest.manifest_bytes
        );
        assert_eq!(source.download.as_ref().unwrap().bundle_offset, 0);
        assert!(r.launch_id.is_none() && !r.start_intent);
        assert!(source.workspace.is_none());
        assert!(!source.operations.contains_key("create"));
        assert!(
            source
                .operations
                .keys()
                .all(|k| !k.starts_with("import-base-"))
        );
        assert!(r.result.is_none());
        assert!(self.peer.report.lock().unwrap().is_none());
        assert!(r.pending_bytes > 0);
        let incoming = self.worker.join("artifacts/incoming").join(
            source
                .download
                .as_ref()
                .unwrap()
                .manifest
                .artifact_id
                .to_string(),
        );
        assert!(incoming.is_dir());
        incoming
    }

    pub(super) async fn assert_lost_release(&self, incoming: &Path) {
        let unknown = self.launch().await;
        assert!(!unknown.status.success());
        assert!(String::from_utf8_lossy(&unknown.stderr).contains("502"));
        assert!(
            incoming.is_dir(),
            "lost release response must preserve partial input"
        );
        let unknown_state = self.journal();
        let held_record = unknown_state.attempts.values().next().unwrap();
        assert!(held_record.result.is_none());
        assert!(held_record.pending_bytes > 0);
        assert_eq!(
            held_record.report.as_ref(),
            self.peer.report.lock().unwrap().as_ref()
        );
    }

    pub(super) async fn assert_recovered(&self, incoming: &Path) {
        let recovered = self.launch().await;
        assert!(
            recovered.status.success(),
            "{} {}",
            String::from_utf8_lossy(&recovered.stdout),
            String::from_utf8_lossy(&recovered.stderr)
        );
        let recovered_state = self.journal();
        assert_eq!(recovered_state.attempts.len(), 1);
        let final_record = recovered_state.attempts.values().next().unwrap();
        assert_eq!(final_record.phase, Phase::Retired);
        assert_eq!(final_record.stop_certainty, StopCertainty::NotStarted);
        assert_eq!(final_record.pending_bytes, 0);
        assert!(final_record.source.as_ref().unwrap().retired);
        assert!(final_record.result.as_ref().unwrap().retention_release);
        assert_eq!(
            final_record.result.as_ref().unwrap().disposition,
            ResultDisposition::RetainedCancelled
        );
        assert!(!incoming.exists());
        assert!(final_record.launch_id.is_none());
        assert!(
            final_record
                .source
                .as_ref()
                .unwrap()
                .operations
                .keys()
                .all(|k| !k.starts_with("import-base-") && k != "create")
        );
        assert!(self.peer.report_lost.load(Ordering::SeqCst));
    }

    pub(super) fn assert_completed(&self, r: &AttemptRecord, output: &std::process::Output) {
        let source = r.source.as_ref().unwrap();
        assert!(
            output.status.success(),
            "{} {}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        assert_eq!(r.phase, Phase::Retired);
        assert_eq!(r.stop_certainty, StopCertainty::ConfirmedStopped);
        assert!(source.download.as_ref().unwrap().verified);
        assert!(source.receipt.is_some());
        assert!(source.report_sealed);
        assert!(r.result.as_ref().unwrap().retention_release);
        assert!(self.peer.report_lost.load(Ordering::SeqCst));
        assert!(
            self.peer
                .report
                .lock()
                .unwrap()
                .as_ref()
                .unwrap()
                .supersedes_report_id
                .is_none()
        );
        assert!(!source.workspace.as_ref().unwrap().path.exists());
        let checkpoint = source.checkpoint.as_ref().unwrap();
        self.peer
            .fixture
            .central_request("workspace.create", json!({"base":checkpoint.source}), 1);
    }
}
