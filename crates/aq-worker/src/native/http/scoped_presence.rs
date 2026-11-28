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
        let response = fetch_retried(bound, || {
            send_json(self.profiled_post(&self.control, path), &request, limit)
        })
        .await?;
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
        let response = (send_json(
            self.profiled_post(&self.control, path),
            &request,
            wire::MAX_REQUEST_BYTES,
        ))
        .await?;
        let bytes = body(response, wire::MAX_RECEIPT_BYTES).await?;
        let receipt: wire::PresenceReceipt = serde_json::from_slice(&bytes)
            .map_err(|_| anyhow::anyhow!("invalid presence receipt"))?;
        operation.finish(receipt)
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    use crate::backend::TemplateDigest;
    fn client() -> NativeClient {
        NativeClient::from_config(&WorkerConfig {
            server_url: "http://127.0.0.1:9".into(),
            node_id: Uuid::from_u128(1),
            credential: "synthetic-node-secret".into(),
            cf_client_id: None,
            cf_client_secret: None,
        })
        .unwrap()
    }
    fn template(client: &NativeClient, path: &str) -> Option<header::HeaderValue> {
        let request = client.profiled_post(&client.control, path).build().unwrap();
        request.headers().get(SANDBOX_TEMPLATE_HEADER).cloned()
    }
    #[test]
    fn each_inventory_names_the_resolved_launch_template_and_no_other_request_does() {
        let digest = TemplateDigest::parse(format!("sha256:{}", "c".repeat(64))).unwrap();
        let reporting = client().with_sandbox_template(Some(&digest));
        assert_eq!(
            template(&reporting, "inventory-v2").unwrap(),
            digest.as_str()
        );
        for path in ["register", "poll", "material-presence", "accept"] {
            assert_eq!(template(&reporting, path), None, "{path}");
        }
        // A node whose template is not present on its daemon reports none.
        let unresolved = client().with_sandbox_template(None);
        assert_eq!(template(&unresolved, "inventory-v2"), None);
    }
}
