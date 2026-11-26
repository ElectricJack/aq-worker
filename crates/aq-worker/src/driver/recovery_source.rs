use super::*;
impl Driver<'_> {
    pub(super) fn export_stopped(&mut self, git: &crate::source::GitClient) -> Result<()> {
        let collect: Vec<_> = self
            .runtime
            .journal
            .state()
            .attempts
            .iter()
            .filter(|(_, r)| {
                r.source
                    .as_ref()
                    .is_some_and(|s| s.workspace.is_some() && s.exported.is_none())
                    && r.stop_certainty == StopCertainty::ConfirmedStopped
                    && r.stopped_proof.is_some()
                    && r.result.is_none()
            })
            .map(|(id, _)| *id)
            .collect();
        for id in collect {
            self.runtime.checkpoint_source(id, git)?;
            self.runtime.export_source(id, git)?;
            println!(
                "{}",
                serde_json::json!({"event":"source_pending","attempt_id":id,"recovered":true})
            );
        }
        Ok(())
    }
    pub(super) async fn upload_stopped(&mut self) -> Result<()> {
        let Some(transfer) = &self.transfer else {
            return Ok(());
        };
        let pending: Vec<_> = self
            .runtime
            .journal
            .state()
            .attempts
            .iter()
            .filter(|(_, r)| {
                r.result.is_none()
                    && r.stop_certainty == StopCertainty::ConfirmedStopped
                    && r.source
                        .as_ref()
                        .is_some_and(|s| s.exported.is_some() && s.receipt.is_none())
            })
            .map(|(id, _)| *id)
            .collect();
        for id in pending {
            transfer.upload_source(&mut self.runtime, id).await?;
        }
        Ok(())
    }
}
