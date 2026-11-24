//! One observation service per node: one managed store, one writer thread, one
//! gapless node sequence. Attempts add and remove transcript sources while it
//! runs. The thread drives the transport owner (Prepare, Activate, boot
//! advance), collects every source in turn and delivers receipt-backed batches.
//!
//! Nothing here blocks a launch: `Observations::register` only validates its
//! arguments and queues a command. A source that is missing, unreadable or
//! refused is marked incomplete and skipped; the others keep their turn.
//!
//! The store keeps every source and its transcript cursor across a worker
//! restart. An attempt that survived the restart is registered again with the
//! same `SourceSpec` and continues from that cursor; a stored source nobody
//! registers within `resume_grace` gets the store's explicit "fenced by worker
//! restart" record and is released.
use super::*;
mod rollout;
mod transcripts;
use transcripts::{Admission, admit};
mod release;
use aq_admission::observation_source;
use aq_observation::{
    discovery::{Discovery, Resolution},
    source::SourceFile,
    spool::OverflowPolicy,
};
use aq_observation_admission::{BatchReply, FENCED_BY_RESTART, INCOMPLETE_UNREADABLE};
use aq_observation_engine as engine;
use std::{
    collections::{BTreeMap, BTreeSet},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
        mpsc,
    },
    time::{Duration, Instant},
};

/// The launch-time source generation (the server's, not the assignment's).
const GENERATION: u64 = 1;
/// Two files carrying one native session key are refused as ambiguous.
const DISCOVERY_CANDIDATES: usize = 2;
/// Consecutive failed collections before a source is given up as incomplete.
const STRIKES: u8 = 3;

/// The server as the node sees it: transport ownership and batch upload.
pub trait NodePeer: AuthenticatedOwnerPeer {
    /// The receipt covers the whole batch; `refused_sources` lists the sources
    /// whose records the server dropped.
    fn batch(&mut self, packet: &Packet) -> Result<BatchReply>;
}
impl NodePeer for HttpOwnerPeer {
    fn batch(&mut self, packet: &Packet) -> Result<BatchReply> {
        HttpOwnerPeer::batch(self, packet)
    }
}

/// Every limit of the node service. The three store sizes are fixed when the
/// node's store is first created; the others apply at each start.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NodeLimits {
    /// Hard size of the store file on disk.
    pub store_file_bytes: u64,
    /// Retained rows in the store (unsent events and per-source parser state).
    pub store_data_bytes: u64,
    /// Page cache of the store, the service's main memory use.
    pub store_cache_bytes: usize,
    /// Transcript items remembered per source for deduplication: the last N.
    /// Older ones are forgotten; a session of any length keeps being collected.
    pub tracked_identities: usize,
    /// Sources (running attempts) the store holds at once. A further one waits
    /// for a finished source to be released.
    pub max_sources: u64,
    /// How long a stored source of an earlier boot waits to be registered
    /// again before it is fenced and released.
    pub resume_grace: Duration,
    /// Directory entries examined when a transcript is found by its session key.
    pub discovery_entries: usize,
    /// Pause between rounds when no source had new bytes.
    pub poll: Duration,
    /// First pause after a failed server call; doubles up to `retry_max`.
    pub retry_start: Duration,
    pub retry_max: Duration,
    /// How long a finished source may keep draining its transcript.
    pub finish_drain: Duration,
}
impl Default for NodeLimits {
    fn default() -> Self {
        Self {
            store_file_bytes: 64 * 1024 * 1024,
            store_data_bytes: 16 * 1024 * 1024,
            store_cache_bytes: 4 * 1024 * 1024,
            tracked_identities: 4096,
            max_sources: 16,
            resume_grace: Duration::from_secs(120),
            discovery_entries: 256,
            poll: Duration::from_millis(250),
            retry_start: Duration::from_secs(1),
            retry_max: Duration::from_secs(30),
            finish_drain: Duration::from_secs(5),
        }
    }
}

