//! Poll for evidence that writers stopped or that deletion finished.
use super::*;
impl BoatBackend {
    pub(super) async fn stop_inner(
        &mut self,
        s: &SandboxIdentity,
    ) -> Result<StoppedWriterProof, BackendError> {
        let e = self.scoped_entry(s).await?;
        if e.state != SandboxState::Archived {
            self.api
                .request(Method::POST, &format!("/sandboxes/{}/stop", s.backend_id))
                .body(json!({"force":false}))
                .json()
                .await?;
        }
        let end = Instant::now() + Duration::from_millis(self.config.poll_timeout_ms);
        loop {
            let e = self.scoped_entry(s).await?;
            if let Confirmation::Confirmed(p) = e.writer_proof() {
                self.replay_vm_time().await?;
                return Ok(p);
            }
            if Instant::now() >= end {
                return Err(BackendError::Uncertain(UnconfirmedReason::Timeout));
            }
            tokio::time::sleep(Duration::from_millis(self.config.poll_interval_ms)).await;
        }
    }
    pub(super) async fn wait_deleted(
        &self,
        s: &SandboxIdentity,
        operation: &str,
    ) -> Result<StoppedWriterProof, BackendError> {
        let path = format!("/sandboxes/{}/snapshots", s.backend_id);
        let end = Instant::now() + Duration::from_millis(self.config.poll_timeout_ms);
        loop {
            if self.deletion_confirmed(s, operation, &path).await? {
                return Ok(StoppedWriterProof::destroyed(s.clone()));
            }
            if Instant::now() >= end {
                return Err(BackendError::Uncertain(UnconfirmedReason::Timeout));
            }
            tokio::time::sleep(Duration::from_millis(self.config.poll_interval_ms)).await;
        }
    }
    async fn deletion_confirmed(
        &self,
        s: &SandboxIdentity,
        operation: &str,
        path: &str,
    ) -> Result<bool, BackendError> {
        let v = self
            .api
            .request(Method::GET, &format!("/deletion-operations/{operation}"))
            .json()
            .await?;
        let op = &v["operation"];
        let matches_target = op["kind"] == "sandbox" && op["targetId"] == s.backend_id;
        if op["id"] != operation || !matches_target {
            return Err(unknown());
        }
        let gone = match self.get(&s.backend_id).await {
            Err(BackendError::NotFound) => true,
            Ok(_) => false,
            Err(e) => return Err(e),
        };
        let snapshots_gone = match self.api.request(Method::GET, path).json().await {
            Ok(v) => v["snapshots"].as_array().is_some_and(Vec::is_empty),
            Err(BackendError::NotFound) => true,
            Err(e) => return Err(e),
        };
        Ok(op["status"] == "completed" && gone && snapshots_gone)
    }
}
