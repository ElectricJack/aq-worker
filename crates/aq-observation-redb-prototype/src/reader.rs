//! Exact file input over the qualified bounded SourceFile helper. Paths remain
//! private local data; no path is fetched by the coordinator or put on wire.
use super::*;
use aq_observation::{
    source::{
        Cursor, FileIdentity, ReadChunk, ReadOutcome, SourceChange, SourceFile, SourcePermit,
    },
    spool::OverflowPolicy,
};
use aq_observation_engine as engine;
use std::os::unix::fs::MetadataExt;
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReaderState {
    pub permit: SourcePermit,
    pub root_identity: FileIdentity,
    pub cursor: Cursor,
    pub discarding: bool,
    pub last_change: Option<SourceChange>,
    pub history_checkpoint: Option<ArtifactRef>,
}
impl ReaderState {
    fn admit(permit: SourcePermit) -> Result<Self> {
        let root = std::fs::symlink_metadata(&permit.root)?;
        let root_identity = FileIdentity {
            device: root.dev(),
            inode: root.ino(),
        };
        let state = Self {
            permit,
            root_identity,
            cursor: Cursor::default(),
            discarding: false,
            last_change: None,
            history_checkpoint: None,
        };
        state.check()?;
        SourceFile::open(state.permit.clone())?;
        state.check()?;
        Ok(state)
    }
    fn change(&self) -> Result<Option<SourceChange>> {
        let root = match std::fs::symlink_metadata(&self.permit.root) {
            Ok(root) => root,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                return Ok(Some(SourceChange::Missing));
            }
            Err(error) => return Err(error.into()),
        };
        if !root.is_dir()
            || root.dev() != self.root_identity.device
            || root.ino() != self.root_identity.inode
            || std::fs::canonicalize(&self.permit.root)? != self.permit.root
        {
            return Ok(Some(SourceChange::Rotated));
        }
        ensure!(
            root.uid() == unsafe { libc::geteuid() } && root.mode() & 0o022 == 0,
            "source root ownership changed"
        );
        Ok(None)
    }
    fn check(&self) -> Result<()> {
        ensure!(self.change()?.is_none(), "source root identity changed");
        let root = std::fs::symlink_metadata(&self.permit.root)?;
        ensure!(
            root.is_dir()
                && root.dev() == self.root_identity.device
                && root.ino() == self.root_identity.inode
                && root.uid() == unsafe { libc::geteuid() }
                && root.mode() & 0o022 == 0
                && std::fs::canonicalize(&self.permit.root)? == self.permit.root,
            "source root identity/ownership changed"
        );
        Ok(())
    }
}
pub(super) struct ReaderAdvance {
    pub expected: ReaderState,
    pub next: ReaderState,
}
pub(super) struct FileBatch<'a> {
    pub expected: &'a ReaderState,
    pub chunk: &'a ReadChunk,
}
impl FileBatch<'_> {
    pub fn check(&self) -> Result<()> {
        self.expected.check()?;
        SourceFile::open(self.expected.permit.clone())?;
        self.expected.check()
    }
    pub fn advance(&self, end: u64, discarding: bool) -> Result<ReaderAdvance> {
        ensure!(end > self.chunk.start, "file consumed range");
        let cursor = self
            .chunk
            .cursor_after(usize::try_from(end - self.chunk.start)?)?;
        let mut next = self.expected.clone();
        next.cursor = cursor;
        next.discarding = discarding;
        next.last_change = None;
        Ok(ReaderAdvance {
            expected: self.expected.clone(),
            next,
        })
    }
}
impl Store {
    pub fn attach_file_engine(
        &self,
        source: engine::Source,
        maximum: usize,
        permit: SourcePermit,
    ) -> Result<()> {
        ensure!(
            source.format != engine::Format::OpenCodeSessionDto6c24ec9
                && source.source_id == permit.source_id
                && source.native_session_key == permit.native_session_key,
            "file source permit scope"
        );
        let reader = ReaderState::admit(permit)?;
        self.attach_engine_input(source, maximum, false, Some(reader))
    }
    pub fn collect_file(
        &self,
        id: Uuid,
        file: &SourceFile,
        observed_at_ms: u64,
        policy: OverflowPolicy,
    ) -> Result<CollectedFrame> {
        ensure!(
            !self.poisoned.get(),
            "store uncertain; reopen before source reads"
        );
        let row = self.source(id)?;
        ensure!(
            row.hold.is_none() && row.engine.as_ref().is_some_and(|e| e.replay.is_none()),
            "file source held/replaying"
        );
        let reader = row.reader.as_ref().context("source has no admitted file")?;
        ensure!(
            &reader.permit == file.permit(),
            "source handle differs from admitted permit"
        );
        if let Some(change) = reader.change()? {
            return self.source_change(id, &reader.permit, change, observed_at_ms);
        }
        let read = match file.read(&reader.cursor, engine::framing::MAX_READ_BYTES)? {
            ReadOutcome::Changed(reason) => {
                return self.source_change(id, &reader.permit, reason, observed_at_ms);
            }
            ReadOutcome::Ready(read) => read,
        };
        let framed = engine::framing::frame(&read.bytes, read.start, reader.discarding)?;
        let input = FileBatch {
            expected: reader,
            chunk: &read,
        };
        input.check()?;
        self.process_batch_input(
            id,
            &framed.records,
            observed_at_ms,
            None,
            policy,
            Some(&input),
        )
    }
    fn source_change(
        &self,
        id: Uuid,
        permit: &SourcePermit,
        change: SourceChange,
        observed_at_ms: u64,
    ) -> Result<CollectedFrame> {
        let tx = self.begin()?;
        let mut meta = read_meta(&tx)?;
        let mut sources = tx.open_table(SOURCES)?;
        let prior = sources
            .get(id.as_bytes().as_slice())?
            .context("source absent")?
            .value()
            .to_vec();
        let mut row: SourceRow = decode(&prior)?;
        ensure!(row.hold.is_none(), "source held");
        let reader = row.reader.as_mut().context("no file source")?;
        ensure!(&reader.permit == permit, "source permit changed");
        if reader.last_change.as_ref() == Some(&change) {
            drop(sources);
            self.abort(tx)?;
            return Ok(CollectedFrame {
                consumed_bytes: 0,
                facts_added: 0,
                indexed_lookups: 0,
                blocked: true,
            });
        }
        let reason = match change {
            SourceChange::Missing => GapReason::SourceMissing,
            SourceChange::Rotated => GapReason::Rotated,
            SourceChange::Truncated | SourceChange::AnchorChanged => GapReason::Truncated,
        };
        let parser = ArtifactRef {
            id: Text::new("row-file-source-monitor")?,
            version: Text::new("0.0.2")?,
            digest: Digest::of(include_bytes!("reader.rs")),
        };
        let body = Observation {
            binding: row.source.binding.clone(),
            provenance: Provenance {
                source_id: id,
                source_generation: row.source.generation,
                source_kind: SourceKind::NativeTranscript,
                native_session_key: row.source.native_session_key.clone(),
                cursor: SourceCursor::Record {
                    id: Text::new(format!(
                        "source-change:{change:?}:{}:{}",
                        row.source.generation, row.cursor
                    ))?,
                },
                parser: parser.clone(),
                raw_record: RawRecordEvidence::Unavailable {
                    reason: reason.clone(),
                },
            },
            sequence: meta.next,
            observed_at_ms,
            kind: ObservationKind::Gap {
                reason,
                affected: None,
            },
        };
        let event = ObservationRecord::seal(stable(&body)?, body)?;
        let bytes = encode(&event)?;
        let digest = encode(&event.body_digest)?;
        ensure!(
            meta.gaps < 16 && meta.gap_bytes + bytes.len() as u64 <= 65536,
            "reserved source-gap quota"
        );
        let mut out = tx.open_table(OUTBOX)?;
        let mut ids = tx.open_table(EVENTS)?;
        ensure!(
            ids.get(event.event_id.as_bytes().as_slice())?.is_none(),
            "source-gap ID conflict"
        );
        out.insert(meta.next, bytes.as_slice())?;
        ids.insert(event.event_id.as_bytes().as_slice(), digest.as_slice())?;
        drop(ids);
        drop(out);
        meta.next = meta.next.checked_add(1).context("sequence exhausted")?;
        meta.gaps += 1;
        meta.gap_bytes += bytes.len() as u64;
        meta.logical_bytes += (bytes.len() + digest.len()) as u64;
        reader.last_change = Some(change);
        if !row.parsers.contains(&parser) {
            ensure!(row.parsers.len() < 16, "source parser history bound");
            row.parsers.push(parser);
        }
        let state = row.engine.as_mut().context("source not engine-owned")?;
        state.revision = state
            .revision
            .checked_add(1)
            .context("source revision exhausted")?;
        let bytes = encode(&row)?;
        meta.logical_bytes = meta.logical_bytes - prior.len() as u64 + bytes.len() as u64;
        ensure!(
            meta.logical_bytes <= self.limits.logical_bytes,
            "source-gap metadata quota"
        );
        sources.insert(id.as_bytes().as_slice(), bytes.as_slice())?;
        drop(sources);
        write_meta(&tx, &meta)?;
        self.commit(tx)?;
        Ok(CollectedFrame {
            consumed_bytes: 0,
            facts_added: 1,
            indexed_lookups: 0,
            blocked: true,
        })
    }
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SourceOperation {
    pub operation_id: Uuid,
    pub digest: Digest,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FileGenerationPlan {
    pub operation_id: Uuid,
    pub source_id: Uuid,
    pub binding: Binding,
    pub expected_generation: u64,
    pub new_generation: u64,
    pub parser: ArtifactRef,
    pub new_permit: SourcePermit,
    pub owner_checkpoint: ArtifactRef,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HistoryBoundary {
    pub operation_id: Uuid,
    pub source_id: Uuid,
    pub binding: Binding,
    pub generation: u64,
    pub cursor: u64,
    pub checkpoint: ArtifactRef,
}
/// Local host/source owner verifies grants and replay boundaries from sealed
/// launch/retention state. No permissive runtime implementation is supplied.
pub trait SourceAuthority {
    fn admit_generation(&self, plan: &FileGenerationPlan) -> Result<()>;
    fn admit_history(&self, boundary: &HistoryBoundary) -> Result<()>;
}
pub struct VerifiedFileGeneration {
    plan: FileGenerationPlan,
    reader: ReaderState,
    digest: Digest,
}
pub struct VerifiedHistoryBoundary {
    plan: HistoryBoundary,
    digest: Digest,
}
fn plan_digest<T: Serialize>(plan: &T) -> Result<Digest> {
    let mut v = serde_json::to_value(plan)?;
    v.sort_all_objects();
    let bytes = serde_json::to_vec(&v)?;
    ensure!(bytes.len() <= 16384, "source operation byte bound");
    Ok(Digest::of(&bytes))
}
impl VerifiedFileGeneration {
    pub fn verify(plan: FileGenerationPlan, owner: &impl SourceAuthority) -> Result<Self> {
        ensure!(
            !plan.operation_id.is_nil()
                && !plan.source_id.is_nil()
                && plan.expected_generation > 0
                && plan.expected_generation.checked_add(1) == Some(plan.new_generation)
                && plan.source_id == plan.new_permit.source_id,
            "file generation identity/sequence"
        );
        plan.binding.validate()?;
        artifact(&plan.parser)?;
        artifact(&plan.owner_checkpoint)?;
        let reader = ReaderState::admit(plan.new_permit.clone())?;
        let digest = plan_digest(&plan)?;
        owner.admit_generation(&plan)?;
        Ok(Self {
            plan,
            reader,
            digest,
        })
    }
}
impl VerifiedHistoryBoundary {
    pub fn verify(plan: HistoryBoundary, owner: &impl SourceAuthority) -> Result<Self> {
        ensure!(
            !plan.operation_id.is_nil() && !plan.source_id.is_nil() && plan.generation > 0,
            "history boundary identity"
        );
        plan.binding.validate()?;
        artifact(&plan.checkpoint)?;
        let digest = plan_digest(&plan)?;
        owner.admit_history(&plan)?;
        Ok(Self { plan, digest })
    }
}
impl Store {
    pub fn adopt_file_generation(
        &self,
        verified: &VerifiedFileGeneration,
        observed_at_ms: u64,
    ) -> Result<()> {
        ensure!(!self.poisoned.get(), "store uncertain; reopen required");
        let plan = &verified.plan;
        let known = self.source(plan.source_id)?;
        if let Some(old) = known
            .source_operations
            .iter()
            .find(|op| op.operation_id == plan.operation_id)
        {
            ensure!(
                old.digest == verified.digest,
                "source operation ID payload conflict"
            );
            return Ok(());
        }
        verified.reader.check()?;
        SourceFile::open(verified.reader.permit.clone())?;
        let tx = self.begin()?;
        let mut meta = read_meta(&tx)?;
        let mut sources = tx.open_table(SOURCES)?;
        let prior = sources
            .get(plan.source_id.as_bytes().as_slice())?
            .context("source absent")?
            .value()
            .to_vec();
        let mut row: SourceRow = decode(&prior)?;
        if let Some(old) = row
            .source_operations
            .iter()
            .find(|op| op.operation_id == plan.operation_id)
        {
            ensure!(
                old.digest == verified.digest,
                "source operation ID payload conflict"
            );
            drop(sources);
            return self.abort(tx);
        }
        let state = row.engine.as_mut().context("source not engine-owned")?;
        ensure!(
            row.reader.is_some()
                && row.source_operations.len() < 64
                && row.generations.len() < 64
                && row.source.binding == plan.binding
                && row.source.generation == plan.expected_generation
                && row.source.native_session_key == plan.new_permit.native_session_key
                && state.replay.is_none()
                && state.context.parser == plan.parser
                && state.context.parser == state.context.source.parser()
                && matches!(row.source.binding.session,SessionIdentity::Remote{boot_id,..}if boot_id==meta.active_boot),
            "file generation scope/parser/active boot"
        );
        let reason = GapReason::Rotated;
        let parser = ArtifactRef {
            id: Text::new("row-file-source-monitor")?,
            version: Text::new("0.0.2")?,
            digest: Digest::of(include_bytes!("reader.rs")),
        };
        let body = Observation {
            binding: row.source.binding.clone(),
            provenance: Provenance {
                source_id: plan.source_id,
                source_generation: row.source.generation,
                source_kind: SourceKind::NativeTranscript,
                native_session_key: row.source.native_session_key.clone(),
                cursor: SourceCursor::Record {
                    id: Text::new(format!(
                        "adopt-generation:{}:{}",
                        plan.operation_id, plan.new_generation
                    ))?,
                },
                parser: parser.clone(),
                raw_record: RawRecordEvidence::Unavailable {
                    reason: reason.clone(),
                },
            },
            sequence: meta.next,
            observed_at_ms,
            kind: ObservationKind::Gap {
                reason,
                affected: None,
            },
        };
        let event = ObservationRecord::seal(stable(&body)?, body)?;
        let bytes = encode(&event)?;
        let digest = encode(&event.body_digest)?;
        ensure!(
            meta.gaps < 16 && meta.gap_bytes + bytes.len() as u64 <= 65536,
            "reserved generation gap quota"
        );
        let mut out = tx.open_table(OUTBOX)?;
        let mut ids = tx.open_table(EVENTS)?;
        ensure!(
            ids.get(event.event_id.as_bytes().as_slice())?.is_none(),
            "generation fact conflict"
        );
        out.insert(meta.next, bytes.as_slice())?;
        ids.insert(event.event_id.as_bytes().as_slice(), digest.as_slice())?;
        drop(ids);
        drop(out);
        meta.next = meta.next.checked_add(1).context("sequence exhausted")?;
        meta.gaps += 1;
        meta.gap_bytes += bytes.len() as u64;
        meta.logical_bytes += (bytes.len() + digest.len()) as u64;
        if !row.parsers.contains(&parser) {
            ensure!(row.parsers.len() < 16, "source parser history bound");
            row.parsers.push(parser);
        }
        row.source.generation = plan.new_generation;
        row.generations.push(plan.new_generation);
        row.cursor = 0;
        row.reader = Some(verified.reader.clone());
        row.hold = None;
        row.source_operations.push(SourceOperation {
            operation_id: plan.operation_id,
            digest: verified.digest.clone(),
        });
        state.context.source.generation = plan.new_generation;
        state.context.observed_native_identity = false;
        state.context.model = None;
        state.context.active_turn = None;
        state.context.history_replay = true;
        state.revision = state
            .revision
            .checked_add(1)
            .context("source revision exhausted")?;
        let bytes = encode(&row)?;
        meta.logical_bytes = meta.logical_bytes - prior.len() as u64 + bytes.len() as u64;
        ensure!(
            meta.logical_bytes <= self.limits.logical_bytes,
            "generation metadata quota"
        );
        sources.insert(plan.source_id.as_bytes().as_slice(), bytes.as_slice())?;
        drop(sources);
        write_meta(&tx, &meta)?;
        self.commit(tx)
    }
    pub fn finish_file_history(&self, verified: &VerifiedHistoryBoundary) -> Result<()> {
        let plan = &verified.plan;
        let tx = self.begin()?;
        let mut meta = read_meta(&tx)?;
        let mut sources = tx.open_table(SOURCES)?;
        let prior = sources
            .get(plan.source_id.as_bytes().as_slice())?
            .context("source absent")?
            .value()
            .to_vec();
        let mut row: SourceRow = decode(&prior)?;
        if let Some(old) = row
            .source_operations
            .iter()
            .find(|op| op.operation_id == plan.operation_id)
        {
            ensure!(
                old.digest == verified.digest,
                "source history operation conflict"
            );
            drop(sources);
            return self.abort(tx);
        }
        let state = row.engine.as_mut().context("source not engine-owned")?;
        ensure!(
            row.source_operations.len() < 64
                && row.source.binding == plan.binding
                && row.source.generation == plan.generation
                && row.cursor == plan.cursor
                && row.hold.is_none()
                && state.replay.is_none()
                && state.context.observed_native_identity
                && matches!(row.source.binding.session,SessionIdentity::Remote{boot_id,..}if boot_id==meta.active_boot),
            "history boundary scope/cursor/status"
        );
        row.reader
            .as_mut()
            .context("source has no file")?
            .history_checkpoint = Some(plan.checkpoint.clone());
        state.context.history_replay = false;
        state.revision = state
            .revision
            .checked_add(1)
            .context("revision exhausted")?;
        row.source_operations.push(SourceOperation {
            operation_id: plan.operation_id,
            digest: verified.digest.clone(),
        });
        let bytes = encode(&row)?;
        meta.logical_bytes = meta.logical_bytes - prior.len() as u64 + bytes.len() as u64;
        ensure!(
            meta.logical_bytes <= self.limits.logical_bytes,
            "history boundary quota"
        );
        sources.insert(plan.source_id.as_bytes().as_slice(), bytes.as_slice())?;
        drop(sources);
        write_meta(&tx, &meta)?;
        self.commit(tx)
    }
}
