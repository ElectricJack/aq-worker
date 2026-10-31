//! Candidate row-backed normalization adapter. Input is one admitted frame;
//! source-file ownership/boot/reparse integration remains a subsequent seam.
use super::*;
use aq_observation_engine::{
    self as engine,
    rows::{ReadView, Table as RowTable},
};
use redb::ReadTransaction;
use serde_json::Value;
use std::collections::BTreeMap;
const NATIVE: TableDefinition<&[u8], &[u8]> = TableDefinition::new("native_messages");
const SNAPSHOTS: TableDefinition<&[u8], &[u8]> = TableDefinition::new("counter_snapshots");
const MUTATIONS: TableDefinition<&[u8], &[u8]> = TableDefinition::new("native_mutations");
const DISPLAY_IDS: TableDefinition<&[u8], &[u8]> = TableDefinition::new("display_ids");
const DISPLAY_ORDINALS: TableDefinition<&[u8], &[u8]> = TableDefinition::new("display_ordinals");
pub(super) const TABLES: [TableDefinition<&[u8], &[u8]>; 5] =
    [NATIVE, SNAPSHOTS, MUTATIONS, DISPLAY_IDS, DISPLAY_ORDINALS];
fn definition(table: RowTable) -> TableDefinition<'static, &'static [u8], &'static [u8]> {
    match table {
        RowTable::Seen => SEEN,
        RowTable::Native => NATIVE,
        RowTable::Snapshot => SNAPSHOTS,
        RowTable::Mutation => MUTATIONS,
        RowTable::DisplayById => DISPLAY_IDS,
        RowTable::DisplayByOrdinal => DISPLAY_ORDINALS,
    }
}
fn key(source: Uuid, epoch: u64, table: RowTable, native: &str) -> Result<Vec<u8>> {
    let mut key = source.as_bytes().to_vec();
    if epoch_scoped(table) {
        key.extend(epoch.to_be_bytes());
    }
    if table == RowTable::DisplayByOrdinal {
        key.extend(native.parse::<u64>()?.to_be_bytes());
    } else {
        ensure!(
            !native.is_empty() && native.len() <= 2048,
            "engine native key bound"
        );
        key.extend(native.as_bytes());
    }
    Ok(key)
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EngineState {
    pub native_epoch: u64,
    pub retired_epochs: Vec<crate::reparse::RetiredEpoch>,
    pub replay: Option<crate::reparse::ActiveReplay>,
    pub completed_replays: Vec<crate::reparse::CompletedReplay>,
    pub context: engine::Context,
    pub revision: u64,
    pub counts: BTreeMap<RowTable, usize>,
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CollectedFrame {
    pub consumed_bytes: u64,
    pub facts_added: usize,
    pub indexed_lookups: usize,
    pub blocked: bool,
}
struct View<'a> {
    tx: &'a ReadTransaction,
    source: Uuid,
    epoch: u64,
    counts: BTreeMap<RowTable, usize>,
}
impl ReadView for View<'_> {
    fn get(&self, table: RowTable, native: &str) -> Result<Option<Value>> {
        let t = self.tx.open_table(definition(table))?;
        let k = key(self.source, self.epoch, table, native)?;
        t.get(k.as_slice())?.map(|v| decode(v.value())).transpose()
    }
    fn count(&self, table: RowTable) -> Result<usize> {
        Ok(self.counts.get(&table).copied().unwrap_or(0))
    }
    fn display_suffix(&self, start: u64, limit: usize) -> Result<Vec<(u64, String)>> {
        ensure!(limit <= 1057, "display range budget");
        let table = self.tx.open_table(DISPLAY_ORDINALS)?;
        let from = key(
            self.source,
            self.epoch,
            RowTable::DisplayByOrdinal,
            &start.to_string(),
        )?;
        let through = key(
            self.source,
            self.epoch,
            RowTable::DisplayByOrdinal,
            &u64::MAX.to_string(),
        )?;
        let mut rows = vec![];
        for pair in table
            .range(from.as_slice()..=through.as_slice())?
            .take(limit)
        {
            let (k, v) = pair?;
            let ordinal = u64::from_be_bytes(k.value()[24..].try_into()?);
            rows.push((ordinal, decode(v.value())?));
        }
        Ok(rows)
    }
}
impl Store {
    pub fn attach_engine(
        &self,
        source: engine::Source,
        maximum: usize,
        scoped_channel: bool,
    ) -> Result<()> {
        self.require_collectible()?;
        self.attach_engine_input(source, maximum, scoped_channel, None)
    }
    pub(super) fn attach_engine_input(
        &self,
        source: engine::Source,
        maximum: usize,
        scoped_channel: bool,
        reader: Option<ReaderState>,
    ) -> Result<()> {
        source.validate()?;
        ensure!(
            source.format != engine::Format::OpenCodeSessionDto6c24ec9 || scoped_channel,
            "public DTO requires explicit admitted native-session channel"
        );
        let mut context = engine::Context::new(source.clone(), maximum)?;
        if scoped_channel {
            context = context.bind_scoped_record_channel(source.native_session_key.as_str())?;
        }
        let reference: Source = serde_json::from_value(serde_json::to_value(&source)?)?;
        ensure!(
            matches!(reference.binding.session,SessionIdentity::Remote{node_id,boot_id,..}if node_id==self.limits.node_id&&boot_id!=Uuid::nil()),
            "engine node/boot scope"
        );
        let tx = self.begin()?;
        let mut meta = read_meta(&tx)?;
        ensure!(
            matches!(reference.binding.session,SessionIdentity::Remote{boot_id,..}if boot_id==meta.active_boot),
            "inactive boot source admission"
        );
        let mut table = tx.open_table(SOURCES)?;
        let id = source.source_id;
        let existing = table
            .get(id.as_bytes().as_slice())?
            .map(|v| v.value().to_vec());
        if let Some(bytes) = existing {
            let row: SourceRow = decode(&bytes)?;
            ensure!(
                row.source == reference
                    && row.reader == reader
                    && row.engine.as_ref().is_some_and(
                        |e| e.context.source == source && e.context.maximum_tracked == maximum
                    ),
                "engine source ID payload conflict"
            );
            drop(table);
            return self.abort(tx);
        }
        ensure!(meta.sources < budget::MAX_SOURCES, "source quota");
        let row = SourceRow {
            source: reference,
            cursor: 0,
            hold: None,
            last_control_sequence: None,
            parsers: vec![context.parser.clone()],
            generations: vec![context.source.generation],
            reader,
            source_operations: vec![],
            record_cursor: None,
            engine: Some(EngineState {
                context,
                native_epoch: 0,
                retired_epochs: vec![],
                replay: None,
                completed_replays: vec![],
                revision: 1,
                counts: BTreeMap::new(),
            }),
        };
        budget::source_policy(&row)?;
        let bytes = encode(&row)?;
        table.insert(id.as_bytes().as_slice(), bytes.as_slice())?;
        drop(table);
        meta.sources += 1;
        meta.logical_bytes += bytes.len() as u64;
        ensure!(
            meta.logical_bytes <= self.limits.ordinary_limit(),
            "logical quota"
        );
        write_meta(&tx, &meta)?;
        self.commit(tx)
    }
    pub fn collect_frame(
        &self,
        id: Uuid,
        frame: &engine::framing::Frame,
        observed_at_ms: u64,
    ) -> Result<CollectedFrame> {
        self.require_collectible()?;
        self.process_frame(id, frame, observed_at_ms, None, None)
    }
    fn process_frame(
        &self,
        id: Uuid,
        frame: &engine::framing::Frame,
        observed_at_ms: u64,
        replay: Option<&Digest>,
        record: Option<RecordAdvance>,
    ) -> Result<CollectedFrame> {
        ensure!(!self.poisoned.get(), "store uncertain; reopen required");
        self.identity.check()?;
        let tx = self.db.begin_read()?;
        let meta: Meta = decode(
            tx.open_table(META)?
                .get("state")?
                .context("meta absent")?
                .value(),
        )?;
        let row: SourceRow = decode(
            tx.open_table(SOURCES)?
                .get(id.as_bytes().as_slice())?
                .context("source absent")?
                .value(),
        )?;
        let state = row.engine.as_ref().context("source not engine-owned")?;
        if let Some(digest) = replay {
            let active = state.replay.as_ref().context("no active retained replay")?;
            ensure!(
                &active.plan_digest == digest && active.offset == frame.start,
                "replay digest/cursor mismatch"
            );
        } else {
            ensure!(
                row.hold.is_none()
                    && state.replay.is_none()
                    && row.reader.is_none()
                    && frame.start == row.cursor,
                "source live cursor/hold/replay refusal"
            );
        }
        if replay.is_none()
            && row.source.format == aq_observation::Format::OpenCodeSessionDto6c24ec9
        {
            let advance = record
                .as_ref()
                .context("public DTO requires scoped record cursor API")?;
            ensure!(
                row.record_cursor == advance.expected,
                "public record cursor changed"
            );
        }
        let live_cursor = row.cursor;
        let mut context = state.context.clone();
        context.next_sequence = meta.next;
        let view = View {
            tx: &tx,
            source: id,
            epoch: state.native_epoch,
            counts: state.counts.clone(),
        };
        let delta = engine::normalize(&context, &view, frame, observed_at_ms)?;
        ensure!(
            delta
                .events
                .iter()
                .all(|event| !budget::reserved_parser(&event.body.provenance.parser)),
            "native output cannot claim reserved monitor provenance"
        );
        let expected_revision = state.revision;
        let expected_next = meta.next;
        let end = frame.end()?;
        drop(tx);
        self.commit_engine_delta(
            FrameIntent {
                id,
                frame_start: frame.start,
                live_cursor,
                end,
                replay: replay.cloned(),
                revision: expected_revision,
                node_next: expected_next,
                hold: None,
                control_parser: None,
                reader: None,
                record,
            },
            delta,
        )
    }
    fn commit_engine_delta(
        &self,
        intent: FrameIntent,
        delta: engine::NormalizationDelta,
    ) -> Result<CollectedFrame> {
        let FrameIntent {
            id,
            frame_start,
            live_cursor,
            end,
            replay,
            revision: expected_revision,
            node_next: expected_next,
            hold,
            control_parser,
            reader,
            record,
        } = intent;
        let tx = self.begin()?;
        let mut meta = read_meta(&tx)?;
        let mut sources = tx.open_table(SOURCES)?;
        let old = sources
            .get(id.as_bytes().as_slice())?
            .context("source absent")?
            .value()
            .to_vec();
        let mut row: SourceRow = decode(&old)?;
        let state = row.engine.as_mut().context("source not engine-owned")?;
        ensure!(
            row.cursor == live_cursor
                && state.revision == expected_revision
                && meta.next == expected_next,
            "normalization CAS conflict; retained cursor"
        );
        let mut out = tx.open_table(OUTBOX)?;
        let mut ids = tx.open_table(EVENTS)?;
        for event in &delta.events {
            event.validate()?;
            ensure!(
                event.body.sequence == meta.next
                    && event.body.binding == row.source.binding
                    && event.body.provenance.source_id == id
                    && (event.body.provenance.parser == state.context.parser
                        || (control_parser.as_ref() == Some(&event.body.provenance.parser)
                            && matches!(
                                event.body.kind,
                                ObservationKind::Gap {
                                    reason: GapReason::SpoolOverflow,
                                    ..
                                }
                            )))
                    && event.body.provenance.source_generation == row.source.generation,
                "engine fact scope"
            );
            ensure!(
                ids.get(event.event_id.as_bytes().as_slice())?.is_none(),
                "retained event ID conflict"
            );
            let bytes = encode(event)?;
            let digest = encode(&event.body_digest)?;
            out.insert(meta.next, bytes.as_slice())?;
            ids.insert(event.event_id.as_bytes().as_slice(), digest.as_slice())?;
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
        drop(ids);
        drop(out);
        for ((table, native), value) in &delta.rows.writes {
            let k = key(id, state.native_epoch, *table, native)?;
            let mut rows = tx.open_table(definition(*table))?;
            let old = rows.get(k.as_slice())?.map(|v| v.value().to_vec());
            let count = state.counts.entry(*table).or_default();
            if let Some(old) = &old {
                meta.logical_bytes -= old.len() as u64;
                if *table != RowTable::Seen {
                    meta.logical_bytes -= k.len() as u64;
                }
            }
            if let Some(value) = value {
                let bytes = encode(value)?;
                rows.insert(k.as_slice(), bytes.as_slice())?;
                meta.logical_bytes += bytes.len() as u64;
                if old.is_none() {
                    *count += 1;
                    if *table == RowTable::Seen {
                        meta.seen += 1;
                        meta.logical_bytes += k.len() as u64;
                    }
                }
                if *table != RowTable::Seen {
                    meta.logical_bytes += k.len() as u64;
                }
            } else if old.is_some() {
                rows.remove(k.as_slice())?;
                *count = count.checked_sub(1).context("engine count underflow")?;
                if *table == RowTable::Seen {
                    meta.seen -= 1;
                    meta.logical_bytes -= k.len() as u64;
                }
            }
        }
        ensure!(
            state.counts.get(&RowTable::Seen).copied().unwrap_or(0)
                <= state.context.maximum_tracked
                && state.counts.get(&RowTable::Native).copied().unwrap_or(0)
                    <= state.context.maximum_tracked
                && state.counts.get(&RowTable::Mutation).copied().unwrap_or(0) <= 128,
            "source engine history quota"
        );
        if let Some(parser) = control_parser
            && !row.parsers.contains(&parser)
        {
            ensure!(row.parsers.len() < 16, "source parser history bound");
            row.parsers.push(parser);
        }
        if let Some(hold) = &hold {
            row.hold = Some(hold.clone());
            row.last_control_sequence = Some(meta.next - 1);
        }
        if let Some(advance) = reader {
            ensure!(
                row.reader.as_ref() == Some(&advance.expected),
                "file reader CAS changed"
            );
            row.reader = Some(advance.next);
        }
        if let Some(advance) = record {
            ensure!(
                row.record_cursor == advance.expected,
                "record cursor CAS changed"
            );
            row.record_cursor = Some(advance.next);
        }
        let facts = delta.events.len();
        let lookups = delta.indexed_lookups;
        state.context = delta.context;
        state.revision = state
            .revision
            .checked_add(1)
            .context("source revision exhausted")?;
        if replay.is_some() {
            state
                .replay
                .as_mut()
                .context("active replay disappeared")?
                .offset = end;
        } else {
            row.cursor = end;
        }
        budget::source_policy(&row)?;
        if hold.is_some() {
            budget::source_growth(&old, &row)?;
        }
        let bytes = encode(&row)?;
        meta.logical_bytes = meta.logical_bytes - old.len() as u64 + bytes.len() as u64;
        sources.insert(id.as_bytes().as_slice(), bytes.as_slice())?;
        drop(sources);
        ensure!(
            meta.ordinary <= 256
                && meta.ordinary_bytes <= 2 * 1024 * 1024
                && meta.gaps <= 16
                && meta.gap_bytes <= 65536
                && meta.logical_bytes
                    <= if hold.is_some() {
                        self.limits.routine_control_limit()
                    } else {
                        self.limits.ordinary_limit()
                    },
            "engine spool quota; cursor held"
        );
        write_meta(&tx, &meta)?;
        self.commit(tx)?;
        Ok(CollectedFrame {
            consumed_bytes: end - frame_start,
            facts_added: facts,
            indexed_lookups: lookups,
            blocked: false,
        })
    }
    pub(super) fn audit_engine_rows(&self, tx: &ReadTransaction) -> Result<u64> {
        let sources = tx.open_table(SOURCES)?;
        let mut counts: BTreeMap<(Uuid, RowTable, u64), usize> = BTreeMap::new();
        let mut bytes = 0;
        for kind in [
            RowTable::Seen,
            RowTable::Native,
            RowTable::Snapshot,
            RowTable::Mutation,
            RowTable::DisplayById,
            RowTable::DisplayByOrdinal,
        ] {
            let table = tx.open_table(definition(kind))?;
            for entry in table.iter()? {
                let (k, v) = entry?;
                ensure!(k.value().len() > 16, "engine row key");
                let id = Uuid::from_slice(&k.value()[..16])?;
                let row: SourceRow = decode(
                    sources
                        .get(id.as_bytes().as_slice())?
                        .context("engine row source absent")?
                        .value(),
                )?;
                if let Some(state) = &row.engine {
                    let epoch = if epoch_scoped(kind) {
                        ensure!(k.value().len() > 24, "native epoch key");
                        u64::from_be_bytes(k.value()[16..24].try_into()?)
                    } else {
                        0
                    };
                    ensure!(
                        !epoch_scoped(kind)
                            || epoch == state.native_epoch
                            || state.retired_epochs.iter().any(|e| e.epoch == epoch),
                        "unknown native epoch"
                    );
                    *counts.entry((id, kind, epoch)).or_default() += 1;
                }
                if kind != RowTable::Seen {
                    ensure!(row.engine.is_some(), "engine row on unowned source");
                    let _: Value = decode(v.value())?;
                    bytes += (k.value().len() + v.value().len()) as u64;
                }
            }
        }
        let global_next = self.snapshot()?.next;
        for entry in sources.iter()? {
            let (_, v) = entry?;
            let row: SourceRow = decode(v.value())?;
            if let Some(state) = row.engine {
                ensure!(
                    state.context.source.source_id == row.source.source_id
                        && state.revision > 0
                        && state.context.next_sequence <= global_next
                        && state.retired_epochs.len() <= 16
                        && state.completed_replays.len() <= 16,
                    "engine source metadata/bounds"
                );
                for kind in [
                    RowTable::Seen,
                    RowTable::Native,
                    RowTable::Snapshot,
                    RowTable::Mutation,
                    RowTable::DisplayById,
                    RowTable::DisplayByOrdinal,
                ] {
                    let epoch = if epoch_scoped(kind) {
                        state.native_epoch
                    } else {
                        0
                    };
                    ensure!(
                        state.counts.get(&kind).copied().unwrap_or(0)
                            == counts
                                .get(&(row.source.source_id, kind, epoch))
                                .copied()
                                .unwrap_or(0),
                        "engine active epoch count mismatch"
                    );
                }
                for retired in &state.retired_epochs {
                    for kind in [
                        RowTable::Native,
                        RowTable::DisplayById,
                        RowTable::DisplayByOrdinal,
                    ] {
                        ensure!(
                            retired.counts.get(&kind).copied().unwrap_or(0)
                                == counts
                                    .get(&(row.source.source_id, kind, retired.epoch))
                                    .copied()
                                    .unwrap_or(0),
                            "engine retired epoch count mismatch"
                        );
                    }
                }
                if let Some(active) = &state.replay {
                    active.plan.validate()?;
                    ensure!(
                        active.plan.digest()? == active.plan_digest
                            && active.offset <= active.plan.transcript.bytes
                            && state
                                .context
                                .reparse
                                .as_ref()
                                .is_some_and(|r| r.operation_id == active.plan.operation_id),
                        "replay intent/context mismatch"
                    );
                } else {
                    ensure!(state.context.reparse.is_none(), "orphan replay context");
                }
            }
        }
        Ok(bytes)
    }
}
pub(super) fn epoch_scoped(table: RowTable) -> bool {
    matches!(
        table,
        RowTable::Native | RowTable::DisplayById | RowTable::DisplayByOrdinal
    )
}

struct FrameIntent {
    id: Uuid,
    frame_start: u64,
    live_cursor: u64,
    end: u64,
    replay: Option<Digest>,
    revision: u64,
    node_next: u64,
    hold: Option<HoldReason>,
    control_parser: Option<ArtifactRef>,
    reader: Option<crate::reader::ReaderAdvance>,
    record: Option<RecordAdvance>,
}
type BatchCache = std::rc::Rc<std::cell::RefCell<BTreeMap<(RowTable, String), Option<Value>>>>;
#[derive(Clone)]
struct BatchView<'a> {
    base: &'a View<'a>,
    writes: engine::rows::RowDelta,
    counts: BTreeMap<RowTable, usize>,
    cache: BatchCache,
    cache_bytes: std::rc::Rc<std::cell::Cell<usize>>,
}
impl BatchView<'_> {
    fn original(&self, table: RowTable, key: &str) -> Result<Option<Value>> {
        let k = (table, key.to_owned());
        if let Some(value) = self.cache.borrow().get(&k) {
            return Ok(value.clone());
        }
        let value = self.base.get(table, key)?;
        let bytes = key.len()
            + value
                .as_ref()
                .map(serde_json::to_vec)
                .transpose()?
                .map_or(0, |v| v.len());
        let total = self
            .cache_bytes
            .get()
            .checked_add(bytes)
            .context("batch read cache overflow")?;
        ensure!(total <= 2 * 1024 * 1024, "bounded batch read cache full");
        self.cache_bytes.set(total);
        self.cache.borrow_mut().insert(k, value.clone());
        Ok(value)
    }
    fn merge(&mut self, delta: &engine::rows::RowDelta) -> Result<bool> {
        for ((table, key), value) in &delta.writes {
            let old = self.get(*table, key)?.is_some();
            let count = self.counts.entry(*table).or_default();
            match (old, value.is_some()) {
                (false, true) => *count = count.checked_add(1).context("batch count overflow")?,
                (true, false) => *count = count.checked_sub(1).context("batch count underflow")?,
                _ => {}
            }
            self.writes
                .writes
                .insert((*table, key.clone()), value.clone());
        }
        Ok(self.writes.validate().is_ok())
    }
    fn logical_change(&self, id: Uuid, epoch: u64) -> Result<i128> {
        let mut change = 0i128;
        for ((table, native), value) in &self.writes.writes {
            let scoped = key(id, epoch, *table, native)?;
            if let Some(old) = self.original(*table, native)? {
                change -= (scoped.len() + encode(&old)?.len()) as i128;
            }
            if let Some(new) = value {
                change += (scoped.len() + encode(new)?.len()) as i128;
            }
        }
        Ok(change)
    }
}
impl ReadView for BatchView<'_> {
    fn get(&self, table: RowTable, key: &str) -> Result<Option<Value>> {
        if let Some(value) = self.writes.writes.get(&(table, key.into())) {
            return Ok(value.clone());
        }
        self.original(table, key)
    }
    fn count(&self, table: RowTable) -> Result<usize> {
        Ok(self.counts.get(&table).copied().unwrap_or(0))
    }
    fn display_suffix(&self, start: u64, limit: usize) -> Result<Vec<(u64, String)>> {
        let removed = self
            .writes
            .writes
            .iter()
            .filter(|((t, _), v)| *t == RowTable::DisplayByOrdinal && v.is_none())
            .count();
        let base = self.base.display_suffix(start, limit + removed)?;
        let mut rows: BTreeMap<u64, String> = base.into_iter().collect();
        for ((table, key), value) in &self.writes.writes {
            if *table == RowTable::DisplayByOrdinal {
                let ordinal = key.parse::<u64>()?;
                if ordinal >= start {
                    if let Some(v) = value {
                        rows.insert(ordinal, decode(&encode(v)?)?);
                    } else {
                        rows.remove(&ordinal);
                    }
                }
            }
        }
        Ok(rows.into_iter().take(limit).collect())
    }
}
impl Store {
    pub fn collect_batch(
        &self,
        id: Uuid,
        frames: &[engine::framing::Frame],
        observed_at_ms: u64,
    ) -> Result<CollectedFrame> {
        self.process_batch(id, frames, observed_at_ms, None)
    }
    pub fn collect_batch_policy(
        &self,
        id: Uuid,
        frames: &[engine::framing::Frame],
        observed_at_ms: u64,
        policy: aq_observation::spool::OverflowPolicy,
    ) -> Result<CollectedFrame> {
        self.require_collectible()?;
        self.process_batch_policy(id, frames, observed_at_ms, None, policy)
    }
    pub(super) fn process_batch(
        &self,
        id: Uuid,
        frames: &[engine::framing::Frame],
        observed_at_ms: u64,
        replay: Option<&Digest>,
    ) -> Result<CollectedFrame> {
        self.process_batch_policy(
            id,
            frames,
            observed_at_ms,
            replay,
            aq_observation::spool::OverflowPolicy::Hold,
        )
    }
    fn process_batch_policy(
        &self,
        id: Uuid,
        frames: &[engine::framing::Frame],
        observed_at_ms: u64,
        replay: Option<&Digest>,
        policy: aq_observation::spool::OverflowPolicy,
    ) -> Result<CollectedFrame> {
        self.process_batch_input(id, frames, observed_at_ms, replay, policy, None)
    }
    pub(super) fn process_batch_input(
        &self,
        id: Uuid,
        frames: &[engine::framing::Frame],
        observed_at_ms: u64,
        replay: Option<&Digest>,
        policy: aq_observation::spool::OverflowPolicy,
        file: Option<&crate::reader::FileBatch<'_>>,
    ) -> Result<CollectedFrame> {
        ensure!(
            frames.len() <= 32
                && frames.iter().map(|f| f.bytes.len()).sum::<usize>()
                    <= engine::framing::MAX_READ_BYTES,
            "collection batch bounds"
        );
        if frames.is_empty() {
            return Ok(CollectedFrame {
                consumed_bytes: 0,
                facts_added: 0,
                indexed_lookups: 0,
                blocked: false,
            });
        }
        ensure!(!self.poisoned.get(), "store uncertain; reopen required");
        self.identity.check()?;
        let tx = self.db.begin_read()?;
        let meta: Meta = decode(
            tx.open_table(META)?
                .get("state")?
                .context("meta absent")?
                .value(),
        )?;
        let source_bytes = tx
            .open_table(SOURCES)?
            .get(id.as_bytes().as_slice())?
            .context("source absent")?
            .value()
            .to_vec();
        let source: SourceRow = decode(&source_bytes)?;
        ensure!(
            policy != aq_observation::spool::OverflowPolicy::GapAndHold
                || source.source.format != aq_observation::Format::OpenCodeSessionDto6c24ec9,
            "structured record channel does not support byte-loss admission; retain cursor"
        );
        ensure!(
            replay.is_some()
                || source.source.format != aq_observation::Format::OpenCodeSessionDto6c24ec9,
            "public DTO requires scoped record cursor API"
        );
        let state = source.engine.as_ref().context("source not engine-owned")?;
        let start = if let Some(digest) = replay {
            let active = state.replay.as_ref().context("replay absent")?;
            ensure!(&active.plan_digest == digest, "replay digest mismatch");
            active.offset
        } else {
            ensure!(
                source.hold.is_none()
                    && state.replay.is_none()
                    && (source.reader.is_none() || file.is_some()),
                "source held/replaying"
            );
            source.cursor
        };
        if let Some(file) = file {
            file.check()?;
            ensure!(
                source.reader.as_ref() == Some(file.expected) && replay.is_none(),
                "admitted file state differs"
            );
        }
        ensure!(frames[0].start == start, "batch source cursor mismatch");
        let base = View {
            tx: &tx,
            source: id,
            epoch: state.native_epoch,
            counts: state.counts.clone(),
        };
        let mut view = BatchView {
            base: &base,
            writes: engine::rows::RowDelta::default(),
            counts: state.counts.clone(),
            cache: Default::default(),
            cache_bytes: Default::default(),
        };
        let mut context = state.context.clone();
        context.next_sequence = meta.next;
        let mut events = vec![];
        let mut end = start;
        let mut lookups = 0;
        let mut blocked = false;
        let mut hold = None;
        let mut control_parser = None;
        let (mut ordinary, mut ordinary_bytes, mut gaps, mut retained_gap_bytes) = (
            meta.ordinary,
            meta.ordinary_bytes,
            meta.gaps,
            meta.gap_bytes,
        );
        for frame in frames {
            ensure!(frame.start == end, "noncontiguous batch frame");
            let delta = engine::normalize(&context, &view, frame, observed_at_ms)?;
            ensure!(
                delta
                    .events
                    .iter()
                    .all(|event| !budget::reserved_parser(&event.body.provenance.parser)),
                "native output cannot claim reserved monitor provenance"
            );
            let mut candidate = view.clone();
            if !candidate.merge(&delta.rows)? {
                blocked = true;
                break;
            }
            let (mut cn, mut cb, mut cg, mut cgb) =
                (ordinary, ordinary_bytes, gaps, retained_gap_bytes);
            let mut added_bytes = 0i128;
            for event in events.iter().chain(delta.events.iter()) {
                added_bytes += (encode(event)?.len() + encode(&event.body_digest)?.len()) as i128;
            }
            for event in &delta.events {
                let bytes = encode(event)?.len() as u64;
                if budget::is_urgent(event) {
                    cg += 1;
                    cgb += bytes;
                } else {
                    cn += 1;
                    cb += bytes;
                }
            }
            let mut row = source.clone();
            let e = row.engine.as_mut().unwrap();
            e.context = delta.context.clone();
            e.counts = candidate.counts.clone();
            e.revision = e.revision.checked_add(1).context("revision exhausted")?;
            if replay.is_some() {
                e.replay.as_mut().unwrap().offset = frame.end()?;
            } else {
                row.cursor = frame.end()?;
            }
            if let Some(file) = file {
                row.reader = Some(
                    file.advance(
                        frame.end()?,
                        frame.oversized && !frame.bytes.ends_with(b"\n"),
                    )?
                    .next,
                );
            }
            let logical = meta.logical_bytes as i128
                + candidate.logical_change(id, state.native_epoch)?
                + added_bytes
                - source_bytes.len() as i128
                + encode(&row)?.len() as i128;
            if cn > 256
                || cb > 2 * 1024 * 1024
                || cg > 16
                || cgb > 65536
                || logical < 0
                || logical > self.limits.ordinary_limit() as i128
            {
                blocked = true;
                if policy == aq_observation::spool::OverflowPolicy::GapAndHold && replay.is_none() {
                    let gap = loss_gap(&source, frame, context.next_sequence, observed_at_ms)?;
                    let gap_bytes = encode(&gap)?.len() as u64;
                    let mut loss_context = context.clone();
                    loss_context.next_sequence = loss_context
                        .next_sequence
                        .checked_add(1)
                        .context("sequence exhausted")?;
                    loss_context.context_gap = true;
                    loss_context.model = None;
                    loss_context.active_turn = None;
                    let mut loss_source = source.clone();
                    loss_source.cursor = frame.end()?;
                    loss_source.hold = Some(HoldReason::SpoolLoss);
                    let e = loss_source.engine.as_mut().unwrap();
                    e.context = loss_context.clone();
                    e.counts = view.counts.clone();
                    e.revision = e.revision.checked_add(1).context("revision exhausted")?;
                    let parser = gap.body.provenance.parser.clone();
                    let parser_capacity =
                        loss_source.parsers.contains(&parser) || loss_source.parsers.len() < 16;
                    if !loss_source.parsers.contains(&parser) {
                        loss_source.parsers.push(parser.clone());
                    }
                    let mut bytes = 0i128;
                    for e in events.iter().chain(std::iter::once(&gap)) {
                        bytes += (encode(e)?.len() + encode(&e.body_digest)?.len()) as i128;
                    }
                    if let Some(file) = file {
                        loss_source.reader = Some(
                            file.advance(
                                frame.end()?,
                                frame.oversized && !frame.bytes.ends_with(b"\n"),
                            )?
                            .next,
                        );
                    }
                    let logical = meta.logical_bytes as i128
                        + view.logical_change(id, state.native_epoch)?
                        + bytes
                        - source_bytes.len() as i128
                        + encode(&loss_source)?.len() as i128;
                    if gaps < 16
                        && gap_bytes + retained_gap_bytes <= 65536
                        && logical >= 0
                        && logical <= self.limits.routine_control_limit() as i128
                        && parser_capacity
                    {
                        events.push(gap);
                        context = loss_context;
                        end = frame.end()?;
                        hold = Some(HoldReason::SpoolLoss);
                        control_parser = Some(parser);
                    }
                }
                break;
            }
            view = candidate;
            context = delta.context;
            events.extend(delta.events);
            end = frame.end()?;
            lookups += delta.indexed_lookups;
            ordinary = cn;
            ordinary_bytes = cb;
            gaps = cg;
            retained_gap_bytes = cgb;
        }
        if end == start {
            return Ok(CollectedFrame {
                consumed_bytes: 0,
                facts_added: 0,
                indexed_lookups: lookups,
                blocked,
            });
        }
        let delta = engine::NormalizationDelta {
            context,
            rows: view.writes,
            events,
            indexed_lookups: lookups,
        };
        let reader = if let Some(file) = file {
            file.check()?;
            let frame = frames
                .iter()
                .find(|f| f.end().ok() == Some(end))
                .context("accepted file boundary absent")?;
            Some(file.advance(end, frame.oversized && !frame.bytes.ends_with(b"\n"))?)
        } else {
            None
        };
        let revision = state.revision;
        let live_cursor = source.cursor;
        drop(tx);
        let mut result = self.commit_engine_delta(
            FrameIntent {
                id,
                frame_start: start,
                live_cursor,
                end,
                replay: replay.cloned(),
                revision,
                node_next: meta.next,
                hold,
                control_parser,
                reader,
                record: None,
            },
            delta,
        )?;
        result.blocked = blocked;
        Ok(result)
    }
}

