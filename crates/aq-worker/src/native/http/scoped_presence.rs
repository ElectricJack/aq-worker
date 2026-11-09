//! Exact scoped-fact routes on existing control capacity, never execution replies (sent by
//! `post`, never retried: an unknown disposition stays the caller's).
use super::*;
use crate::native::preparation::{AcknowledgedInventory, InventoryOperation, PresenceOperation};
use aq_admission::scoped_presence as wire;
impl NativeClient {
    pub(crate) async fn scoped_inventory(
        &self,
        operation: &InventoryOperation,
    ) -> Result<AcknowledgedInventory> {
        let _slot = self.control_slots.try_acquire()?;
        let (request, _flight) = operation.begin_send(boot_ns()?)?;
        request.validate()?;
        ensure!(
            request.identity.node_id == self.node,
            "inventory node scope"
        );
        // #121 answers lock timeouts with 503; a report replays by operation id: resent for 10 s.
        let bound = boot_ns()? + request.ttl_ms.min(10_000) * 1_000_000;
        let (path, limit) = ("inventory-v2", wire::MAX_REQUEST_BYTES);
        let response =
            fetch_retried(bound, || self.post(&self.control, path, &request, limit)).await?;
        let bytes = body(response, wire::MAX_RECEIPT_BYTES).await?;
        let received = boot_ns()?;
        let receipt: wire::InventoryReceipt = serde_json::from_slice(&bytes)
            .map_err(|_| anyhow::anyhow!("invalid inventory receipt"))?;
        operation.finish(receipt, received)
    }
    pub(crate) async fn scoped_presence(
        &self,
        operation: &PresenceOperation,
    ) -> Result<wire::PresenceReceipt> {
        let _slot = self.control_slots.try_acquire()?;
        let (request, _flight) = operation.begin_send(boot_ns()?)?;
        request.validate()?;
        self.scope(&request.binding)?;
        let path = "material-presence";
        let response = (self.post(&self.control, path, &request, wire::MAX_REQUEST_BYTES)).await?;
        let bytes = body(response, wire::MAX_RECEIPT_BYTES).await?;
        let receipt: wire::PresenceReceipt = serde_json::from_slice(&bytes)
            .map_err(|_| anyhow::anyhow!("invalid presence receipt"))?;
        operation.finish(receipt)
    }
}
