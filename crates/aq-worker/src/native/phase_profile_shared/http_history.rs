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
                // Reads share ordinary control capacity and never borrow Stopped reserve.
                let _slot = self.control_slots.try_acquire()?;
                let response = self.phase_terminal_post("history", q, false).await?;
                Ok(wire::decode_history_receipt(
                    &body(response, wire::MAX_BYTES).await?,
                    q,
                )?)
            }
        }
    };
}
pub(crate) use http_history;
