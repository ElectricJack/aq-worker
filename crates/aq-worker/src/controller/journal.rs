use super::{storage::Storage, types::*};
use anyhow::{Result, ensure};
use aq_admission_sandbox::transport::WorkerIdentity;
use aq_session_protocol::{Digest, SessionIdentity, Validate};
use serde::{Deserialize, Serialize};
use std::{collections::BTreeMap, path::Path};
use uuid::Uuid;
pub const FORMAT: u32 = 1;
pub const MAX_BYTES: usize = 16 * 1024 * 1024;
pub const MAX_ATTEMPTS: usize = 4096;
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Snapshot {
    digest: Digest,
    state: State,
}
pub(super) struct Journal {
    pub storage: Storage,
    pub state: State,
    pub poisoned: bool,
}
impl State {
    fn validate(&self) -> Result<()> {
        ensure!(
            self.format == FORMAT
                && !self.identity.node_id.is_nil()
                && !self.identity.boot_id.is_nil()
                && self.revision > 0
                && self.attempts.len() <= MAX_ATTEMPTS
                && self.orphans.len() <= MAX_ATTEMPTS
                && self.readoptions.len() <= MAX_ATTEMPTS,
            "invalid controller state"
        );
        for (id, a) in &self.attempts {
            a.request.manifest.validate()?;
            a.create.validate()?;
            ensure!(
                *id == a.request.manifest.bindings.attempt_id
                    && *id == a.create.owner.attempt_id
                    && a.create.owner.node_id == self.identity.node_id
                    && !a.accept_operation_id.is_nil(),
                "controller attempt identity mismatch"
            );
            ensure!(
                matches!(a.request.manifest.bindings.session,SessionIdentity::Remote{node_id,boot_id,..} if node_id==a.create.owner.node_id&&boot_id==a.create.owner.boot_id),
                "controller manifest scope mismatch"
            );
            ensure!(
                a.create.template_digest.as_str()
                    == format!(
                        "sha256:{}",
                        a.request.manifest.harness.environment.digest.as_str()
                    )
                    && a.create.hard_deadline_ms >= a.request.hard_deadline_ms
                    && a.create.hard_deadline_ms - a.request.hard_deadline_ms <= 30_000,
                "controller template/deadline mismatch"
            );
            if let Some(s) = &a.sandbox {
                ensure!(
                    s.owner == a.create.owner && !s.backend_id.is_empty(),
                    "controller sandbox mismatch"
                );
            }
            if let Some(g) = &a.grant {
                ensure!(
                    !g.boot_id.is_nil()
                        && !g.permit_id.is_nil()
                        && !g.sandbox_id.is_nil()
                        && !g.channel_id.is_nil()
                        && g.generation == a.request.manifest.bindings.assignment_generation
                        && g.renewal_deadline_ms > 0
                        && g.renewal_deadline_ms <= a.request.hard_deadline_ms
                        && a.permit_id == Some(g.permit_id)
                        && a.logical_sandbox_id == Some(g.sandbox_id)
                        && a.channel_id == Some(g.channel_id)
                        && a.sandbox.is_some(),
                    "controller grant monitor mismatch"
                );
            }
            if let Some(c) = &a.cancellation {
                ensure!(
                    !c.cancellation_id.is_nil() && c.authority_revision > 0 && c.stop_after_ms > 0,
                    "invalid cancellation observation"
                );
            }
            ensure!(
                a.phase != Phase::Abandoned
                    || (a.sandbox.is_none() && a.stopped.is_none() && a.grant.is_none()),
                "abandoned attempt may have writers"
            );
            if let Some(p) = &a.stopped {
                ensure!(
                    a.sandbox.as_ref() == Some(p.sandbox()),
                    "controller proof mismatch"
                );
            }
        }
        for (key, o) in &self.orphans {
            let sandbox = &o.entry.sandbox;
            ensure!(
                *key == sandbox.backend_id
                    && !key.is_empty()
                    && sandbox.owner.node_id == self.identity.node_id
                    && !sandbox.owner.boot_id.is_nil()
                    && !sandbox.owner.attempt_id.is_nil(),
                "orphan scope mismatch"
            );
            if let Some(proof) = &o.stopped {
                ensure!(proof.sandbox() == sandbox, "orphan proof mismatch");
            }
            if let Some(auth) = &o.authorization {
                ensure!(auth.authorizes(sandbox), "orphan retention mismatch");
            }
            ensure!(
                !o.destroyed
                    || (o.authorization.is_some()
                        && o.stopped.as_ref().is_some_and(
                            |p| p.kind() == crate::backend::WriterProofKind::Destroyed
                        )),
                "orphan destruction lacks proof or retention"
            );
        }
        for request in &self.readoptions {
            request.validate()?;
            ensure!(
                request.current == self.identity,
                "re-adoption incarnation mismatch"
            );
        }
        Ok(())
    }
}
impl Journal {
    pub fn open(root: &Path, node: Uuid) -> Result<Self> {
        ensure!(!node.is_nil(), "nil controller node");
        let (storage, new) = Storage::open(root)?;
        let mut state = if new {
            State {
                format: FORMAT,
                identity: WorkerIdentity {
                    node_id: node,
                    boot_id: Uuid::new_v4(),
                },
                revision: 1,
                registration_operation_id: None,
                attempts: BTreeMap::new(),
                estimated_spend: BTreeMap::new(),
                last_refusal: None,
                orphans: BTreeMap::new(),
                readoptions: Vec::new(),
            }
        } else {
            let bytes = storage.read("journal.json", MAX_BYTES)?;
            let raw: serde_json::Value = serde_json::from_slice(&bytes)?;
            ensure!(
                raw["state"]["format"].as_u64() == Some(FORMAT.into()),
                "unsupported controller state format; preserve evidence"
            );
            let saved: Snapshot = serde_json::from_slice(&bytes)?;
            ensure!(
                saved.digest == Digest::of(&serde_json::to_vec(&saved.state)?),
                "controller state checksum mismatch"
            );
            saved.state.validate()?;
            ensure!(
                saved.state.identity.node_id == node,
                "controller state belongs to another node"
            );
            storage.discard_pending()?;
            saved.state
        };
        state.identity.boot_id = Uuid::new_v4();
        state.registration_operation_id = None;
        state.readoptions.clear();
        let mut j = Self {
            storage,
            state,
            poisoned: false,
        };
        j.persist(j.state.clone())?;
        Ok(j)
    }
    pub fn check(&self) -> Result<()> {
        ensure!(!self.poisoned, "controller journal poisoned");
        self.storage.check()
    }
    pub fn persist(&mut self, mut next: State) -> Result<()> {
        self.check()?;
        next.revision = self
            .state
            .revision
            .checked_add(1)
            .ok_or_else(|| anyhow::anyhow!("controller sequence overflow"))?;
        next.validate()?;
        let bytes = serde_json::to_vec(&Snapshot {
            digest: Digest::of(&serde_json::to_vec(&next)?),
            state: next.clone(),
        })?;
        ensure!(bytes.len() <= MAX_BYTES, "controller journal full");
        self.poisoned = true;
        self.storage.write_state(&bytes)?;
        self.state = next;
        self.poisoned = false;
        Ok(())
    }
}
