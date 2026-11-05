//! Node runner for server check attempts (`aq.check.sandbox/v1`).
//!
//! The server queues one check attempt per check request on the node that ran
//! the pinned profiled attempt. This node claims it, runs the pinned validator
//! against the retained checkpoint source in a C2 sandbox, and reports exactly
//! one durable stop. The server alone derives evidence from that stop.
pub mod client;
pub mod journal;
pub mod runner;
#[cfg(test)]
mod tests;

use anyhow::{Result, ensure};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest as _, Sha256};
use uuid::Uuid;

pub const EXECUTION_PROFILE: &str = "aq.check.sandbox/v1";
pub const MAX_CLAIM_LIMIT: u32 = 16;
pub const MAX_STOP_BODY_BYTES: usize = 64 * 1024;
pub const MAX_RESULT_ARTIFACTS: usize = 16;
pub const EXECUTOR_VERSION: &str = concat!("aq-worker/", env!("CARGO_PKG_VERSION"));

/// A pinned artifact: the validator, or a result artifact.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ArtifactPin {
    pub id: String,
    pub version: String,
    pub digest: String,
}
impl ArtifactPin {
    fn valid(&self) -> bool {
        text(&self.id, 256) && text(&self.version, 64) && sha256_text(&self.digest)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Subject {
    pub id: String,
    pub revision: i64,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CheckSpec {
    pub id: String,
    pub validator: ArtifactPin,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SourceRef {
    pub format: String,
    pub hex: String,
}

/// One claimed check attempt, as returned by `/v1/worker/checks/claim`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CheckClaim {
    pub operation_id: Uuid,
    pub attempt_id: Uuid,
    pub source_operation_id: Uuid,
    pub subject: Subject,
    pub check: CheckSpec,
    pub source: SourceRef,
    pub execution_profile: String,
}
impl CheckClaim {
    /// Refuses anything this node cannot run faithfully. A refused claim is
    /// still stopped exactly once, as `lost` with incomplete inputs.
    pub fn validate(&self) -> Result<()> {
        ensure!(
            self.execution_profile == EXECUTION_PROFILE,
            "unsupported check execution profile"
        );
        ensure!(
            !self.operation_id.is_nil()
                && !self.attempt_id.is_nil()
                && !self.source_operation_id.is_nil(),
            "check claim has a nil identity"
        );
        ensure!(
            text(&self.check.id, 256) && self.check.validator.valid(),
            "invalid check or validator pin"
        );
        let hex_len = match self.source.format.as_str() {
            "sha1" => 40,
            "sha256" => 64,
            _ => anyhow::bail!("unsupported checkpoint source format"),
        };
        ensure!(
            self.source.hex.len() == hex_len && lower_hex(&self.source.hex),
            "invalid checkpoint source object id"
        );
        Ok(())
    }
}

/// Digest of the claimed inputs as delivered by the server (canonical JSON,
/// sorted keys), excluding the per-attempt identities. Computed from the raw
/// entry so even an unparseable claim gets a stable digest.
pub fn inputs_digest(raw: &Value) -> String {
    let inputs = serde_json::json!({
        "execution_profile": raw.get("execution_profile"),
        "source_operation_id": raw.get("source_operation_id"),
        "subject": raw.get("subject"),
        "check": raw.get("check"),
        "source": raw.get("source"),
    });
    let mut text = String::new();
    canonical(&inputs, &mut text);
    format!("sha256:{:x}", Sha256::digest(text.as_bytes()))
}

fn canonical(value: &Value, out: &mut String) {
    match value {
        Value::Object(map) => {
            let mut keys: Vec<&String> = map.keys().collect();
            keys.sort();
            out.push('{');
            for (i, key) in keys.into_iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                out.push_str(&Value::String(key.clone()).to_string());
                out.push(':');
                canonical(&map[key], out);
            }
            out.push('}');
        }
        Value::Array(items) => {
            out.push('[');
            for (i, item) in items.iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                canonical(item, out);
            }
            out.push(']');
        }
        scalar => out.push_str(&scalar.to_string()),
    }
}

/// How the sandbox stopped the validator.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum StopKind {
    Exited { code: i32 },
    Signalled,
    TimedOut,
    Cancelled,
    Lost,
}

