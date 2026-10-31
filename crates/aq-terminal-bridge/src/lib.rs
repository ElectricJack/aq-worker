//! Finite control-only bridge component. No launch, snapshot or renderer API.
//! Input payloads exist only in an opaque one-use preparation and the guarded
//! host's encrypted mailbox. Recovery never resubmits a received input.
pub mod http;
mod journal;
pub mod output;
use anyhow::{Result, bail, ensure};
use aq_session_host::terminal_io::{QualifiedClock, Receipt, TerminalBinding, TerminalHandle};
use aq_session_protocol::{Capability, Digest, InputDisposition, TerminalIdentity, Text};
use aq_terminal_relay_protocol::{
    self as wire, Effect, Envelope, Fence, Peer, PeerUpload, Register, RelayPoll, UploadAck,
};
use serde::{Deserialize, Serialize};
use std::{cell::Cell, collections::BTreeMap, path::Path, sync::Arc};
use uuid::Uuid;

pub const MAX_TERMINALS: usize = 32;
pub const MAX_OPERATIONS: usize = 1024;
pub const MAX_BATCH: usize = 64;

/// Trusted local adapter seam; implementations must enforce the same durable
/// no-replay boundary as TerminalHandle. Network identities cannot implement it.
pub trait OwnedTerminal: Send + Sync {
    fn binding(&self) -> &TerminalBinding;
    fn submit(&self, effect: &Effect, clock: QualifiedClock) -> Result<Option<Receipt>>;
    fn receipt(&self, id: Uuid) -> Result<Option<Receipt>>;
    fn reject_unstarted_input(&self, id: Uuid, digest: &Digest) -> Result<Receipt>;
}
impl OwnedTerminal for TerminalHandle {
    fn binding(&self) -> &TerminalBinding {
        self.binding()
    }
    fn submit(&self, e: &Effect, c: QualifiedClock) -> Result<Option<Receipt>> {
        self.submit(e, c)
    }
    fn receipt(&self, id: Uuid) -> Result<Option<Receipt>> {
        self.receipt(id)
    }
    fn reject_unstarted_input(&self, id: Uuid, d: &Digest) -> Result<Receipt> {
        self.reject_unstarted_input(id, d)
    }
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(
    tag = "kind",
    content = "body",
    rename_all = "snake_case",
    deny_unknown_fields
)]
enum Replayable {
    Fence(Fence),
    Resize(aq_session_protocol::ResizeCommand),
}
impl Replayable {
    fn effect(&self) -> Effect {
        match self {
            Self::Fence(v) => Effect::Fence(v.clone()),
            Self::Resize(v) => Effect::Resize(v.clone()),
        }
    }
}
#[derive(Clone, Serialize, Deserialize)]
#[serde(
    tag = "kind",
    content = "body",
    rename_all = "snake_case",
    deny_unknown_fields
)]
enum Phase {
    Received,
    Publishing,
    Submitted,
    Complete(Receipt),
}
#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Entry {
    terminal: TerminalIdentity,
    operation_id: Uuid,
    digest: Digest,
    input_deadline_ms: Option<u64>,
    replayable: Option<Replayable>,
    phase: Phase,
    uploaded: bool,
}
#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Upload {
    request: PeerUpload,
    keys: Vec<String>,
}
#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct State {
    peer: Peer,
    cursor: u64,
    bindings: BTreeMap<Uuid, TerminalBinding>,
    registrations: BTreeMap<Uuid, Registration>,
    entries: BTreeMap<String, Entry>,
    upload: Option<Upload>,
}
#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Registration {
    request: Register,
    confirmed: bool,
}
/// Not Clone/Serialize: execution consumes this token, and persisted entry state
/// independently prevents a stale token from authorizing another submission.
pub struct PreparedEffect {
    key: String,
    effect: Effect,
}

