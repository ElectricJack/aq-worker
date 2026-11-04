//! Claim → sandboxed validator run → one durable stop → sandbox release.
//!
//! Invariants: a journal record exists before any sandbox is created; a check
//! is never run twice (a record without a stop after a restart becomes `lost`);
//! the stop is persisted before it is sent and resent unchanged; the sandbox is
//! destroyed only after the server recorded that stop.
use std::fs::OpenOptions;
use std::io::Read as _;
use std::os::unix::fs::OpenOptionsExt as _;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{Context as _, Result};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio::sync::watch;
use uuid::Uuid;
use zeroize::Zeroizing;

use super::client::{CheckCallError, CheckTransport};
use super::journal::{Delivery, Journal, Record};
use super::{ArtifactPin, CheckClaim, StopKind, StopReport, inputs_digest};
use crate::backend::{
    Confirmation, CreateRequest, DestroyAuthorization, Ownership, PlacementBackend,
    PrivateMaterial, ResourceLimits, RunnerExit, SandboxIdentity, SandboxState, TemplateDigest,
};

/// Supplies the retained checkpoint source delivered to the validator as the
/// sandbox's private material.
pub trait CheckpointSource: Send + Sync {
    /// `Ok(None)`: this node does not hold that checkpoint.
    fn materialize(&self, claim: &CheckClaim) -> Result<Option<PrivateMaterial>>;
}

/// `<root>/<source.format>-<source.hex>.bundle`: one git bundle per retained
/// checkpoint, read without following symlinks and bounded in size.
pub struct BundleDirSource {
    pub root: PathBuf,
    pub max_bytes: u64,
}
impl CheckpointSource for BundleDirSource {
    fn materialize(&self, claim: &CheckClaim) -> Result<Option<PrivateMaterial>> {
        // `validate` restricted format and hex to safe path components.
        let path = self.root.join(format!(
            "{}-{}.bundle",
            claim.source.format, claim.source.hex
        ));
        let file = match OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_NOFOLLOW)
            .open(&path)
        {
            Ok(f) => f,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(e).context("open retained checkpoint"),
        };
        anyhow::ensure!(file.metadata()?.is_file(), "checkpoint is not a file");
        let mut bytes = Zeroizing::new(Vec::new());
        file.take(self.max_bytes + 1).read_to_end(&mut bytes)?;
        anyhow::ensure!(
            bytes.len() as u64 <= self.max_bytes,
            "checkpoint exceeds size limit"
        );
        Ok(Some(PrivateMaterial::new(std::mem::take(&mut *bytes))))
    }
}

/// A validator this node may run: the server's pin and the sandbox template
/// that implements it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ValidatorTemplate {
    pub validator: ArtifactPin,
    pub template: TemplateDigest,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RunnerConfig {
    pub validators: Vec<ValidatorTemplate>,
    pub timeout_ms: u64,
    #[serde(default = "default_poll_ms")]
    pub poll_interval_ms: u64,
    #[serde(default = "default_claim_ms")]
    pub claim_interval_ms: u64,
    #[serde(default = "default_claim_limit")]
    pub claim_limit: u32,
    pub resources: ResourceLimits,
}
fn default_poll_ms() -> u64 {
    500
}
fn default_claim_ms() -> u64 {
    5_000
}
fn default_claim_limit() -> u32 {
    1
}

/// A transport failure the runner cannot recover from by retrying.
#[derive(Debug)]
pub struct Fatal(pub CheckCallError);
impl std::fmt::Display for Fatal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "check transport: {}", self.0)
    }
}
impl std::error::Error for Fatal {}

pub struct CheckRunner<B, T, S> {
    backend: B,
    transport: T,
    source: S,
    journal: Journal,
    config: RunnerConfig,
    node_id: Uuid,
    boot_id: Uuid,
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_millis() as u64)
}

impl<B: PlacementBackend, T: CheckTransport, S: CheckpointSource> CheckRunner<B, T, S> {
    pub fn new(
        backend: B,
        transport: T,
        source: S,
        journal_dir: &Path,
        config: RunnerConfig,
        node_id: Uuid,
    ) -> Result<Self> {
        anyhow::ensure!(!node_id.is_nil(), "nil node identity");
        if let Some(r) = config.resources.validate().err() {
            anyhow::bail!("invalid check resources: {r}");
        }
        Ok(Self {
            backend,
            transport,
            source,
            journal: Journal::open(journal_dir)?,
            config,
            node_id,
            boot_id: Uuid::new_v4(),
        })
    }
    pub fn backend(&self) -> &B {
        &self.backend
    }
    pub fn backend_mut(&mut self) -> &mut B {
        &mut self.backend
    }
    pub fn journal(&self) -> &Journal {
        &self.journal
    }

