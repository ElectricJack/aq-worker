//! Finite authenticated source transport. The caller owns durable operation IDs,
//! progress and receipt validation; an HTTP success never releases local source.
//! This client has its own connection pool and bounded bulk lane, independent of
//! execution renewals/cancellation. It performs no automatic retries.
use crate::config::WorkerConfig;
use anyhow::{Result, ensure};
use aq_worker_protocol::*;
use reqwest::{Method, StatusCode, header};
use serde::{Serialize, de::DeserializeOwned};
use sha2::{Digest, Sha256};
use std::{io::Read as _, path::Path, sync::Arc, time::Duration};
use tokio::sync::{Semaphore, SemaphorePermit};
use uuid::Uuid;

const JSON_LIMIT: usize = 64 * 1024;
const PREFIX: &str = "v1/execution/sources";
/// Central retention is confirmed by one bounded exchange (finalize, pushed).
const RETENTION_TIMEOUT: Duration = Duration::from_secs(40);

#[derive(Clone)]
pub struct TransferClient {
    http: reqwest::Client,
    base: String,
    node: Uuid,
    credential: header::HeaderValue,
    /// The same authentication as `Name: value` lines for `git push`, and the
    /// values alone, which never reach a diagnostic.
    git_headers: Arc<[String]>,
    secrets: Arc<[String]>,
    bulk: Arc<Semaphore>,
}

/// Status only: neither remote bodies nor credential-bearing URLs are reflected
/// into diagnostics. A failed mutating exchange can have an uncertain outcome.
#[derive(Debug)]
pub struct TransferHttpError {
    pub status: StatusCode,
    pub retry_after_secs: Option<u64>,
}
impl std::fmt::Display for TransferHttpError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "source transfer endpoint returned {}", self.status)
    }
}
impl std::error::Error for TransferHttpError {}

#[derive(Serialize)]
struct Begin<'a> {
    identity: &'a NodeIdentity,
    operation_id: Uuid,
    manifest: &'a TransferManifest,
}

/// Admission of a checkpoint this node pushed to the attempt's Git remote.
#[derive(Clone, Debug, Serialize)]
pub struct PushedSource {
    pub identity: NodeIdentity,
    pub operation_id: Uuid,
    pub transfer_id: Uuid,
    pub attempt: AttemptRef,
    pub resolved_spec_digest: String,
    pub source: SourceBinding,
    pub checkpoint: ObjectId,
}

impl TransferClient {
    pub fn from_config(config: &WorkerConfig, max_bulk: usize) -> Result<Self> {
        let url = reqwest::Url::parse(&config.server_url)
            .map_err(|_| anyhow::anyhow!("invalid source coordinator URL"))?;
        ensure!(
            matches!(url.scheme(), "http" | "https")
                && url.host_str().is_some()
                && url.username().is_empty()
                && url.password().is_none()
                && url.query().is_none()
                && url.fragment().is_none()
                && !config.node_id.is_nil()
                && !config.credential.is_empty()
                && (1..=8).contains(&max_bulk),
            "invalid source transport configuration"
        );
        ensure!(
            config.cf_client_id.is_some() == config.cf_client_secret.is_some(),
            "incomplete machine authentication configuration"
        );
        let secret_header = |value: &str| -> Result<header::HeaderValue> {
            let mut value = header::HeaderValue::from_str(value)
                .map_err(|_| anyhow::anyhow!("invalid protected transport header"))?;
            value.set_sensitive(true);
            Ok(value)
        };
        let credential = secret_header(&format!("Bearer {}", config.credential))?;
        let mut headers = header::HeaderMap::new();
        let mut git_headers = vec![format!("Authorization: Bearer {}", config.credential)];
        let mut secrets = vec![config.credential.clone()];
        if let (Some(id), Some(secret)) = (&config.cf_client_id, &config.cf_client_secret) {
            headers.insert("cf-access-client-id", secret_header(id)?);
            headers.insert("cf-access-client-secret", secret_header(secret)?);
            git_headers.push(format!("cf-access-client-id: {id}"));
            git_headers.push(format!("cf-access-client-secret: {secret}"));
            secrets.extend([id.clone(), secret.clone()]);
        }
        let http = reqwest::Client::builder()
            .connect_timeout(Duration::from_secs(3))
            .timeout(Duration::from_secs(10))
            .redirect(reqwest::redirect::Policy::none())
            .default_headers(headers)
            .build()?;
        Ok(Self {
            http,
            base: config.server_url.trim_end_matches('/').into(),
            node: config.node_id,
            credential,
            git_headers: git_headers.into(),
            secrets: secrets.into(),
            bulk: Arc::new(Semaphore::new(max_bulk)),
        })
    }

