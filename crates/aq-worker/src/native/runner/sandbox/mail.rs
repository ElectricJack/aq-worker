//! Runner mail loop: poll and ack AQ mail under the delegated permit, and
//! deliver each `delivery_id` at most once, either to the agent's inbox or to
//! a qualified safe-boundary hook. The journal is the exactly-once fence; it
//! is written durably before every effect.
//!
//! Stages per delivery, each a create-new durable file in the journal:
//! - `received`: `{message_id, payload_digest, receipt_op}`; a replay must match.
//! - `receipt-acked`: the deferred `received_by_node` receipt was acked.
//! - `intent`: an injection is about to be attempted; never retried.
//! - `outcome`: `{disposition, ack_op}`; the final disposition.
//! - `acked`: the server receipt for the outcome was validated.
//!
//! Keystrokes are never typed at an unknown prompt: native delivery happens
//! only through `SafeBoundary`, and an uncertain injection is
//! `delivery_unknown`.
use super::auth::{RunnerHttp, RunnerRoute};
use anyhow::{Context as _, Result, ensure};
use aq_agent_client::mail;
use aq_session_protocol::SessionIdentity;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{
    collections::BTreeSet,
    fs,
    os::unix::fs::DirBuilderExt,
    path::{Path, PathBuf},
};
use uuid::Uuid;

/// Envelopes requested per poll (server maximum).
pub const POLL_LIMIT: usize = 16;
const MAX_POLL_REQUEST: usize = 4096;
const MAX_POLL_RESPONSE: usize = 1024 * 1024;
const MAX_ACK_REQUEST: usize = 4096;
const MAX_ACK_RESPONSE: usize = 16 * 1024;

/// What a qualified boundary reports after an injection attempt.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Injection {
    /// The harness accepted the message at a safe boundary.
    Handed,
    /// The outcome is uncertain; it is reported and never retried.
    Unknown,
}
/// A qualified adapter readiness/composer guard. H3 nudges use this hook.
pub trait SafeBoundary: Send {
    /// Whether the harness is at a safe boundary now. `false` defers delivery
    /// to a later replay with nothing injected.
    fn ready(&mut self) -> bool;
    /// Inject one delivery envelope (body included) at the safe boundary.
    fn inject(&mut self, delivery: &Value) -> Injection;
}
/// Where accepted mail goes for this attempt's harness.
pub enum Delivery<'a> {
    /// The agent reads mail with `aq-agent message inbox`.
    Inbox,
    /// The qualified adapter hands mail to the harness at a safe boundary.
    Native(&'a mut dyn SafeBoundary),
}
/// Whether the attempt still accepts input. A stopped or suspending attempt
/// rejects new deliveries as `stale`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AttemptInput {
    Accepting,
    Rejecting,
}

#[derive(Serialize, Deserialize, PartialEq)]
struct Received {
    message_id: Uuid,
    payload_digest: String,
    receipt_op: Uuid,
}
#[derive(Serialize, Deserialize)]
struct Outcome {
    disposition: String,
    ack_op: Uuid,
}