    /// Until `shutdown`: settle the journal, claim, run, report. Returns Err
    /// only for a fatal (non-retryable) transport or journal failure.
    pub async fn run(&mut self, mut shutdown: watch::Receiver<bool>) -> Result<()> {
        loop {
            if *shutdown.borrow() {
                return Ok(());
            }
            self.tick(&shutdown).await?;
            let wait = Duration::from_millis(self.config.claim_interval_ms);
            tokio::select! {
                _ = tokio::time::sleep(wait) => {}
                _ = shutdown.changed() => {}
            }
        }
    }

    /// One pass: settle unsettled records, then claim and run each check.
    pub async fn tick(&mut self, shutdown: &watch::Receiver<bool>) -> Result<usize> {
        self.settle().await?;
        let claimed = match self.transport.claim(self.config.claim_limit).await {
            Ok(claimed) => claimed,
            Err(CheckCallError::Transient(m)) => {
                tracing::warn!("check claim failed: {m}; retrying");
                return Ok(0);
            }
            Err(e) => return Err(Fatal(e).into()),
        };
        let count = claimed.len();
        for raw in claimed {
            self.handle(raw, shutdown).await?;
        }
        Ok(count)
    }

    /// Finishes every record left by a crash or a failed send: a record with
    /// no stop is never rerun (its sandbox is stopped and it is reported
    /// `lost`); a pending stop is resent unchanged; a recorded stop releases.
    pub async fn settle(&mut self) -> Result<()> {
        for mut record in self.journal.unsettled()? {
            self.settle_one(&mut record).await?;
        }
        Ok(())
    }

    async fn handle(&mut self, raw: Value, shutdown: &watch::Receiver<bool>) -> Result<()> {
        let Some(operation) = raw
            .get("operation_id")
            .and_then(|v| serde_json::from_value::<Uuid>(v.clone()).ok())
            .filter(|op| !op.is_nil())
        else {
            tracing::warn!("ignoring a check claim without an operation identity");
            return Ok(());
        };
        if let Some(mut record) = self.journal.load(operation)? {
            // Re-claimed: the server has not recorded our stop. Never rerun.
            if record.stop.is_none() || record.delivery == Delivery::Pending {
                self.settle_one(&mut record).await?;
            }
            return Ok(());
        }
        let mut record = Record::new(operation, inputs_digest(&raw));
        self.journal.save(&record)?;
        let (stop, complete) = self.execute(raw, &mut record, shutdown).await?;
        let report = StopReport::new(operation, stop, record.inputs_digest.clone(), complete);
        record.decide(report);
        self.journal.save(&record)?;
        self.deliver(&mut record).await
    }

    async fn settle_one(&mut self, record: &mut Record) -> Result<()> {
        if record.stop.is_none() {
            if let Some(sandbox) = record.sandbox.clone() {
                let _ = self.backend.stop(&sandbox).await;
            }
            let lost = StopReport::new(
                record.operation_id,
                StopKind::Lost,
                record.inputs_digest.clone(),
                false,
            );
            record.decide(lost);
            self.journal.save(record)?;
        }
        self.deliver(record).await
    }

