//! Transactional normalizer state. Clone/apply is committed with its outgoing
//! observations by the spool; a reader must not save only the returned cursor.
use crate::rows::{Overlay, ReadView, RowDelta, Table};
use crate::{formats, framing::Frame, lifecycle::*};
use anyhow::{Context as _, Result, ensure};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest as _, Sha256};
use std::collections::{BTreeMap, BTreeSet};
use uuid::Uuid;
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum Format {
    CodexRollout67f0994,
    ClaudeV1Jsonl9a9b8e8,
    GeminiRecording6814e00,
    OpenCodeSessionDto6c24ec9,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Source {
    pub binding: Binding,
    pub source_id: Uuid,
    pub generation: u64,
    pub native_session_key: Text,
    pub format: Format,
    pub provider: Text,
    pub account_reference: Text,
    /// Owner-registered provenance key (profiled attempts use
    /// `aq.attempt/<attempt_id>/<generation>`). Emitted events carry it, while
    /// native transcript identity checks keep using `native_session_key`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub registered_session_key: Option<Text>,
}
impl Source {
    pub fn validate(&self) -> Result<()> {
        self.binding.validate()?;
        ensure!(
            !self.source_id.is_nil() && self.generation > 0,
            "source identity/generation"
        );
        self.native_session_key.validate()?;
        self.provider.validate()?;
        self.account_reference.validate()?;
        if let Some(key) = &self.registered_session_key {
            key.validate()?;
        }
        Ok(())
    }
    /// The session key emitted in observation provenance.
    pub fn provenance_session_key(&self) -> &Text {
        self.registered_session_key
            .as_ref()
            .unwrap_or(&self.native_session_key)
    }
    pub fn parser(&self) -> ArtifactRef {
        let (id, bytes) = match self.format {
            Format::CodexRollout67f0994 => (
                "codex-rollout-67f0994",
                include_bytes!("formats/codex.rs").as_slice(),
            ),
            Format::ClaudeV1Jsonl9a9b8e8 => (
                "claude-v1-9a9b8e8",
                include_bytes!("formats/claude.rs").as_slice(),
            ),
            Format::GeminiRecording6814e00 => (
                "gemini-recording-6814e00",
                include_bytes!("formats/gemini.rs").as_slice(),
            ),
            Format::OpenCodeSessionDto6c24ec9 => (
                "opencode-session-dto-6c24ec9",
                include_bytes!("formats/opencode.rs").as_slice(),
            ),
        };
        ArtifactRef {
            id: Text::new(id).expect("static parser id"),
            version: Text::new("0.0.1-engine").expect("static parser version"),
            digest: Digest::of(
                &[
                    bytes,
                    include_bytes!("normalize.rs").as_slice(),
                    include_bytes!("framing.rs").as_slice(),
                    include_bytes!("json.rs").as_slice(),
                    include_bytes!("rows.rs").as_slice(),
                ]
                .concat(),
            ),
        }
    }
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Seen {
    pub normalized_digest: Digest,
    pub event: EventRef,
    pub raw_record_digests: Vec<Digest>,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Mutation {
    pub source_record_digest: Digest,
    pub operation: Text,
    pub affected_native_ids: Vec<Text>,
    pub prior_events: Vec<EventRef>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReplayFact {
    pub prior: EventRef,
    pub selected_record_digest: Digest,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReparseMetadata {
    pub operation_id: Uuid,
    pub known: BTreeMap<String, ReplayFact>,
    pub handled: BTreeSet<String>,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Context {
    pub source: Source,
    pub next_sequence: u64,
    pub parser: ArtifactRef,
    pub history_replay: bool,
    pub reparse: Option<ReparseMetadata>,
    pub observed_native_identity: bool,
    pub model: Option<Text>,
    pub active_turn: Option<Text>,
    pub mixed_models: bool,
    pub context_gap: bool,
    pub counter_generation: u64,
    pub last_cumulative: Option<Value>,
    pub maximum_tracked: usize,
    pub next_display_ordinal: u64,
}
impl Context {
    pub fn new(source: Source, maximum_tracked: usize) -> Result<Self> {
        source.validate()?;
        ensure!(
            (16..=4096).contains(&maximum_tracked),
            "tracked identity bound"
        );
        let parser = source.parser();
        Ok(Self {
            source,
            next_sequence: 1,
            parser,
            history_replay: false,
            reparse: None,
            observed_native_identity: false,
            model: None,
            active_turn: None,
            mixed_models: false,
            context_gap: false,
            counter_generation: 1,
            last_cumulative: None,
            maximum_tracked,
            next_display_ordinal: 0,
        })
    }
    pub fn bind_scoped_record_channel(mut self, key: &str) -> Result<Self> {
        ensure!(
            self.source.format == Format::OpenCodeSessionDto6c24ec9,
            "only DTO uses scoped record channel"
        );
        identity(key == self.source.native_session_key.as_str())?;
        self.observed_native_identity = true;
        Ok(self)
    }
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NormalizationDelta {
    pub context: Context,
    pub rows: RowDelta,
    pub events: Vec<ObservationRecord>,
    pub indexed_lookups: usize,
}
pub fn normalize(
    context: &Context,
    view: &dyn ReadView,
    frame: &Frame,
    observed_at_ms: u64,
) -> Result<NormalizationDelta> {
    ensure!(
        (16..=4096).contains(&context.maximum_tracked)
            && context.next_sequence > 0
            && context.counter_generation > 0,
        "context counter/bounds"
    );
    ensure!(
        serde_json::to_vec(context)?.len() <= 65536,
        "context byte bound"
    );
    if let Some(r) = &context.reparse {
        ensure!(
            r.known.len() <= 128
                && r.handled.len() <= r.known.len()
                && r.handled.iter().all(|k| r.known.contains_key(k)),
            "reparse context bound"
        );
    }
    let state = Normalizer {
        context: context.clone(),
        rows: Overlay::new(view),
    };
    let (next, events) = state.apply(frame, observed_at_ms)?;
    next.rows.delta.validate()?;
    ensure!(
        next.rows.delta.writes.len() <= 512 && serde_json::to_vec(&next.context)?.len() <= 65536,
        "delta context bound"
    );
    Ok(NormalizationDelta {
        context: next.context,
        rows: next.rows.delta,
        events,
        indexed_lookups: next.rows.lookups,
    })
}
#[derive(Clone)]
pub(crate) struct Draft {
    pub key: String,
    pub kind: ObservationKind,
}
#[derive(Clone)]
pub(crate) struct Normalizer<'a> {
    context: Context,
    rows: Overlay<'a>,
}
impl std::ops::Deref for Normalizer<'_> {
    type Target = Context;
    fn deref(&self) -> &Context {
        &self.context
    }
}
impl std::ops::DerefMut for Normalizer<'_> {
    fn deref_mut(&mut self) -> &mut Context {
        &mut self.context
    }
}
impl Normalizer<'_> {
    pub fn seen_get(&mut self, key: &str) -> Result<Option<Seen>> {
        let value: Option<Seen> = self.rows.get(Table::Seen, key)?;
        if let Some(row) = &value {
            row.event
                .validate()
                .map_err(|e| crate::rows::StateFailure(e.to_string()))?;
            if row.raw_record_digests.is_empty() || row.raw_record_digests.len() > 32 {
                return Err(crate::rows::StateFailure("retained seen bound".into()).into());
            }
        }
        Ok(value)
    }
    pub fn seen_put(&mut self, key: String, value: Seen) -> Result<()> {
        if key.starts_with("codex-total:") {
            let suffix = key.rsplit(':').next().unwrap();
            self.rows.put(Table::Snapshot, suffix.to_owned(), true)?;
        }
        self.rows.put(Table::Seen, key, value)
    }
    pub fn snapshot_known(&mut self, digest: &Digest) -> Result<bool> {
        Ok(self
            .rows
            .get::<bool>(Table::Snapshot, digest.as_str())?
            .unwrap_or(false))
    }
    pub fn native_get(&mut self, key: &str) -> Result<Option<Value>> {
        self.rows.get(Table::Native, key)
    }
    pub fn native_put(&mut self, key: String, value: Value) -> Result<()> {
        self.rows.put(Table::Native, key, value)
    }
    pub fn native_remove(&mut self, key: &str) -> Result<()> {
        self.rows.remove(Table::Native, key.to_owned())
    }
    pub fn mutation_known(&mut self, digest: &Digest, operation: &str) -> Result<bool> {
        Ok(self
            .rows
            .get::<Mutation>(Table::Mutation, &format!("{}:{operation}", digest.as_str()))?
            .is_some())
    }
    pub fn mutation_put(&mut self, mutation: Mutation) -> Result<()> {
        self.rows.put(
            Table::Mutation,
            format!(
                "{}:{}",
                mutation.source_record_digest.as_str(),
                mutation.operation.as_str()
            ),
            mutation,
        )
    }
    pub fn display_append(&mut self, id: &str) -> Result<()> {
        let ordinal = self.next_display_ordinal;
        self.next_display_ordinal = ordinal
            .checked_add(1)
            .context("display ordinal exhausted")?;
        self.rows.put(Table::DisplayById, id.into(), ordinal)?;
        self.rows
            .put(Table::DisplayByOrdinal, ordinal.to_string(), id)
    }
    pub fn display_remove(&mut self, id: &str) -> Result<()> {
        if let Some(ordinal) = self.rows.get::<u64>(Table::DisplayById, id)? {
            self.rows
                .remove(Table::DisplayByOrdinal, ordinal.to_string())?;
            self.rows.remove(Table::DisplayById, id.into())?;
        }
        Ok(())
    }
    pub fn display_suffix(&mut self, id: Option<&str>) -> Result<Vec<String>> {
        let start = match id {
            Some(id) => self.rows.get::<u64>(Table::DisplayById, id)?.unwrap_or(0),
            None => 0,
        };
        let rows = self.rows.display_suffix(start)?;
        ensure!(
            rows.len() <= 32,
            "display mutation exceeds bounded32 visible rows"
        );
        Ok(rows.into_iter().map(|(_, id)| id).collect())
    }
    pub fn apply(
        &self,
        frame: &Frame,
        observed_at_ms: u64,
    ) -> Result<(Self, Vec<ObservationRecord>)> {
        ensure!(
            !frame.bytes.is_empty() && frame.bytes.len() <= crate::framing::MAX_RECORD_BYTES,
            "frame byte bound"
        );
        self.source.validate()?;
        ensure!(
            self.parser == self.source.parser(),
            "parser changed; explicit reconciliation required"
        );
        let mut next = self.clone();
        let raw_digest = Digest::of(&frame.bytes);
        let source_cursor = if self.source.format == Format::OpenCodeSessionDto6c24ec9 {
            let record = crate::json::parse(&frame.bytes).ok();
            let id = record.as_ref().and_then(|r| r["id"].as_str());
            SourceCursor::Record {
                id: Text::new(
                    id.unwrap_or(&format!("unidentified-record:{}", raw_digest.as_str())),
                )?,
            }
        } else {
            SourceCursor::Bytes {
                start: frame.start,
                end: frame.end()?,
            }
        };
        let drafts = if frame.oversized {
            vec![gap(
                GapReason::Malformed,
                format!("oversized:{}:{}", self.source.generation, frame.start),
            )]
        } else {
            match crate::json::parse(&frame.bytes) {
                Ok(value) => {
                    let result = match next.source.format {
                        Format::CodexRollout67f0994 => {
                            formats::codex::normalize(&mut next, &value, &raw_digest)
                        }
                        Format::ClaudeV1Jsonl9a9b8e8 => {
                            formats::claude::normalize(&mut next, &value)
                        }
                        Format::GeminiRecording6814e00 => {
                            formats::gemini::normalize(&mut next, &value, &raw_digest)
                        }
                        Format::OpenCodeSessionDto6c24ec9 => {
                            formats::opencode::normalize(&mut next, &value)
                        }
                    };
                    match result {
                        Ok(v) => v,
                        Err(error)
                            if error.downcast_ref::<SourceMismatch>().is_some()
                                || error.downcast_ref::<crate::rows::StateFailure>().is_some() =>
                        {
                            return Err(error);
                        }
                        Err(_) => {
                            next = self.clone();
                            vec![gap(
                                GapReason::Malformed,
                                format!("malformed:{}:{}", self.source.generation, frame.start),
                            )]
                        }
                    }
                }
                Err(_) => vec![gap(
                    GapReason::Malformed,
                    format!("malformed:{}:{}", self.source.generation, frame.start),
                )],
            }
        };
        if drafts.iter().any(|d| {
            matches!(
                d.kind,
                ObservationKind::Gap {
                    reason: GapReason::Malformed | GapReason::UnsupportedFormat,
                    ..
                }
            )
        }) {
            next.context_gap = true;
            next.model = None;
            next.active_turn = None;
        }
        ensure!(
            drafts.len() <= 32,
            "record fact budget exceeded; hold cursor"
        );
        let mut events = vec![];
        for mut draft in drafts {
            if let Some(reparse) = &mut next.reparse {
                match reparse.known.get(&draft.key) {
                    Some(known) => {
                        if known.selected_record_digest != raw_digest
                            || reparse.handled.contains(&draft.key)
                        {
                            continue;
                        }
                        reparse.handled.insert(draft.key.clone());
                    }
                    None => {
                        draft = gap(
                            GapReason::ReplayUnavailable,
                            format!(
                                "unknown-reparse:{}:{}:{}",
                                reparse.operation_id,
                                draft.key,
                                raw_digest.as_str()
                            ),
                        );
                    }
                }
            }
            let mut normalized_digest = canonical_digest(&draft.kind)?;
            if let Some(mut old) = next.seen_get(&draft.key)? {
                if next.reparse.is_none() && old.raw_record_digests.contains(&raw_digest) {
                    continue;
                }
                if old.normalized_digest == normalized_digest {
                    if old.raw_record_digests.contains(&raw_digest) {
                        continue;
                    }
                    ensure!(
                        old.raw_record_digests.len() < 32,
                        "native replay identity history full; hold cursor"
                    );
                    old.raw_record_digests.push(raw_digest.clone());
                    next.seen_put(draft.key.clone(), old)?;
                    continue;
                }
                if next.history_replay && next.reparse.is_none() && draft.kind.is_usage() {
                    draft = gap(
                        GapReason::ReplayUnavailable,
                        format!(
                            "historical-changed-usage:{}:{}",
                            draft.key,
                            raw_digest.as_str()
                        ),
                    );
                    normalized_digest = canonical_digest(&draft.kind)?;
                }
            }
            ensure!(
                next.rows.count(Table::Seen)? < next.maximum_tracked
                    || next.seen_get(&draft.key)?.is_some(),
                "native identity history full; hold cursor"
            );
            let mut kind = draft.kind;
            if let ObservationKind::Usage { usage } = &mut kind {
                usage.correction_of = next.seen_get(&draft.key)?.map(|v| v.event.clone());
            }
            let event_id = stable_id(
                &serde_json::json!({"binding":next.source.binding,"source_id":next.source.source_id,"native_key":draft.key,"fact_digest":normalized_digest,"parser":next.parser,"previous":next.seen_get(&draft.key)?.map(|v|v.event)}),
            )?;
            let body = Observation {
                binding: next.source.binding.clone(),
                provenance: Provenance {
                    source_id: next.source.source_id,
                    source_generation: next.source.generation,
                    source_kind: if next.history_replay {
                        SourceKind::RetainedReplay
                    } else if next.source.format == Format::OpenCodeSessionDto6c24ec9 {
                        SourceKind::StructuredProviderEvent
                    } else {
                        SourceKind::NativeTranscript
                    },
                    native_session_key: next.source.provenance_session_key().clone(),
                    cursor: source_cursor.clone(),
                    parser: next.parser.clone(),
                    raw_record: RawRecordEvidence::Available {
                        digest: raw_digest.clone(),
                        byte_length: frame.bytes.len() as u64,
                    },
                },
                sequence: next.next_sequence,
                observed_at_ms,
                kind,
            };
            let event = ObservationRecord::seal(event_id, body)?;
            next.next_sequence = next
                .next_sequence
                .checked_add(1)
                .context("observation sequence exhausted")?;
            let mut raw_record_digests = next
                .seen_get(&draft.key)?
                .map(|v| v.raw_record_digests.clone())
                .unwrap_or_default();
            ensure!(
                raw_record_digests.len() < 32,
                "per-call correction history full; hold cursor"
            );
            if !raw_record_digests.contains(&raw_digest) {
                raw_record_digests.push(raw_digest.clone());
            }
            next.seen_put(
                draft.key,
                Seen {
                    normalized_digest,
                    raw_record_digests,
                    event: EventRef {
                        event_id,
                        digest: event.body_digest.clone(),
                    },
                },
            )?;
            events.push(event);
        }
        ensure!(
            next.rows.count(Table::Native)? <= next.maximum_tracked
                && next.rows.count(Table::Mutation)? <= 128,
            "native state bound; hold cursor"
        );
        Ok((next, events))
    }
}
#[derive(Debug)]
pub(crate) struct SourceMismatch;
impl std::fmt::Display for SourceMismatch {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("native source does not match admitted session")
    }
}
impl std::error::Error for SourceMismatch {}
pub(crate) fn identity(ok: bool) -> Result<()> {
    if ok {
        Ok(())
    } else {
        Err(SourceMismatch.into())
    }
}
pub(crate) fn stable_id(value: &Value) -> Result<Uuid> {
    let b = value_bytes(value)?;
    let hash = Sha256::digest(b);
    let mut id = [0; 16];
    id.copy_from_slice(&hash[..16]);
    id[6] = (id[6] & 0x0f) | 0x80;
    id[8] = (id[8] & 0x3f) | 0x80;
    Ok(Uuid::from_bytes(id))
}
pub(crate) fn gap(reason: GapReason, key: String) -> Draft {
    Draft {
        key,
        kind: ObservationKind::Gap {
            reason,
            affected: None,
        },
    }
}
pub(crate) fn text(value: &Value) -> Result<&str> {
    let s = value.as_str().context("native string absent")?;
    ensure!(!s.is_empty() && s.len() <= 512, "native identity bound");
    Ok(s)
}
pub(crate) fn count(value: Option<&Value>) -> Result<Counter> {
    match value {
        None | Some(Value::Null) => Ok(Counter::Unknown {
            reason: UnknownReason::NotReported,
        }),
        Some(v) => Ok(Counter::Known {
            value: v
                .as_u64()
                .context("native token count not a nonnegative integer")?,
        }),
    }
}
pub(crate) fn unsupported() -> Counter {
    Counter::Unknown {
        reason: UnknownReason::Unsupported,
    }
}
pub(crate) fn content_ref(value: &Value) -> Result<ArtifactRef> {
    let digest = if let Some(s) = value.as_str() {
        Digest::of(s.as_bytes())
    } else {
        value_digest(value)?
    };
    Ok(ArtifactRef {
        id: Text::new(format!("logical-content:{}", digest.as_str()))?,
        version: Text::new("1")?,
        digest,
    })
}
pub(crate) fn content_draft(key: String, role: TextRole, value: &Value) -> Result<Draft> {
    Ok(Draft {
        key,
        kind: ObservationKind::TextReference {
            role,
            content: content_ref(value)?,
            utf8_bytes: if let Some(s) = value.as_str() {
                s.len() as u64
            } else {
                serde_json::to_vec(value)?.len() as u64
            },
        },
    })
}
pub(crate) fn usage(
    source: &Source,
    model: Option<&str>,
    call_id: &str,
    basis: CounterBasis,
    counters: Counters,
) -> Result<Usage> {
    Ok(Usage {
        call: ProviderCall {
            provider: source.provider.clone(),
            account_reference: source.account_reference.clone(),
            model: Text::new(model.unwrap_or("unreported:model"))?,
            native_call_id: Text::new(call_id)?,
        },
        basis,
        counters,
        category_semantics: CategorySemantics::MayOverlap {
            policy: source.parser(),
        },
        cost: Cost::Unknown {
            reason: UnknownReason::NotReported,
        },
        completeness: Completeness::Partial {
            reason: UnknownReason::Incomplete,
        },
        correction_of: None,
    })
}
pub(crate) fn usage_draft(key: String, usage: Usage) -> Draft {
    Draft {
        key,
        kind: ObservationKind::Usage {
            usage: Box::new(usage),
        },
    }
}

pub(crate) fn value_bytes(value: &Value) -> Result<Vec<u8>> {
    let mut sorted = value.clone();
    sorted.sort_all_objects();
    Ok(serde_json::to_vec(&sorted)?)
}
pub(crate) fn value_digest(value: &Value) -> Result<Digest> {
    Ok(Digest::of(&value_bytes(value)?))
}
