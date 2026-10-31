//! Existing native machine routes, consumed through the frozen public contract.
//! No retries, redirects, legacy execution decoder or runnable grant inference.
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
use reqwest::{Response, StatusCode, header};
use serde::{Serialize, de::DeserializeOwned};
use std::{sync::Arc, time::Duration};
use tokio::sync::Semaphore;
use uuid::Uuid;
use zeroize::Zeroizing;
pub mod control;

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
}
#[derive(Debug)]
pub struct HttpStatus(pub StatusCode);
impl std::fmt::Display for HttpStatus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "native endpoint returned {}", self.0)
    }
}
impl std::error::Error for HttpStatus {}

/// No Debug/Serialize: private body bytes must not become logs or journal JSON.
pub struct PrivateMaterial {
    pub(super) delivery: MaterialDelivery,
    pub(super) retrieval_deadline_ns: u64,
    pub(super) operation_id: Uuid,
    pub(super) sent_ns: u64,
    pub(super) received_ns: u64,
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
fn metadata<T: DeserializeOwned>(response: &Response, name: &str) -> Result<T> {
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
fn body_type(response: &Response) -> Result<()> {
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
    let mut time = libc::timespec {
        tv_sec: 0,
        tv_nsec: 0,
    };
    ensure!(
        unsafe { libc::clock_gettime(libc::CLOCK_BOOTTIME, &mut time) } == 0,
        "boot clock unavailable"
    );
    let seconds = u64::try_from(time.tv_sec)?;
    let nanos = u64::try_from(time.tv_nsec)?;
    seconds
        .checked_mul(1_000_000_000)
        .and_then(|n| n.checked_add(nanos))
        .ok_or_else(|| anyhow::anyhow!("boot clock overflow"))
}
async fn body(mut response: Response, limit: usize) -> Result<Zeroizing<Vec<u8>>> {
    ensure!(
        response.content_length().is_none_or(|n| n <= limit as u64),
        "native response size"
    );
    let mut bytes = Zeroizing::new(Vec::new());
    while let Some(chunk) = response
        .chunk()
        .await
        .map_err(|_| anyhow::anyhow!("native response read failed"))?
    {
        ensure!(
            chunk.len() <= limit.saturating_sub(bytes.len()),
            "native response size"
        );
        bytes.extend_from_slice(&chunk);
    }
    Ok(bytes)
}
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
        })
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
        let bytes = serde_json::to_vec(request)?;
        ensure!(bytes.len() <= limit, "native request size");
        let response = client
            .post(format!("{}/v1/worker/profiled/{path}", self.base))
            .header(header::AUTHORIZATION, self.credential.clone())
            .header(header::CONTENT_TYPE, "application/json")
            .body(bytes)
            .send()
            .await
            .map_err(|_| anyhow::anyhow!("native request transport failed; disposition unknown"))?;
        if !response.status().is_success() {
            return Err(HttpStatus(response.status()).into());
        }
        Ok(response)
    }
    pub async fn register(&self, request: &Register) -> Result<Register> {
        let document = TransportDocument::Register(request.clone());
        document.validate()?;
        ensure!(request.identity.node_id == self.node, "native node scope");
        let _slot = self.control_slots.try_acquire()?;
        let response = self
            .post(&self.control, "register", &document, 65_536)
            .await?;
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
    ) -> Result<result_record::Receipt> {
        result_record::validate_report(request)?;
        self.scope(&request.binding)?;
        let _slot = self.bulk_slots.try_acquire()?;
        let response = self
            .post(&self.bulk, "results", request, result_record::MAX_BYTES)
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
        let response = self.post(&self.bulk, "artifact", request, 16_384).await?;
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
        let sent = boot_ns()?;
        let response = self.post(&self.bulk, "material", request, 16_384).await?;
        body_type(&response)?;
        let delivery: MaterialDelivery = metadata(&response, "aq-material-delivery")?;
        ensure!(
            serde_json::to_value(&delivery.lease)? == serde_json::to_value(expected)?,
            "material lease changed"
        );
        let bytes = body(response, expected.material_bytes as usize).await?;
        let received_ns = boot_ns()?;
        let deadline = delivery.verify_received(&request.binding, &bytes, sent, received_ns)?;
        Ok(PrivateMaterial {
            delivery,
            retrieval_deadline_ns: deadline,
            operation_id: request.operation_id,
            sent_ns: sent,
            received_ns,
            bytes,
        })
    }
}