    /// Runs the validator. Every failure before a trustworthy exit is `lost`
    /// with incomplete inputs; the server derives no evidence from it.
    async fn execute(
        &mut self,
        raw: Value,
        record: &mut Record,
        shutdown: &watch::Receiver<bool>,
    ) -> Result<(StopKind, bool)> {
        let operation = record.operation_id;
        let claim: CheckClaim = match serde_json::from_value(raw) {
            Ok(claim) => claim,
            Err(_) => {
                tracing::warn!(%operation, "malformed check claim; reporting lost");
                return Ok((StopKind::Lost, false));
            }
        };
        if let Err(e) = claim.validate() {
            tracing::warn!(%operation, "refusing check claim: {e}; reporting lost");
            return Ok((StopKind::Lost, false));
        }
        let Some(template) = self
            .config
            .validators
            .iter()
            .find(|v| v.validator == claim.check.validator)
            .map(|v| v.template.clone())
        else {
            tracing::warn!(%operation, "validator is not installed on this node; reporting lost");
            return Ok((StopKind::Lost, false));
        };
        let material = match self.source.materialize(&claim) {
            Ok(Some(material)) => material,
            Ok(None) => {
                tracing::warn!(%operation, "checkpoint source is not retained here; reporting lost");
                return Ok((StopKind::Lost, false));
            }
            Err(e) => {
                tracing::warn!(%operation, "checkpoint source unreadable: {e:#}; reporting lost");
                return Ok((StopKind::Lost, false));
            }
        };

        let owner = Ownership {
            node_id: self.node_id,
            boot_id: self.boot_id,
            attempt_id: operation,
        };
        let started = now_ms();
        let deadline = started.saturating_add(self.config.timeout_ms).max(1);
        let request = CreateRequest::new(owner.clone(), template, deadline, Vec::new())
            .with_resources(self.config.resources);
        record.sandbox = Some(SandboxIdentity {
            kind: self.backend.kind(),
            backend_id: request.name.clone(),
            owner,
        });
        self.journal.save(record)?;
        let sandbox = match self.backend.create(&request).await {
            Ok(sandbox) => sandbox,
            Err(e) => {
                tracing::warn!(%operation, "check sandbox create failed: {e}; reporting lost");
                return Ok((StopKind::Lost, false));
            }
        };
        if record.sandbox.as_ref() != Some(&sandbox) {
            record.sandbox = Some(sandbox.clone());
            self.journal.save(record)?;
        }
        let begun = match self
            .backend
            .deliver_private_material(&sandbox, material)
            .await
        {
            Ok(()) => self.backend.start_runner(&sandbox).await,
            Err(e) => Err(e),
        };
        if let Err(e) = begun {
            tracing::warn!(%operation, "check sandbox start failed: {e}; reporting lost");
            let _ = self.backend.stop(&sandbox).await;
            return Ok((StopKind::Lost, false));
        }
        let poll = Duration::from_millis(self.config.poll_interval_ms);
        loop {
            let state = match self.backend.inspect(&sandbox).await {
                Ok(entry) => entry.state,
                Err(e) => {
                    tracing::warn!(%operation, "check sandbox inspect failed: {e}");
                    SandboxState::Unknown
                }
            };
            match state {
                SandboxState::Created | SandboxState::Running => {}
                SandboxState::Stopped => {
                    return Ok(match self.backend.runner_exit(&sandbox).await {
                        Ok(RunnerExit::Exited(code)) => (StopKind::Exited { code }, true),
                        Ok(RunnerExit::Signalled) => (StopKind::Signalled, false),
                        Err(e) => {
                            tracing::warn!(%operation, "validator exit unknown: {e}");
                            (StopKind::Lost, false)
                        }
                    });
                }
                SandboxState::Archived | SandboxState::Destroyed | SandboxState::Unknown => {
                    let _ = self.backend.stop(&sandbox).await;
                    return Ok((StopKind::Lost, false));
                }
            }
            if now_ms() >= deadline {
                let _ = self.backend.stop(&sandbox).await;
                return Ok((StopKind::TimedOut, false));
            }
            if *shutdown.borrow() {
                let _ = self.backend.stop(&sandbox).await;
                return Ok((StopKind::Lost, false));
            }
            let mut stopping = shutdown.clone();
            tokio::select! {
                _ = tokio::time::sleep(poll) => {}
                _ = stopping.changed() => {}
            }
        }
    }

    /// Sends the persisted stop. Transient failures stay pending for an
    /// identical resend; a refusal is recorded and never retried.
    async fn deliver(&mut self, record: &mut Record) -> Result<()> {
        if record.delivery == Delivery::Pending {
            let stop = record.stop.as_ref().context("deliver without a stop")?;
            let outcome = match stop.body() {
                Ok(body) => self.transport.stop(&body).await,
                Err(e) => Err(CheckCallError::Invalid(e.to_string())),
            };
            let operation = record.operation_id;
            match outcome {
                Ok(()) => record.delivery = Delivery::Recorded,
                Err(CheckCallError::Transient(m)) => {
                    tracing::warn!(%operation, "check stop not delivered: {m}; will resend");
                    return Ok(());
                }
                Err(
                    e @ (CheckCallError::Conflict
                    | CheckCallError::NotFound
                    | CheckCallError::Invalid(_)),
                ) => {
                    tracing::error!(%operation, "check stop refused: {e}; sandbox retained");
                    record.delivery = Delivery::Refused {
                        reason: e.to_string(),
                    };
                }
                Err(e) => return Err(Fatal(e).into()),
            }
            self.journal.save(record)?;
        }
        self.release(record).await
    }

    async fn release(&mut self, record: &mut Record) -> Result<()> {
        let Some(sandbox) = record.sandbox.clone() else {
            return Ok(());
        };
        if record.delivery != Delivery::Recorded || record.released {
            return Ok(());
        }
        let _ = self.backend.stop(&sandbox).await;
        let receipt = format!("check-stop:{}", record.operation_id);
        let destroyed = match DestroyAuthorization::retained(sandbox.clone(), receipt) {
            Ok(auth) => self.backend.destroy(&sandbox, &auth).await,
            Err(_) => return Ok(()),
        };
        if let Confirmation::Confirmed(_) = destroyed {
            record.released = true;
            self.journal.save(record)?;
        } else {
            tracing::warn!(operation = %record.operation_id, "check sandbox not yet destroyed; will retry");
        }
        Ok(())
    }
}
