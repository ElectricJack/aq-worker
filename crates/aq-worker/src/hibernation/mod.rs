//! H5 physical suspension (worker spec 0004 §3) and continuation bootstrap (§4).
//!
//! The state machines here are durable and port-driven: every external effect
//! (AQ lifecycle API, guardian stop, central retention, compute/disk release,
//! session launch) goes through a trait so the H4 route adapter, the aq-git
//! snapshot owner and the common tmux host can plug in without changing the
//! journal, credential and ordering rules enforced here.
pub mod archive;
pub mod continuation;
pub mod credentials;
pub mod http;
pub mod journal;
pub mod launcher;
pub mod ports;
pub mod suspend;

use anyhow::{Result, anyhow, bail};
use aq_lifecycle_protocol::{
    ArtifactRef, ArtifactRole, Binding, Digest, Packet, Receipt, RetentionReceipt, Text,
};
use uuid::Uuid;

pub use continuation::{ContinuationBootstrap, OperationsGate, SessionLauncher, Woken};
pub use credentials::{CredentialLayout, ScrubbedSecrets};
pub use http::{Directives, HttpLifecycleApi, LifecycleStatus};
pub use launcher::{
    FreshSessionLauncher, IssuedCredentials, SessionHostStart, SuccessorCredentials,
};
pub use ports::{AgentLifecycle, LocalGuardian, NodePorts, WriterGuardian};
pub use suspend::{
    CheckpointRequest, InputFence, InputGate, Suspended, Suspender, SuspensionPorts, WriterStop,
    agent_checkpoint_claim,
};

/// The scoped AQ lifecycle API (`HttpLifecycleApi` over the worker lifecycle
/// routes in production, `ConformancePeer` in tests).
pub trait LifecycleApi {
    fn send(&mut self, packet: &Packet) -> Result<Receipt>;
}

/// Central retention: resumable chunked upload with exact receipts, and fetch
/// on any node. Uploads are keyed by a journaled operation id so a resumed
/// chunk at the same offset is idempotent.
pub trait CentralRetention {
    /// Store `chunk` at `offset` of the upload; returns the committed length.
    fn upload_chunk(&mut self, operation_id: Uuid, offset: u64, chunk: &[u8]) -> Result<u64>;
    /// Seal the upload as `artifact` and return the central receipt.
    fn commit(
        &mut self,
        operation_id: Uuid,
        artifact: &ArtifactRef,
        role: ArtifactRole,
        binding: &Binding,
    ) -> Result<RetentionReceipt>;
    /// Fetch retained bytes named by a receipt.
    fn fetch(&self, receipt: &RetentionReceipt) -> Result<Vec<u8>>;
}

/// Error marking an injected crash at a named durable boundary.
#[derive(Debug)]
pub struct Crashed(pub String);
impl std::fmt::Display for Crashed {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "injected crash after boundary {}", self.0)
    }
}
impl std::error::Error for Crashed {}

/// Crash-injection hook fired after each durable write.
#[derive(Default)]
pub struct Boundaries {
    crash_after: Option<String>,
}
impl Boundaries {
    pub fn crash_after(name: &str) -> Self {
        Self {
            crash_after: Some(name.to_owned()),
        }
    }
    pub(crate) fn mark(&mut self, name: &str) -> Result<()> {
        if self.crash_after.as_deref() == Some(name) {
            self.crash_after = None;
            return Err(Crashed(name.to_owned()).into());
        }
        Ok(())
    }
}

pub(crate) fn text(value: impl Into<String>) -> Result<Text> {
    Ok(Text::new(value)?)
}

pub(crate) fn artifact_ref(id: String, bytes: &[u8]) -> Result<ArtifactRef> {
    Ok(ArtifactRef {
        id: text(id)?,
        version: text("1")?,
        digest: Digest::of(bytes),
    })
}

/// Send `packet` and require the exact receipt class for it.
pub(crate) fn send_exact(api: &mut impl LifecycleApi, packet: &Packet) -> Result<Receipt> {
    let receipt = api.send(packet)?;
    receipt
        .validate_against(packet)
        .map_err(|e| anyhow!("lifecycle receipt does not match packet: {e}"))?;
    Ok(receipt)
}

pub(crate) fn require(ok: bool, why: &str) -> Result<()> {
    if !ok {
        bail!("{why}");
    }
    Ok(())
}
