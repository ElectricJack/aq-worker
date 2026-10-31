//! Experimental store only. No transport authentication, bootstrap authority,
//! transcript normalization adapter or migration from the qualified v1 spool.
pub mod backend;
use anyhow::{Context, Result, ensure};
use aq_lifecycle_protocol::*;
use aq_observation::Source;
use backend::{CappedFile, Monitor};
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
                && (65536..=8 * 1024 * 1024).contains(&self.cache_bytes),
            "prototype bounds"
        );
        Ok(())
    }
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
struct Meta {
    version: u32,
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
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Snapshot {
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
    _owner: File,
    _directory: File,
    root: PathBuf,
    limits: Limits,
    monitor: Monitor,
    poisoned: Cell<bool>,
}
fn encode<T: Serialize>(value: &T) -> Result<Vec<u8>> {
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
    tx.open_table(META)?
        .insert("state", encode(meta)?.as_slice())?;
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
impl Store {
    pub fn create(root: &Path, limits: Limits) -> Result<Self> {
        Self::open_inner(root, limits, true)
    }
    pub fn open(root: &Path, limits: Limits) -> Result<Self> {
        Self::open_inner(root, limits, false)
    }
    fn open_inner(root: &Path, limits: Limits, create: bool) -> Result<Self> {
        limits.validate()?;
        let (directory, owner) = backend::private_root(root, create)?;
        let monitor = Monitor::default();
        let backend = CappedFile::open(&directory, create, limits.physical_bytes, monitor.clone())?;
        let db = Database::builder()
            .set_cache_size(limits.cache_bytes)
            .create_with_backend(backend)?;
        let store = Self {
            db,
            _owner: owner,
            _directory: directory,
            root: root.into(),
            limits,
            monitor,
            poisoned: Cell::new(false),
        };
        if create {
            let tx = store.begin()?;
            for table in [SOURCES, SEEN, EVENTS] {
                tx.open_table(table)?;
            }
            tx.open_table(OUTBOX)?;
            tx.open_table(DELIVERY)?;
            write_meta(
                &tx,
                &Meta {
                    version: 1,
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
            meta.version == 1 && meta.limits == store.limits,
            "store version/identity/limits"
        );
        drop(table);
        drop(tx);
        store.audit()?;
        store.monitor.reset();
        Ok(store)
    }
    fn begin(&self) -> Result<WriteTransaction> {
        ensure!(!self.poisoned.get(), "store uncertain; reopen required");
        self.poisoned.set(true);
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
            matches!(source.binding.session,SessionIdentity::Remote{node_id,boot_id,..}if node_id==self.limits.node_id&&boot_id==self.limits.boot_id),
            "source node/boot"
        );
        let tx = self.begin()?;
        let mut meta = read_meta(&tx)?;
        let row = SourceRow { source, cursor: 0 };
        let id = row.source.source_id;
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
            ensure!(meta.sources < 16, "source quota");
            table.insert(id.as_bytes().as_slice(), bytes.as_slice())?;
        }
        meta.sources += 1;
        meta.logical_bytes += bytes.len() as u64;
        ensure!(
            meta.logical_bytes <= self.limits.logical_bytes,
            "logical quota"
        );
        write_meta(&tx, &meta)?;
        self.commit(tx)
    }
    pub fn source(&self, id: Uuid) -> Result<SourceRow> {
        let tx = self.db.begin_read()?;
        let table = tx.open_table(SOURCES)?;
        decode(
            table
                .get(id.as_bytes().as_slice())?
                .context("source absent")?
                .value(),
        )
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
                        && event.body.provenance.native_session_key
                            == source.source.native_session_key
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
                if matches!(event.body.kind, ObservationKind::Gap { .. }) {
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
                && meta.logical_bytes <= self.limits.logical_bytes,
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
            meta.logical_bytes <= self.limits.logical_bytes,
            "logical source quota"
        );
        write_meta(&tx, &meta)?;
        self.commit(tx)
    }
    pub fn snapshot(&self) -> Result<Snapshot> {
        let tx = self.db.begin_read()?;
        let meta: Meta = decode(tx.open_table(META)?.get("state")?.unwrap().value())?;
        let delivery = tx.open_table(DELIVERY)?;
        let pending = delivery
            .get("pending")?
            .map(|v| decode(v.value()))
            .transpose()?;
        let receipt = delivery
            .get("receipt")?
            .map(|v| decode(v.value()))
            .transpose()?;
        Ok(Snapshot {
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
        ensure!(!self.poisoned.get(), "store uncertain; reopen required");
        if let Some(d) = self.snapshot()?.delivery {
            return Ok(Some(d.packet));
        }
        let tx = self.begin()?;
        let meta = read_meta(&tx)?;
        let mut events = vec![];
        {
            let table = tx.open_table(OUTBOX)?;
            for result in table.iter()?.take(MAX_EVENTS) {
                let (_, v) = result?;
                let event: ObservationRecord = decode(v.value())?;
                let mut candidate = events.clone();
                candidate.push(event);
                let batch = ObservationBatch {
                    node_id: self.limits.node_id,
                    boot_id: self.limits.boot_id,
                    first_sequence: meta.acked + 1,
                    last_sequence: candidate.last().unwrap().body.sequence,
                    events: candidate,
                };
                let id = stable(&batch)?;
                match Packet::seal(id, Message::Batch(batch)) {
                    Ok(packet) => {
                        let Message::Batch(b) = packet.body.message else {
                            unreachable!()
                        };
                        events = b.events;
                    }
                    Err(_) => break,
                }
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
            boot_id: self.limits.boot_id,
            first_sequence: meta.acked + 1,
            last_sequence: events.last().unwrap().body.sequence,
            events,
        };
        let packet = Packet::seal(stable(&batch)?, Message::Batch(batch))?;
        let delivery = Delivery {
            packet: packet.clone(),
            attempts: 0,
            first_ms: None,
            last_ms: None,
            next_retry_ms: 0,
        };
        tx.open_table(DELIVERY)?
            .insert("pending", encode(&delivery)?.as_slice())?;
        self.commit(tx)?;
        Ok(Some(packet))
    }
    pub fn begin_send(&self, now: u64) -> Result<Option<Packet>> {
        let Some(packet) = self.prepare()? else {
            return Ok(None);
        };
        let tx = self.begin()?;
        let mut table = tx.open_table(DELIVERY)?;
        let mut d: Delivery = decode(table.get("pending")?.context("pending absent")?.value())?;
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
        table.insert("pending", encode(&d)?.as_slice())?;
        drop(table);
        self.commit(tx)?;
        Ok(Some(packet))
    }
    pub fn acknowledge(&self, receipt: &Receipt) -> Result<()> {
        if self.snapshot()?.receipt.as_ref() == Some(receipt) {
            return Ok(());
        }
        let tx = self.begin()?;
        let mut meta = read_meta(&tx)?;
        let mut delivery = tx.open_table(DELIVERY)?;
        let d: Delivery = decode(delivery.get("pending")?.context("pending absent")?.value())?;
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
                if matches!(event.body.kind, ObservationKind::Gap { .. }) {
                    meta.gaps -= 1;
                    meta.gap_bytes -= bytes.len() as u64;
                } else {
                    meta.ordinary -= 1;
                    meta.ordinary_bytes -= bytes.len() as u64;
                }
            }
        }
        meta.acked = batch.last_sequence;
        delivery.remove("pending")?;
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
        let sources = tx.open_table(SOURCES)?;
        ensure!(
            sources.len()? == meta.sources && meta.sources <= 16,
            "source count"
        );
        let seen = tx.open_table(SEEN)?;
        ensure!(seen.len()? == meta.seen && meta.seen <= 65536, "seen count");
        let ids = tx.open_table(EVENTS)?;
        let out = tx.open_table(OUTBOX)?;
        let mut sequence = meta.acked + 1;
        let (mut ordinary, mut ordinary_bytes, mut gaps, mut gap_bytes) = (0, 0, 0, 0);
        let mut logical = 0;
        for value in sources.iter()? {
            let (k, v) = value?;
            let row: SourceRow = decode(v.value())?;
            row.source.validate()?;
            ensure!(
                row.source.source_id.as_bytes().as_slice() == k.value()
                    && matches!(row.source.binding.session,SessionIdentity::Remote{node_id,boot_id,..}if node_id==self.limits.node_id&&boot_id==self.limits.boot_id),
                "source identity"
            );
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
                    && e.body.provenance.source_generation == row.source.generation
                    && e.body.provenance.native_session_key == row.source.native_session_key
                    && e.body.provenance.parser == row.source.parser(),
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
            if matches!(e.body.kind, ObservationKind::Gap { .. }) {
                gaps += 1;
                gap_bytes += v.value().len() as u64;
            } else {
                ordinary += 1;
                ordinary_bytes += v.value().len() as u64;
            }
        }
        ensure!(
            sequence == meta.next
                && ordinary == meta.ordinary
                && ordinary_bytes == meta.ordinary_bytes
                && gaps == meta.gaps
                && gap_bytes == meta.gap_bytes
                && logical == meta.logical_bytes
                && logical <= self.limits.logical_bytes,
            "projection/quota counts"
        );
        let delivery = tx.open_table(DELIVERY)?;
        if let Some(v) = delivery.get("pending")? {
            let d: Delivery = decode(v.value())?;
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
