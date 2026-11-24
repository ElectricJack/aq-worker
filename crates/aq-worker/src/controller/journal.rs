use super::{storage::Storage, types::*};
use anyhow::{Result, ensure};
use aq_admission_sandbox::transport::WorkerIdentity;
use aq_session_protocol::{Digest, Validate};
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
    fn initial(node: Uuid) -> Self {
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
    }
    fn read(storage: &Storage, node: Uuid) -> Result<Self> {
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
        Ok(saved.state)
    }

    fn validate(&self) -> Result<()> {
        self.validate_header()?;
        for (id, a) in &self.attempts {
            a.validate_identity(*id, self.identity.node_id)?;
            a.validate_writers()?;
        }
        for (key, o) in &self.orphans {
            o.validate(key, self.identity.node_id)?;
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
    fn validate_header(&self) -> Result<()> {
        ensure!(
            self.format == FORMAT
                && ![self.identity.node_id, self.identity.boot_id]
                    .iter()
                    .any(Uuid::is_nil)
                && self.revision > 0
                && [
                    self.attempts.len(),
                    self.orphans.len(),
                    self.readoptions.len()
                ]
                .into_iter()
                .all(|n| n <= MAX_ATTEMPTS),
            "invalid controller state"
        );
        Ok(())
    }
}
impl Attempt {
    fn validate_identity(&self, id: Uuid, node: Uuid) -> Result<()> {
        self.request.manifest.validate()?;
        self.create.validate()?;
        ensure!(
            id == self.request.manifest.bindings.attempt_id
                && id == self.create.owner.attempt_id
                && self.create.owner.node_id == node
                && !self.accept_operation_id.is_nil(),
            "controller attempt identity mismatch"
        );
        ensure!(
            self.request.matches_node(&WorkerIdentity {
                node_id: self.create.owner.node_id,
                boot_id: self.create.owner.boot_id
            }),
            "controller manifest scope mismatch"
        );
        ensure!(
            self.create.template_digest.as_str()
                == format!(
                    "sha256:{}",
                    self.request.manifest.harness.environment.digest.as_str()
                )
                && self
                    .create
                    .hard_deadline_ms
                    .checked_sub(self.request.hard_deadline_ms)
                    .is_some_and(|margin| margin <= 30_000),
            "controller template/deadline mismatch"
        );
        Ok(())
    }
    fn validate_writers(&self) -> Result<()> {
        if let Some(s) = &self.sandbox {
            ensure!(
                s.owner == self.create.owner && !s.backend_id.is_empty(),
                "controller sandbox mismatch"
            );
        }
        self.validate_grant()?;
        self.check_cancellation()?;
        ensure!(
            self.phase != Phase::Abandoned
                || (self.sandbox.is_none() && self.stopped.is_none() && self.grant.is_none()),
            "abandoned attempt may have writers"
        );
        if let Some(p) = &self.stopped {
            ensure!(
                self.sandbox.as_ref() == Some(p.sandbox()),
                "controller proof mismatch"
            );
        }
        Ok(())
    }
    fn check_cancellation(&self) -> Result<()> {
        if let Some(c) = &self.cancellation {
            ensure!(
                !c.cancellation_id.is_nil() && c.authority_revision > 0 && c.stop_after_ms > 0,
                "invalid cancellation observation"
            );
        }
        Ok(())
    }
    fn validate_grant(&self) -> Result<()> {
        let Some(g) = &self.grant else { return Ok(()) };
        ensure!(
            [g.boot_id, g.permit_id, g.sandbox_id, g.channel_id]
                .into_iter()
                .all(|id| !id.is_nil())
                && g.generation == self.request.manifest.bindings.assignment_generation
                && (1..=self.request.hard_deadline_ms).contains(&g.renewal_deadline_ms),
            "controller grant monitor mismatch"
        );
        ensure!(
            self.permit_id == Some(g.permit_id)
                && self.logical_sandbox_id == Some(g.sandbox_id)
                && self.channel_id == Some(g.channel_id)
                && self.sandbox.is_some(),
            "controller grant monitor mismatch"
        );
        Ok(())
    }
}
impl Orphan {
    fn validate(&self, key: &str, node: Uuid) -> Result<()> {
        let sandbox = &self.entry.sandbox;
        ensure!(
            key == sandbox.backend_id
                    && !key.is_empty()
                    && sandbox.owner.node_id == node
                    // Boat inventory without a local mapping has no trustworthy boot.
                    // Such entries are orphans only: they can be stopped/preserved,
                    // never adopted as an attempt or used for grant issuance.
                    && (!sandbox.owner.boot_id.is_nil() || sandbox.kind == crate::backend::BackendKind::Boat)
                    && !sandbox.owner.attempt_id.is_nil(),
            "orphan scope mismatch"
        );
        self.validate_retention()
    }
    fn validate_retention(&self) -> Result<()> {
        let sandbox = &self.entry.sandbox;
        if let Some(proof) = &self.stopped {
            ensure!(proof.sandbox() == sandbox, "orphan proof mismatch");
        }
        if let Some(auth) = &self.authorization {
            ensure!(auth.authorizes(sandbox), "orphan retention mismatch");
        }
        ensure!(
            !self.destroyed
                || (self.authorization.is_some()
                    && self
                        .stopped
                        .as_ref()
                        .is_some_and(|p| p.kind() == crate::backend::WriterProofKind::Destroyed)),
            "orphan destruction lacks proof or retention"
        );
        Ok(())
    }
}
impl Journal {
    pub fn open(root: &Path, node: Uuid) -> Result<Self> {
        ensure!(!node.is_nil(), "nil controller node");
        let (storage, new) = Storage::open(root)?;
        let mut state = if new {
            State::initial(node)
        } else {
            State::read(&storage, node)?
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