/// One attempt's transcript. The transcript is written by the sandbox, so it is
/// read as untrusted input: exact file, no symlink, inside `authorized_root`.
#[derive(Clone, Debug)]
pub struct SourceSpec {
    /// `aq_admission::transport::lifecycle_binding(&offer)`.
    pub binding: Binding,
    /// `aq_admission::observation_source::entry(&manifest.modules)`.
    pub catalog_entry: ArtifactRef,
    /// `ClaudeV1Jsonl9a9b8e8` or `CodexRollout67f0994`.
    pub format: engine::Format,
    /// Absolute path on the node of the transcript file, or of the directory
    /// that will hold it (the file is then found by `native_session_key`).
    pub transcript: PathBuf,
    /// Directory the transcript must stay inside (the attempt's transcript mount).
    pub authorized_root: PathBuf,
    /// The harness's own session ID (Claude `sessionId`, Codex rollout ID). Empty
    /// for Codex, which chooses its rollout ID itself: `transcript` is then its
    /// `sessions` directory and the ID is read from the first rollout's file name.
    pub native_session_key: String,
    /// Provider and account reference of the launch's credential lease.
    pub provider: String,
    pub account_reference: String,
    /// The host uid that owns the transcript when it is not the worker's.
    pub owner_uid: Option<u32>,
}
impl SourceSpec {
    /// The source of a docker-local launch whose transcript directory is mounted at
    /// `transcript` on the node. `None` when the launch delivers no catalog entry:
    /// the owner's catalog has no source for this provider and harness.
    pub fn of_launch(
        binding: Binding,
        modules: &[ArtifactRef],
        harness: &str,
        transcript: PathBuf,
        provider: &str,
        account_reference: &str,
    ) -> Result<Option<Self>> {
        let Some(catalog_entry) = observation_source::entry(modules)? else {
            return Ok(None);
        };
        let (format, native_session_key) = match harness {
            // The node driver starts `claude --session-id <attempt>`.
            "claude" => (
                engine::Format::ClaudeV1Jsonl9a9b8e8,
                binding.attempt_id.to_string(),
            ),
            "codex" => (engine::Format::CodexRollout67f0994, String::new()),
            other => anyhow::bail!("no transcript format for harness {other:?}"),
        };
        Ok(Some(Self {
            binding,
            catalog_entry,
            format,
            authorized_root: transcript.clone(),
            transcript,
            native_session_key,
            provider: provider.into(),
            account_reference: account_reference.into(),
            owner_uid: None,
        }))
    }
    /// The source the server enrolls for this attempt: both sides derive its ID
    /// and session key from the delivered catalog entry.
    pub fn source(&self) -> Result<engine::Source> {
        ensure!(
            matches!(
                self.format,
                engine::Format::ClaudeV1Jsonl9a9b8e8 | engine::Format::CodexRollout67f0994
            ),
            "unsupported transcript format"
        );
        let plain = |path: &Path| {
            path.is_absolute()
                && path.components().all(|c| {
                    !matches!(
                        c,
                        std::path::Component::ParentDir | std::path::Component::CurDir
                    )
                })
        };
        ensure!(
            plain(&self.transcript)
                && plain(&self.authorized_root)
                && self.transcript.starts_with(&self.authorized_root),
            "transcript must be an absolute path inside its authorized root"
        );
        let attempt = self.binding.attempt_id;
        let registered = observation_source::session_key(attempt, GENERATION)?;
        // Until the rollout is found (`codex_rollout`) the registered key stands in.
        let native_session_key = match self.native_session_key.as_str() {
            "" if self.format == engine::Format::CodexRollout67f0994 => registered.clone(),
            key => Text::new(key)?,
        };
        let source = engine::Source {
            binding: self.binding.clone(),
            source_id: observation_source::source_id(attempt, GENERATION, &self.catalog_entry)?,
            generation: GENERATION,
            native_session_key,
            format: self.format.clone(),
            provider: Text::new(self.provider.as_str())?,
            account_reference: Text::new(self.account_reference.as_str())?,
            registered_session_key: Some(registered),
        };
        source.validate()?;
        Ok(source)
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SourceStatus {
    /// Registered; the transcript or the node's transport is not there yet.
    Waiting,
    Collecting,
    /// Events of this source are missing; the reason is local evidence only.
    Incomplete(String),
}
type Statuses = Arc<Mutex<BTreeMap<Uuid, SourceStatus>>>;

enum Command {
    Register(Box<(SourceSpec, engine::Source)>),
    Finish(Uuid),
}

/// Registers sources with a running node service. Cheap to clone.
#[derive(Clone)]
pub struct Observations {
    commands: mpsc::Sender<Command>,
    statuses: Statuses,
}
impl Observations {
    /// Starts observing one attempt's transcript and returns at once; collection
    /// begins when the file appears. Dropping the handle finishes the source: a
    /// last drain of the transcript, then its release once its events are
    /// acknowledged. An error means the arguments were refused.
    ///
    /// This is also how an attempt that survived a worker restart resumes: the
    /// recovery code registers the same `SourceSpec` (the binding of the
    /// original launch, so the same source ID at generation 1) within
    /// `resume_grace` of the worker's start. The store still holds the source
    /// and its cursor, so collection continues where it stopped, with no
    /// duplicate and no missing sequence.
    pub fn register(&self, spec: SourceSpec) -> Result<SourceHandle> {
        let source = spec.source()?;
        let id = source.source_id;
        {
            let mut statuses = self.statuses.lock().unwrap();
            ensure!(!statuses.contains_key(&id), "source already registered");
            statuses.insert(id, SourceStatus::Waiting);
        }
        self.commands
            .send(Command::Register(Box::new((spec, source))))
            .map_err(|_| anyhow::anyhow!("node observation service stopped"))?;
        Ok(SourceHandle {
            id,
            observations: self.clone(),
        })
    }
}
pub struct SourceHandle {
    id: Uuid,
    observations: Observations,
}
impl SourceHandle {
    pub fn source_id(&self) -> Uuid {
        self.id
    }
    /// `None` once the source is released.
    pub fn status(&self) -> Option<SourceStatus> {
        self.observations
            .statuses
            .lock()
            .unwrap()
            .get(&self.id)
            .cloned()
    }
}
impl Drop for SourceHandle {
    fn drop(&mut self) {
        let _ = self.observations.commands.send(Command::Finish(self.id));
    }
}

/// The running service. Dropping it stops the thread at its next round; the
/// store is durable, so nothing needs to be flushed.
pub struct Node {
    observations: Observations,
    stop: Arc<AtomicBool>,
    thread: Option<std::thread::JoinHandle<()>>,
}
impl Node {
    /// `directory` is the node's private observation directory (created 0700);
    /// the store lives in `<directory>/<node_id>` and survives restarts.
    /// `boot_id` is the boot this process registers with the server. `log`
    /// receives one line per state change.
    pub fn start(
        directory: &Path,
        node_id: Uuid,
        boot_id: Uuid,
        limits: NodeLimits,
        peer: impl NodePeer + Send + 'static,
        log: impl Fn(String) + Send + 'static,
    ) -> Result<Self> {
        ensure!(!node_id.is_nil() && !boot_id.is_nil(), "node identity");
        ensure!(
            limits.tracked_identities >= 16
                && limits.max_sources >= 1
                && (1..=4096).contains(&limits.discovery_entries)
                && !limits.poll.is_zero()
                && !limits.retry_start.is_zero()
                && limits.retry_start <= limits.retry_max,
            "observation limits out of range"
        );
        use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
        std::fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(directory)?;
        std::fs::set_permissions(directory, std::fs::Permissions::from_mode(0o700))?;
        let root = std::fs::canonicalize(directory)?.join(node_id.to_string());
        let (commands, inbox) = mpsc::channel();
        let statuses = Statuses::default();
        let stop = Arc::new(AtomicBool::new(false));
        let mut service = Service {
            root,
            limits: Limits {
                node_id,
                boot_id,
                physical_bytes: limits.store_file_bytes,
                logical_bytes: limits.store_data_bytes,
                cache_bytes: limits.store_cache_bytes,
                quick_repair: false,
            },
            node: limits,
            peer,
            log: Box::new(log),
            statuses: statuses.clone(),
            sources: BTreeMap::new(),
            orphans: BTreeMap::new(),
            released: BTreeSet::new(),
            turn: 0,
            transport: Retry::default(),
            delivery: Retry::default(),
            note: String::new(),
        };
        let stopped = stop.clone();
        let thread = std::thread::Builder::new()
            .name("aq-node-observations".into())
            .spawn(move || service.run(&inbox, &stopped))?;
        Ok(Self {
            observations: Observations { commands, statuses },
            stop,
            thread: Some(thread),
        })
    }
    pub fn observations(&self) -> Observations {
        self.observations.clone()
    }
    /// Stops and waits for the thread (at most one server call).
    pub fn shutdown(mut self) {
        self.stop.store(true, Ordering::SeqCst);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}
impl Drop for Node {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
    }
}

struct Orphan {
    /// When the wait for a resuming registration ends.
    deadline: Instant,
    fenced: bool,
    /// The store's acknowledged sequence at the last refused release.
    release_tried: Option<u64>,
}

#[derive(Default)]
struct Retry {
    at: Option<Instant>,
    pause: Duration,
}
impl Retry {
    fn due(&self) -> bool {
        self.at.is_none_or(|at| Instant::now() >= at)
    }
    fn failed(&mut self, limits: &NodeLimits) {
        self.pause = (self.pause * 2).clamp(limits.retry_start, limits.retry_max);
        self.at = Some(Instant::now() + self.pause);
    }
}

struct Tracked {
    spec: SourceSpec,
    source: engine::Source,
    file: Option<SourceFile>,
    strikes: u8,
    /// No further collection: refused, held by the store, or struck out.
    closed: bool,
    /// The last round read the transcript to its end.
    drained: bool,
    /// Set when the handle is dropped: the drain deadline.
    finish: Option<Instant>,
    /// The store's acknowledged sequence at the last refused release.
    release_tried: Option<u64>,
}

struct Service<P> {
    root: PathBuf,
    limits: Limits,
    node: NodeLimits,
    peer: P,
    log: Box<dyn Fn(String) + Send>,
    statuses: Statuses,
    sources: BTreeMap<Uuid, Tracked>,
    /// Store rows no handle owns: sources of earlier boots, waiting to be
    /// registered again (resumed) or, after `resume_grace`, fenced and released.
    orphans: BTreeMap<Uuid, Orphan>,
    /// Sources released during this boot. Re-reading one from the start would
    /// send its events twice.
    released: BTreeSet<Uuid>,
    turn: usize,
    transport: Retry,
    delivery: Retry,
    note: String,
}

fn wall_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_millis() as u64)
}

