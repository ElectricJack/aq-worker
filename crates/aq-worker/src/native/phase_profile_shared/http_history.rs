macro_rules! http_history {
    ($wire:path,$history:ident) => {
        // History projections are plain data, never an ExecutionObservation or permit.
        use super::*;
        use $wire as wire;
        impl NativeClient {
            /// A known-key lookup remains valid after delivery loss or expiry. Snapshot
            /// absence is never NeverGranted; repeated reads may advance None to Some.
            /// No retry, delivery clock, journal adoption or local liveness is inferred.
            pub async fn $history(&self, q: &wire::HistoryRequest) -> Result<wire::HistoryReceipt> {
                q.validate()?;
                self.scope(&q.binding)?;
                let bytes = serde_json::to_vec(q)?;
                ensure!(
                    bytes.len() <= wire::MAX_BYTES,
                    "phase history request bound"
                );
                // Reads share ordinary control capacity and never borrow Stopped reserve.
                let _slot = self.control_slots.try_acquire()?;
                let response = self
                    .control
                    .post(format!("{}/v1/worker/profiled-v2/history", self.base))
                    .header(header::AUTHORIZATION, self.credential.clone())
                    .header(header::CONTENT_TYPE, "application/json")
                    .header("aq-phase-terminal-api-version", "1")
                    .body(bytes)
                    .send()
                    .await
                    .map_err(|_| anyhow::anyhow!("phase history transport failed"))?;
                if !response.status().is_success() {
                    return Err(refused(response).await);
                }
                let mut versions = response
                    .headers()
                    .get_all("aq-phase-terminal-api-version")
                    .iter();
                ensure!(
                    versions.next().is_some_and(|v| v == "1")
                        && versions.next().is_none()
                        && response
                            .headers()
                            .get(header::CONTENT_TYPE)
                            .is_some_and(|v| v == "application/json"),
                    "phase history response version/type"
                );
                Ok(wire::decode_history_receipt(
                    &body(response, wire::MAX_BYTES).await?,
                    q,
                )?)
            }
        }
    };
}
pub(crate) use http_history;
