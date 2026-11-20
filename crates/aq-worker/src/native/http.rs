//! Existing native machine routes, consumed through the frozen public contract.
//! No redirects, legacy execution decoder or runnable grant inference; only read-only
//! preparation fetches (artifacts, materials, source) retry transient failures.
//! Durable request ownership and recovery remain the caller's responsibility.
use crate::config::WorkerConfig;
use anyhow::{Result, ensure};
use aq_admission::{
    ArtifactRef, LaunchBinding, MaterialLease, RetainedArtifact, SessionIdentity, result_record,
    started, stopped,
    transport::{
        self, ArtifactFetch, MaterialDelivery, MaterialFetch, Register, TransportDocument,
    },
};
use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use reqwest::{Response, header};
use serde::{Serialize, de::DeserializeOwned};
use std::{sync::Arc, time::Duration};
use tokio::sync::Semaphore;
use uuid::Uuid;
use zeroize::Zeroizing;
pub mod control;
mod phase_history;
pub mod phase_terminal;
pub mod preparation_refresh_v1;
pub mod preparation_v2;
mod refresh_history;
mod refresh_terminal;
mod scoped_presence;
pub mod source;
#[cfg(test)]
pub(crate) mod test_clock;

#[derive(Clone)]
pub struct NativeClient {
    control: reqwest::Client,
    bulk: reqwest::Client,
    renewal: reqwest::Client,
    base: String,
    node: Uuid,
    credential: header::HeaderValue,
    control_slots: Arc<Semaphore>,
    bulk_slots: Arc<Semaphore>,
    renewal_slots: Arc<Semaphore>,
    phase_terminal_safety: reqwest::Client,
    phase_terminal_safety_slots: Arc<Semaphore>,
    /// Boot-clock deadline before which fetch retries must end (`fetch_until`).
    fetch_deadline_ns: u64,
    node_slot: Option<(u32, u32)>,
    checkpoint_author: bool,
}
pub use aq_node_service::native_transport::{
    FETCH_BACKOFF, FETCH_SPAN, FETCH_TRIES, HttpStatus, TransportFailed, fetch_retry_wait,
};
pub(crate) use aq_node_service::native_transport::{refused, retry_same};
pub async fn fetch_retried<T, F>(deadline_ns: u64, call: impl FnMut() -> F) -> Result<T>
where
    F: Future<Output = Result<T>>,
{
    aq_node_service::native_transport::fetch_retried(deadline_ns, call, boot_ns).await
}

#[cfg(feature = "test-support")]
pub use fetch_retried as fetch_retried_for_test;
/// No Debug/Serialize: private body bytes must not become logs or journal JSON.
pub struct PrivateMaterial {
    pub(super) delivery: MaterialDelivery,
    pub(super) retrieval_deadline_ns: u64,
    pub(super) operation_id: Uuid,
    pub(super) sent_ns: u64,
    pub(super) received_ns: u64,
    /// C5b: the stored version of a subscription login (`aq-material-version`).
    pub(super) login_version: Option<i64>,
    pub(super) bytes: Zeroizing<Vec<u8>>,
}
impl PrivateMaterial {
    pub fn delivery(&self) -> &MaterialDelivery {
        &self.delivery
    }
    pub fn retrieval_deadline_ns(&self) -> u64 {
        self.retrieval_deadline_ns
    }
    pub fn bytes(&self) -> &[u8] {
        &self.bytes
    }
}
pub struct ArtifactChunk {
    pub descriptor: RetainedArtifact,
    pub offset: u64,
    pub bytes: Vec<u8>,
}
fn secret(value: &str) -> Result<header::HeaderValue> {
    let mut value = header::HeaderValue::from_str(value)
        .map_err(|_| anyhow::anyhow!("invalid protected transport header"))?;
    value.set_sensitive(true);
    Ok(value)
}
pub(crate) fn metadata<T: DeserializeOwned>(response: &Response, name: &str) -> Result<T> {
    let mut values = response.headers().get_all(name).iter();
    let value = values
        .next()
        .ok_or_else(|| anyhow::anyhow!("missing native metadata"))?;
    ensure!(
        values.next().is_none() && value.len() <= 4096,
        "native metadata bound"
    );
    let bytes = URL_SAFE_NO_PAD
        .decode(value.as_bytes())
        .map_err(|_| anyhow::anyhow!("invalid native metadata encoding"))?;
    serde_json::from_slice(&bytes).map_err(|_| anyhow::anyhow!("invalid native metadata shape"))
}
pub(crate) fn body_type(response: &Response) -> Result<()> {
    ensure!(
        response
            .headers()
            .get(header::CONTENT_TYPE)
            .is_some_and(|v| v == "application/octet-stream")
            && response
                .headers()
                .get(header::CACHE_CONTROL)
                .is_some_and(|v| v == "no-store"),
        "native private body headers"
    );
    Ok(())
}
pub(super) fn boot_ns() -> Result<u64> {
    #[cfg(test)]
    if let Some(now) = test_clock::now() {
        return Ok(now);
    }
    aq_node_service::native_transport::boot_ns()
}

