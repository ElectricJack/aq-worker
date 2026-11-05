//! Continuation bootstrap (spec 0004 §4). A wake is a normal fresh grant with
//! a `ContinuationAttachment`. Retained source and summary are fetched from
//! central retention (no local cache is needed), verified, restored into the
//! new sandbox, and a fresh session starts with fresh scoped credentials.
//! Normal operations stay closed until AQ records the exact `BootstrapAck`.
use super::archive::SourceArchive;
use super::credentials::CredentialLayout;
use super::journal::{Bound, Journal};
use super::{Boundaries, CentralRetention, LifecycleApi, require, send_exact};
use anyhow::{Context, Result, bail};
use aq_lifecycle_protocol::{
    ArtifactRole, Binding, BootstrapAck, Checkpoint, CheckpointClaim, ContinuationAttachment,
    Digest, Message, Packet, Receipt, ReceiptDisposition, RetentionReceipt, Validate,
    canonical_digest,
};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use uuid::Uuid;

pub const CONTINUATION_BOUNDARIES: &[&str] = &[
    "attachment_journaled",
    "hydrated",
    "session_launched",
    "ack_journaled",
    "ack_recorded",
];

/// Bootstrap instructions placed in the fresh sandbox for the new session.
pub const BOOTSTRAP_FILE: &str = "aq-continuation/bootstrap.json";

/// The common host that starts a fresh tmux session for a grant.
pub trait SessionLauncher {
    /// Start a fresh session for `successor`, issuing fresh scoped credentials
    /// into `layout`, with the bootstrap instructions at `bootstrap`.
    fn launch(
        &mut self,
        successor: &Binding,
        layout: &CredentialLayout,
        bootstrap: &Path,
    ) -> Result<()>;
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum OperationsGate {
    AwaitingBootstrapAck,
    Open,
}

#[derive(Clone, Debug)]
pub struct Woken {
    pub attachment_digest: Digest,
    pub ack_message_id: Uuid,
    pub summary: CheckpointClaim,
    pub gate: OperationsGate,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct State {
    attachment_digest: Digest,
    attachment: ContinuationAttachment,
    ack_message_id: Uuid,
    summary: Option<CheckpointClaim>,
    launched: bool,
    ack: Option<BootstrapAck>,
    ack_receipt: Option<Receipt>,
}
impl Bound for State {
    fn bound_digest(&self) -> &Digest {
        &self.attachment_digest
    }
}

#[derive(Serialize)]
struct Instructions<'a> {
    instruction: &'static str,
    admission_id: Uuid,
    attachment_digest: &'a Digest,
    continuation: &'a aq_lifecycle_protocol::ContinuationRef,
    outcome: &'a aq_lifecycle_protocol::OutcomeRef,
    summary: &'a CheckpointClaim,
    pending_operation_ids: &'a [Uuid],
    held_task_message_ids: &'a [Uuid],
}

pub struct ContinuationBootstrap {
    dir: PathBuf,
    grant: Binding,
    layout: CredentialLayout,
    boundaries: Boundaries,
}

impl ContinuationBootstrap {
    /// `grant` is the binding of the fresh grant this worker was admitted for.
    pub fn new(dir: impl Into<PathBuf>, grant: Binding, layout: CredentialLayout) -> Self {
        Self {
            dir: dir.into(),
            grant,
            layout,
            boundaries: Boundaries::default(),
        }
    }
    pub fn with_boundaries(mut self, boundaries: Boundaries) -> Self {
        self.boundaries = boundaries;
        self
    }
    fn journal(&self) -> Result<Journal> {
        Journal::new(&self.dir, "continuation")
    }
    fn save(&mut self, journal: &Journal, state: &State, boundary: &str) -> Result<()> {
        journal.store(state)?;
        self.boundaries.mark(boundary)
    }

