//! Candidate embedded observation store and durable owner handshake. Network
//! authentication is supplied by a trusted adapter; no production worker wiring
//! or migration from the qualified JSON reference is provided.
pub mod backend;
use anyhow::{Context, Result, ensure};
use aq_lifecycle_protocol::*;
use aq_observation::Source;
use backend::{CappedFile, IdentityGuard, Monitor};
use redb::{
    Database, Durability, ReadableDatabase, ReadableTable, ReadableTableMetadata, TableDefinition,
    WriteTransaction,
};
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use sha2::{Digest as _, Sha256};
use std::{
    cell::Cell,
    fs::File,
    path::{Path, PathBuf},
};
use uuid::Uuid;
mod budget;
mod diagnostics;
mod engine_store;
mod http_owner;
mod intent_files;
mod local_intent;
pub use http_owner::{HttpOwnerConfig, HttpOwnerPeer};
mod owner;
pub use local_intent::{AuthenticatedOwnerPeer, LocalIntent, LocalStage};
mod reader;
mod reparse;
pub use reader::{
    FileGenerationPlan, HistoryBoundary, ReaderState, SourceAuthority, VerifiedFileGeneration,
    VerifiedHistoryBoundary,
};
mod sessions;
pub use engine_store::{CollectedFrame, EngineState};
pub use owner::VerifiedNewStore;
pub use reparse::{ReplayAuthority, ReplayPlan, VerifiedRetained};
pub use sessions::{HoldReason, VerifiedBootAdmission};
const META: TableDefinition<&str, &[u8]> = TableDefinition::new("meta");
const SOURCES: TableDefinition<&[u8], &[u8]> = TableDefinition::new("sources");
const SEEN: TableDefinition<&[u8], &[u8]> = TableDefinition::new("seen");
const EVENTS: TableDefinition<&[u8], &[u8]> = TableDefinition::new("event_ids");
const OUTBOX: TableDefinition<u64, &[u8]> = TableDefinition::new("outbox");
const DELIVERY: TableDefinition<&str, &[u8]> = TableDefinition::new("delivery");
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Limits {
    pub node_id: Uuid,
    pub boot_id: Uuid,
    pub physical_bytes: u64,
    pub logical_bytes: u64,
    pub cache_bytes: usize,
    pub quick_repair: bool,
}
impl Limits {
    pub fn fixture() -> Self {
        Self {
            node_id: Uuid::from_u128(5),
            boot_id: Uuid::from_u128(6),
            physical_bytes: 64 * 1024 * 1024,
            logical_bytes: 16 * 1024 * 1024,
            cache_bytes: 4 * 1024 * 1024,
            quick_repair: false,
        }
    }
    fn validate(&self) -> Result<()> {
        ensure!(
            !self.node_id.is_nil()
                && !self.boot_id.is_nil()
                && (1024 * 1024..=128 * 1024 * 1024).contains(&self.physical_bytes)
                && (4096..=16 * 1024 * 1024).contains(&self.logical_bytes)
                && self.logical_bytes < self.physical_bytes
                && self.ordinary_limit() >= budget::MIN_ORDINARY_BYTES
                && (65536..=8 * 1024 * 1024).contains(&self.cache_bytes),
            "prototype bounds"
        );
        Ok(())
    }
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Meta {
    version: u32,
    control_catalog: Digest,
    transport: Option<aq_observation::InitialTransport>,
    managed: Option<local_intent::Managed>,
    active_boot: Uuid,
    boot_history: Vec<aq_observation::BootTransition>,
    limits: Limits,
    next: u64,
    acked: u64,
    sources: u64,
    seen: u64,
    logical_bytes: u64,
    ordinary: u64,
    ordinary_bytes: u64,
    gaps: u64,
    gap_bytes: u64,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SourceRow {
    pub source: Source,
    pub cursor: u64,
    pub engine: Option<EngineState>,
    pub hold: Option<HoldReason>,
    pub last_control_sequence: Option<u64>,
    pub parsers: Vec<ArtifactRef>,
    pub generations: Vec<u64>,
    pub reader: Option<ReaderState>,
    pub source_operations: Vec<reader::SourceOperation>,
    pub record_cursor: Option<Text>,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SeenRow {
    pub normalized_digest: Digest,
    pub event: EventRef,
    pub raw_record_digests: Vec<Digest>,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Delivery {
    pub packet: Packet,
    pub attempts: u32,
    pub first_ms: Option<u64>,
    pub last_ms: Option<u64>,
    pub next_retry_ms: u64,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Retry {
    message_id: Uuid,
    body_digest: Digest,
    attempts: u32,
    first_ms: Option<u64>,
    last_ms: Option<u64>,
    next_retry_ms: u64,
}
impl Retry {
    fn initial(packet: &Packet) -> Self {
        Self {
            message_id: packet.body.message_id,
            body_digest: packet.body_digest.clone(),
            attempts: 0,
            first_ms: None,
            last_ms: None,
            next_retry_ms: 0,
        }
    }
    fn check(&self, packet: &Packet) -> Result<()> {
        ensure!(
            self.message_id == packet.body.message_id && self.body_digest == packet.body_digest,
            "retry row packet identity mismatch"
        );
        Ok(())
    }
}
fn load_delivery(
    table: &impl ReadableTable<&'static str, &'static [u8]>,
) -> Result<Option<Delivery>> {
    let packet = table
        .get("packet")?
        .map(|v| decode::<Packet>(v.value()))
        .transpose()?;
    let retry = table
        .get("retry")?
        .map(|v| decode::<Retry>(v.value()))
        .transpose()?;
    match (packet, retry) {
        (None, None) => Ok(None),
        (Some(packet), Some(retry)) => {
            retry.check(&packet)?;
            Ok(Some(Delivery {
                packet,
                attempts: retry.attempts,
                first_ms: retry.first_ms,
                last_ms: retry.last_ms,
                next_retry_ms: retry.next_retry_ms,
            }))
        }
        _ => anyhow::bail!("incomplete packet/retry atomic intent"),
    }
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Snapshot {
    pub persistence_uncertain: bool,
    pub active_boot: Uuid,
    pub next: u64,
    pub acked: u64,
    pub queued: u64,
    pub seen: u64,
    pub logical_bytes: u64,
    pub delivery: Option<Delivery>,
    pub receipt: Option<Receipt>,
}
pub struct Store {
    db: Database,
    identity: std::sync::Arc<IdentityGuard>,
    _owner: File,
    _directory: File,
    root: PathBuf,
    limits: Limits,
    monitor: Monitor,
    poisoned: Cell<bool>,
}
fn encode<T: Serialize + ?Sized>(value: &T) -> Result<Vec<u8>> {
    Ok(serde_json::to_vec(value)?)
}
fn decode<T: DeserializeOwned>(value: &[u8]) -> Result<T> {
    Ok(serde_json::from_slice(value)?)
}
fn read_meta(tx: &WriteTransaction) -> Result<Meta> {
    let table = tx.open_table(META)?;
    decode(table.get("state")?.context("metadata absent")?.value())
}
fn write_meta(tx: &WriteTransaction, meta: &Meta) -> Result<()> {
    let bytes = encode(meta)?;
    ensure!(
        bytes.len() as u64 <= budget::META_ALLOWANCE,
        "bounded metadata allowance exceeded"
    );
    ensure!(
        meta.logical_bytes <= meta.limits.data_limit(),
        "hard logical data/metadata reserve"
    );
    tx.open_table(META)?.insert("state", bytes.as_slice())?;
    Ok(())
}
fn stable<T: Serialize>(value: &T) -> Result<Uuid> {
    let mut v = serde_json::to_value(value)?;
    v.sort_all_objects();
    let hash = Sha256::digest(serde_json::to_vec(&v)?);
    let mut id = [0; 16];
    id.copy_from_slice(&hash[..16]);
    id[6] = (id[6] & 15) | 128;
    id[8] = (id[8] & 63) | 128;
    Ok(Uuid::from_bytes(id))
}
fn seen_key(source: Uuid, key: &str) -> Result<Vec<u8>> {
    ensure!(!key.is_empty() && key.len() <= 1024, "seen key bound");
    let mut out = source.as_bytes().to_vec();
    out.extend_from_slice(key.as_bytes());
    Ok(out)
}
// Exact serde envelope length with an empty event vector. UUID/digest text
// lengths are fixed; no candidate is sealed or trusted from a size hint. The
// final actual packet is sealed/validated and its full length rechecked.
fn envelope_bytes(node: Uuid, boot: Uuid, first: u64, last: u64, usage: bool) -> Result<usize> {
    let mut required_capabilities = vec![Capability {
        id: Text::new(OBSERVATION_CAPABILITY)?,
        version: 1,
    }];
    if usage {
        required_capabilities.push(Capability {
            id: Text::new(USAGE_CAPABILITY)?,
            version: 1,
        });
    }
    let length_only = Packet {
        body: PacketBody {
            schema_version: SCHEMA_VERSION,
            message_id: Uuid::from_u128(1),
            required_capabilities,
            message: Message::Batch(ObservationBatch {
                node_id: node,
                boot_id: boot,
                first_sequence: first,
                last_sequence: last,
                events: vec![],
            }),
        },
        body_digest: Digest::of(b"length-only"),
    };
    Ok(encode(&length_only)?.len())
}
impl Store {
    pub fn create(root: &Path, limits: Limits) -> Result<Self> {
        Self::open_inner(root, limits, true, None, None, false)
    }
    pub fn open(root: &Path, limits: Limits) -> Result<Self> {
        Self::open_inner(root, limits, false, None, None, false)
    }
    fn open_inner(
        root: &Path,
        limits: Limits,
        create: bool,
        transport: Option<aq_observation::InitialTransport>,
        managed: Option<local_intent::Managed>,
        managed_open: bool,
    ) -> Result<Self> {
        limits.validate()?;
        let (directory, owner) = backend::private_root(root, create)?;
        if create {
            backend::write_format(&directory)?;
        }
        let monitor = Monitor::default();
        let backend = CappedFile::open(&directory, create, limits.physical_bytes, monitor.clone())?
            .bind_root(root)?;
        let identity = backend.identity_guard();
        let db = Database::builder()
            .set_cache_size(limits.cache_bytes)
            .create_with_backend(backend)?;
        let store = Self {
            db,
            identity,
            _owner: owner,
            _directory: directory,
            root: root.into(),
            limits,
            monitor,
            poisoned: Cell::new(false),
        };
        if create {
            let tx = store.begin_owner()?;
            for table in [SOURCES, SEEN, EVENTS] {
                tx.open_table(table)?;
            }
            tx.open_table(OUTBOX)?;
            tx.open_table(DELIVERY)?;
            for table in engine_store::TABLES {
                tx.open_table(table)?;
            }
            write_meta(
                &tx,
                &Meta {
                    version: budget::FORMAT,
                    control_catalog: budget::catalog_digest(),
                    transport,
                    managed,
                    active_boot: store.limits.boot_id,
                    boot_history: vec![],
                    limits: store.limits.clone(),
                    next: 1,
                    acked: 0,
                    sources: 0,
                    seen: 0,
                    logical_bytes: 0,
                    ordinary: 0,
                    ordinary_bytes: 0,
                    gaps: 0,
                    gap_bytes: 0,
                },
            )?;
            store.commit(tx)?;
            store._directory.sync_all()?;
        }
        let tx = store.db.begin_read()?;
        let table = tx.open_table(META)?;
        let meta: Meta = decode(
            table
                .get("state")?
                .context("metadata absent; no reseeding")?
                .value(),
        )?;
        ensure!(
            meta.version == budget::FORMAT
                && meta.control_catalog == budget::catalog_digest()
                && meta.limits == store.limits
                && (meta.managed.is_none() || managed_open),
            "store version/identity/limits"
        );
        drop(table);
        drop(tx);
        store.audit()?;
        store.monitor.reset();
        Ok(store)
    }
    fn begin(&self) -> Result<WriteTransaction> {
        self.require_collectible()?;
        self.begin_owner()
    }
    fn require_collectible(&self) -> Result<()> {
        let tx = self.db.begin_read()?;
        let table = tx.open_table(META)?;
        let meta: Meta = decode(table.get("state")?.context("metadata absent")?.value())?;
        if let Some(managed) = meta.managed {
            ensure!(
                managed.owner.phase == aq_observation_admission::Phase::Active && !managed.blocked,
                "managed store inactive or recovery held"
            );
        }
        Ok(())
    }
    fn begin_owner(&self) -> Result<WriteTransaction> {
        ensure!(!self.poisoned.get(), "store uncertain; reopen required");
        self.poisoned.set(true);
        self.identity.check()?;
        let mut tx = self.db.begin_write()?;
        tx.set_durability(Durability::Immediate)?;
        tx.set_two_phase_commit(true);
        tx.set_quick_repair(self.limits.quick_repair);
        Ok(tx)
    }
    fn commit(&self, tx: WriteTransaction) -> Result<()> {
        tx.commit()?;
        self.poisoned.set(false);
        Ok(())
    }
    fn abort(&self, tx: WriteTransaction) -> Result<()> {
        tx.abort()?;
        self.poisoned.set(false);
        Ok(())
    }
    pub fn monitor(&self) -> Monitor {
        self.monitor.clone()
    }
    pub fn file_bytes(&self) -> Result<u64> {
        Ok(std::fs::metadata(self.root.join("store.redb"))?.len())
    }
    pub fn attach(&self, source: Source) -> Result<()> {
        source.validate()?;
        ensure!(
            matches!(source.binding.session,SessionIdentity::Remote{node_id,boot_id,..}if node_id==self.limits.node_id&&boot_id!=Uuid::nil()),
            "source node/boot"
        );
        let tx = self.begin()?;
        let mut meta = read_meta(&tx)?;
        let parsers = vec![source.parser()];
        let generations = vec![source.generation];
        let row = SourceRow {
            source,
            cursor: 0,
            engine: None,
            hold: None,
            last_control_sequence: None,
            parsers,
            generations,
            reader: None,
            source_operations: vec![],
            record_cursor: None,
        };
        ensure!(
            matches!(row.source.binding.session,SessionIdentity::Remote{boot_id,..}if boot_id==meta.active_boot),
            "inactive boot source admission"
        );
        let id = row.source.source_id;
        budget::source_policy(&row)?;
        let bytes = encode(&row)?;
        let existing = {
            let table = tx.open_table(SOURCES)?;
            table
                .get(id.as_bytes().as_slice())?
                .map(|v| v.value().to_vec())
        };
        if let Some(old) = existing {
            let old: SourceRow = decode(&old)?;
            ensure!(old.source == row.source, "source ID payload conflict");
            return self.abort(tx);
        }
        {
            let mut table = tx.open_table(SOURCES)?;
            ensure!(meta.sources < budget::MAX_SOURCES, "source quota");
            table.insert(id.as_bytes().as_slice(), bytes.as_slice())?;
        }
        meta.sources += 1;
        meta.logical_bytes += bytes.len() as u64;
        ensure!(
            meta.logical_bytes <= self.limits.ordinary_limit(),
            "logical quota"
        );
        write_meta(&tx, &meta)?;
        self.commit(tx)
    }
    pub fn source(&self, id: Uuid) -> Result<SourceRow> {
        self.source_optional(id)?.context("source absent")
    }
    pub fn source_optional(&self, id: Uuid) -> Result<Option<SourceRow>> {
        let tx = self.db.begin_read()?;
        let table = tx.open_table(SOURCES)?;
        table
            .get(id.as_bytes().as_slice())?
            .map(|v| decode(v.value()))
            .transpose()
    }
    pub fn source_count(&self) -> Result<u64> {
        Ok(self.db.begin_read()?.open_table(SOURCES)?.len()?)
    }
    /// Prototype adapter input is already-normalized evidence; no parser auth claim.
    pub fn append(
        &self,
        id: Uuid,
        expected_cursor: u64,
        next_cursor: u64,
        events: &[ObservationRecord],
        seen: &[(String, SeenRow)],
    ) -> Result<()> {
        ensure!(
            next_cursor > expected_cursor && events.len() <= 32 && seen.len() <= 128,
            "append bounds"
        );
        ensure!(
            events
                .iter()
                .all(|event| !budget::reserved_parser(&event.body.provenance.parser)),
            "reserved local monitor provenance cannot enter raw ingress"
        );
        let tx = self.begin()?;
        let mut meta = read_meta(&tx)?;
        let mut source: SourceRow = {
            let t = tx.open_table(SOURCES)?;
            decode(
                t.get(id.as_bytes().as_slice())?
                    .context("source absent")?
                    .value(),
            )?
        };
        ensure!(
            source.engine.is_none(),
            "raw normalized append excluded for engine-owned source"
        );
        ensure!(
            source.hold.is_none()
                && matches!(source.source.binding.session,SessionIdentity::Remote{boot_id,..}if boot_id==meta.active_boot),
            "raw live source fenced"
        );
        ensure!(source.cursor == expected_cursor, "source cursor conflict");
        {
            let mut out = tx.open_table(OUTBOX)?;
            let mut ids = tx.open_table(EVENTS)?;
            for event in events {
                event.validate()?;
                ensure!(
                    event.body.sequence == meta.next
                        && event.body.binding == source.source.binding
                        && event.body.provenance.source_id == id
                        && event.body.provenance.source_generation == source.source.generation
                        && &event.body.provenance.native_session_key
                            == source.source.provenance_session_key()
                        && event.body.provenance.parser == source.source.parser(),
                    "fact scope/sequence/parser"
                );
                ensure!(
                    ids.get(event.event_id.as_bytes().as_slice())?.is_none(),
                    "event ID already retained"
                );
                let bytes = encode(event)?;
                let digest = encode(&event.body_digest)?;
                ids.insert(event.event_id.as_bytes().as_slice(), digest.as_slice())?;
                out.insert(meta.next, bytes.as_slice())?;
                meta.next = meta.next.checked_add(1).context("sequence exhausted")?;
                meta.logical_bytes += (bytes.len() + digest.len()) as u64;
                if budget::is_urgent(event) {
                    meta.gaps += 1;
                    meta.gap_bytes += bytes.len() as u64;
                } else {
                    meta.ordinary += 1;
                    meta.ordinary_bytes += bytes.len() as u64;
                }
            }
        }
        {
            let mut table = tx.open_table(SEEN)?;
            for (key, row) in seen {
                row.event.validate()?;
                ensure!(
                    !row.raw_record_digests.is_empty() && row.raw_record_digests.len() <= 32,
                    "fingerprint bound"
                );
                let key = seen_key(id, key)?;
                let bytes = encode(row)?;
                if let Some(old) = table.get(key.as_slice())? {
                    meta.logical_bytes -= old.value().len() as u64;
                } else {
                    meta.seen += 1;
                    meta.logical_bytes += key.len() as u64;
                }
                table.insert(key.as_slice(), bytes.as_slice())?;
                meta.logical_bytes += bytes.len() as u64;
            }
        }
        ensure!(
            meta.ordinary <= 256
                && meta.ordinary_bytes <= 2 * 1024 * 1024
                && meta.gaps <= 16
                && meta.gap_bytes <= 65536
                && meta.seen <= 65536
                && meta.logical_bytes <= self.limits.ordinary_limit(),
            "logical event/history quota"
        );
        {
            let mut t = tx.open_table(SOURCES)?;
            let old = t.get(id.as_bytes().as_slice())?.unwrap().value().len();
            source.cursor = next_cursor;
            let bytes = encode(&source)?;
            meta.logical_bytes = meta.logical_bytes - old as u64 + bytes.len() as u64;
            t.insert(id.as_bytes().as_slice(), bytes.as_slice())?;
        }
        ensure!(
            meta.logical_bytes <= self.limits.ordinary_limit(),
            "logical source quota"
        );
        write_meta(&tx, &meta)?;
        self.commit(tx)
    }
    pub fn snapshot(&self) -> Result<Snapshot> {
        let tx = self.db.begin_read()?;
        let meta: Meta = decode(tx.open_table(META)?.get("state")?.unwrap().value())?;
        let delivery = tx.open_table(DELIVERY)?;
        let pending = load_delivery(&delivery)?;
        let receipt = delivery
            .get("receipt")?
            .map(|v| decode(v.value()))
            .transpose()?;
        Ok(Snapshot {
            persistence_uncertain: self.poisoned.get(),
            active_boot: meta.active_boot,
            next: meta.next,
            acked: meta.acked,
            queued: meta.ordinary + meta.gaps,
            seen: meta.seen,
            logical_bytes: meta.logical_bytes,
            delivery: pending,
            receipt,
        })
    }
    pub fn prepare(&self) -> Result<Option<Packet>> {
        self.require_collectible()?;
        ensure!(!self.poisoned.get(), "store uncertain; reopen required");
        if let Some(d) = self.snapshot()?.delivery {
            return Ok(Some(d.packet));
        }
        let tx = self.begin()?;
        let meta = read_meta(&tx)?;
        let mut events = vec![];
        let mut boot = None;
        let mut event_bytes = 0usize;
        let mut usage = false;
        let mut selected_bytes = None;
        {
            let table = tx.open_table(OUTBOX)?;
            for result in table.iter()?.take(MAX_EVENTS) {
                let (_, v) = result?;
                let event: ObservationRecord = decode(v.value())?;
                let candidate_boot = sessions::event_boot(&event)?;
                if boot.is_some_and(|b| b != candidate_boot) {
                    break;
                }
                boot = Some(candidate_boot);
                event.validate()?;
                let candidate_usage =
                    usage || matches!(event.body.kind, ObservationKind::Usage { .. });
                let candidate_bytes = event_bytes
                    .checked_add(encode(&event)?.len())
                    .context("packet size overflow")?;
                let size = envelope_bytes(
                    self.limits.node_id,
                    candidate_boot,
                    meta.acked + 1,
                    event.body.sequence,
                    candidate_usage,
                )?
                .checked_add(candidate_bytes)
                .and_then(|n| n.checked_add(events.len()))
                .context("packet size overflow")?;
                if size > MAX_BATCH_BYTES {
                    break;
                }
                event_bytes = candidate_bytes;
                usage = candidate_usage;
                selected_bytes = Some(size);
                events.push(event);
            }
        }
        if events.is_empty() {
            self.abort(tx)?;
            ensure!(
                meta.next == meta.acked + 1,
                "single event cannot fit packet"
            );
            return Ok(None);
        }
        let batch = ObservationBatch {
            node_id: self.limits.node_id,
            boot_id: boot.context("batch boot absent")?,
            first_sequence: meta.acked + 1,
            last_sequence: events.last().unwrap().body.sequence,
            events,
        };
        let packet = Packet::seal(stable(&batch)?, Message::Batch(batch))?;
        ensure!(
            selected_bytes == Some(encode(&packet)?.len()),
            "exact packet length disagrees with selected prefix"
        );
        {
            let mut table = tx.open_table(DELIVERY)?;
            table.insert("packet", encode(&packet)?.as_slice())?;
            table.insert("retry", encode(&Retry::initial(&packet))?.as_slice())?;
        }
        self.commit(tx)?;
        Ok(Some(packet))
    }
    pub fn begin_send(&self, now: u64) -> Result<Option<Packet>> {
        self.require_collectible()?;
        let Some(packet) = self.prepare()? else {
            return Ok(None);
        };
        let tx = self.begin()?;
        let mut table = tx.open_table(DELIVERY)?;
        let mut d: Retry = decode(table.get("retry")?.context("retry absent")?.value())?;
        d.check(&packet)?;
        ensure!(
            d.attempts < 3
                && d.last_ms.is_none_or(|t| now >= t)
                && d.first_ms.is_none_or(|t| now.saturating_sub(t) < 30000)
                && now >= d.next_retry_ms,
            "retry bound; intent retained"
        );
        d.first_ms.get_or_insert(now);
        d.last_ms = Some(now);
        d.attempts += 1;
        d.next_retry_ms = now
            .checked_add((250u64 << (d.attempts - 1)).min(2000))
            .context("clock overflow")?;
        table.insert("retry", encode(&d)?.as_slice())?;
        drop(table);
        self.commit(tx)?;
        Ok(Some(packet))
    }
    pub fn acknowledge(&self, receipt: &Receipt) -> Result<()> {
        ensure!(!self.poisoned.get(), "store uncertain; reopen required");
        if self.snapshot()?.receipt.as_ref() == Some(receipt) {
            return Ok(());
        }
        let tx = self.begin()?;
        let mut meta = read_meta(&tx)?;
        let mut delivery = tx.open_table(DELIVERY)?;
        let d = load_delivery(&delivery)?.context("pending absent")?;
        receipt.validate_against(&d.packet)?;
        let Message::Batch(batch) = &d.packet.body.message else {
            anyhow::bail!("pending packet class")
        };
        {
            let mut out = tx.open_table(OUTBOX)?;
            for event in &batch.events {
                let retained = out
                    .get(event.body.sequence)?
                    .context("pending prefix absent")?;
                let bytes = encode(event)?;
                ensure!(retained.value() == bytes, "pending prefix changed");
                drop(retained);
                out.remove(event.body.sequence)?;
                meta.logical_bytes -= bytes.len() as u64;
                if budget::is_urgent(event) {
                    meta.gaps -= 1;
                    meta.gap_bytes -= bytes.len() as u64;
                } else {
                    meta.ordinary -= 1;
                    meta.ordinary_bytes -= bytes.len() as u64;
                }
            }
        }
        meta.acked = batch.last_sequence;
        delivery.remove("packet")?;
        delivery.remove("retry")?;
        delivery.insert("receipt", encode(receipt)?.as_slice())?;
        drop(delivery);
        write_meta(&tx, &meta)?;
        self.commit(tx)
    }
    /// Expensive startup/audit path, never called by small delivery transactions.
    pub fn audit(&self) -> Result<()> {
        let tx = self.db.begin_read()?;
        let meta: Meta = decode(
            tx.open_table(META)?
                .get("state")?
                .context("metadata absent")?
                .value(),
        )?;
        ensure!(
            meta.limits == self.limits && meta.next > meta.acked,
            "metadata scope/cursor"
        );
        sessions::validate_boot_history(&meta)?;
        local_intent::validate_managed(&meta)?;
        if let Some(initial) = &meta.transport {
            ensure!(
                initial.initial_sequence == 1
                    && initial.node.node_id == self.limits.node_id
                    && initial.node.boot_id == self.limits.boot_id
                    && !initial.operation_id.is_nil()
                    && !initial.transport_owner_id.is_nil(),
                "initial store admission scope"
            );
            artifact(&initial.owner_receipt)?;
        }
        let sources = tx.open_table(SOURCES)?;
        ensure!(
            sources.len()? == meta.sources && meta.sources <= budget::MAX_SOURCES,
            "source count"
        );
        let seen = tx.open_table(SEEN)?;
        ensure!(seen.len()? == meta.seen && meta.seen <= 65536, "seen count");
        let ids = tx.open_table(EVENTS)?;
        let out = tx.open_table(OUTBOX)?;
        let mut sequence = meta.acked + 1;
        let (mut ordinary, mut ordinary_bytes, mut gaps, mut gap_bytes) = (0, 0, 0, 0);
        let mut logical = 0;
        let mut live_sources = 0;
        for value in sources.iter()? {
            let (k, v) = value?;
            let row: SourceRow = decode(v.value())?;
            budget::source_policy(&row)?;
            live_sources += u64::from(budget::live(&row));
            ensure!(
                !budget::live(&row) || row.last_control_sequence.is_none_or(|n| n <= meta.acked),
                "live source retains urgent credit before ACK"
            );
            ensure!(
                row.last_control_sequence.is_none_or(|n| n < meta.next),
                "source control sequence"
            );
            row.source.validate()?;
            if let Some(reader) = &row.reader {
                reader.cursor.validate()?;
                ensure!(
                    reader.cursor.offset == row.cursor
                        && reader.permit.source_id == row.source.source_id
                        && reader.permit.native_session_key == row.source.native_session_key,
                    "file reader scope/cursor"
                );
            }
            ensure!(
                row.source.source_id.as_bytes().as_slice() == k.value()
                    && matches!(row.source.binding.session,SessionIdentity::Remote{node_id,boot_id,..}if node_id==self.limits.node_id&&sessions::known_boot(&meta,boot_id)),
                "source identity"
            );
            ensure!(
                !row.parsers.is_empty()
                    && row.parsers.len() <= 16
                    && row.generations.contains(&row.source.generation)
                    && row.generations.len() <= 64,
                "source provenance history bounds"
            );
            ensure!(
                row.source_operations.len() <= 64
                    && row
                        .source_operations
                        .iter()
                        .all(|op| !op.operation_id.is_nil())
                    && row
                        .source_operations
                        .iter()
                        .map(|op| op.operation_id)
                        .collect::<std::collections::BTreeSet<_>>()
                        .len()
                        == row.source_operations.len(),
                "source operation history bounds/identity"
            );
            for parser in &row.parsers {
                artifact(parser)?;
            }
            logical += v.value().len() as u64;
        }
        for value in seen.iter()? {
            let (k, v) = value?;
            ensure!(
                k.value().len() > 16
                    && k.value().len() <= 1040
                    && sources.get(&k.value()[..16])?.is_some(),
                "seen key/source"
            );
            let row: SeenRow = decode(v.value())?;
            row.event.validate()?;
            ensure!(
                !row.raw_record_digests.is_empty() && row.raw_record_digests.len() <= 32,
                "seen raw bound"
            );
            logical += (k.value().len() + v.value().len()) as u64;
        }
        for value in ids.iter()? {
            let (k, v) = value?;
            ensure!(k.value().len() == 16, "event ID key");
            let _: Digest = decode(v.value())?;
            logical += v.value().len() as u64;
        }
        for value in out.iter()? {
            let (k, v) = value?;
            let e: ObservationRecord = decode(v.value())?;
            e.validate()?;
            ensure!(
                k.value() == sequence && e.body.sequence == sequence,
                "outbox sequence"
            );
            let row: SourceRow = decode(
                sources
                    .get(e.body.provenance.source_id.as_bytes().as_slice())?
                    .context("fact source absent")?
                    .value(),
            )?;
            ensure!(
                e.body.binding == row.source.binding
                    && row
                        .generations
                        .contains(&e.body.provenance.source_generation)
                    && &e.body.provenance.native_session_key == row.source.provenance_session_key()
                    && row.parsers.contains(&e.body.provenance.parser),
                "fact source scope"
            );
            let digest: Digest = decode(
                ids.get(e.event_id.as_bytes().as_slice())?
                    .context("event ID absent")?
                    .value(),
            )?;
            ensure!(digest == e.body_digest, "event ID digest");
            sequence = sequence.checked_add(1).context("sequence overflow")?;
            logical += v.value().len() as u64;
            if budget::is_urgent(&e) {
                gaps += 1;
                gap_bytes += v.value().len() as u64;
            } else {
                ordinary += 1;
                ordinary_bytes += v.value().len() as u64;
            }
        }
        logical += self.audit_engine_rows(&tx)?;
        ensure!(
            sequence == meta.next
                && ordinary == meta.ordinary
                && ordinary_bytes == meta.ordinary_bytes
                && gaps == meta.gaps
                && gap_bytes == meta.gap_bytes
                && live_sources + gaps <= budget::MAX_SOURCES
                && logical == meta.logical_bytes
                && logical <= self.limits.data_limit(),
            "projection/quota counts"
        );
        let delivery = tx.open_table(DELIVERY)?;
        ensure!(delivery.len()? <= 3, "delivery row bound");
        for entry in delivery.iter()? {
            let (key, _) = entry?;
            ensure!(
                matches!(key.value(), "packet" | "retry" | "receipt"),
                "unknown delivery row; no silent format fallback"
            );
        }
        if let Some(d) = load_delivery(&delivery)? {
            d.packet.validate()?;
            let Message::Batch(batch) = d.packet.body.message else {
                anyhow::bail!("pending class")
            };
            ensure!(
                batch.first_sequence == meta.acked + 1
                    && batch.last_sequence < meta.next
                    && d.attempts <= 3
                    && (d.attempts == 0) == d.first_ms.is_none()
                    && d.first_ms.is_some() == d.last_ms.is_some(),
                "pending metadata"
            );
            for e in batch.events {
                let bytes = encode(&e)?;
                ensure!(
                    out.get(e.body.sequence)?
                        .context("pending prefix absent")?
                        .value()
                        == bytes,
                    "pending exact prefix"
                );
            }
        }
        if let Some(v) = delivery.get("receipt")? {
            let r: Receipt = decode(v.value())?;
            r.validate()?;
            ensure!(r.through_sequence == Some(meta.acked), "receipt watermark");
        }
        Ok(())
    }
}
