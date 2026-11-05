//! CON-03 central artifact retention over aq-server `/v1/execution/artifacts`.
//! The server derives the attempt [`Binding`] and signs nothing the worker
//! must trust blindly: every receipt is checked against the caller's binding
//! with [`validate_retention`] before it can authorize destroying evidence.
//! Operation IDs are derived from (attempt, artifact, role, step) so a retry
//! after an uncertain exchange replays idempotently instead of re-uploading.
use crate::{config::WorkerConfig, transfer_client::TransferClient};
use anyhow::{Result, ensure};
use aq_lifecycle_protocol::{
    ArtifactRef, ArtifactRole, Binding, Digest, RetentionReceipt, Text, Validate,
    validate_retention,
};
use reqwest::Method;
use serde::{Deserialize, Serialize};
use sha2::{Digest as _, Sha256};
use std::future::Future;
use uuid::Uuid;

/// Server-side maximum `PUT .../data` body.
pub const CHUNK_BYTES: usize = 256 * 1024;
const PREFIX: &str = "v1/execution/artifacts";
const JSON_LIMIT: usize = 64 * 1024;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BeginArtifact {
    pub operation_id: Uuid,
    pub attempt_id: Uuid,
    pub artifact: ArtifactRef,
    pub role: ArtifactRole,
    pub bytes: u64,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ArtifactTransferStatus {
    pub transfer_id: Uuid,
    pub offset: u64,
    pub bytes: u64,
    pub state: String,
    pub receipt: Option<RetentionReceipt>,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CommitArtifact {
    pub operation_id: Uuid,
}

/// The three mutating artifact routes. Implemented over HTTP by
/// [`ArtifactClient`]; tests substitute an in-memory server.
pub trait ArtifactRoutes {
    fn begin(
        &self,
        request: &BeginArtifact,
    ) -> impl Future<Output = Result<ArtifactTransferStatus>> + Send;
    fn chunk(
        &self,
        transfer: Uuid,
        operation_id: Uuid,
        offset: u64,
        bytes: &[u8],
    ) -> impl Future<Output = Result<ArtifactTransferStatus>> + Send;
    fn commit(
        &self,
        transfer: Uuid,
        request: &CommitArtifact,
    ) -> impl Future<Output = Result<ArtifactTransferStatus>> + Send;
}

/// Node-credentialed HTTP client sharing the source transport's connection
/// policy (timeouts, no redirects, status-only errors).
#[derive(Clone)]
pub struct ArtifactClient {
    transport: TransferClient,
}
impl ArtifactClient {
    pub fn from_config(config: &WorkerConfig) -> Result<Self> {
        Ok(Self {
            transport: TransferClient::from_config(config, 1)?,
        })
    }
    async fn status(&self, request: reqwest::RequestBuilder) -> Result<ArtifactTransferStatus> {
        let bytes = TransferClient::bytes(request, JSON_LIMIT).await?;
        serde_json::from_slice(&bytes).map_err(|_| anyhow::anyhow!("invalid artifact response"))
    }
}
impl ArtifactRoutes for ArtifactClient {
    async fn begin(&self, request: &BeginArtifact) -> Result<ArtifactTransferStatus> {
        self.status(
            self.transport
                .at(Method::POST, &format!("{PREFIX}/begin"))
                .json(request),
        )
        .await
    }
    async fn chunk(
        &self,
        transfer: Uuid,
        operation_id: Uuid,
        offset: u64,
        bytes: &[u8],
    ) -> Result<ArtifactTransferStatus> {
        ensure!(bytes.len() <= CHUNK_BYTES, "artifact chunk exceeds bound");
        let digest = Digest::of(bytes);
        self.status(
            self.transport
                .at(Method::PUT, &format!("{PREFIX}/{transfer}/data"))
                .query(&[
                    ("operation_id", operation_id.to_string()),
                    ("offset", offset.to_string()),
                    ("digest", digest.as_str().to_owned()),
                ])
                .body(bytes.to_vec()),
        )
        .await
    }
    async fn commit(
        &self,
        transfer: Uuid,
        request: &CommitArtifact,
    ) -> Result<ArtifactTransferStatus> {
        self.status(
            self.transport
                .at(Method::POST, &format!("{PREFIX}/{transfer}/commit"))
                .json(request),
        )
        .await
    }
}

fn operation_id(
    binding: &Binding,
    artifact: &ArtifactRef,
    role: &ArtifactRole,
    step: &str,
) -> Uuid {
    let role = serde_json::to_string(role).unwrap_or_default();
    let hash = Sha256::digest(format!(
        "aq-worker/artifact/v1/{}/{}/{}/{}/{role}/{step}",
        binding.attempt_id,
        binding.attempt_generation,
        artifact.id.as_str(),
        artifact.digest.as_str(),
    ));
    let mut bytes = [0u8; 16];
    bytes.copy_from_slice(&hash[..16]);
    uuid::Builder::from_custom_bytes(bytes).into_uuid()
}

/// Upload `bytes` as one artifact for `binding`'s attempt and return the
/// central receipt only after it names exactly this artifact and passes
/// [`validate_retention`] against `binding`.
pub async fn retain_artifact<R: ArtifactRoutes>(
    routes: &R,
    binding: &Binding,
    role: ArtifactRole,
    id: &str,
    bytes: &[u8],
) -> Result<RetentionReceipt> {
    binding.validate()?;
    ensure!(!bytes.is_empty(), "empty artifact cannot be retained");
    let total = bytes.len() as u64;
    let artifact = ArtifactRef {
        id: Text::new(id)?,
        version: Text::new("1")?,
        digest: Digest::of(bytes),
    };
    let op = |step: &str| operation_id(binding, &artifact, &role, step);
    let mut status = routes
        .begin(&BeginArtifact {
            operation_id: op("begin"),
            attempt_id: binding.attempt_id,
            artifact: artifact.clone(),
            role: role.clone(),
            bytes: total,
        })
        .await?;
    let transfer = status.transfer_id;
    ensure!(
        status.bytes == total && status.offset <= total,
        "artifact begin association mismatch"
    );
    while status.receipt.is_none() && status.offset < total {
        let start = status.offset as usize;
        let end = (start + CHUNK_BYTES).min(bytes.len());
        status = routes
            .chunk(
                transfer,
                op(&format!("data/{start}")),
                status.offset,
                &bytes[start..end],
            )
            .await?;
        ensure!(
            status.transfer_id == transfer && status.offset == end as u64,
            "artifact chunk progress mismatch"
        );
    }
    if status.receipt.is_none() {
        status = routes
            .commit(
                transfer,
                &CommitArtifact {
                    operation_id: op("commit"),
                },
            )
            .await?;
    }
    ensure!(
        status.transfer_id == transfer && status.state == "retained",
        "artifact not retained"
    );
    let receipt = status
        .receipt
        .ok_or_else(|| anyhow::anyhow!("retained artifact without receipt"))?;
    ensure!(
        receipt.artifact == artifact && receipt.role == role && receipt.bytes == total,
        "retention receipt does not name the uploaded artifact"
    );
    validate_retention(std::slice::from_ref(&receipt), binding)?;
    Ok(receipt)
}