    /// Hydrate the retained checkpoint and start the fresh session.
    pub fn wake(
        &mut self,
        packet: &Packet,
        retention: &impl CentralRetention,
        launcher: &mut impl SessionLauncher,
    ) -> Result<Woken> {
        packet.validate()?;
        let Message::Continuation(attachment) = &packet.body.message else {
            bail!("not a continuation attachment");
        };
        let attachment = attachment.as_ref();
        attachment.validate()?;
        require(
            attachment.successor == self.grant,
            "continuation attachment is not bound to this grant",
        )?;
        let digest = canonical_digest(attachment)?;
        let journal = self.journal()?;
        let workspace = self.layout.workspace();
        let (mut state, created) = journal.open_bound(&digest, || {
            if workspace.exists() && std::fs::read_dir(&workspace)?.next().is_some() {
                bail!("continuation requires a fresh sandbox workspace");
            }
            Ok(State {
                attachment_digest: digest.clone(),
                attachment: attachment.clone(),
                ack_message_id: Uuid::new_v4(),
                summary: None,
                launched: false,
                ack: None,
                ack_receipt: None,
            })
        })?;
        if created {
            self.boundaries.mark("attachment_journaled")?;
        }

        if state.summary.is_none() {
            let source = fetch_verified(retention, &attachment.retained, ArtifactRole::Source)?;
            let checkpoint_bytes =
                fetch_verified(retention, &attachment.retained, ArtifactRole::Checkpoint)?;
            let checkpoint: Checkpoint =
                serde_json::from_slice(&checkpoint_bytes).context("retained checkpoint")?;
            checkpoint.validate()?;
            require(
                checkpoint.binding == attachment.predecessor
                    && checkpoint.continuation == attachment.continuation,
                "retained checkpoint belongs to another attempt or continuation generation",
            )?;
            let archive = SourceArchive::from_bytes(&source)?;
            let staging = self.layout.work_root.join(".repo.hydrating");
            remove_dir_if_present(&staging)?;
            archive.restore(&staging)?;
            // Only this operation's earlier, unjournaled hydration can be here.
            remove_dir_if_present(&workspace)?;
            std::fs::rename(&staging, &workspace)?;
            let instructions = Instructions {
                instruction: "Read this continuation, then acknowledge exactly this admission and continuation generation through the scoped AQ API before any other operation.",
                admission_id: attachment.admission_id,
                attachment_digest: &digest,
                continuation: &attachment.continuation,
                outcome: &attachment.outcome,
                summary: &checkpoint.claim,
                pending_operation_ids: &attachment.pending_operation_ids,
                held_task_message_ids: &attachment.held_task_message_ids,
            };
            let path = self.layout.work_root.join(BOOTSTRAP_FILE);
            std::fs::create_dir_all(path.parent().context("bootstrap dir")?)?;
            crate::guardian::durable_replace(&path, &serde_json::to_vec_pretty(&instructions)?)?;
            state.summary = Some(checkpoint.claim);
            self.save(&journal, &state, "hydrated")?;
        }
        if !state.launched {
            launcher.launch(
                &attachment.successor,
                &self.layout,
                &self.layout.work_root.join(BOOTSTRAP_FILE),
            )?;
            state.launched = true;
            self.save(&journal, &state, "session_launched")?;
        }
        Ok(Woken {
            attachment_digest: digest,
            ack_message_id: state.ack_message_id,
            summary: state.summary.clone().context("hydrated summary")?,
            gate: gate(&state),
        })
    }

    /// Current gate from the durable journal.
    pub fn gate(&self) -> Result<OperationsGate> {
        Ok(self
            .journal()?
            .load::<State>()?
            .map_or(OperationsGate::AwaitingBootstrapAck, |s| gate(&s)))
    }

    /// Forward the new session's acknowledgment and open operations only on
    /// AQ's exact `BootstrapAcknowledgmentRecorded` receipt. Retries reuse the
    /// journaled ack and message id; a stale or foreign ack never opens the gate.
    pub fn acknowledge(
        &mut self,
        ack: &BootstrapAck,
        aq: &mut impl LifecycleApi,
    ) -> Result<OperationsGate> {
        let journal = self.journal()?;
        let mut state: State = journal.load()?.context("no continuation to acknowledge")?;
        require(state.launched, "bootstrap session has not started")?;
        if state.ack_receipt.is_some() {
            return Ok(OperationsGate::Open);
        }
        ack.validate()?;
        let expected = &state.attachment;
        require(
            ack.successor == self.grant
                && ack.admission_id == expected.admission_id
                && ack.continuation == expected.continuation
                && ack.attachment_digest == state.attachment_digest
                && ack.outcome == expected.outcome,
            "stale bootstrap acknowledgment: not for this attempt and continuation generation",
        )?;
        match &state.ack {
            Some(journaled) => require(
                journaled == ack,
                "bootstrap acknowledgment conflicts with the journaled one",
            )?,
            None => {
                state.ack = Some(ack.clone());
                self.save(&journal, &state, "ack_journaled")?;
            }
        }
        let packet = Packet::seal(state.ack_message_id, Message::BootstrapAck(ack.clone()))?;
        let receipt = send_exact(aq, &packet)?;
        require(
            receipt.disposition == ReceiptDisposition::BootstrapAcknowledgmentRecorded,
            "AQ did not record the bootstrap acknowledgment",
        )?;
        state.ack_receipt = Some(receipt);
        self.save(&journal, &state, "ack_recorded")?;
        Ok(OperationsGate::Open)
    }
}

fn gate(state: &State) -> OperationsGate {
    if state.ack_receipt.is_some() {
        OperationsGate::Open
    } else {
        OperationsGate::AwaitingBootstrapAck
    }
}

fn fetch_verified(
    retention: &impl CentralRetention,
    retained: &[RetentionReceipt],
    role: ArtifactRole,
) -> Result<Vec<u8>> {
    let receipt = retained
        .iter()
        .find(|r| r.role == role)
        .context("continuation lacks a retained artifact")?;
    let bytes = retention
        .fetch(receipt)
        .with_context(|| format!("retained {:?} artifact unavailable", receipt.role))?;
    require(
        bytes.len() as u64 == receipt.bytes && Digest::of(&bytes) == receipt.artifact.digest,
        "retained artifact digest mismatch",
    )?;
    Ok(bytes)
}

fn remove_dir_if_present(path: &Path) -> Result<()> {
    match std::fs::remove_dir_all(path) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e.into()),
    }
}