    fn bulk_permit(&self) -> Result<SemaphorePermit<'_>> {
        self.bulk
            .try_acquire()
            .map_err(|_| anyhow::anyhow!("local source transfer capacity exhausted"))
    }

    fn identity(&self, identity: &NodeIdentity) -> Result<()> {
        identity.validate()?;
        ensure!(
            identity.node_id == self.node,
            "source node identity mismatch"
        );
        Ok(())
    }

    fn request(&self, method: Method, suffix: &str) -> reqwest::RequestBuilder {
        self.at(method, &format!("{PREFIX}/{suffix}"))
    }

    /// Node-authenticated request to `path` on the coordinator (shared with
    /// [`crate::artifact_client::ArtifactClient`]).
    pub(crate) fn at(&self, method: Method, path: &str) -> reqwest::RequestBuilder {
        self.http
            .request(method, format!("{}/{path}", self.base))
            .header(header::AUTHORIZATION, self.credential.clone())
    }

    pub(crate) async fn bytes(request: reqwest::RequestBuilder, limit: usize) -> Result<Vec<u8>> {
        let mut response = request
            .send()
            .await
            .map_err(|_| anyhow::anyhow!("source transport exchange unconfirmed"))?;
        if !response.status().is_success() {
            return Err(TransferHttpError {
                status: response.status(),
                retry_after_secs: response
                    .headers()
                    .get(header::RETRY_AFTER)
                    .and_then(|s| s.to_str().ok())
                    .and_then(|s| s.parse::<u64>().ok())
                    .filter(|s| *s <= 3600),
            }
            .into());
        }
        ensure!(
            response.content_length().is_none_or(|n| n <= limit as u64),
            "source response exceeds bound"
        );
        let mut bytes = Vec::new();
        while let Some(chunk) = response
            .chunk()
            .await
            .map_err(|_| anyhow::anyhow!("source response body unconfirmed"))?
        {
            ensure!(
                chunk.len() <= limit - bytes.len(),
                "source response exceeds bound"
            );
            bytes.extend_from_slice(&chunk);
        }
        Ok(bytes)
    }

    async fn json<T: DeserializeOwned + Validate>(request: reqwest::RequestBuilder) -> Result<T> {
        let bytes = Self::bytes(request, JSON_LIMIT).await?;
        let result: T = serde_json::from_slice(&bytes)
            .map_err(|_| anyhow::anyhow!("invalid source response"))?;
        result.validate()?;
        Ok(result)
    }

    pub async fn begin(
        &self,
        identity: &NodeIdentity,
        operation_id: Uuid,
        manifest: &TransferManifest,
    ) -> Result<TransferStatus> {
        self.identity(identity)?;
        manifest.validate()?;
        ensure!(!operation_id.is_nil(), "nil source operation ID");
        let _permit = self.bulk_permit()?;
        let response: TransferStatus =
            Self::json(self.request(Method::POST, "begin").json(&Begin {
                identity,
                operation_id,
                manifest,
            }))
            .await?;
        ensure!(
            response.transfer_id == manifest.transfer_id
                && response.manifest_offset <= manifest.manifest_bytes
                && response.bundle_offset <= manifest.bundle_bytes,
            "source begin response association mismatch"
        );
        Ok(response)
    }

    /// Recovery/status has no bulk semaphore and cannot consume its admission
    /// slots. The durable owner still limits outstanding status operations.
    pub async fn status(&self, request: &TransferStatusRequest) -> Result<TransferStatus> {
        self.identity(&request.identity)?;
        request.validate()?;
        let response: TransferStatus = Self::json(
            self.request(Method::POST, "status")
                .timeout(Duration::from_secs(5))
                .json(request),
        )
        .await?;
        ensure!(
            response.transfer_id == request.transfer_id,
            "source status association mismatch"
        );
        Ok(response)
    }

    pub async fn finalize(&self, request: &TransferStatusRequest) -> Result<TransferStatus> {
        self.identity(&request.identity)?;
        request.validate()?;
        let _permit = self.bulk_permit()?;
        let response: TransferStatus = Self::json(
            self.request(Method::POST, "finalize")
                .timeout(RETENTION_TIMEOUT)
                .json(request),
        )
        .await?;
        ensure!(
            response.transfer_id == request.transfer_id && response.receipt.is_some(),
            "source finalize retention remains unconfirmed"
        );
        Ok(response)
    }

    /// Asks the coordinator to retain the pushed checkpoint. Idempotent on the
    /// transfer ID, so the original request is resent after a lost response.
    /// It takes no bulk permit: a finishing result is never refused locally.
    pub async fn pushed(&self, request: &PushedSource) -> Result<TransferStatus> {
        self.identity(&request.identity)?;
        ensure!(
            !request.operation_id.is_nil() && !request.transfer_id.is_nil(),
            "nil pushed source operation ID"
        );
        let response: TransferStatus = Self::json(
            self.request(Method::POST, "pushed")
                .timeout(RETENTION_TIMEOUT)
                .json(request),
        )
        .await?;
        ensure!(
            response.transfer_id == request.transfer_id && response.receipt.is_some(),
            "pushed source retention remains unconfirmed"
        );
        Ok(response)
    }

    /// Blocking `git push` of `checkpoint` from the bare repository `pool` to
    /// the attempt's own ref on the coordinator, by the node's pinned `git`.
    /// Authentication reaches git only through its environment. The child and
    /// its transport helper are killed at `timeout`. A ref that already holds
    /// the checkpoint is a success, so a push is repeated after a lost reply.
    pub fn push_blocking(
        &self,
        git: &Path,
        pool: &Path,
        attempt: Uuid,
        checkpoint: &ObjectId,
        timeout: Duration,
    ) -> Result<()> {
        use std::os::unix::process::CommandExt as _;
        checkpoint.validate()?;
        ensure!(!attempt.is_nil(), "nil source attempt ID");
        let mut command = std::process::Command::new(git);
        command.arg("-C").arg(pool);
        for setting in [
            "credential.helper=",
            "http.followRedirects=false",
            "core.hooksPath=/dev/null",
            "gc.auto=0",
        ] {
            command.args(["-c", setting]);
        }
        command
            .args(["push", "--porcelain", "--no-verify"])
            .arg(format!("{}/v1/execution/git/{attempt}", self.base))
            .arg(format!("{}:refs/aq/attempts/{attempt}", checkpoint.hex))
            .env_clear()
            .env("GIT_TERMINAL_PROMPT", "0")
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("HOME", "/dev/null")
            .env("GIT_CONFIG_COUNT", self.git_headers.len().to_string());
        for (n, line) in self.git_headers.iter().enumerate() {
            command
                .env(format!("GIT_CONFIG_KEY_{n}"), "http.extraHeader")
                .env(format!("GIT_CONFIG_VALUE_{n}"), line);
        }
        let mut child = command
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::piped())
            .process_group(0)
            .spawn()
            .map_err(|_| anyhow::anyhow!("git push could not start"))?;
        let mut stderr = child.stderr.take().expect("piped stderr");
        let reader = std::thread::spawn(move || {
            let mut kept = Vec::new();
            let _ = (&mut stderr).take(2048).read_to_end(&mut kept);
            let _ = std::io::copy(&mut stderr, &mut std::io::sink());
            kept
        });
        let deadline = std::time::Instant::now() + timeout;
        let status = loop {
            if let Some(status) = child.try_wait()? {
                break Some(status);
            }
            if std::time::Instant::now() >= deadline {
                // The unreaped child still owns its process group.
                unsafe { libc::kill(-(child.id() as libc::pid_t), libc::SIGKILL) };
                child.wait()?;
                break None;
            }
            std::thread::sleep(Duration::from_millis(20));
        };
        let kept = reader.join().unwrap_or_default();
        if status.is_some_and(|s| s.success()) {
            return Ok(());
        }
        let mut said = String::from_utf8_lossy(&kept).into_owned();
        for secret in self.secrets.iter() {
            said = said.replace(secret.as_str(), "<redacted>");
        }
        match status {
            Some(status) => {
                anyhow::bail!("git push of the result failed ({status}): {}", said.trim())
            }
            None => anyhow::bail!(
                "git push of the result exceeded {timeout:?}: {}",
                said.trim()
            ),
        }
    }

    pub async fn upload(
        &self,
        request: &TransferChunkRequest,
        bytes: Vec<u8>,
    ) -> Result<TransferStatus> {
        self.identity(&request.identity)?;
        request.validate()?;
        ensure!(
            bytes.len() == request.length as usize
                && format!("{:x}", Sha256::digest(&bytes)) == request.digest,
            "source chunk length or digest mismatch"
        );
        let _permit = self.bulk_permit()?;
        let response: TransferStatus = Self::json(
            self.request(
                Method::PUT,
                &format!("{}/{}", request.transfer_id, file_name(&request.file)),
            )
            .query(&[
                ("operation_id", request.operation_id.to_string()),
                ("offset", request.offset.to_string()),
                ("digest", request.digest.clone()),
            ])
            .header(header::CONTENT_TYPE, "application/octet-stream")
            .body(bytes),
        )
        .await?;
        let offset = match request.file {
            TransferFile::Manifest => response.manifest_offset,
            TransferFile::Bundle => response.bundle_offset,
        };
        ensure!(
            response.transfer_id == request.transfer_id
                && offset == request.offset + u64::from(request.length),
            "source chunk acknowledgement mismatch"
        );
        Ok(response)
    }

    pub async fn base(&self, attempt_id: Uuid) -> Result<TransferManifest> {
        ensure!(!attempt_id.is_nil(), "nil source attempt ID");
        let _permit = self.bulk_permit()?;
        let manifest: TransferManifest =
            Self::json(self.request(Method::GET, &format!("base/{attempt_id}"))).await?;
        ensure!(
            manifest.attempt.attempt_id == attempt_id && manifest.role == SourceRole::Base,
            "base response association mismatch"
        );
        Ok(manifest)
    }

    pub async fn download(
        &self,
        attempt_id: Uuid,
        file: TransferFile,
        offset: u64,
        length: usize,
    ) -> Result<Vec<u8>> {
        ensure!(
            !attempt_id.is_nil()
                && (1..=MAX_CHUNK_BYTES).contains(&length)
                && offset.checked_add(length as u64).is_some(),
            "invalid source download range"
        );
        let _permit = self.bulk_permit()?;
        let bytes = Self::bytes(
            self.request(
                Method::GET,
                &format!("base/{attempt_id}/{}", file_name(&file)),
            )
            .query(&[("offset", offset), ("limit", length as u64)]),
            length,
        )
        .await?;
        ensure!(bytes.len() == length, "incomplete source download range");
        Ok(bytes)
    }
}

fn file_name(file: &TransferFile) -> &'static str {
    match file {
        TransferFile::Manifest => "manifest",
        TransferFile::Bundle => "bundle",
    }
}
