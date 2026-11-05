//! `LifecycleApi` over the AQ worker lifecycle routes (aq-server
//! `hibernation.rs`, azure-orbit-61):
//!
//! - `GET /v1/worker/lifecycle/directives?attempt_id=<uuid>` returns the
//!   pending suspend, disk-release and continuation directives for one attempt.
//! - `POST /v1/worker/lifecycle/packets` takes one CON-03 packet (Checkpoint,
//!   ParkingReport, BootstrapAck) and returns its receipt. AQ deduplicates by
//!   `message_id` and body digest, so a resend after a lost receipt replays the
//!   original receipt.
//!
//! Both routes authenticate with the node bearer or a delegated permit for the
//! attempt. No retries, redirects or proxies: a failed call is an error and the
//! journaled caller resends under the same message id.
use anyhow::{Context, Result, anyhow, bail};
use aq_lifecycle_protocol::{ArtifactRef, MAX_EVENT_BYTES, Message, Packet, Receipt, Validate};
use reqwest::{StatusCode, header};
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use std::time::Duration;
use uuid::Uuid;

use super::LifecycleApi;

pub const DIRECTIVES_PATH: &str = "/v1/worker/lifecycle/directives";
pub const PACKETS_PATH: &str = "/v1/worker/lifecycle/packets";
/// Receipts and directives are small; anything larger is refused unread.
const MAX_RESPONSE_BYTES: usize = 4 * MAX_EVENT_BYTES;

/// Pending directives for one attempt, mirroring the server's response shape.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Directives {
    pub suspend: Option<Packet>,
    pub disk_release: Option<ArtifactRef>,
    pub continuation: Option<Packet>,
}
impl Directives {
    /// Every packet is sealed and of the class its slot names.
    fn validate(&self) -> Result<()> {
        if let Some(p) = &self.suspend {
            p.validate()
                .map_err(|e| anyhow!("suspend directive: {e}"))?;
            if !matches!(p.body.message, Message::Suspend(_)) {
                bail!("suspend slot holds a non-suspend packet");
            }
        }
        if let Some(p) = &self.continuation {
            p.validate()
                .map_err(|e| anyhow!("continuation directive: {e}"))?;
            if !matches!(p.body.message, Message::Continuation(_)) {
                bail!("continuation slot holds a non-continuation packet");
            }
        }
        if let Some(a) = &self.disk_release {
            aq_lifecycle_protocol::artifact(a)
                .map_err(|e| anyhow!("disk release directive: {e}"))?;
        }
        Ok(())
    }
}

/// A non-success status from a lifecycle route. The body is not kept: it is
/// never authority and must not reach logs.
#[derive(Debug)]
pub struct LifecycleStatus(pub StatusCode);
impl std::fmt::Display for LifecycleStatus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "lifecycle route returned {}", self.0)
    }
}
impl std::error::Error for LifecycleStatus {}

/// Blocking adapter: owns a current-thread runtime, so call it from a plain
/// thread (or `spawn_blocking`), never from inside an async task.
pub struct HttpLifecycleApi {
    client: reqwest::Client,
    runtime: tokio::runtime::Runtime,
    base: String,
    credential: header::HeaderValue,
}
impl HttpLifecycleApi {
    /// `credential` is the node bearer or a delegated permit token.
    pub fn new(base: &str, credential: &str) -> Result<Self> {
        let mut credential = header::HeaderValue::from_str(&format!("Bearer {credential}"))
            .context("lifecycle credential is not a valid header value")?;
        credential.set_sensitive(true);
        let client = reqwest::Client::builder()
            .no_proxy()
            .redirect(reqwest::redirect::Policy::none())
            .connect_timeout(Duration::from_secs(3))
            .timeout(Duration::from_secs(22))
            .build()?;
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()?;
        Ok(Self {
            client,
            runtime,
            base: base.trim_end_matches('/').to_owned(),
            credential,
        })
    }

    /// Pending directives for `attempt_id`.
    pub fn directives(&self, attempt_id: Uuid) -> Result<Directives> {
        let request = self
            .client
            .get(format!("{}{DIRECTIVES_PATH}", self.base))
            .query(&[("attempt_id", attempt_id.to_string())])
            .header(header::AUTHORIZATION, self.credential.clone());
        let directives: Directives = self.call(request)?;
        directives.validate()?;
        Ok(directives)
    }

    fn call<T: DeserializeOwned>(&self, request: reqwest::RequestBuilder) -> Result<T> {
        self.runtime.block_on(async {
            let mut response = request.send().await?;
            let status = response.status();
            if !status.is_success() {
                return Err(LifecycleStatus(status).into());
            }
            if response
                .content_length()
                .is_some_and(|n| n > MAX_RESPONSE_BYTES as u64)
            {
                bail!("lifecycle response exceeds {MAX_RESPONSE_BYTES} bytes");
            }
            let mut body = Vec::new();
            while let Some(chunk) = response.chunk().await? {
                if body.len() + chunk.len() > MAX_RESPONSE_BYTES {
                    bail!("lifecycle response exceeds {MAX_RESPONSE_BYTES} bytes");
                }
                body.extend_from_slice(&chunk);
            }
            Ok(serde_json::from_slice(&body)?)
        })
    }
}
impl LifecycleApi for HttpLifecycleApi {
    fn send(&mut self, packet: &Packet) -> Result<Receipt> {
        let body = serde_json::to_vec(packet)?;
        if body.len() > MAX_EVENT_BYTES {
            bail!("lifecycle packet exceeds {MAX_EVENT_BYTES} bytes");
        }
        let request = self
            .client
            .post(format!("{}{PACKETS_PATH}", self.base))
            .header(header::AUTHORIZATION, self.credential.clone())
            .header(header::CONTENT_TYPE, "application/json")
            .body(body);
        let receipt: Receipt = self.call(request)?;
        receipt
            .validate()
            .map_err(|e| anyhow!("lifecycle receipt: {e}"))?;
        Ok(receipt)
    }
}
