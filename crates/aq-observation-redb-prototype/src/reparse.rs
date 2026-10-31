//! Explicit verified retained replay, known-fact corrections only. No source
//! path is opened here and no worker-authored receipt authenticates retention.
use super::*;
use aq_observation_engine::{self as engine, rows::Table as RowTable};
use std::collections::{BTreeMap, BTreeSet};
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReplayPlan {
    pub operation_id: Uuid,
    pub source_id: Uuid,
    pub source_generation: u64,
    pub binding: Binding,
    pub native_session_key: Text,
    pub old_parser: ArtifactRef,
    pub new_parser: ArtifactRef,
    pub retained_cursor: u64,
    pub transcript: RetentionReceipt,
    pub known: BTreeMap<String, engine::ReplayFact>,
}
impl ReplayPlan {
    pub fn validate(&self) -> Result<()> {
        ensure!(
            !self.operation_id.is_nil()
                && !self.source_id.is_nil()
                && self.source_generation > 0
                && self.retained_cursor > 0
                && !self.known.is_empty()
                && self.known.len() <= 128,
            "replay identity/bounds"
        );
        self.binding.validate()?;
        self.native_session_key.validate()?;
        artifact(&self.old_parser)?;
        artifact(&self.new_parser)?;
        ensure!(
            self.old_parser != self.new_parser,
            "replay requires distinct parser"
        );
        self.transcript.validate()?;
        ensure!(
            self.transcript.role == ArtifactRole::Transcript
                && self.transcript.binding_digest == canonical_digest(&self.binding)?
                && self.transcript.bytes == self.retained_cursor
                && self.transcript.bytes <= 4 * 1024 * 1024,
            "retained transcript scope/length"
        );
        for (key, fact) in &self.known {
            ensure!(
                !key.is_empty() && key.len() <= 1024,
                "replay native key bound"
            );
            fact.prior.validate()?;
        }
        Ok(())
    }
    pub fn digest(&self) -> Result<Digest> {
        self.validate()?;
        let mut value = serde_json::to_value(self)?;
        value.sort_all_objects();
        Ok(Digest::of(&serde_json::to_vec(&value)?))
    }
}
pub trait ReplayAuthority {
    fn admit(&self, plan: &ReplayPlan) -> Result<()>;
}
pub struct VerifiedRetained {
    plan: ReplayPlan,
    bytes: Vec<u8>,
}
impl VerifiedRetained {
    pub fn verify(
        plan: ReplayPlan,
        bytes: Vec<u8>,
        now: u64,
        authority: &impl ReplayAuthority,
    ) -> Result<Self> {
        plan.validate()?;
        ensure!(
            bytes.len() as u64 == plan.transcript.bytes
                && Digest::of(&bytes) == plan.transcript.artifact.digest
                && now >= plan.transcript.retained_at_ms
                && now < plan.transcript.retained_until_ms
                && bytes.ends_with(b"\n"),
            "retained bytes/digest/interval"
        );
        let mut records = BTreeSet::new();
        let mut count = 0;
        for record in bytes.split_inclusive(|b| *b == b'\n') {
            count += 1;
            ensure!(
                count <= 4096 && record.len() <= engine::framing::MAX_RECORD_BYTES,
                "replay record bound"
            );
            records.insert(Digest::of(record));
        }
        ensure!(
            plan.known
                .values()
                .all(|f| records.contains(&f.selected_record_digest)),
            "selected replay bytes absent"
        );
        authority.admit(&plan)?;
        Ok(Self { plan, bytes })
    }
    pub fn plan(&self) -> &ReplayPlan {
        &self.plan
    }
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RetiredEpoch {
    pub epoch: u64,
    pub counts: BTreeMap<RowTable, usize>,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ActiveReplay {
    pub plan: ReplayPlan,
    pub plan_digest: Digest,
    pub offset: u64,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct CompletedReplay {
    pub operation_id: Uuid,
    pub plan_digest: Digest,
}
impl Store {
    pub fn begin_reparse(&self, replay: &VerifiedRetained) -> Result<()> {
        let plan = &replay.plan;
        let digest = plan.digest()?;
        let tx = self.begin()?;
        let mut meta = read_meta(&tx)?;
        let mut sources = tx.open_table(SOURCES)?;
        let prior = sources
            .get(plan.source_id.as_bytes().as_slice())?
            .context("source absent")?
            .value()
            .to_vec();
        let mut row: SourceRow = decode(&prior)?;
        let state = row.engine.as_mut().context("source not engine-owned")?;
        if let Some(old) = state
            .completed_replays
            .iter()
            .find(|r| r.operation_id == plan.operation_id)
        {
            ensure!(
                old.plan_digest == digest,
                "completed replay ID payload conflict"
            );
            drop(sources);
            return self.abort(tx);
        }
        if let Some(active) = &state.replay {
            ensure!(
                active.plan_digest == digest && active.plan.operation_id == plan.operation_id,
                "active replay ID payload conflict"
            );
            drop(sources);
            return self.abort(tx);
        }
        ensure!(
            state.completed_replays.len() < 16
                && state.retired_epochs.len() < 16
                && row.cursor == plan.retained_cursor
                && row.source.source_id == plan.source_id
                && row.source.generation == plan.source_generation
                && row.source.binding == plan.binding
                && row.source.native_session_key == plan.native_session_key
                && state.context.parser == plan.old_parser
                && state.context.source.parser() == plan.new_parser,
            "reparse source/generation/parser/retained cursor scope"
        );
        {
            let seen = tx.open_table(SEEN)?;
            for (key, known) in &plan.known {
                let scoped = seen_key(plan.source_id, key)?;
                let old: SeenRow = decode(
                    seen.get(scoped.as_slice())?
                        .context("prior fact unknown")?
                        .value(),
                )?;
                ensure!(old.event == known.prior, "prior fact changed");
            }
        }
        if let Some(reader) = &row.reader
            && let Some(anchor) = &reader.cursor.anchor
        {
            let bytes = &replay.bytes;
            ensure!(
                bytes.len() >= anchor.bytes as usize
                    && Digest::of(&bytes[bytes.len() - anchor.bytes as usize..]) == anchor.digest,
                "retained file anchor mismatch"
            );
        }
        let mut counts = BTreeMap::new();
        for table in [
            RowTable::Native,
            RowTable::DisplayById,
            RowTable::DisplayByOrdinal,
        ] {
            counts.insert(table, state.counts.remove(&table).unwrap_or(0));
        }
        state.retired_epochs.push(RetiredEpoch {
            epoch: state.native_epoch,
            counts,
        });
        state.native_epoch = state
            .native_epoch
            .checked_add(1)
            .context("native epoch exhausted")?;
        let context = &mut state.context;
        context.parser = plan.new_parser.clone();
        context.history_replay = true;
        context.observed_native_identity =
            context.source.format == engine::Format::OpenCodeSessionDto6c24ec9;
        context.model = None;
        context.active_turn = None;
        context.mixed_models = false;
        context.context_gap = false;
        context.counter_generation = 1;
        context.last_cumulative = None;
        context.next_display_ordinal = 0;
        context.reparse = Some(engine::ReparseMetadata {
            operation_id: plan.operation_id,
            known: plan.known.clone(),
            handled: BTreeSet::new(),
        });
        if !row.parsers.contains(&plan.new_parser) {
            ensure!(row.parsers.len() < 16, "parser history bound");
            row.parsers.push(plan.new_parser.clone());
        }
        state.replay = Some(ActiveReplay {
            plan: plan.clone(),
            plan_digest: digest,
            offset: 0,
        });
        state.revision = state
            .revision
            .checked_add(1)
            .context("source revision exhausted")?;
        let bytes = encode(&row)?;
        meta.logical_bytes = meta.logical_bytes - prior.len() as u64 + bytes.len() as u64;
        ensure!(
            meta.logical_bytes <= self.limits.logical_bytes,
            "reparse metadata quota"
        );
        sources.insert(plan.source_id.as_bytes().as_slice(), bytes.as_slice())?;
        drop(sources);
        write_meta(&tx, &meta)?;
        self.commit(tx)
    }
    pub fn replay_next(&self, replay: &VerifiedRetained, now: u64) -> Result<CollectedFrame> {
        let plan = &replay.plan;
        ensure!(
            now >= plan.transcript.retained_at_ms && now < plan.transcript.retained_until_ms,
            "retained interval expired"
        );
        let row = self.source(plan.source_id)?;
        let active = row
            .engine
            .as_ref()
            .context("source not engine-owned")?
            .replay
            .as_ref()
            .context("no active replay")?;
        let digest = plan.digest()?;
        ensure!(
            active.plan_digest == digest && active.plan.operation_id == plan.operation_id,
            "replay handle mismatch"
        );
        let start = usize::try_from(active.offset)?;
        ensure!(start <= replay.bytes.len(), "retained replay cursor");
        let end = (start + engine::framing::MAX_READ_BYTES).min(replay.bytes.len());
        let framed = engine::framing::frame(&replay.bytes[start..end], active.offset, false)?;
        self.process_batch(plan.source_id, &framed.records, now, Some(&digest))
    }
    pub fn finish_reparse(&self, id: Uuid, operation_id: Uuid, digest: &Digest) -> Result<()> {
        let tx = self.begin()?;
        let mut meta = read_meta(&tx)?;
        let mut sources = tx.open_table(SOURCES)?;
        let prior = sources
            .get(id.as_bytes().as_slice())?
            .context("source absent")?
            .value()
            .to_vec();
        let mut row: SourceRow = decode(&prior)?;
        let state = row.engine.as_mut().context("source not engine-owned")?;
        if let Some(old) = state
            .completed_replays
            .iter()
            .find(|r| r.operation_id == operation_id)
        {
            ensure!(&old.plan_digest == digest, "completed replay ID conflict");
            drop(sources);
            return self.abort(tx);
        }
        let active = state.replay.as_ref().context("no active replay")?;
        let context = state
            .context
            .reparse
            .as_ref()
            .context("replay context absent")?;
        ensure!(
            active.plan.operation_id == operation_id
                && &active.plan_digest == digest
                && active.offset == active.plan.transcript.bytes
                && context.known.keys().all(|k| context.handled.contains(k)),
            "reparse identity/cursor/selected facts incomplete"
        );
        state.context.reparse = None;
        state.context.history_replay = false;
        state.completed_replays.push(CompletedReplay {
            operation_id,
            plan_digest: digest.clone(),
        });
        state.replay = None;
        state.revision = state
            .revision
            .checked_add(1)
            .context("source revision exhausted")?;
        let bytes = encode(&row)?;
        meta.logical_bytes = meta.logical_bytes - prior.len() as u64 + bytes.len() as u64;
        ensure!(
            meta.logical_bytes <= self.limits.logical_bytes,
            "reparse completion quota"
        );
        sources.insert(id.as_bytes().as_slice(), bytes.as_slice())?;
        drop(sources);
        write_meta(&tx, &meta)?;
        self.commit(tx)
    }
}
