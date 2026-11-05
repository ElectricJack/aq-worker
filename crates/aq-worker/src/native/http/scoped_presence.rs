//! Exact scoped-fact routes on existing control capacity, never execution replies.
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
        let bytes = serde_json::to_vec(&request)?;
        ensure!(
            bytes.len() <= wire::MAX_REQUEST_BYTES,
            "inventory request bound"
        );
        let response = self
            .control
            .post(format!("{}/v1/worker/profiled/inventory-v2", self.base))
            .header(header::AUTHORIZATION, self.credential.clone())
            .header(header::CONTENT_TYPE, "application/json")
            .body(bytes)
            .send()
            .await
            .map_err(|_| anyhow::anyhow!("inventory publication failed; disposition unknown"))?;
        if !response.status().is_success() {
            {
                let status = response.status();
                let path = response.url().path().to_owned();
                let body: String = response
                    .text()
                    .await
                    .unwrap_or_default()
                    .chars()
                    .take(400)
                    .collect();
                tracing::warn!(%path, %status, %body, "native endpoint refused");
                return Err(HttpStatus(status).into());
            }
        }
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
        let bytes = serde_json::to_vec(&request)?;
        ensure!(
            bytes.len() <= wire::MAX_REQUEST_BYTES,
            "presence request bound"
        );
        let response = self
            .control
            .post(format!(
                "{}/v1/worker/profiled/material-presence",
                self.base
            ))
            .header(header::AUTHORIZATION, self.credential.clone())
            .header(header::CONTENT_TYPE, "application/json")
            .body(bytes)
            .send()
            .await
            .map_err(|_| anyhow::anyhow!("presence publication failed; disposition unknown"))?;
        if !response.status().is_success() {
            {
                let status = response.status();
                let path = response.url().path().to_owned();
                let body: String = response
                    .text()
                    .await
                    .unwrap_or_default()
                    .chars()
                    .take(400)
                    .collect();
                tracing::warn!(%path, %status, %body, "native endpoint refused");
                return Err(HttpStatus(status).into());
            }
        }
        let bytes = body(response, wire::MAX_RECEIPT_BYTES).await?;
        let receipt: wire::PresenceReceipt = serde_json::from_slice(&bytes)
            .map_err(|_| anyhow::anyhow!("invalid presence receipt"))?;
        operation.finish(receipt)
    }
}
