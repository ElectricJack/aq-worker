//! Independent node spool. Atomic journal replacement covers parser state,
//! source cursor and facts together. No network or worker authority lock lives
//! here; caller authentication of coordinator receipts is a later adapter seam.
use crate::{
    Format, Normalizer,
    framing::{self, Frame},
    lifecycle::*,
    source::{Cursor, ReadOutcome, SourceChange, SourceFile, SourcePermit},
};
use anyhow::{Context as _, Result, ensure};
use serde::{Deserialize, Serialize};
use std::{
    collections::BTreeMap,
    fs::File,
    io::{Read, Write},
    os::{
        fd::{AsRawFd, FromRawFd},
        unix::fs::{MetadataExt, OpenOptionsExt},
    },
    path::{Path, PathBuf},
};
use uuid::Uuid;
const MAX_JOURNAL: usize = 16 * 1024 * 1024;
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Node {
    pub node_id: Uuid,
    pub boot_id: Uuid,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Limits {
    pub sources: usize,
    pub ordinary_events: usize,
    pub ordinary_bytes: usize,
    pub reserved_gap_events: usize,
    pub reserved_gap_bytes: usize,
    pub journal_bytes: usize,
    pub max_send_attempts: u32,
    pub max_send_elapsed_ms: u64,
}
impl Default for Limits {
    fn default() -> Self {
        Self {
            sources: 16,
            ordinary_events: 256,
            ordinary_bytes: 2 * 1024 * 1024,
            reserved_gap_events: 16,
            reserved_gap_bytes: 64 * 1024,
            journal_bytes: 8 * 1024 * 1024,
            max_send_attempts: 3,
            max_send_elapsed_ms: 30000,
        }
    }
}
impl Limits {
    fn validate(&self) -> Result<()> {
        ensure!(
            (1..=16).contains(&self.sources)
                && (1..=4096).contains(&self.ordinary_events)
                && (4096..=8 * 1024 * 1024).contains(&self.ordinary_bytes)
                && (1..=64).contains(&self.reserved_gap_events)
                && (4096..=256 * 1024).contains(&self.reserved_gap_bytes)
                && self.journal_bytes <= MAX_JOURNAL
                && self.journal_bytes >= self.ordinary_bytes + self.reserved_gap_bytes + 65536
                && (1..=8).contains(&self.max_send_attempts)
                && (1..=120000).contains(&self.max_send_elapsed_ms),
            "spool bounds refused"
        );
        Ok(())
    }
}
#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct SourceState {
    normalizer: Normalizer,
    permit: Option<SourcePermit>,
    cursor: Cursor,
    discarding: bool,
    record_cursor: Option<Text>,
    held_for_reconciliation: bool,
    last_change: Option<SourceChange>,
    history_checkpoint: Option<ArtifactRef>,
    replay: Option<crate::reparse::ReplayState>,
    completed_replays: Vec<crate::reparse::ReplayCompletion>,
}
#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Pending {
    packet: Packet,
    attempts: u32,
    first_ms: Option<u64>,
    last_ms: Option<u64>,
    next_retry_ms: u64,
}
#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct State {
    format_version: u32,
    node: Node,
    initial_transport: crate::InitialTransport,
    boot_transitions: Vec<crate::BootTransition>,
    limits: Limits,
    next_sequence: u64,
    acked_through: u64,
    sources: BTreeMap<Uuid, SourceState>,
    outbox: Vec<ObservationRecord>,
    pending: Option<Pending>,
    last_receipt: Option<Receipt>,
}
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Envelope {
    digest: Digest,
    state: State,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum OverflowPolicy {
    Hold,
    GapAndHold,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WriteFault {
    None,
    /// IO-error injection cleans the known uncommitted temporary file.
    BeforeRename,
    /// Process-crash test boundary: retain the fsynced temporary journal.
    /// Reopen refuses this ambiguous state; it never selects the temporary file.
    BeforeRenameRetainedTemp,
    AfterRename,
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Health {
    pub queued_events: usize,
    pub ordinary_bytes: usize,
    pub reserved_gap_events: usize,
    pub held_sources: usize,
    pub acked_through: u64,
    pub pending_message: Option<Uuid>,
    pub persistence_uncertain: bool,
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Collected {
    pub consumed_bytes: u64,
    pub facts_added: usize,
    pub partial: bool,
    pub blocked: bool,
    pub source_change: Option<SourceChange>,
}
pub struct Spool {
    root: PathBuf,
    directory: File,
    _owner: File,
    state: State,
    poisoned: bool,
    fault: WriteFault,
}
fn state_digest(state: &State) -> Result<Digest> {
    crate::normalize::value_digest(&serde_json::to_value(state)?)
}
fn open_at(directory: &File, name: &str, create: bool) -> Result<File> {
    let name = std::ffi::CString::new(name)?;
    let flags = if create {
        libc::O_RDWR | libc::O_CREAT
    } else {
        libc::O_RDONLY
    };
    let fd = unsafe {
        libc::openat(
            directory.as_raw_fd(),
            name.as_ptr(),
            flags | libc::O_NOFOLLOW | libc::O_NONBLOCK | libc::O_CLOEXEC,
            0o600,
        )
    };
    if fd < 0 {
        return Err(std::io::Error::last_os_error().into());
    }
    let file = unsafe { File::from_raw_fd(fd) };
    let m = file.metadata()?;
    ensure!(
        m.is_file()
            && m.uid() == unsafe { libc::geteuid() }
            && m.mode() & 0o077 == 0
            && m.nlink() == 1,
        "private spool member refused"
    );
    Ok(file)
}
fn lane_usage(events: &[ObservationRecord]) -> Result<(usize, usize, usize, usize)> {
    let (mut n, mut b, mut gaps, mut gap_bytes) = (0, 0, 0, 0);
    for event in events {
        let bytes = serde_json::to_vec(event)?.len();
        if matches!(event.body.kind, ObservationKind::Gap { .. }) {
            gaps += 1;
            gap_bytes += bytes;
        } else {
            n += 1;
            b += bytes;
        }
    }
    Ok((n, b, gaps, gap_bytes))
}
// Compute retained lane costs once per collection, then count only new facts.
#[derive(Clone, Copy)]
struct LaneUsage(usize, usize, usize, usize);
impl LaneUsage {
    fn retained(events: &[ObservationRecord]) -> Result<Self> {
        let (n, b, g, gb) = lane_usage(events)?;
        Ok(Self(n, b, g, gb))
    }
    fn adding(self, events: &[ObservationRecord], limits: &Limits) -> Result<Option<Self>> {
        let (n, b, g, gb) = lane_usage(events)?;
        let total = Self(self.0 + n, self.1 + b, self.2 + g, self.3 + gb);
        Ok((total.0 <= limits.ordinary_events
            && total.1 <= limits.ordinary_bytes
            && total.2 <= limits.reserved_gap_events
            && total.3 <= limits.reserved_gap_bytes)
            .then_some(total))
    }
}
fn within(state: &State) -> Result<bool> {
    let (n, b, g, gb) = lane_usage(&state.outbox)?;
    Ok(n <= state.limits.ordinary_events
        && b <= state.limits.ordinary_bytes
        && g <= state.limits.reserved_gap_events
        && gb <= state.limits.reserved_gap_bytes)
}
fn remote(node: &Node, binding: &Binding) -> Result<()> {
    binding.validate()?;
    ensure!(
        matches!(binding.session,SessionIdentity::Remote{node_id,boot_id,..}if node_id==node.node_id&&boot_id==node.boot_id),
        "active spool node/boot scope mismatch; central hosts use a separate permit adapter"
    );
    Ok(())
}
fn known_remote(state: &State, binding: &Binding) -> Result<()> {
    binding.validate()?;
    let SessionIdentity::Remote {
        node_id, boot_id, ..
    } = binding.session
    else {
        anyhow::bail!("remote transport requires a remote observation binding")
    };
    ensure!(
        node_id == state.node.node_id
            && (boot_id == state.initial_transport.node.boot_id
                || state
                    .boot_transitions
                    .iter()
                    .any(|t| t.new_boot_id == boot_id)),
        "historical transport boot is not admitted"
    );
    Ok(())
}
fn event_boot(event: &ObservationRecord) -> Uuid {
    let SessionIdentity::Remote { boot_id, .. } = event.body.binding.session else {
        unreachable!("validated remote outbox")
    };
    boot_id
}
impl Spool {
    pub fn initialize(
        root: &Path,
        limits: Limits,
        admission: crate::VerifiedInitial,
    ) -> Result<Self> {
        Self::open_inner(
            root,
            admission.plan.node.node_id,
            Some(admission.plan.node.boot_id),
            limits,
            Some(&admission),
        )
    }
    pub fn open(root: &Path, node: Node, limits: Limits) -> Result<Self> {
        Self::open_inner(root, node.node_id, Some(node.boot_id), limits, None)
    }
    pub fn open_existing(root: &Path, node_id: Uuid, limits: Limits) -> Result<Self> {
        Self::open_inner(root, node_id, None, limits, None)
    }
    fn open_inner(
        root: &Path,
        node_id: Uuid,
        expected_boot: Option<Uuid>,
        limits: Limits,
        initial: Option<&crate::VerifiedInitial>,
    ) -> Result<Self> {
        ensure!(
            !node_id.is_nil() && expected_boot.is_none_or(|id| !id.is_nil()),
            "spool node identity"
        );
        limits.validate()?;
        ensure!(root.is_absolute(), "absolute private spool root required");
        use std::os::unix::fs::DirBuilderExt;
        match std::fs::DirBuilder::new().mode(0o700).create(root) {
            Ok(()) => File::open(root.parent().context("spool parent absent")?)?.sync_all()?,
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {}
            Err(e) => return Err(e.into()),
        };
        ensure!(
            std::fs::canonicalize(root)? == root,
            "canonical spool root required"
        );
        let directory = std::fs::OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC)
            .open(root)?;
        let m = directory.metadata()?;
        ensure!(
            m.uid() == unsafe { libc::geteuid() } && m.mode() & 0o077 == 0,
            "private spool root permissions refused"
        );
        let owner = open_at(&directory, "owner.lock", true)?;
        fs2::FileExt::try_lock_exclusive(&owner)?;
        directory.sync_all()?;
        for entry in std::fs::read_dir(root)? {
            let entry = entry?;
            ensure!(
                matches!(
                    entry.file_name().to_str(),
                    Some("owner.lock" | "spool.json")
                ),
                "unknown spool member; preserve for explicit recovery"
            );
        }
        let state = match open_at(&directory, "spool.json", false) {
            Ok(file) => {
                ensure!(
                    file.metadata()?.len() <= limits.journal_bytes as u64,
                    "spool journal byte bound"
                );
                let mut bytes = vec![];
                file.take(limits.journal_bytes as u64 + 1)
                    .read_to_end(&mut bytes)?;
                ensure!(
                    bytes.len() <= limits.journal_bytes,
                    "spool journal byte bound"
                );
                let env: Envelope = serde_json::from_value(crate::json::parse(&bytes)?)?;
                ensure!(
                    state_digest(&env.state)? == env.digest,
                    "spool journal digest mismatch"
                );
                env.state
            }
            Err(e)
                if e.downcast_ref::<std::io::Error>()
                    .is_some_and(|e| e.kind() == std::io::ErrorKind::NotFound) =>
            {
                let admission=initial.context("spool missing; authenticated central transport admission/recovery required; never reseed locally")?;
                State {
                    format_version: 1,
                    node: admission.plan.node.clone(),
                    initial_transport: admission.plan.clone(),
                    boot_transitions: vec![],
                    limits: limits.clone(),
                    next_sequence: 1,
                    acked_through: 0,
                    sources: BTreeMap::new(),
                    outbox: vec![],
                    pending: None,
                    last_receipt: None,
                }
            }
            Err(e) => return Err(e),
        };
        ensure!(
            state.node.node_id == node_id
                && expected_boot.is_none_or(|boot| state.node.boot_id == boot)
                && state.limits == limits,
            "spool identity/limits changed"
        );
        if let Some(initial) = initial {
            ensure!(
                state.initial_transport == initial.plan,
                "initial transport ID payload conflict"
            );
        }
        let mut spool = Self {
            root: root.into(),
            directory,
            _owner: owner,
            state,
            poisoned: false,
            fault: WriteFault::None,
        };
        spool.validate_state()?;
        if initial.is_some() && !root.join("spool.json").exists() {
            spool.commit(spool.state.clone())?;
        }
        Ok(spool)
    }
    fn validate_state(&self) -> Result<()> {
        let state = &self.state;
        ensure!(
            state.format_version == 1
                && state.next_sequence > state.acked_through
                && state.sources.len() <= state.limits.sources
                && within(state)?,
            "spool state/version/bounds"
        );
        state.initial_transport.validate()?;
        ensure!(
            state.initial_transport.node.node_id == state.node.node_id
                && state.initial_transport.initial_sequence == 1
                && state.boot_transitions.len() <= 64,
            "transport owner history bound/scope"
        );
        let mut active = state.initial_transport.node.boot_id;
        let mut boot_ids = std::collections::BTreeSet::from([active]);
        let mut operation_ids =
            std::collections::BTreeSet::from([state.initial_transport.operation_id]);
        for transition in &state.boot_transitions {
            transition.validate()?;
            ensure!(
                transition.node_id == state.node.node_id
                    && transition.transport_owner_id == state.initial_transport.transport_owner_id
                    && transition.expected_boot_id == active
                    && transition.expected_next_sequence <= state.next_sequence
                    && operation_ids.insert(transition.operation_id)
                    && boot_ids.insert(transition.new_boot_id),
                "boot history owner/sequence/payload mismatch"
            );
            active = transition.new_boot_id;
        }
        ensure!(
            active == state.node.boot_id,
            "active transport boot mismatch"
        );
        let mut expected = state
            .acked_through
            .checked_add(1)
            .context("sequence exhausted")?;
        let mut ids = std::collections::BTreeSet::new();
        for event in &state.outbox {
            event.validate()?;
            known_remote(state, &event.body.binding)?;
            ensure!(
                event.body.sequence == expected && ids.insert(event.event_id),
                "spool sequence or event identity mismatch"
            );
            let source = state
                .sources
                .get(&event.body.provenance.source_id)
                .context("fact source absent")?;
            ensure!(
                source.normalizer.source.binding == event.body.binding,
                "fact binding changed"
            );
            expected = expected.checked_add(1).context("sequence exhausted")?;
        }
        ensure!(
            expected == state.next_sequence,
            "spool cursor/facts mismatch"
        );
        for (id, source) in &state.sources {
            source.normalizer.source.validate()?;
            known_remote(state, &source.normalizer.source.binding)?;
            source.cursor.validate()?;
            ensure!(
                *id == source.normalizer.source.source_id
                    && source.normalizer.next_sequence <= state.next_sequence
                    && source.normalizer.seen.len() <= source.normalizer.maximum_tracked
                    && source.normalizer.native_messages.len() <= source.normalizer.maximum_tracked
                    && source.normalizer.mutations.len() <= 128,
                "normalizer state bound/scope"
            );
            ensure!(
                source.completed_replays.len() <= 16,
                "reparse history bound"
            );
            let mut replay_ids = std::collections::BTreeSet::new();
            for done in &source.completed_replays {
                ensure!(
                    replay_ids.insert(done.operation_id),
                    "duplicate reparse operation"
                );
            }
            if let Some(replay) = &source.replay {
                replay.plan.validate()?;
                ensure!(
                    replay.plan.digest()? == replay.plan_digest
                        && replay.offset <= replay.plan.transcript.bytes
                        && replay.plan.source_id == *id
                        && replay.plan.source_generation == source.normalizer.source.generation
                        && replay.plan.binding == source.normalizer.source.binding
                        && source
                            .normalizer
                            .reparse
                            .as_ref()
                            .is_some_and(|r| r.operation_id == replay.plan.operation_id
                                && r.known == replay.plan.known
                                && r.handled.len() <= r.known.len()
                                && r.handled.iter().all(|k| r.known.contains_key(k)))
                        && !replay_ids.contains(&replay.plan.operation_id),
                    "reparse intent/parser state mismatch"
                );
            } else {
                ensure!(
                    source.normalizer.reparse.is_none(),
                    "orphan parser reconciliation state"
                );
            }
            if let Some(permit) = &source.permit {
                ensure!(
                    permit.source_id == *id
                        && permit.native_session_key == source.normalizer.source.native_session_key,
                    "source permit scope"
                );
            }
        }
        if let Some(pending) = &state.pending {
            pending.packet.validate()?;
            let Message::Batch(batch) = &pending.packet.body.message else {
                anyhow::bail!("spool pending class")
            };
            ensure!(
                batch.events.len() <= state.outbox.len()
                    && state.outbox[..batch.events.len()] == batch.events
                    && pending.attempts <= state.limits.max_send_attempts
                    && (pending.attempts == 0) == pending.first_ms.is_none()
                    && pending.first_ms.is_some() == pending.last_ms.is_some(),
                "pending batch intent changed"
            );
        }
        if let Some(receipt) = &state.last_receipt {
            receipt.validate()?;
            ensure!(
                receipt.through_sequence == Some(state.acked_through),
                "receipt watermark mismatch"
            );
        }
        Ok(())
    }
    pub fn node(&self) -> &Node {
        &self.state.node
    }
    pub fn next_sequence(&self) -> u64 {
        self.state.next_sequence
    }
    pub fn adopt_boot(
        &mut self,
        admission: &crate::VerifiedBootTransition,
        observed_at_ms: u64,
    ) -> Result<()> {
        let plan = &admission.plan;
        plan.validate()?;
        if let Some(old) = self
            .state
            .boot_transitions
            .iter()
            .find(|t| t.operation_id == plan.operation_id)
        {
            ensure!(old == plan, "boot transition ID payload conflict");
            return Ok(());
        }
        ensure!(
            plan.operation_id != self.state.initial_transport.operation_id
                && plan.node_id == self.state.node.node_id
                && plan.transport_owner_id == self.state.initial_transport.transport_owner_id
                && plan.expected_boot_id == self.state.node.boot_id
                && plan.expected_next_sequence == self.state.next_sequence
                && self.state.boot_transitions.len() < 64
                && plan.new_boot_id != self.state.initial_transport.node.boot_id
                && !self
                    .state
                    .boot_transitions
                    .iter()
                    .any(|t| t.new_boot_id == plan.new_boot_id),
            "boot transition owner/CAS refused; retained spool required"
        );
        let mut next = self.state.clone();
        let old_sources=next.sources.iter().filter(|(_,s)|matches!(s.normalizer.source.binding.session,SessionIdentity::Remote{boot_id,..}if boot_id==plan.expected_boot_id)).map(|(id,_)|*id).collect::<Vec<_>>();
        for id in old_sources {
            let gap = boot_fence_gap(&next, id, plan.operation_id, observed_at_ms)?;
            next.next_sequence = next
                .next_sequence
                .checked_add(1)
                .context("node sequence exhausted")?;
            next.outbox.push(gap);
            next.sources.get_mut(&id).unwrap().held_for_reconciliation = true;
        }
        next.node.boot_id = plan.new_boot_id;
        next.boot_transitions.push(plan.clone());
        ensure!(
            within(&next)?,
            "reserved boot-gap capacity full; retain existing transport and require recovery"
        );
        self.commit(next)
    }
    pub fn inject_write_fault(&mut self, fault: WriteFault) {
        self.fault = fault;
    }
    fn commit(&mut self, state: State) -> Result<()> {
        ensure!(
            !self.poisoned,
            "spool persistence uncertain; reopen required"
        );
        let bytes = serde_json::to_vec(&Envelope {
            digest: state_digest(&state)?,
            state: state.clone(),
        })?;
        ensure!(
            bytes.len() <= self.state.limits.journal_bytes,
            "spool metadata retention full; hold cursor"
        );
        self.poisoned = true;
        let temp = std::ffi::CString::new("pending.tmp")?;
        let dest = std::ffi::CString::new("spool.json")?;
        let fd = unsafe {
            libc::openat(
                self.directory.as_raw_fd(),
                temp.as_ptr(),
                libc::O_WRONLY | libc::O_CREAT | libc::O_EXCL | libc::O_NOFOLLOW | libc::O_CLOEXEC,
                0o600,
            )
        };
        if fd < 0 {
            return Err(std::io::Error::last_os_error().into());
        }
        let mut file = unsafe { File::from_raw_fd(fd) };
        file.write_all(&bytes)?;
        file.sync_all()?;
        if self.fault == WriteFault::BeforeRenameRetainedTemp {
            anyhow::bail!("injected durable-temp boundary; temporary file retained");
        }
        if self.fault == WriteFault::BeforeRename {
            unsafe { libc::unlinkat(self.directory.as_raw_fd(), temp.as_ptr(), 0) };
            anyhow::bail!("injected pre-rename persistence failure");
        }
        ensure!(
            unsafe {
                libc::renameat(
                    self.directory.as_raw_fd(),
                    temp.as_ptr(),
                    self.directory.as_raw_fd(),
                    dest.as_ptr(),
                )
            } == 0,
            "spool atomic rename failed"
        );
        if self.fault == WriteFault::AfterRename {
            anyhow::bail!("injected post-rename persistence uncertainty");
        }
        self.directory.sync_all()?;
        self.state = state;
        self.poisoned = false;
        Ok(())
    }
    pub fn attach_file(&mut self, normalizer: Normalizer, permit: SourcePermit) -> Result<()> {
        ensure!(
            normalizer.source.format != Format::OpenCodeSessionDto6c24ec9
                && permit.source_id == normalizer.source.source_id
                && permit.native_session_key == normalizer.source.native_session_key,
            "file source permit mismatch"
        );
        SourceFile::open(permit.clone())?;
        self.attach(normalizer, Some(permit))
    }
    pub fn attach_record_channel(&mut self, normalizer: Normalizer) -> Result<()> {
        ensure!(
            normalizer.source.format == Format::OpenCodeSessionDto6c24ec9
                && normalizer.observed_native_identity,
            "record channel scope not admitted"
        );
        self.attach(normalizer, None)
    }
    fn attach(&mut self, normalizer: Normalizer, permit: Option<SourcePermit>) -> Result<()> {
        remote(&self.state.node, &normalizer.source.binding)?;
        normalizer.source.validate()?;
        let id = normalizer.source.source_id;
        if let Some(old) = self.state.sources.get(&id) {
            ensure!(
                old.normalizer.source == normalizer.source && old.permit == permit,
                "source ID payload conflict"
            );
            return Ok(());
        }
        ensure!(
            normalizer.next_sequence == 1
                && normalizer.seen.is_empty()
                && normalizer.native_messages.is_empty()
                && normalizer.display_order.is_empty()
                && normalizer.mutations.is_empty()
                && normalizer.reparse.is_none(),
            "new source cannot invent consumed history"
        );
        let mut next = self.state.clone();
        ensure!(
            next.sources.len() < next.limits.sources,
            "node source count full"
        );
        next.sources.insert(
            id,
            SourceState {
                normalizer,
                permit,
                cursor: Cursor::default(),
                discarding: false,
                record_cursor: None,
                held_for_reconciliation: false,
                last_change: None,
                history_checkpoint: None,
                replay: None,
                completed_replays: vec![],
            },
        );
        self.commit(next)
    }
    pub fn cursor(&self, id: Uuid) -> Result<Cursor> {
        Ok(self
            .state
            .sources
            .get(&id)
            .context("source absent")?
            .cursor
            .clone())
    }
    pub fn health(&self) -> Result<Health> {
        let (_, bytes, gaps, _) = lane_usage(&self.state.outbox)?;
        Ok(Health {
            queued_events: self.state.outbox.len(),
            ordinary_bytes: bytes,
            reserved_gap_events: gaps,
            held_sources: self
                .state
                .sources
                .values()
                .filter(|s| s.held_for_reconciliation)
                .count(),
            acked_through: self.state.acked_through,
            pending_message: self
                .state
                .pending
                .as_ref()
                .map(|p| p.packet.body.message_id),
            persistence_uncertain: self.poisoned,
        })
    }
    pub fn collect(
        &mut self,
        id: Uuid,
        file: &SourceFile,
        observed_at_ms: u64,
        policy: OverflowPolicy,
    ) -> Result<Collected> {
        ensure!(!self.poisoned, "spool persistence uncertain");
        let source = self.state.sources.get(&id).context("source absent")?;
        ensure!(
            source.permit.as_ref() == Some(file.permit()),
            "source handle differs from admitted permit"
        );
        ensure!(
            source.replay.is_none(),
            "live collection excluded during parser reconciliation"
        );
        ensure!(
            !source.held_for_reconciliation,
            "source held after explicit loss; reconciliation required"
        );
        ensure!(
            source.normalizer.parser == source.normalizer.source.parser(),
            "parser changed; explicit retained reconciliation required"
        );
        let read = match file.read(&source.cursor, framing::MAX_READ_BYTES)? {
            ReadOutcome::Changed(reason) => {
                let mut added = 0;
                if source.last_change.as_ref() != Some(&reason) {
                    let gap = change_gap(&self.state, id, &reason, observed_at_ms)?;
                    let mut next = self.state.clone();
                    next.next_sequence = next
                        .next_sequence
                        .checked_add(1)
                        .context("sequence exhausted")?;
                    next.outbox.push(gap);
                    next.sources.get_mut(&id).unwrap().last_change = Some(reason.clone());
                    ensure!(
                        within(&next)?,
                        "reserved change-gap capacity full; source cursor retained"
                    );
                    self.commit(next)?;
                    added = 1;
                }
                return Ok(Collected {
                    consumed_bytes: 0,
                    facts_added: added,
                    partial: false,
                    blocked: true,
                    source_change: Some(reason),
                });
            }
            ReadOutcome::Ready(read) => read,
        };
        let framed = framing::frame(&read.bytes, read.start, source.discarding)?;
        // Idle/partial polls do not clone unrelated histories or serialize the outbox.
        if framed.records.is_empty() {
            return Ok(Collected {
                consumed_bytes: 0,
                facts_added: 0,
                partial: framed.partial,
                blocked: false,
                source_change: None,
            });
        }
        let original_offset = source.cursor.offset;
        let mut tentative = source.clone();
        let mut sequence = self.state.next_sequence;
        let mut usage = LaneUsage::retained(&self.state.outbox)?;
        let mut pending = Vec::new();
        let mut blocked = false;
        let mut consumed = 0usize;
        for frame in framed.records {
            tentative.normalizer.next_sequence = sequence;
            let (normalizer, events) = tentative.normalizer.apply(&frame, observed_at_ms)?;
            if let Some(updated) = usage.adding(&events, &self.state.limits)? {
                tentative.normalizer = normalizer;
                sequence = tentative.normalizer.next_sequence;
                tentative.last_change = None;
                tentative.cursor = read.cursor_after(consumed + frame.bytes.len())?;
                tentative.discarding = frame.oversized && !frame.bytes.ends_with(b"\n");
                usage = updated;
                pending.extend(events);
                consumed += frame.bytes.len();
                continue;
            }
            blocked = true;
            if policy == OverflowPolicy::GapAndHold {
                let gap = loss_gap(
                    &tentative.normalizer.source,
                    sequence,
                    &frame,
                    observed_at_ms,
                )?;
                if usage
                    .adding(std::slice::from_ref(&gap), &self.state.limits)?
                    .is_some()
                {
                    sequence = sequence.checked_add(1).context("sequence exhausted")?;
                    pending.push(gap);
                    tentative.cursor = read.cursor_after(consumed + frame.bytes.len())?;
                    tentative.held_for_reconciliation = true;
                }
            }
            break;
        }
        let consumed_bytes = tentative.cursor.offset - original_offset;
        let added = pending.len();
        if consumed_bytes > 0 {
            // One whole-journal commit per bounded batch; still O(retained metadata).
            let mut next = self.state.clone();
            next.sources.insert(id, tentative);
            next.next_sequence = sequence;
            next.outbox.extend(pending);
            self.commit(next)?;
        }
        Ok(Collected {
            consumed_bytes,
            facts_added: added,
            partial: framed.partial,
            blocked,
            source_change: None,
        })
    }
    /// Host-admitted replacement only. A gap is recorded before reset; prior
    /// native usage identities survive. No old writer or credential is revived.
    pub fn adopt_file_generation(
        &mut self,
        id: Uuid,
        new_permit: SourcePermit,
        new_generation: u64,
        observed_at_ms: u64,
    ) -> Result<()> {
        let old = self.state.sources.get(&id).context("source absent")?;
        remote(&self.state.node, &old.normalizer.source.binding)?;
        ensure!(
            old.permit.is_some()
                && old.replay.is_none()
                && new_permit.source_id == id
                && new_permit.native_session_key == old.normalizer.source.native_session_key
                && old.normalizer.source.generation.checked_add(1) == Some(new_generation),
            "source generation adoption mismatch"
        );
        SourceFile::open(new_permit.clone())?;
        let gap = change_gap(
            &self.state,
            id,
            &old.last_change.clone().unwrap_or(SourceChange::Rotated),
            observed_at_ms,
        )?;
        let mut next = self.state.clone();
        next.next_sequence = next
            .next_sequence
            .checked_add(1)
            .context("sequence exhausted")?;
        next.outbox.push(gap);
        let source = next.sources.get_mut(&id).unwrap();
        source.permit = Some(new_permit);
        source.normalizer.source.generation = new_generation;
        source.normalizer.observed_native_identity = false;
        source.normalizer.model = None;
        source.normalizer.active_turn = None;
        source.normalizer.begin_history_replay();
        source.cursor = Cursor::default();
        source.discarding = false;
        source.last_change = None;
        source.held_for_reconciliation = false;
        source.history_checkpoint = None;
        ensure!(within(&next)?, "reserved generation-gap capacity full");
        self.commit(next)
    }
    /// A checkpoint reference is provenance, not authentication. The eventual
    /// host adapter must verify the central/native replay boundary independently.
    pub fn finish_history_replay(
        &mut self,
        id: Uuid,
        generation: u64,
        checkpoint: ArtifactRef,
    ) -> Result<()> {
        artifact(&checkpoint)?;
        let source = self.state.sources.get(&id).context("source absent")?;
        ensure!(
            source.normalizer.source.generation == generation
                && source.replay.is_none()
                && source.normalizer.observed_native_identity
                && !source.held_for_reconciliation,
            "history boundary scope/status mismatch"
        );
        let mut next = self.state.clone();
        let source = next.sources.get_mut(&id).unwrap();
        source.normalizer.finish_history_replay();
        source.history_checkpoint = Some(checkpoint);
        self.commit(next)
    }
    pub fn ingest_record(
        &mut self,
        id: Uuid,
        expected_cursor: Option<&str>,
        next_cursor: Text,
        bytes: Vec<u8>,
        observed_at_ms: u64,
    ) -> Result<usize> {
        let source = self.state.sources.get(&id).context("source absent")?;
        ensure!(
            source.permit.is_none()
                && source.normalizer.source.format == Format::OpenCodeSessionDto6c24ec9
                && !source.held_for_reconciliation
                && source.replay.is_none()
                && source.record_cursor.as_ref().map(Text::as_str) == expected_cursor,
            "record channel cursor/scope mismatch"
        );
        next_cursor.validate()?;
        let mut normalizer = source.normalizer.clone();
        normalizer.next_sequence = self.state.next_sequence;
        let (normalizer, events) = normalizer.apply(
            &Frame {
                start: 0,
                bytes,
                oversized: false,
            },
            observed_at_ms,
        )?;
        let count = events.len();
        let mut next = self.state.clone();
        next.next_sequence = normalizer.next_sequence;
        let entry = next.sources.get_mut(&id).unwrap();
        entry.normalizer = normalizer;
        entry.record_cursor = Some(next_cursor);
        next.outbox.extend(events);
        ensure!(within(&next)?, "record spool full; retain native cursor");
        self.commit(next)?;
        Ok(count)
    }
    /// A verified handle carries exact retained bytes plus host-owner admission.
    /// Intent is immutable and durable before any replay cursor can advance.
    pub fn begin_reparse(&mut self, replay: &crate::VerifiedReplay) -> Result<()> {
        let plan = replay.plan();
        let digest = plan.digest()?;
        let source = self
            .state
            .sources
            .get(&plan.source_id)
            .context("source absent")?;
        if let Some(done) = source
            .completed_replays
            .iter()
            .find(|r| r.operation_id == plan.operation_id)
        {
            ensure!(
                done.plan_digest == digest,
                "completed reparse ID payload conflict"
            );
            return Ok(());
        }
        if let Some(active) = &source.replay {
            ensure!(
                active.plan.operation_id == plan.operation_id && active.plan_digest == digest,
                "active reparse ID payload conflict"
            );
            return Ok(());
        }
        ensure!(
            source.completed_replays.len() < 16,
            "reconciliation intent history full"
        );
        ensure!(
            source.cursor == plan.retained_cursor,
            "reparse source cursor changed"
        );
        if source.permit.is_some() {
            ensure!(
                source.cursor.offset == plan.transcript.bytes,
                "retained file prefix length differs from consumed source"
            );
        }
        if let Some(anchor) = &source.cursor.anchor {
            let bytes = replay.bytes();
            ensure!(
                bytes.len() >= anchor.bytes as usize
                    && Digest::of(&bytes[bytes.len() - anchor.bytes as usize..]) == anchor.digest,
                "retained source prefix anchor differs"
            );
        }
        let normalizer = source.normalizer.begin_reparse(plan)?;
        let mut next = self.state.clone();
        let source = next.sources.get_mut(&plan.source_id).unwrap();
        source.normalizer = normalizer;
        source.replay = Some(crate::reparse::ReplayState {
            plan: plan.clone(),
            plan_digest: digest,
            offset: 0,
        });
        self.commit(next)
    }
    pub fn replay_next(
        &mut self,
        replay: &crate::VerifiedReplay,
        observed_at_ms: u64,
    ) -> Result<Collected> {
        let plan = replay.plan();
        ensure!(
            observed_at_ms >= plan.transcript.retained_at_ms
                && observed_at_ms < plan.transcript.retained_until_ms,
            "retained raw replay interval expired"
        );
        let source = self
            .state
            .sources
            .get(&plan.source_id)
            .context("source absent")?;
        let active = source.replay.as_ref().context("no active parser replay")?;
        ensure!(
            active.plan.operation_id == plan.operation_id && active.plan_digest == plan.digest()?,
            "replay handle differs from durable intent"
        );
        let start = usize::try_from(active.offset)?;
        ensure!(
            start <= replay.bytes().len(),
            "replay cursor exceeds retained bytes"
        );
        let end = (start + framing::MAX_READ_BYTES).min(replay.bytes().len());
        let framed = framing::frame(&replay.bytes()[start..end], active.offset, false)?;
        if framed.records.is_empty() {
            return Ok(Collected {
                consumed_bytes: 0,
                facts_added: 0,
                partial: framed.partial,
                blocked: false,
                source_change: None,
            });
        }
        let original_offset = active.offset;
        let mut tentative = source.clone();
        let mut sequence = self.state.next_sequence;
        let mut usage = LaneUsage::retained(&self.state.outbox)?;
        let mut pending = Vec::new();
        let mut blocked = false;
        for frame in framed.records {
            tentative.normalizer.next_sequence = sequence;
            let (normalizer, events) = tentative.normalizer.apply(&frame, observed_at_ms)?;
            let Some(updated) = usage.adding(&events, &self.state.limits)? else {
                blocked = true;
                break;
            };
            sequence = normalizer.next_sequence;
            tentative.normalizer = normalizer;
            tentative.replay.as_mut().unwrap().offset = frame.end()?;
            usage = updated;
            pending.extend(events);
        }
        let consumed = tentative.replay.as_ref().unwrap().offset - original_offset;
        let added = pending.len();
        if consumed > 0 {
            let mut next = self.state.clone();
            next.sources.insert(plan.source_id, tentative);
            next.next_sequence = sequence;
            next.outbox.extend(pending);
            self.commit(next)?;
        }
        Ok(Collected {
            consumed_bytes: consumed,
            facts_added: added,
            partial: framed.partial,
            blocked,
            source_change: None,
        })
    }
    pub fn finish_reparse(
        &mut self,
        source_id: Uuid,
        operation_id: Uuid,
        plan_digest: &Digest,
    ) -> Result<()> {
        let source = self
            .state
            .sources
            .get(&source_id)
            .context("source absent")?;
        if let Some(done) = source
            .completed_replays
            .iter()
            .find(|r| r.operation_id == operation_id)
        {
            ensure!(
                &done.plan_digest == plan_digest,
                "completed replay ID payload conflict"
            );
            return Ok(());
        }
        let replay = source.replay.as_ref().context("no active parser replay")?;
        ensure!(
            replay.plan.operation_id == operation_id
                && &replay.plan_digest == plan_digest
                && replay.offset == replay.plan.transcript.bytes,
            "parser replay identity or cursor incomplete"
        );
        let parsed = source
            .normalizer
            .reparse
            .as_ref()
            .context("reparse parser state absent")?;
        ensure!(
            parsed.known.keys().all(|key| parsed.handled.contains(key)),
            "selected known facts were not reconciled"
        );
        let mut next = self.state.clone();
        let source = next.sources.get_mut(&source_id).unwrap();
        source.normalizer.reparse = None;
        source.normalizer.finish_history_replay();
        source
            .completed_replays
            .push(crate::reparse::ReplayCompletion {
                operation_id,
                plan_digest: plan_digest.clone(),
            });
        source.replay = None;
        self.commit(next)
    }
    pub fn prepare_packet(&mut self) -> Result<Option<Packet>> {
        if let Some(pending) = &self.state.pending {
            return Ok(Some(pending.packet.clone()));
        }
        if self.state.outbox.is_empty() {
            return Ok(None);
        }
        let mut events = vec![];
        let boot = event_boot(&self.state.outbox[0]);
        for event in self.state.outbox.iter().take(MAX_EVENTS) {
            if event_boot(event) != boot {
                break;
            }
            let mut candidate = events.clone();
            candidate.push(event.clone());
            let batch = ObservationBatch {
                node_id: self.state.node.node_id,
                boot_id: boot,
                first_sequence: candidate[0].body.sequence,
                last_sequence: candidate.last().unwrap().body.sequence,
                events: candidate,
            };
            let id = crate::normalize::stable_id(&serde_json::to_value(&batch)?)?;
            match Packet::seal(id, Message::Batch(batch)) {
                Ok(packet) => {
                    let Message::Batch(batch) = packet.body.message else {
                        unreachable!()
                    };
                    events = batch.events
                }
                Err(_) => break,
            }
        }
        ensure!(!events.is_empty(), "single event cannot fit batch bound");
        let batch = ObservationBatch {
            node_id: self.state.node.node_id,
            boot_id: boot,
            first_sequence: events[0].body.sequence,
            last_sequence: events.last().unwrap().body.sequence,
            events,
        };
        let id = crate::normalize::stable_id(&serde_json::to_value(&batch)?)?;
        let packet = Packet::seal(id, Message::Batch(batch))?;
        let mut next = self.state.clone();
        next.pending = Some(Pending {
            packet: packet.clone(),
            attempts: 0,
            first_ms: None,
            last_ms: None,
            next_retry_ms: 0,
        });
        self.commit(next)?;
        Ok(Some(packet))
    }
    pub fn begin_send(&mut self, now_ms: u64) -> Result<Option<Packet>> {
        let Some(packet) = self.prepare_packet()? else {
            return Ok(None);
        };
        let mut next = self.state.clone();
        let pending = next.pending.as_mut().unwrap();
        ensure!(
            pending.attempts < next.limits.max_send_attempts
                && pending.last_ms.is_none_or(|last| now_ms >= last)
                && pending.first_ms.is_none_or(
                    |first| now_ms.saturating_sub(first) < next.limits.max_send_elapsed_ms
                )
                && now_ms >= pending.next_retry_ms,
            "outbound retry bound exhausted; intent retained for reconciliation"
        );
        pending.first_ms.get_or_insert(now_ms);
        pending.last_ms = Some(now_ms);
        pending.attempts += 1;
        pending.next_retry_ms = now_ms
            .checked_add(
                250u64
                    .checked_shl(pending.attempts - 1)
                    .unwrap_or(2000)
                    .min(2000),
            )
            .context("retry clock overflow")?;
        self.commit(next)?;
        Ok(Some(packet))
    }
    pub fn acknowledge(&mut self, receipt: &Receipt) -> Result<()> {
        if self.state.last_receipt.as_ref() == Some(receipt) {
            return Ok(());
        }
        let pending = self
            .state
            .pending
            .as_ref()
            .context("no pending batch receipt")?;
        receipt.validate_against(&pending.packet)?;
        let Message::Batch(batch) = &pending.packet.body.message else {
            unreachable!()
        };
        let count = batch.events.len();
        let mut next = self.state.clone();
        ensure!(
            next.outbox[..count] == batch.events,
            "pending batch differs from retained facts"
        );
        next.outbox.drain(..count);
        next.acked_through = batch.last_sequence;
        next.pending = None;
        next.last_receipt = Some(receipt.clone());
        self.commit(next)
    }
    pub fn path(&self) -> &Path {
        &self.root
    }
}
fn loss_gap(
    source: &crate::Source,
    sequence: u64,
    frame: &Frame,
    observed_at_ms: u64,
) -> Result<ObservationRecord> {
    let cursor = SourceCursor::Bytes {
        start: frame.start,
        end: frame.end()?,
    };
    let body = Observation {
        binding: source.binding.clone(),
        provenance: Provenance {
            source_id: source.source_id,
            source_generation: source.generation,
            source_kind: SourceKind::NativeTranscript,
            native_session_key: source.native_session_key.clone(),
            cursor: cursor.clone(),
            parser: source.parser(),
            raw_record: RawRecordEvidence::Available {
                digest: Digest::of(&frame.bytes),
                byte_length: frame.bytes.len() as u64,
            },
        },
        sequence,
        observed_at_ms,
        kind: ObservationKind::Gap {
            reason: GapReason::SpoolOverflow,
            affected: Some(cursor),
        },
    };
    let event_id = crate::normalize::stable_id(&serde_json::to_value(&body)?)?;
    ObservationRecord::seal(event_id, body).map_err(Into::into)
}

fn change_gap(
    state: &State,
    id: Uuid,
    change: &SourceChange,
    observed_at_ms: u64,
) -> Result<ObservationRecord> {
    let source = &state.sources[&id];
    let normal = &source.normalizer;
    let reason = match change {
        SourceChange::Missing => GapReason::SourceMissing,
        SourceChange::Rotated => GapReason::Rotated,
        SourceChange::Truncated | SourceChange::AnchorChanged => GapReason::Truncated,
    };
    let cursor = SourceCursor::Record {
        id: Text::new(format!(
            "source-change:{change:?}:{}:{}",
            normal.source.generation, source.cursor.offset
        ))?,
    };
    let parser = ArtifactRef {
        id: Text::new("source-handle-monitor")?,
        version: Text::new("0.1.0")?,
        digest: Digest::of(
            &[
                include_bytes!("source.rs").as_slice(),
                include_bytes!("spool.rs").as_slice(),
            ]
            .concat(),
        ),
    };
    let body = Observation {
        binding: normal.source.binding.clone(),
        provenance: Provenance {
            source_id: id,
            source_generation: normal.source.generation,
            source_kind: SourceKind::NativeTranscript,
            native_session_key: normal.source.native_session_key.clone(),
            cursor,
            parser,
            raw_record: RawRecordEvidence::Unavailable {
                reason: reason.clone(),
            },
        },
        sequence: state.next_sequence,
        observed_at_ms,
        kind: ObservationKind::Gap {
            reason,
            affected: None,
        },
    };
    let event_id = crate::normalize::stable_id(&serde_json::to_value(&body)?)?;
    ObservationRecord::seal(event_id, body).map_err(Into::into)
}

fn boot_fence_gap(
    state: &State,
    id: Uuid,
    operation_id: Uuid,
    observed_at_ms: u64,
) -> Result<ObservationRecord> {
    let source = &state.sources[&id].normalizer.source;
    let reason = GapReason::ReplayUnavailable;
    let body = Observation {
        binding: source.binding.clone(),
        provenance: Provenance {
            source_id: id,
            source_generation: source.generation,
            source_kind: if source.format == Format::OpenCodeSessionDto6c24ec9 {
                SourceKind::StructuredProviderEvent
            } else {
                SourceKind::NativeTranscript
            },
            native_session_key: source.native_session_key.clone(),
            cursor: SourceCursor::Record {
                id: Text::new(format!("boot-fence:{operation_id}:{id}"))?,
            },
            parser: ArtifactRef {
                id: Text::new("boot-source-fence")?,
                version: Text::new("0.1.0")?,
                digest: Digest::of(
                    &[
                        include_bytes!("boot.rs").as_slice(),
                        include_bytes!("spool.rs").as_slice(),
                    ]
                    .concat(),
                ),
            },
            raw_record: RawRecordEvidence::Unavailable {
                reason: reason.clone(),
            },
        },
        sequence: state.next_sequence,
        observed_at_ms,
        kind: ObservationKind::Gap {
            reason,
            affected: None,
        },
    };
    let id = crate::normalize::stable_id(&serde_json::to_value(&body)?)?;
    ObservationRecord::seal(id, body).map_err(Into::into)
}