pub(crate) use aq_node_service::native_transport::body;

impl NativeClient {
    pub fn from_config(config: &WorkerConfig) -> Result<Self> {
        let url = reqwest::Url::parse(&config.server_url)
            .map_err(|_| anyhow::anyhow!("invalid native coordinator URL"))?;
        // Resolve no names to grant the plaintext exception. A hostname may
        // change addresses; only an explicitly numeric loopback is local.
        let numeric_loopback = url.host_str().is_some_and(|host| {
            host.trim_start_matches('[')
                .trim_end_matches(']')
                .parse::<std::net::IpAddr>()
                .is_ok_and(|address| address.is_loopback())
        });
        ensure!(
            (url.scheme() == "https" || (url.scheme() == "http" && numeric_loopback))
                && url.host_str().is_some()
                && url.username().is_empty()
                && url.password().is_none()
                && url.query().is_none()
                && url.fragment().is_none()
                && !config.node_id.is_nil()
                && !config.credential.is_empty()
                && config.cf_client_id.is_some() == config.cf_client_secret.is_some(),
            "invalid native transport configuration"
        );
        let credential = secret(&format!("Bearer {}", config.credential))?;
        let mut headers = header::HeaderMap::new();
        if let (Some(id), Some(key)) = (&config.cf_client_id, &config.cf_client_secret) {
            headers.insert("cf-access-client-id", secret(id)?);
            headers.insert("cf-access-client-secret", secret(key)?);
        }
        let client = || {
            reqwest::Client::builder()
                .redirect(reqwest::redirect::Policy::none())
                .retry(reqwest::retry::never())
                .no_proxy()
                .connect_timeout(Duration::from_secs(3))
                .timeout(Duration::from_secs(10))
                .default_headers(headers.clone())
                .build()
        };
        Ok(Self {
            control: client()?,
            bulk: client()?,
            renewal: client()?,
            base: config.server_url.trim_end_matches('/').into(),
            node: config.node_id,
            credential,
            control_slots: Arc::new(Semaphore::new(2)),
            bulk_slots: Arc::new(Semaphore::new(2)),
            renewal_slots: Arc::new(Semaphore::new(1)),
            phase_terminal_safety: client()?,
            phase_terminal_safety_slots: Arc::new(Semaphore::new(1)),
            fetch_deadline_ns: u64::MAX,
            node_slot: None,
            checkpoint_author: false,
        })
    }
    /// This client, on the same lanes, with fetch retries that end before `deadline_ns`: the
    /// boot-clock preparation or retrieval deadline of the inputs it reads.
    pub fn for_node_slot(mut self, slot: u32, registered_slots: u32) -> Self {
        self.node_slot = Some((slot, registered_slots));
        self
    }
    pub fn with_git_author_support(mut self, supported: bool) -> Self {
        self.checkpoint_author = supported;
        self
    }
    fn slot_headers(
        &self,
        request: reqwest::RequestBuilder,
        path: &str,
    ) -> reqwest::RequestBuilder {
        match (self.node_slot, path) {
            (Some((slot, _)), "poll") => {
                request.header(aq_admission::node_slots::SLOT_HEADER, slot)
            }
            (Some((_, slots)), "register") => {
                request.header(aq_admission::node_slots::SLOTS_HEADER, slots)
            }
            _ => request,
        }
    }
    pub fn fetch_until(&self, deadline_ns: u64) -> Self {
        let mut client = self.clone();
        client.fetch_deadline_ns = deadline_ns;
        client
    }
    fn scope(&self, binding: &LaunchBinding) -> Result<()> {
        ensure!(
            matches!(binding.session, SessionIdentity::Remote { node_id, .. } if node_id == self.node),
            "native node scope"
        );
        Ok(())
    }
    async fn post(
        &self,
        client: &reqwest::Client,
        path: &str,
        request: &impl Serialize,
        limit: usize,
    ) -> Result<Response> {
        self.post_with(client, path, request, limit, vec![]).await
    }
    async fn post_with(
        &self,
        client: &reqwest::Client,
        path: &str,
        request: &impl Serialize,
        limit: usize,
        headers: Vec<(&'static str, String)>,
    ) -> Result<Response> {
        let bytes = serde_json::to_vec(request)?;
        ensure!(bytes.len() <= limit, "native request size");
        let mut request = client
            .post(format!("{}/v1/worker/profiled/{path}", self.base))
            .header(header::AUTHORIZATION, self.credential.clone())
            .header(header::CONTENT_TYPE, "application/json")
            .body(bytes);
        for (name, value) in headers {
            request = request.header(name, value);
        }
        let response =
            self.slot_headers(request, path).send().await.map_err(|_| {
                TransportFailed("native request transport failed; disposition unknown")
            })?;
        if !response.status().is_success() {
            return Err(refused(response).await);
        }
        Ok(response)
    }
    pub async fn register(&self, request: &Register) -> Result<Register> {
        let document = TransportDocument::Register(request.clone());
        document.validate()?;
        ensure!(request.identity.node_id == self.node, "native node scope");
        let _slot = self.control_slots.try_acquire()?;
        // #121 answers lock timeouts with 503; the server replays a registration by operation id.
        let send = || self.post(&self.control, "register", &document, 65_536);
        let response = fetch_retried(boot_ns()? + FETCH_SPAN.as_nanos() as u64, send).await?;
        let response = transport::decode_transport(&body(response, 65_536).await?)?;
        ensure!(
            serde_json::to_value(&response)? == serde_json::to_value(&document)?,
            "native registration correlation"
        );
        let TransportDocument::Register(registered) = response else {
            unreachable!()
        };
        Ok(registered)
    }
    pub async fn started(&self, request: &started::Request) -> Result<started::Receipt> {
        request.validate()?;
        self.scope(&request.binding)?;
        let _slot = self.control_slots.try_acquire()?;
        let response = self
            .post(&self.control, "started", request, started::MAX_BYTES)
            .await?;
        Ok(started::decode_receipt(
            &body(response, started::MAX_BYTES).await?,
            request,
        )?)
    }
    pub async fn stopped(&self, request: &stopped::Request) -> Result<stopped::Receipt> {
        request.validate()?;
        self.scope(&request.evidence.binding)?;
        let _slot = self.control_slots.try_acquire()?;
        let response = self
            .post(&self.control, "stopped", request, stopped::MAX_BYTES)
            .await?;
        Ok(stopped::decode_receipt(
            &body(response, stopped::MAX_BYTES).await?,
            request,
        )?)
    }
    pub async fn report(
        &self,
        request: &transport::ResultReport,
        expected_outcome: &ArtifactRef,
        stopped_receipt: &ArtifactRef,
        headers: Vec<(&'static str, String)>,
    ) -> Result<result_record::Receipt> {
        result_record::validate_report(request)?;
        self.scope(&request.binding)?;
        let _slot = self.bulk_slots.try_acquire()?;
        let (limit, client) = (result_record::MAX_BYTES, &self.bulk);
        let response = self
            .post_with(client, "results", request, limit, headers)
            .await?;
        let receipt = result_record::decode_receipt(
            &body(response, result_record::MAX_BYTES).await?,
            request,
        )?;
        ensure!(
            receipt.expected_outcome == *expected_outcome
                && receipt.stopped_receipt == *stopped_receipt,
            "result receipt owner pins changed"
        );
        Ok(receipt)
    }
    pub async fn artifact(
        &self,
        request: &ArtifactFetch,
        expected: &RetainedArtifact,
    ) -> Result<ArtifactChunk> {
        TransportDocument::ArtifactFetch(request.clone()).validate()?;
        self.scope(&request.binding)?;
        expected.validate()?;
        ensure!(
            expected.artifact == request.artifact,
            "expected artifact scope"
        );
        let _slot = self.bulk_slots.try_acquire()?;
        let response = fetch_retried(self.fetch_deadline_ns, || {
            self.post(&self.bulk, "artifact", request, 16_384)
        })
        .await?;
        body_type(&response)?;
        let descriptor: RetainedArtifact = metadata(&response, "aq-artifact-retention")?;
        descriptor.validate()?;
        ensure!(
            serde_json::to_value(&descriptor)? == serde_json::to_value(expected)?,
            "artifact retention descriptor changed"
        );
        let offsets: Vec<_> = response
            .headers()
            .get_all("aq-artifact-offset")
            .iter()
            .collect();
        ensure!(
            offsets.len() == 1
                && offsets[0].to_str().ok().and_then(|v| v.parse::<u64>().ok())
                    == Some(request.offset)
                && descriptor.artifact == request.artifact,
            "artifact response correlation"
        );
        let bytes = body(response, request.length as usize).await?;
        ensure!(
            !bytes.is_empty()
                && request
                    .offset
                    .checked_add(bytes.len() as u64)
                    .is_some_and(|n| n <= descriptor.bytes),
            "artifact response range"
        );
        Ok(ArtifactChunk {
            descriptor,
            offset: request.offset,
            bytes: bytes.to_vec(),
        })
    }
    pub async fn material(
        &self,
        request: &MaterialFetch,
        expected: &MaterialLease,
    ) -> Result<PrivateMaterial> {
        TransportDocument::MaterialFetch(request.clone()).validate()?;
        self.scope(&request.binding)?;
        expected.validate()?;
        ensure!(
            expected.lease_id == request.lease_id && expected.binding == request.binding,
            "expected material scope"
        );
        let _slot = self.bulk_slots.try_acquire()?;
        // The material route only reads the admitted lease (no server write), so an overloaded
        // read is retried with the same request; each try is timed from its own send.
        let (sent, response) = fetch_retried(self.fetch_deadline_ns, || async move {
            Ok((
                boot_ns()?,
                self.post(&self.bulk, "material", request, 16_384).await?,
            ))
        })
        .await?;
        body_type(&response)?;
        let delivery: MaterialDelivery = metadata(&response, "aq-material-delivery")?;
        ensure!(
            serde_json::to_value(&delivery.lease)? == serde_json::to_value(expected)?,
            "material lease changed"
        );
        let version = response.headers().get("aq-material-version");
        let login_version = version.and_then(|v| v.to_str().ok()?.parse().ok());
        let bytes = body(response, expected.material_bytes as usize).await?;
        let received_ns = boot_ns()?;
        let deadline = delivery.verify_received(&request.binding, &bytes, sent, received_ns)?;
        Ok(PrivateMaterial {
            delivery,
            retrieval_deadline_ns: deadline,
            operation_id: request.operation_id,
            sent_ns: sent,
            received_ns,
            login_version,
            bytes,
        })
    }
}

#[cfg(test)]
mod refresh_terminal_tests;

#[cfg(test)]
mod node_slot_tests {
    use super::*;
    #[test]
    fn headers_are_per_slot_only_on_poll_and_registration_and_never_on_legacy() {
        let config = WorkerConfig {
            server_url: "https://aq.example".into(),
            node_id: Uuid::new_v4(),
            credential: "synthetic".into(),
            cf_client_id: None,
            cf_client_secret: None,
        };
        let legacy = NativeClient::from_config(&config).unwrap();
        for slot in [1, 2, 1024] {
            let node = legacy.clone().for_node_slot(slot, 3);
            for path in ["poll", "register", "started", "stopped"] {
                let request = node
                    .slot_headers(node.control.post("https://aq.example"), path)
                    .build()
                    .unwrap();
                let expected = match path {
                    "poll" => Some(("aq-node-slot", slot)),
                    "register" => Some(("aq-node-slots", 3)),
                    _ => None,
                };
                if let Some((name, value)) = expected {
                    assert_eq!(request.headers()[name], value.to_string());
                } else {
                    assert!(request.headers().is_empty());
                }
                assert!(
                    legacy
                        .slot_headers(legacy.control.post("https://aq.example"), path)
                        .build()
                        .unwrap()
                        .headers()
                        .is_empty()
                );
            }
        }
    }
}