pub struct Bridge {
    owner: journal::Owner,
    poisoned: Cell<bool>,
    state: State,
    hosts: BTreeMap<Uuid, Arc<dyn OwnedTerminal>>,
    clock: QualifiedClock,
    recovery_after: Option<String>,
}
impl Bridge {
    /// Explicit first installation, never a recovery fallback. Fails if the
    /// ownership member already exists, including a competing installer.
    pub fn create(root: &Path, peer: Peer, clock: QualifiedClock) -> Result<Self> {
        Self::open_mode(root, peer, clock, true)
    }
    /// Reopens established authority only. Missing root, lock or journal refuses
    /// rather than silently resetting input history.
    pub fn open(root: &Path, peer: Peer, clock: QualifiedClock) -> Result<Self> {
        Self::open_mode(root, peer, clock, false)
    }
    fn open_mode(root: &Path, peer: Peer, clock: QualifiedClock, create: bool) -> Result<Self> {
        peer.validate()?;
        let mut owner = journal::Owner::open(root, create)?;
        let state = match owner.read()? {
            Some(state) => state,
            None => {
                let state = State {
                    peer: peer.clone(),
                    cursor: 0,
                    bindings: BTreeMap::new(),
                    registrations: BTreeMap::new(),
                    entries: BTreeMap::new(),
                    upload: None,
                };
                owner.write(&state)?;
                state
            }
        };
        ensure!(state.peer == peer, "bridge node/boot identity mismatch");
        validate_state(&state)?;
        Ok(Self {
            owner,
            poisoned: Cell::new(false),
            state,
            hosts: BTreeMap::new(),
            clock,
            recovery_after: None,
        })
    }
    pub fn peer(&self) -> &Peer {
        &self.state.peer
    }
    pub fn operation_count(&self) -> usize {
        self.state.entries.len()
    }
    /// Explicit installation/reconnection of an already verified local host.
    /// No path from an envelope is used to locate or create a host.
    pub fn attach(&mut self, host: Arc<dyn OwnedTerminal>) -> Result<()> {
        self.healthy()?;
        let binding = host.binding();
        binding.validate()?;
        ensure!(
            self.state.peer.matches(&binding.terminal),
            "bridge host peer mismatch"
        );
        let mut next = self.state.clone();
        if let Some(old) = next.bindings.get(&binding.terminal.terminal_id) {
            ensure!(old == binding, "bridge host replacement refused");
        } else {
            ensure!(
                next.bindings.len() < MAX_TERMINALS,
                "bridge terminal capacity"
            );
            next.bindings
                .insert(binding.terminal.terminal_id, binding.clone());
            journal::check_admission(&next)?;
        }
        self.save(next)?;
        self.hosts.insert(host.binding().terminal.terminal_id, host);
        Ok(())
    }
    pub fn poll_request(&self, hold_ms: u32) -> Result<RelayPoll> {
        self.healthy()?;
        let r = RelayPoll {
            version: wire::VERSION,
            capability: Text::new(wire::CAPABILITY)?,
            peer: self.state.peer.clone(),
            cursor: self.state.cursor,
            hold_ms,
            active_subscriptions: 0,
            active_controllers: self.hosts.len() as u32,
            advertised_capabilities: vec![Capability {
                id: Text::new(wire::CAPABILITY)?,
                version: wire::VERSION,
            }],
        };
        r.validate()?;
        Ok(r)
    }
    /// Persist exact request identity before network registration. Reconnection
    /// cannot silently replace the installed authority revision.
    pub fn prepare_registration(
        &mut self,
        terminal: &TerminalIdentity,
        authority_revision: u64,
    ) -> Result<Register> {
        self.healthy()?;
        ensure!(
            authority_revision > 0
                && self
                    .state
                    .bindings
                    .get(&terminal.terminal_id)
                    .is_some_and(|binding| binding.terminal == *terminal),
            "registration requires exact local binding"
        );
        if let Some(old) = self.state.registrations.get(&terminal.terminal_id) {
            ensure!(
                old.request.terminal == *terminal
                    && old.request.expected_authority_revision == authority_revision,
                "bridge registration authority replacement refused"
            );
            return Ok(old.request.clone());
        }
        let request = Register {
            version: wire::VERSION,
            capability: Text::new(wire::CAPABILITY)?,
            operation_id: Uuid::new_v4(),
            peer: self.state.peer.clone(),
            terminal: terminal.clone(),
            expected_authority_revision: authority_revision,
            input_mode: wire::InputMode::FiniteV1,
            advertised_capabilities: vec![Capability {
                id: Text::new(wire::CAPABILITY)?,
                version: wire::VERSION,
            }],
        };
        request.validate()?;
        let mut next = self.state.clone();
        next.registrations.insert(
            terminal.terminal_id,
            Registration {
                request: request.clone(),
                confirmed: false,
            },
        );
        journal::check_admission(&next)?;
        self.save(next)?;
        Ok(request)
    }
    /// Caller uses this only after RelayHttp::register validates the public DTO.
    pub fn confirm_registration(&mut self, request: &Register) -> Result<()> {
        self.healthy()?;
        let saved = self
            .state
            .registrations
            .get(&request.terminal.terminal_id)
            .ok_or_else(|| anyhow::anyhow!("bridge registration not prepared"))?;
        ensure!(
            digest(&saved.request)? == digest(request)?,
            "bridge registration reply request mismatch"
        );
        let mut next = self.state.clone();
        next.registrations
            .get_mut(&request.terminal.terminal_id)
            .unwrap()
            .confirmed = true;
        self.save(next)
    }
    /// Durable metadata admission precedes the first local effect. This does not
    /// persist input bytes; lost preparations are refused during recovery.
    pub fn receive(&mut self, envelope: &Envelope) -> Result<Vec<PreparedEffect>> {
        self.healthy()?;
        envelope.validate()?;
        ensure!(
            envelope.peer == self.state.peer
                && envelope.capability.as_str() == wire::CAPABILITY
                && envelope.cursor >= self.state.cursor,
            "bridge envelope identity/cursor mismatch"
        );
        let mut next = self.state.clone();
        let mut prepared = Vec::new();
        for effect in &envelope.effects {
            let terminal = effect.terminal();
            let binding = next
                .bindings
                .get(&terminal.terminal_id)
                .ok_or_else(|| anyhow::anyhow!("unregistered bridge terminal"))?;
            ensure!(
                binding.terminal == *terminal && self.hosts.contains_key(&terminal.terminal_id),
                "bridge terminal incarnation mismatch"
            );
            ensure!(
                next.registrations
                    .get(&terminal.terminal_id)
                    .is_some_and(|r| r.confirmed && r.request.terminal == *terminal),
                "bridge terminal registration unconfirmed"
            );
            let (id, digest, replayable) = identity(effect)?;
            let key = key(terminal, id);
            if let Some(old) = next.entries.get_mut(&key) {
                ensure!(old.digest == digest, "changed bridge operation payload");
                if matches!(old.phase, Phase::Complete(_)) {
                    old.uploaded = false;
                }
                continue;
            }
            ensure!(
                next.entries.len() < MAX_OPERATIONS,
                "bridge finite operation capacity"
            );
            next.entries.insert(
                key.clone(),
                Entry {
                    terminal: terminal.clone(),
                    operation_id: id,
                    digest,
                    input_deadline_ms: match effect {
                        Effect::Input(v) => Some(v.deadline_ms),
                        _ => None,
                    },
                    replayable,
                    phase: Phase::Received,
                    uploaded: false,
                },
            );
            prepared.push(PreparedEffect {
                key,
                effect: effect.clone(),
            });
        }
        next.cursor = envelope.cursor;
        if next.entries.len() > self.state.entries.len() {
            journal::check_admission(&next)?;
        }
        self.save(next)?;
        Ok(prepared)
    }
    pub fn execute(&mut self, prepared: PreparedEffect) -> Result<()> {
        self.healthy()?;
        let entry = self
            .state
            .entries
            .get(&prepared.key)
            .ok_or_else(|| anyhow::anyhow!("unknown prepared bridge operation"))?;
        ensure!(
            identity(&prepared.effect)?.1 == entry.digest,
            "prepared bridge digest mismatch"
        );
        if !matches!(entry.phase, Phase::Received) {
            return Ok(());
        }
        let host = self
            .hosts
            .get(&entry.terminal.terminal_id)
            .cloned()
            .ok_or_else(|| anyhow::anyhow!("bridge host unattached"))?;
        let mut next = self.state.clone();
        next.entries.get_mut(&prepared.key).unwrap().phase = Phase::Publishing;
        self.save(next)?; // No error or restart below may resend input.
        if let Some(receipt) = host.submit(&prepared.effect, self.clock)? {
            self.complete(&prepared.key, receipt)?;
        } else {
            let mut next = self.state.clone();
            next.entries.get_mut(&prepared.key).unwrap().phase = Phase::Submitted;
            self.save(next)?;
        }
        Ok(())
    }
    /// Bounded recovery/receipt refresh. Received input is never resubmitted;
    /// authoritative local absence becomes Rejected under guardian admission.
    pub fn recover_pending(&mut self) -> Result<usize> {
        self.healthy()?;
        let pending: Vec<_> = self
            .state
            .entries
            .iter()
            .filter(|(_, e)| {
                matches!(
                    e.phase,
                    Phase::Received | Phase::Publishing | Phase::Submitted
                )
            })
            .map(|(k, _)| k.clone())
            .collect();
        let keys: Vec<_> = pending
            .iter()
            .filter(|k| self.recovery_after.as_ref().is_none_or(|after| *k > after))
            .chain(pending.iter().filter(|k| {
                self.recovery_after
                    .as_ref()
                    .is_some_and(|after| *k <= after)
            }))
            .take(MAX_BATCH)
            .cloned()
            .collect();
        let mut completed = 0;
        for key in keys {
            self.recovery_after = Some(key.clone());
            let entry = self.state.entries[&key].clone();
            let Some(host) = self.hosts.get(&entry.terminal.terminal_id).cloned() else {
                continue;
            };
            let Ok(observed) = host.receipt(entry.operation_id) else {
                continue;
            };
            let receipt = if let Some(receipt) = observed {
                Some(receipt)
            } else if let Some(replayable) = &entry.replayable {
                host.submit(&replayable.effect(), self.clock)
                    .unwrap_or_default()
            } else {
                let now = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)?
                    .as_millis();
                if matches!(entry.phase, Phase::Received | Phase::Publishing)
                    || entry
                        .input_deadline_ms
                        .is_some_and(|deadline| now >= deadline as u128)
                {
                    host.reject_unstarted_input(entry.operation_id, &entry.digest)
                        .ok()
                } else {
                    None
                }
            };
            if let Some(receipt) = receipt
                && self.complete(&key, receipt)?
            {
                completed += 1;
            }
        }
        Ok(completed)
    }
    fn complete(&mut self, key: &str, receipt: Receipt) -> Result<bool> {
        ensure!(
            serde_json::to_vec(&receipt)?.len() <= 1024,
            "bridge receipt byte bound"
        );
        let entry = self
            .state
            .entries
            .get(key)
            .ok_or_else(|| anyhow::anyhow!("unknown bridge receipt"))?;
        if !check_receipt(entry, &receipt)? {
            return Ok(false);
        }
        let mut next = self.state.clone();
        let entry = next.entries.get_mut(key).unwrap();
        entry.phase = Phase::Complete(receipt);
        entry.uploaded = false;
        self.save(next)?;
        Ok(true)
    }
    /// All stage1 uploads are metadata-only and use reserved control capacity.
    /// Retrying a lost upload repeats this exact persisted metadata exchange.
    pub fn prepare_control_upload(&mut self) -> Result<Option<PeerUpload>> {
        self.healthy()?;
        if let Some(upload) = &self.state.upload {
            return Ok(Some(upload.request.clone()));
        }
        let keys: Vec<_> = self
            .state
            .entries
            .iter()
            .filter(|(_, e)| !e.uploaded && matches!(e.phase, Phase::Complete(_)))
            .take(MAX_BATCH)
            .map(|(k, _)| k.clone())
            .collect();
        if keys.is_empty() {
            return Ok(None);
        }
        let mut request = PeerUpload {
            version: wire::VERSION,
            capability: Text::new(wire::CAPABILITY)?,
            peer: self.state.peer.clone(),
            exchange_id: Uuid::new_v4(),
            cursor: self.state.cursor,
            receipts: vec![],
            epoch_receipts: vec![],
            resize_receipts: vec![],
            fence_acks: vec![],
            frames: vec![],
            snapshots: vec![],
            gaps: vec![],
        };
        for key in &keys {
            match &self.state.entries[key].phase {
                Phase::Complete(Receipt::Input(v)) => request.receipts.push(v.clone()),
                Phase::Complete(Receipt::Fence(v)) => request.fence_acks.push(v.clone()),
                Phase::Complete(Receipt::Resize(v)) => request.resize_receipts.push(v.clone()),
                _ => unreachable!(),
            }
        }
        request.validate()?;
        ensure!(
            serde_json::to_vec(&request)?.len() <= 256 * 1024,
            "bridge control upload bytes"
        );
        let mut next = self.state.clone();
        next.upload = Some(Upload {
            request: request.clone(),
            keys,
        });
        self.save(next)?;
        Ok(Some(request))
    }
    pub fn acknowledge_control_upload(&mut self, ack: &UploadAck) -> Result<()> {
        self.healthy()?;
        ack.validate()?;
        let upload = self
            .state
            .upload
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("bridge upload not pending"))?;
        ensure!(
            ack.peer == self.state.peer
                && ack.exchange_id == upload.request.exchange_id
                && ack.cursor == upload.request.cursor
                && ack.capability == upload.request.capability
                && ack.retirements.is_empty()
                && ack.dropped_output.is_empty(),
            "bridge upload acknowledgement mismatch"
        );
        let mut next = self.state.clone();
        for key in &upload.keys {
            next.entries
                .get_mut(key)
                .ok_or_else(|| anyhow::anyhow!("bridge upload entry missing"))?
                .uploaded = true;
        }
        next.upload = None;
        self.save(next)
    }
    fn save(&mut self, next: State) -> Result<()> {
        validate_state(&next)?;
        if let Err(error) = self.owner.write(&next) {
            self.poisoned.set(true);
            return Err(error);
        }
        self.state = next;
        Ok(())
    }
    fn healthy(&self) -> Result<()> {
        ensure!(
            !self.poisoned.get(),
            "bridge persistence uncertain; close and explicitly recover"
        );
        if let Err(error) = self.owner.verify_current() {
            self.poisoned.set(true);
            return Err(error);
        }
        Ok(())
    }
}
fn key(terminal: &TerminalIdentity, id: Uuid) -> String {
    format!("{}/{}", terminal.terminal_id, id)
}
fn digest(value: &impl Serialize) -> Result<Digest> {
    let mut value = serde_json::to_value(value)?;
    value.sort_all_objects();
    Ok(Digest::of(&serde_json::to_vec(&value)?))
}
fn identity(effect: &Effect) -> Result<(Uuid, Digest, Option<Replayable>)> {
    effect.validate()?;
    Ok(match effect {
        Effect::Fence(v) => (v.fence_id, digest(v)?, Some(Replayable::Fence(v.clone()))),
        Effect::Input(v) => (v.operation_id, digest(v)?, None),
        Effect::Resize(v) => (
            v.operation_id,
            digest(v)?,
            Some(Replayable::Resize(v.clone())),
        ),
        _ => bail!("stage1 bridge has no output/capture/epoch qualification"),
    })
}
fn validate_state(state: &State) -> Result<()> {
    state.peer.validate()?;
    ensure!(
        state.bindings.len() <= MAX_TERMINALS && state.entries.len() <= MAX_OPERATIONS,
        "bridge journal bounds"
    );
    for (id, binding) in &state.bindings {
        binding.validate()?;
        ensure!(
            *id == binding.terminal.terminal_id && state.peer.matches(&binding.terminal),
            "bridge journal host identity"
        );
    }
    ensure!(
        state.registrations.len() <= MAX_TERMINALS,
        "bridge registration capacity"
    );
    for (id, registration) in &state.registrations {
        registration.request.validate()?;
        ensure!(
            *id == registration.request.terminal.terminal_id
                && registration.request.peer == state.peer
                && registration.request.input_mode == wire::InputMode::FiniteV1
                && state
                    .bindings
                    .get(id)
                    .is_some_and(|b| b.terminal == registration.request.terminal),
            "bridge registration journal binding"
        );
    }
    for (k, entry) in &state.entries {
        ensure!(
            entry.replayable.is_none() == entry.input_deadline_ms.is_some()
                && entry.input_deadline_ms.is_none_or(|deadline| deadline > 0),
            "bridge journal input deadline"
        );
        ensure!(
            !entry.operation_id.is_nil()
                && *k == key(&entry.terminal, entry.operation_id)
                && state
                    .bindings
                    .get(&entry.terminal.terminal_id)
                    .is_some_and(|b| b.terminal == entry.terminal),
            "bridge journal operation identity"
        );
        if let Some(replayable) = &entry.replayable {
            let (id, d, _) = identity(&replayable.effect())?;
            ensure!(
                id == entry.operation_id && d == entry.digest,
                "bridge journal replay digest"
            );
        }
        if let Phase::Complete(receipt) = &entry.phase {
            ensure!(
                check_receipt(entry, receipt)?,
                "nonfinal completed bridge receipt"
            );
        }
    }
    if let Some(upload) = &state.upload {
        upload.request.validate()?;
        ensure!(
            upload.request.peer == state.peer
                && upload.keys.len() <= MAX_BATCH
                && upload.keys.iter().all(|k| state.entries.contains_key(k)),
            "bridge journal upload binding"
        );
    }
    Ok(())
}
fn check_receipt(entry: &Entry, receipt: &Receipt) -> Result<bool> {
    ensure!(
        serde_json::to_vec(receipt)?.len() <= 1024,
        "bridge receipt byte bound"
    );
    match (receipt, &entry.replayable) {
        (Receipt::Input(v), None) => {
            ensure!(
                v.terminal == entry.terminal
                    && v.operation_id == entry.operation_id
                    && v.command_digest == entry.digest,
                "bridge input receipt mismatch"
            );
            if !matches!(
                v.disposition,
                InputDisposition::Applied
                    | InputDisposition::DeliveryUnknown
                    | InputDisposition::Expired
                    | InputDisposition::Rejected
            ) {
                return Ok(false);
            }
        }
        (Receipt::Fence(v), Some(Replayable::Fence(f))) => ensure!(
            v.terminal == entry.terminal
                && v.fence_id == entry.operation_id
                && v.generation == f.generation,
            "bridge fence receipt mismatch"
        ),
        (Receipt::Resize(v), Some(Replayable::Resize(r))) => ensure!(
            v.terminal == entry.terminal
                && v.operation_id == entry.operation_id
                && v.command_digest == entry.digest
                && v.sequence == r.sequence,
            "bridge resize receipt mismatch"
        ),
        _ => bail!("bridge receipt class mismatch"),
    }
    Ok(true)
}
