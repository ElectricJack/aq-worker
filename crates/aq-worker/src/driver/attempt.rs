use super::*;
impl Driver<'_> {
    pub(super) async fn prepare(
        &mut self,
        attempt_id: Uuid,
        source: Option<&SourceBinding>,
    ) -> Result<()> {
        crate::fault::checkpoint(self.runtime.journal.root(), "grant-durable", None)?;
        let observation = self
            .runtime
            .observation(attempt_id, "authorized synthetic fixture".into())?;
        let ack = self.client.observe(&observation).await?;
        self.runtime.acknowledge_observation(attempt_id, &ack)?;
        if source.is_some() {
            if let (Some(transfer), Some(binding)) = (&self.transfer, source)
                && transfer.git.base_available(binding).is_err()
            {
                transfer
                    .download_base(&mut self.runtime, attempt_id)
                    .await?;
            }
            self.runtime
                .prepare_source(attempt_id, self.git.context("source service absent")?)?;
        }
        Ok(())
    }
    pub(super) async fn run_fixture(&mut self, attempt_id: Uuid) -> Result<ResultReport> {
        let mut handle = self.runtime.launch_fixture(attempt_id, self.executable)?;
        let control =
            control::control_fixture(&mut self.runtime, self.client, attempt_id, &mut handle).await;
        if let Err(error) = control {
            // Even failed persistence/transport cannot justify leaving the
            // owned synthetic writer alive. The durable state stays protected.
            let _ = handle.kill();
            while handle.try_wait()?.is_none() {
                retry_delay().await;
            }
            let _ = self.runtime.finish_fixture(attempt_id, handle);
            return Err(error);
        }
        self.runtime.finish_fixture(attempt_id, handle)
    }
    pub(super) async fn collect_source(
        &mut self,
        attempt_id: Uuid,
    ) -> Result<Option<ResultReport>> {
        let git = self.git.context("source service absent")?;
        self.runtime.checkpoint_source(attempt_id, git)?;
        let exported = self.runtime.export_source(attempt_id, git)?;
        println!(
            "{}",
            serde_json::json!({"event":"source_pending","attempt_id":attempt_id,"artifact_id":exported["artifact_handle"],"protected_usage":self.runtime.journal.protected_usage()?,"pending_bytes":self.runtime.journal.state().attempts[&attempt_id].pending_bytes})
        );
        if let Some(transfer) = &self.transfer {
            transfer
                .upload_source(&mut self.runtime, attempt_id)
                .await?;
            let report = self.runtime.journal.state().attempts[&attempt_id]
                .report
                .clone()
                .context("retained source report missing")?;

            Ok(Some(report))
        } else {
            Ok(None)
        }
    }
    pub(super) async fn deliver_report(
        &mut self,
        attempt_id: Uuid,
        report: &ResultReport,
    ) -> Result<()> {
        let report_retry_deadline = self.runtime.clock.now_ms().saturating_add(30_000);
        loop {
            ensure!(
                self.runtime.clock.now_ms() < report_retry_deadline,
                "report remains durable and unacknowledged after retry timeout"
            );
            match submit_report(self.client, &mut self.runtime, report).await {
                Ok(_) if self.options.drop_report_response_once => {
                    self.options.drop_report_response_once = false;
                    continue;
                }
                Ok(Some(result)) => {
                    self.runtime.acknowledge_result(result.clone())?;
                    println!(
                        "{}",
                        serde_json::json!({"event":"result_acknowledged","attempt":report.attempt,"report_id":result.report_id,"disposition":result.disposition})
                    );
                    if result.retention_release {
                        retire_local(&mut self.runtime, attempt_id, self.git)?;
                    }
                    break;
                }
                Ok(None) => break,
                Err(e) if retryable(&e) => retry_delay().await,
                Err(e) => return Err(e),
            }
        }
        Ok(())
    }
}