pub struct MailLoop {
    http: RunnerHttp,
    dir: PathBuf,
    identity: Value,
    attempt: Value,
}
impl MailLoop {
    /// Open (or resume) the journal for this execution's attempt.
    pub fn open(http: RunnerHttp, journal_dir: &Path) -> Result<Self> {
        let SessionIdentity::Remote {
            node_id,
            boot_id,
            attempt_id,
            assignment_generation,
            session_id,
        } = http.launch_binding().session
        else {
            anyhow::bail!("runner mail requires a remote session");
        };
        fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(journal_dir)?;
        Ok(Self {
            http,
            dir: journal_dir.to_owned(),
            identity: json!({"node_id":node_id,"boot_id":boot_id}),
            attempt: json!({"attempt_id":attempt_id,"session_id":session_id,
                "generation":assignment_generation}),
        })
    }
    fn path(&self, id: Uuid, stage: &str) -> PathBuf {
        self.dir.join(format!("{id}.{stage}.json"))
    }
    fn has(&self, id: Uuid, stage: &str) -> bool {
        self.path(id, stage).exists()
    }
    fn write(&self, id: Uuid, stage: &str, value: &impl Serialize) -> Result<()> {
        aq_session_host::durable_new(&self.path(id, stage), &serde_json::to_vec(value)?)
    }
    fn read<T: for<'de> Deserialize<'de>>(&self, id: Uuid, stage: &str) -> Result<T> {
        Ok(serde_json::from_slice(&fs::read(self.path(id, stage))?)?)
    }
    /// Journal the receipt of a delivery, or require a replay to match it.
    fn receive(&self, id: Uuid, message_id: Uuid, digest: &str) -> Result<()> {
        if self.has(id, "received") {
            let known: Received = self.read(id, "received")?;
            ensure!(
                known.message_id == message_id && known.payload_digest == digest,
                "mail delivery {id} replayed with a different message"
            );
            return Ok(());
        }
        self.write(
            id,
            "received",
            &Received {
                message_id,
                payload_digest: digest.to_owned(),
                receipt_op: Uuid::new_v4(),
            },
        )
    }
    /// Durably record that an injection of `delivery_id` is being attempted.
    /// After this, the delivery is never injected again.
    pub fn record_intent(&self, delivery_id: Uuid, message_id: Uuid, digest: &str) -> Result<()> {
        self.receive(delivery_id, message_id, digest)?;
        self.write(delivery_id, "intent", &json!({"message_id":message_id}))
    }
    fn decide(&self, id: Uuid, disposition: &str) -> Result<()> {
        self.write(
            id,
            "outcome",
            &Outcome {
                disposition: disposition.to_owned(),
                ack_op: Uuid::new_v4(),
            },
        )
    }
    async fn ack(&self, id: Uuid, operation_id: Uuid, disposition: &str) -> Result<()> {
        let request = json!({"operation_id":operation_id,"identity":self.identity,
            "attempt":self.attempt,"delivery_id":id,"disposition":disposition});
        ensure!(
            mail::valid("MessageNodeAck", &request),
            "mail ack contract refused"
        );
        let bytes = self
            .http
            .post_bounded(
                RunnerRoute::MailAck,
                &self.http.binding(),
                &request,
                MAX_ACK_REQUEST,
                MAX_ACK_RESPONSE,
            )
            .await?;
        let receipt: Value = serde_json::from_slice(&bytes).context("mail ack receipt")?;
        ensure!(
            mail::valid("MessageNodeAckReceipt", &receipt)
                && receipt["operation_id"] == json!(operation_id)
                && receipt["delivery_id"] == json!(id)
                && receipt["attempt"] == self.attempt
                && receipt["disposition"] == disposition,
            "mail ack receipt does not match the ack"
        );
        Ok(())
    }
    /// One poll pass. Returns the `(delivery_id, disposition)` pairs acked.
    pub async fn poll_once(
        &mut self,
        input: AttemptInput,
        mut delivery: Delivery<'_>,
    ) -> Result<Vec<(Uuid, &'static str)>> {
        let request = json!({"identity":self.identity,"attempt":self.attempt,"limit":POLL_LIMIT});
        ensure!(
            mail::valid("MessageNodePoll", &request),
            "mail poll contract refused"
        );
        let bytes = self
            .http
            .post_bounded(
                RunnerRoute::MailPoll,
                &self.http.binding(),
                &request,
                MAX_POLL_REQUEST,
                MAX_POLL_RESPONSE,
            )
            .await?;
        let batch: Value = serde_json::from_slice(&bytes).context("mail poll batch")?;
        let deliveries = batch["deliveries"].as_array().cloned().unwrap_or_default();
        ensure!(
            mail::valid("MessageNodeBatch", &batch)
                && batch["attempt"] == self.attempt
                && deliveries.len() <= POLL_LIMIT,
            "mail poll batch refused"
        );
        // Journal (or match) every receipt before any effect in this batch.
        let mut seen = BTreeSet::new();
        let mut items = vec![];
        for d in &deliveries {
            let id: Uuid = serde_json::from_value(d["delivery_id"].clone())?;
            let message_id: Uuid = serde_json::from_value(d["message_id"].clone())?;
            ensure!(seen.insert(id), "mail batch repeats delivery {id}");
            self.receive(
                id,
                message_id,
                d["payload_digest"].as_str().unwrap_or_default(),
            )?;
            items.push((id, message_id, d));
        }
        let mut acked = vec![];
        for (id, message_id, d) in items {
            if self.has(id, "acked") {
                continue;
            }
            if !self.has(id, "outcome") {
                let decided = if self.has(id, "intent") {
                    Some("delivery_unknown")
                } else if input == AttemptInput::Rejecting {
                    Some("stale")
                } else {
                    match &mut delivery {
                        Delivery::Inbox => Some("received_by_node"),
                        Delivery::Native(boundary) => {
                            if boundary.ready() {
                                let digest = d["payload_digest"].as_str().unwrap_or_default();
                                self.record_intent(id, message_id, digest)?;
                                Some(match boundary.inject(d) {
                                    Injection::Handed => "handed_to_harness",
                                    Injection::Unknown => "delivery_unknown",
                                })
                            } else {
                                None
                            }
                        }
                    }
                };
                let Some(disposition) = decided else {
                    // Deferred: acknowledge receipt once; inject on a later replay.
                    if !self.has(id, "receipt-acked") {
                        let received: Received = self.read(id, "received")?;
                        self.ack(id, received.receipt_op, "received_by_node")
                            .await?;
                        self.write(id, "receipt-acked", &json!({}))?;
                        acked.push((id, "received_by_node"));
                    }
                    continue;
                };
                self.decide(id, disposition)?;
            }
            let outcome: Outcome = self.read(id, "outcome")?;
            let disposition = disposition(&outcome.disposition)?;
            self.ack(id, outcome.ack_op, disposition).await?;
            self.write(id, "acked", &json!({"disposition":disposition}))?;
            acked.push((id, disposition));
        }
        Ok(acked)
    }
}
fn disposition(value: &str) -> Result<&'static str> {
    [
        "received_by_node",
        "handed_to_harness",
        "delivery_unknown",
        "stale",
    ]
    .into_iter()
    .find(|d| *d == value)
    .context("journaled mail disposition")
}