pub(super) fn loss_parser() -> ArtifactRef {
    ArtifactRef {
        id: Text::new("row-spool-loss-monitor").unwrap(),
        version: Text::new("0.0.5").unwrap(),
        digest: Digest::of(include_bytes!("engine_store.rs")),
    }
}
fn loss_gap(
    source: &SourceRow,
    frame: &engine::framing::Frame,
    sequence: u64,
    observed_at_ms: u64,
) -> Result<ObservationRecord> {
    let cursor = SourceCursor::Bytes {
        start: frame.start,
        end: frame.end()?,
    };
    let parser = loss_parser();
    let body = Observation {
        binding: source.source.binding.clone(),
        provenance: Provenance {
            source_id: source.source.source_id,
            source_generation: source.source.generation,
            source_kind: SourceKind::NativeTranscript,
            native_session_key: source.source.native_session_key.clone(),
            cursor: cursor.clone(),
            parser,
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
    ObservationRecord::seal(stable(&body)?, body).map_err(Into::into)
}

struct RecordAdvance {
    expected: Option<Text>,
    next: Text,
}
impl Store {
    pub fn ingest_record(
        &self,
        id: Uuid,
        expected_cursor: Option<&str>,
        next_cursor: Text,
        bytes: Vec<u8>,
        observed_at_ms: u64,
    ) -> Result<CollectedFrame> {
        self.require_collectible()?;
        next_cursor.validate()?;
        let source = self.source(id)?;
        ensure!(
            source.source.format == aq_observation::Format::OpenCodeSessionDto6c24ec9
                && source.reader.is_none()
                && source.record_cursor.as_ref().map(Text::as_str) == expected_cursor,
            "public record channel scope/cursor mismatch"
        );
        let frame = engine::framing::Frame {
            start: source.cursor,
            bytes,
            oversized: false,
        };
        self.process_frame(
            id,
            &frame,
            observed_at_ms,
            None,
            Some(RecordAdvance {
                expected: source.record_cursor,
                next: next_cursor,
            }),
        )
    }
}
