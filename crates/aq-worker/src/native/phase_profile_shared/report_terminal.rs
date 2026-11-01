macro_rules! report_terminal {
    ($wire:path,$history_type:ty,$history:ident,$terminal:ident,$report_started:ident,$report_stopped:ident,$http_started:ident,$http_stopped:ident,$ack_started:ident,$ack_stopped:ident) => {
        use $wire as wire;
        impl InputPipeline {
            /// Durable facts/readback only; valid after restart, never a live host permit.
            pub async fn $history(&self, id: Uuid) -> Result<Option<$history_type>> {
                let state = self.snapshot().await?;
                Ok(state
                    .attempts
                    .get(&id)
                    .and_then(|a| a.native_host.as_ref())
                    .and_then(|h| h.$terminal.clone()))
            }
            pub async fn $report_started(&self, id: Uuid) -> Result<wire::StartedReceipt> {
                let h = self
                    .$history(id)
                    .await?
                    .context("phase Started outbox absent")?;
                if let Some(receipt) = h.started.receipt {
                    return Ok(receipt);
                }
                let receipt = self.client.$http_started(&h.started.request).await?;
                let saved = receipt.clone();
                self.storage(move |owner| owner.journal.$ack_started(id, saved))
                    .await?;
                Ok(receipt)
            }
            pub async fn $report_stopped(&self, id: Uuid) -> Result<wire::StoppedReceipt> {
                let h = self
                    .$history(id)
                    .await?
                    .and_then(|h| h.stopped)
                    .context("phase Stopped outbox absent")?;
                if let Some(receipt) = h.receipt {
                    return Ok(receipt);
                }
                let receipt = self.client.$http_stopped(&h.request).await?;
                let saved = receipt.clone();
                self.storage(move |owner| owner.journal.$ack_stopped(id, saved))
                    .await?;
                Ok(receipt)
            }
        }
    };
}
pub(crate) use report_terminal;
