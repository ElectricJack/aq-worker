//! H5: physical suspension and continuation bootstrap over fake ports, with
//! `ConformancePeer` as the AQ lifecycle endpoint and CON-03 fixtures.
use anyhow::{Result, anyhow, bail};
use aq_lifecycle_protocol::*;
use aq_worker::hibernation::archive::SourceArchive;
use aq_worker::hibernation::continuation::{BOOTSTRAP_FILE, CONTINUATION_BOUNDARIES};
use aq_worker::hibernation::suspend::SUSPENSION_BOUNDARIES;
use aq_worker::hibernation::*;
use serde_json::Value;
use std::cell::RefCell;
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::process::Command;
use uuid::Uuid;

// ---------- fixtures ----------

fn fixture() -> Value {
    serde_json::from_str(include_str!(
        "../../aq-lifecycle-protocol/artifacts/fixtures.json"
    ))
    .unwrap()
}
fn packet(index: usize) -> Packet {
    serde_json::from_value(fixture()["valid_packets"][index].clone()).unwrap()
}
fn caps() -> Vec<Capability> {
    serde_json::from_value(fixture()["advertised_capabilities"].clone()).unwrap()
}
fn predecessor() -> Binding {
    serde_json::from_value(fixture()["predecessor"].clone()).unwrap()
}
fn successor() -> Binding {
    serde_json::from_value(fixture()["successor"].clone()).unwrap()
}
fn suspend_directive() -> Packet {
    packet(2)
}
fn intent() -> SuspensionIntent {
    match suspend_directive().body.message {
        Message::Suspend(i) => i,
        _ => unreachable!(),
    }
}
fn summary(text: &str) -> SummaryClaim {
    SummaryClaim {
        text: SummaryText::new(text).unwrap(),
        context_references: vec![],
        pending_operation_ids: vec![Uuid::from_u128(0x32)],
    }
}
const GUARDIAN: Uuid = Uuid::from_u128(0x41);
fn writer_set() -> Digest {
    Digest::of(b"writers")
}
fn artifact(id: &str) -> ArtifactRef {
    ArtifactRef {
        id: Text::new(id).unwrap(),
        version: Text::new("1").unwrap(),
        digest: Digest::of(id.as_bytes()),
    }
}
fn summary_claim(text: &str) -> CheckpointClaim {
    let summary = summary(text);
    CheckpointClaim::Supplied {
        summary_digest: canonical_digest(&summary).unwrap(),
        summary,
    }
}

const SUMMARY: &str =
    "Goal: finish H5. Done: journal. Next: wire continuation. Waiting on wait 14.";

// ---------- fakes ----------

struct FakeAq {
    peer: ConformancePeer,
    sent: Vec<Uuid>,
    drop_receipts: usize,
}
impl FakeAq {
    fn new(binding: Binding) -> Self {
        Self {
            peer: ConformancePeer::new(binding, caps()).unwrap(),
            sent: Vec::new(),
            drop_receipts: 0,
        }
    }
}
impl LifecycleApi for FakeAq {
    fn send(&mut self, packet: &Packet) -> Result<Receipt> {
        self.sent.push(packet.body.message_id);
        let receipt = self
            .peer
            .receive(packet)
            .map_err(|e| anyhow!("aq refused: {e}"))?;
        if self.drop_receipts > 0 {
            self.drop_receipts -= 1;
            bail!("receipt lost in transit");
        }
        Ok(receipt)
    }
}