impl<P: NodePeer> Service<P> {
    fn run(&mut self, inbox: &mpsc::Receiver<Command>, stop: &AtomicBool) {
        let mut intent = None;
        while !stop.load(Ordering::SeqCst) {
            while let Ok(command) = inbox.try_recv() {
                self.command(command);
            }
            if self.round(&mut intent) {
                continue;
            }
            match inbox.recv_timeout(self.node.poll) {
                Ok(command) => self.command(command),
                Err(mpsc::RecvTimeoutError::Timeout) => {}
                // Every registrar and handle is gone: nothing can be observed.
                Err(mpsc::RecvTimeoutError::Disconnected) => return,
            }
        }
    }
    fn command(&mut self, command: Command) {
        match command {
            Command::Register(registration) => {
                let (spec, source) = *registration;
                let id = source.source_id;
                // A stored source of an earlier boot is resumed, not fenced.
                self.orphans.remove(&id);
                self.sources.insert(
                    id,
                    Tracked {
                        spec,
                        source,
                        file: None,
                        strikes: 0,
                        closed: false,
                        drained: false,
                        finish: None,
                        release_tried: None,
                    },
                );
                if self.released.contains(&id) {
                    self.close(id, "source was already finished on this boot".into());
                }
            }
            Command::Finish(id) => {
                if let Some(tracked) = self.sources.get_mut(&id) {
                    tracked.finish = Some(Instant::now() + self.node.finish_drain);
                    tracked.drained = false;
                }
            }
        }
    }
    /// The transport and delivery state, logged only when it changes.
    fn note(&mut self, note: String) {
        if self.note != note {
            (self.log)(format!("node observations: {note}"));
            self.note = note;
        }
    }
    fn status(&mut self, id: Uuid, status: SourceStatus) {
        let mut statuses = self.statuses.lock().unwrap();
        if statuses.get(&id) != Some(&status) {
            if let SourceStatus::Incomplete(reason) = &status {
                (self.log)(format!("observation source {id} incomplete: {reason}"));
            }
            statuses.insert(id, status);
        }
    }
    fn close(&mut self, id: Uuid, reason: String) {
        if let Some(tracked) = self.sources.get_mut(&id) {
            tracked.closed = true;
        }
        self.status(id, SourceStatus::Incomplete(reason));
    }
    /// One pass over transport, delivery and every source. `true` when another
    /// pass should follow without a pause.
    fn round(&mut self, intent: &mut Option<LocalIntent>) -> bool {
        if intent.is_none() {
            if !self.transport.due() {
                return false;
            }
            match self.activate() {
                Ok(active) => {
                    self.transport = Retry::default();
                    self.note("transport owner active".into());
                    *intent = Some(active);
                }
                Err(error) => {
                    self.transport.failed(&self.node);
                    self.note(format!("transport owner not active: {error:#}"));
                    return false;
                }
            }
        }
        let active = intent.as_ref().expect("intent set above");
        match active.with_active(|store| self.work(store)) {
            Ok(busy) => busy,
            Err(error) => {
                // The store must be reopened (and the owner reconciled) before
                // any further write; source file handles stay valid.
                *intent = None;
                self.transport.failed(&self.node);
                self.note(format!("store reopened after: {error:#}"));
                false
            }
        }
    }
    /// Drives the durable owner handshake from wherever it stopped to `Active`
    /// on this boot. Every step is persisted before its server call, so a
    /// failure is retried by calling this again.
    fn activate(&mut self) -> Result<LocalIntent> {
        let boot = self.limits.boot_id;
        let mut intent = self.open_or_create_intent()?;
        let now = wall_ms();
        // Sources of the ended boot stay collectable so their attempts can resume.
        intent.resume_sources_after_boot();
        loop {
            let peer = &mut self.peer;
            match intent.stage() {
                LocalStage::PreparePending => intent.prepare(peer)?,
                LocalStage::Prepared | LocalStage::CreationStarted => intent.create_store(peer)?,
                LocalStage::StoreCreated => intent.activate(peer, Uuid::new_v4())?,
                LocalStage::ActivationPending => {
                    let operation = intent.pending_activation().context("activation absent")?;
                    if intent.activate(peer, operation).is_err() {
                        intent.resume(peer)?;
                    }
                }
                LocalStage::Active if intent.owner_boot() == Some(boot) => {
                    intent.resume(peer)?;
                    self.resume_orphans(&intent)?;
                    return Ok(intent);
                }
                LocalStage::Active => intent.advance_boot(peer, Uuid::new_v4(), boot, now)?,
                LocalStage::BootPending => {
                    if intent.pending_boot() != Some(boot) {
                        // Refused when the server applied the old request; the
                        // replay below then adopts it and the loop advances again.
                        let _ = intent.retarget_boot(peer, Uuid::new_v4());
                    }
                    intent.recover_boot(peer, now)?;
                }
            }
        }
    }
    fn resume_orphans(&mut self, intent: &LocalIntent) -> Result<()> {
        let stored = intent.with_active(|store| store.source_ids())?;
        let deadline = Instant::now() + self.node.resume_grace;
        self.orphans.retain(|id, _| stored.contains(id));
        for id in stored {
            if !self.sources.contains_key(&id) {
                self.orphans.entry(id).or_insert(Orphan {
                    deadline,
                    fenced: false,
                    release_tried: None,
                });
            }
        }
        Ok(())
    }
    fn open_or_create_intent(&mut self) -> Result<LocalIntent> {
        let (root, boot) = (self.root.clone(), self.limits.boot_id);
        // `intent.pending` exists only while an intent is being replaced, which
        // happens before the server call it describes: the committed intent is
        // still the truth.
        let _ = std::fs::remove_file(root.join("intent.pending"));
        let unstarted = !root.join("intent.json").exists() && !root.join("store").exists();
        if unstarted && root.exists() {
            std::fs::remove_dir_all(&root)?;
        }
        let mut intent = if unstarted {
            self.create()?
        } else {
            LocalIntent::open(&root, boot)?
        };
        let handshaking = !matches!(intent.stage(), LocalStage::Active | LocalStage::BootPending);
        if handshaking && intent.initial_boot() != boot {
            // A handshake of an ended boot can never complete. Its store is
            // still empty, so start over when the server holds no owner for it.
            let status = self.peer.status(self.limits.node_id)?;
            ensure!(
                status.owner.is_none(),
                "the server holds a transport owner from an unfinished handshake of an ended boot"
            );
            drop(intent);
            std::fs::remove_dir_all(&root)?;
            intent = self.create()?;
        }
        Ok(intent)
    }
    fn create(&self) -> Result<LocalIntent> {
        LocalIntent::create(
            &self.root,
            self.limits.clone(),
            Uuid::new_v4(),
            Uuid::new_v4(),
        )
    }
    fn work(&mut self, store: &Store) -> Result<bool> {
        let mut busy = self.deliver(store)?;
        let ids: Vec<Uuid> = self.sources.keys().copied().collect();
        // A different source goes first each round, so one that keeps failing
        // cannot hold the others back.
        self.turn = self.turn.wrapping_add(1);
        for offset in 0..ids.len() {
            let id = ids[(self.turn + offset) % ids.len()];
            match self.collect(store, id) {
                Ok(progress) => busy |= progress,
                Err(error) => {
                    let tracked = self.sources.get_mut(&id).expect("tracked source");
                    tracked.strikes += 1;
                    let reason = format!("{error:#}");
                    if tracked.strikes >= STRIKES {
                        // The server is told why this source stops, when the
                        // store can still write; the local status says it too.
                        let _ = store.fence_source(id, INCOMPLETE_UNREADABLE, wall_ms());
                        self.close(id, format!("unreadable: {reason}"));
                    } else {
                        self.status(id, SourceStatus::Incomplete(reason));
                    }
                    ensure!(
                        !store.snapshot()?.persistence_uncertain,
                        "a failed write for source {id}"
                    );
                }
            }
        }
        self.release(store)?;
        Ok(busy)
    }
    /// One sealed packet at a time. It stays in the store, byte for byte, until
    /// the server's receipt for it arrives; only then is the next one sealed.
    fn deliver(&mut self, store: &Store) -> Result<bool> {
        let snapshot = store.snapshot()?;
        let has_packets = snapshot.delivery.is_some() || snapshot.queued != 0;
        if !self.delivery.due() || !has_packets {
            return Ok(false);
        }
        let Some(packet) = store.prepare()? else {
            return Ok(false);
        };
        let reply = self.peer.batch(&packet).and_then(|reply| {
            reply.validate_against(&packet)?;
            Ok(reply)
        });
        match reply {
            Ok(reply) => {
                store.acknowledge(&reply.receipt)?;
                // The server dropped these sources' records and advanced past
                // them: stop collecting each, keep the others going.
                for refused in &reply.refused_sources {
                    if self.sources.contains_key(&refused.source_id) {
                        self.close(
                            refused.source_id,
                            format!("refused by the server: {:?}", refused.reason),
                        );
                    }
                }
                self.delivery = Retry::default();
                self.note("transport owner active".into());
                Ok(true)
            }
            Err(error) => {
                self.delivery.failed(&self.node);
                self.note(format!("delivery failing, batch kept: {error:#}"));
                Ok(false)
            }
        }
    }
    fn collect(&mut self, store: &Store, id: Uuid) -> Result<bool> {
        let tracked = self.sources.get_mut(&id).expect("tracked source");
        if tracked.closed {
            return Ok(false);
        }
        if tracked.file.is_none() {
            match admit(store, tracked, &self.node, self.limits.boot_id)? {
                Admission::Ready(file) => tracked.file = Some(file),
                Admission::Wait => return Ok(false),
                Admission::Refused(reason) => {
                    self.close(id, reason);
                    return Ok(false);
                }
            }
        }
        let file = tracked.file.as_ref().expect("admitted above");
        // `Hold` leaves bytes in the transcript while the outbox is full: the
        // file is the buffer, so a slow server costs delay, not events.
        let frame = store.collect_file(id, file, wall_ms(), OverflowPolicy::Hold)?;
        tracked.strikes = 0;
        tracked.drained = frame.consumed_bytes == 0 && !frame.blocked;
        let stopped = frame.blocked && frame.consumed_bytes == 0;
        if stopped && let Some(hold) = store.source(id)?.hold {
            // The store queued an explicit gap record for this source.
            self.close(id, format!("held by the store: {hold:?}"));
            return Ok(false);
        }
        self.status(id, SourceStatus::Collecting);
        Ok(frame.consumed_bytes > 0)
    }
}