/// The one stop reported for an operation. Persisted before it is sent and
/// resent byte-for-byte, so a retry is an identical (accepted) repeat.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StopReport {
    pub operation_id: Uuid,
    pub stop: StopKind,
    pub executor_version: String,
    pub inputs_digest: String,
    pub inputs_complete: bool,
    pub result_artifacts: Vec<ArtifactPin>,
}
impl StopReport {
    pub fn new(operation_id: Uuid, stop: StopKind, inputs_digest: String, complete: bool) -> Self {
        // Only an exited run can have had complete inputs that matter.
        let inputs_complete = complete && matches!(stop, StopKind::Exited { .. });
        Self {
            operation_id,
            stop,
            executor_version: EXECUTOR_VERSION.into(),
            inputs_digest,
            inputs_complete,
            result_artifacts: Vec::new(),
        }
    }
    /// The server's stop validation, plus the 64 KiB body limit.
    pub fn body(&self) -> Result<Vec<u8>> {
        ensure!(!self.operation_id.is_nil(), "nil check operation");
        ensure!(
            text(&self.executor_version, 128)
                && sha256_text(&self.inputs_digest)
                && self.result_artifacts.len() <= MAX_RESULT_ARTIFACTS
                && self.result_artifacts.iter().all(ArtifactPin::valid),
            "invalid check stop"
        );
        let body = serde_json::to_vec(self)?;
        ensure!(
            body.len() <= MAX_STOP_BODY_BYTES,
            "check stop exceeds 64 KiB"
        );
        Ok(body)
    }
}

fn text(s: &str, max: usize) -> bool {
    !s.is_empty() && s.len() <= max && s.bytes().all(|b| b.is_ascii_graphic())
}
fn lower_hex(s: &str) -> bool {
    s.bytes()
        .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}
pub fn sha256_text(s: &str) -> bool {
    s.strip_prefix("sha256:")
        .is_some_and(|h| h.len() == 64 && lower_hex(h))
}

/// Operator JSON for `aq-worker run-checks`. Every validator template must
/// also be in `docker_local.templates`; the runner receives the retained
/// checkpoint bundle as its private material at `docker_local.material_path`.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ChecksConfig {
    pub journal_dir: std::path::PathBuf,
    /// Holds `<format>-<hex>.bundle` retained checkpoints.
    pub checkpoint_dir: std::path::PathBuf,
    #[serde(default = "default_max_checkpoint_bytes")]
    pub max_checkpoint_bytes: u64,
    pub docker_local: crate::backend::docker_local::DockerLocalConfig,
    pub runner: runner::RunnerConfig,
}
fn default_max_checkpoint_bytes() -> u64 {
    512 * 1024 * 1024
}
impl ChecksConfig {
    pub fn load(path: &std::path::Path) -> Result<Self> {
        use std::io::Read as _;
        let mut bytes = Vec::new();
        std::fs::File::open(path)?
            .take(1_048_577)
            .read_to_end(&mut bytes)?;
        ensure!(bytes.len() <= 1_048_576, "checks configuration too large");
        Ok(serde_json::from_slice(&bytes)?)
    }
}

/// Runs the node's check runner on the Docker-local sandbox until `shutdown`.
pub async fn run(
    worker: &crate::config::WorkerConfig,
    config: ChecksConfig,
    shutdown: tokio::sync::watch::Receiver<bool>,
) -> Result<()> {
    let backend =
        crate::backend::docker_local::DockerLocalBackend::connect_unix(config.docker_local)
            .await
            .map_err(|e| anyhow::anyhow!("check sandbox backend unavailable: {e}"))?;
    let source = runner::BundleDirSource {
        root: config.checkpoint_dir,
        max_bytes: config.max_checkpoint_bytes,
    };
    let mut runner = runner::CheckRunner::new(
        backend,
        client::CheckClient::from_config(worker)?,
        source,
        &config.journal_dir,
        config.runner,
        worker.node_id,
    )?;
    runner.run(shutdown).await
}
