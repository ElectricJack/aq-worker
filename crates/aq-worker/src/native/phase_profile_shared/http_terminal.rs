macro_rules! http_terminal {
    ($wire:path,$started:ident,$stopped:ident) => {
        use $wire as wire;
        impl NativeClient {
            pub async fn $started(&self, q: &wire::StartedRequest) -> Result<wire::StartedReceipt> {
                q.validate()?;
                self.scope(&q.original.binding)?;
                let _slot = self.control_slots.try_acquire()?;
                let response = self.phase_terminal_post("started", q, false).await?;
                Ok(wire::decode_started_receipt(
                    &body(response, wire::MAX_BYTES).await?,
                    q,
                )?)
            }
            /// Separate single safety-report lane; blocked Started/bulk/renew traffic
            /// cannot consume it. Physical local stop never waits for this network lane.
            pub async fn $stopped(&self, q: &wire::StoppedRequest) -> Result<wire::StoppedReceipt> {
                q.validate()?;
                self.scope(&q.original.evidence.binding)?;
                let _slot = self.phase_terminal_safety_slots.try_acquire()?;
                let response = self.phase_terminal_post("stopped", q, true).await?;
                Ok(wire::decode_stopped_receipt(
                    &body(response, wire::MAX_BYTES).await?,
                    q,
                )?)
            }
        }
    };
}
pub(crate) use http_terminal;