/// Central retention shared by every node; no node-local cache.
#[derive(Default)]
struct FakeRetention {
    uploads: BTreeMap<Uuid, Vec<u8>>,
    committed: BTreeMap<Uuid, (RetentionReceipt, Vec<u8>)>,
    chunks: usize,
    commits: usize,
    corrupt: Option<ArtifactRole>,
    lose: Option<ArtifactRole>,
    /// Artifacts the agent retained itself (POST /v1/execution/artifacts/*).
    agent: BTreeMap<Uuid, (RetentionReceipt, Vec<u8>)>,
}
impl CentralRetention for FakeRetention {
    fn upload_chunk(&mut self, operation_id: Uuid, offset: u64, chunk: &[u8]) -> Result<u64> {
        self.chunks += 1;
        let buf = self.uploads.entry(operation_id).or_default();
        let offset = offset as usize;
        if offset > buf.len() {
            bail!("upload gap");
        }
        if offset < buf.len() {
            // Resumed chunk: must replay identical bytes.
            let end = (offset + chunk.len()).min(buf.len());
            if buf[offset..end] != chunk[..end - offset] {
                bail!("resumed chunk differs");
            }
            buf.truncate(offset);
        }
        buf.extend_from_slice(chunk);
        Ok(buf.len() as u64)
    }
    fn commit(
        &mut self,
        operation_id: Uuid,
        artifact: &ArtifactRef,
        role: ArtifactRole,
        binding: &Binding,
    ) -> Result<RetentionReceipt> {
        if let Some((receipt, _)) = self.committed.get(&operation_id) {
            return Ok(receipt.clone());
        }
        let bytes = self.uploads.get(&operation_id).cloned().unwrap_or_default();
        if Digest::of(&bytes) != artifact.digest {
            bail!("upload digest mismatch");
        }
        self.commits += 1;
        let receipt = RetentionReceipt {
            receipt_id: Uuid::new_v4(),
            owner: Text::new("central-retention-owner").unwrap(),
            binding_digest: canonical_digest(binding).unwrap(),
            artifact: artifact.clone(),
            role,
            bytes: bytes.len() as u64,
            retained_at_ms: 1500,
            retained_until_ms: 1_000_000,
        };
        self.committed
            .insert(operation_id, (receipt.clone(), bytes));
        Ok(receipt)
    }
    fn fetch(&self, receipt: &RetentionReceipt) -> Result<Vec<u8>> {
        if self.lose.as_ref() == Some(&receipt.role) {
            bail!("artifact missing from central retention");
        }
        let (_, bytes) = self
            .committed
            .values()
            .chain(self.agent.values())
            .find(|(r, _)| r.receipt_id == receipt.receipt_id)
            .ok_or_else(|| anyhow!("unknown receipt"))?;
        let mut bytes = bytes.clone();
        if self.corrupt.as_ref() == Some(&receipt.role) {
            bytes[0] ^= 0xff;
        }
        Ok(bytes)
    }
}
impl FakeRetention {
    /// The agent's own checkpoint retention through the artifacts routes.
    fn agent_retain(
        &mut self,
        request: &CheckpointRequest,
        role: ArtifactRole,
        bytes: &[u8],
    ) -> RetentionReceipt {
        let receipt = RetentionReceipt {
            receipt_id: Uuid::new_v4(),
            owner: Text::new("central-retention-owner").unwrap(),
            binding_digest: canonical_digest(&request.binding).unwrap(),
            artifact: ArtifactRef {
                id: request.artifact_id.clone(),
                version: Text::new("1").unwrap(),
                digest: Digest::of(bytes),
            },
            role,
            bytes: bytes.len() as u64,
            retained_at_ms: 1500,
            retained_until_ms: 1_000_000,
        };
        self.agent
            .insert(receipt.receipt_id, (receipt.clone(), bytes.to_vec()));
        receipt
    }
    fn all_bytes(&self) -> Vec<(ArtifactRole, Vec<u8>)> {
        self.committed
            .values()
            .map(|(r, b)| (r.role.clone(), b.clone()))
            .collect()
    }
}

#[path = "hibernation/routes.rs"]
mod routes;
include!("hibernation/node.rs");
include!("hibernation/credential_retention.rs");
include!("hibernation/suspension.rs");
include!("hibernation/checkpoint.rs");
include!("hibernation/continuation.rs");
include!("hibernation/http_lifecycle.rs");
