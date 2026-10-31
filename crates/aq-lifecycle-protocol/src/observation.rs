use crate::*;
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum SourceCursor {
    Bytes { start: u64, end: u64 },
    Record { id: Text },
}
impl Validate for SourceCursor {
    fn validate(&self) -> Result<(), ContractError> {
        match self {
            Self::Bytes { start, end } => ensure(end > start, "source byte interval"),
            Self::Record { id } => id.validate(),
        }
    }
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum SourceKind {
    NativeTranscript,
    StructuredProviderEvent,
    RetainedReplay,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum RawRecordEvidence {
    Available { digest: Digest, byte_length: u64 },
    Unavailable { reason: GapReason },
}
record!(Provenance {
    source_id: Uuid,
    source_generation: u64,
    source_kind: SourceKind,
    native_session_key: Text,
    cursor: SourceCursor,
    parser: ArtifactRef,
    raw_record: RawRecordEvidence
});
impl Validate for Provenance {
    fn validate(&self) -> Result<(), ContractError> {
        uuid(self.source_id)?;
        ensure(self.source_generation > 0, "source generation")?;
        self.native_session_key.validate()?;
        self.cursor.validate()?;
        if let RawRecordEvidence::Available { byte_length, .. } = &self.raw_record {
            ensure(*byte_length > 0, "raw record byte length")?;
            if let SourceCursor::Bytes { start, end } = &self.cursor {
                ensure(
                    end.checked_sub(*start) == Some(*byte_length),
                    "raw record span/length mismatch",
                )?;
            }
        }
        artifact(&self.parser)
    }
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum UnknownReason {
    NotReported,
    Unsupported,
    SourceGap,
    NotApplicable,
    Incomplete,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum Counter {
    Known { value: u64 },
    Unknown { reason: UnknownReason },
}
record!(Counters {
    input: Counter,
    output: Counter,
    cache_read: Counter,
    cache_write: Counter,
    reasoning: Counter
});
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum CategorySemantics {
    DeclaredDisjoint { policy: ArtifactRef },
    MayOverlap { policy: ArtifactRef },
}
record!(ProviderCall {
    provider: Text,
    account_reference: Text,
    model: Text,
    native_call_id: Text
});
impl Validate for ProviderCall {
    fn validate(&self) -> Result<(), ContractError> {
        self.provider.validate()?;
        self.account_reference.validate()?;
        self.model.validate()?;
        self.native_call_id.validate()
    }
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum CounterBasis {
    PerCall,
    Cumulative {
        counter_id: Text,
        counter_generation: u64,
    },
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum EstimateBasis {
    TokenRates,
    SubscriptionAllocation,
    LocalModelAllocation,
}
/// An estimate is explicitly not an invoice. Reported charges still require
/// provenance/owner verification; no wire claim authorizes double-billing.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum Cost {
    Unknown {
        reason: UnknownReason,
    },
    ProviderReported {
        amount_microunits: u64,
        currency: Text,
        receipt: ArtifactRef,
    },
    NonInvoiceEstimate {
        amount_microunits: u64,
        currency: Text,
        rate_card: ArtifactRef,
        basis: EstimateBasis,
        allocation_policy: ArtifactRef,
    },
}
impl Validate for Cost {
    fn validate(&self) -> Result<(), ContractError> {
        match self {
            Self::Unknown { .. } => Ok(()),
            Self::ProviderReported {
                currency, receipt, ..
            } => {
                ensure(
                    currency.as_str().len() == 3
                        && currency.as_str().bytes().all(|b| b.is_ascii_uppercase()),
                    "currency",
                )?;
                artifact(receipt)
            }
            Self::NonInvoiceEstimate {
                currency,
                rate_card,
                allocation_policy,
                ..
            } => {
                ensure(
                    currency.as_str().len() == 3
                        && currency.as_str().bytes().all(|b| b.is_ascii_uppercase()),
                    "currency",
                )?;
                artifact(rate_card)?;
                artifact(allocation_policy)
            }
        }
    }
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum Completeness {
    ReportedComplete,
    Partial { reason: UnknownReason },
    Gap { reason: UnknownReason },
}
record!(Usage {call:ProviderCall,basis:CounterBasis,counters:Counters,category_semantics:CategorySemantics,cost:Cost,completeness:Completeness,correction_of:Option<EventRef>});
impl Validate for Usage {
    fn validate(&self) -> Result<(), ContractError> {
        self.call.validate()?;
        if let CounterBasis::Cumulative {
            counter_id,
            counter_generation,
        } = &self.basis
        {
            counter_id.validate()?;
            ensure(*counter_generation > 0, "cumulative counter generation")?;
        }
        match &self.category_semantics {
            CategorySemantics::DeclaredDisjoint { policy }
            | CategorySemantics::MayOverlap { policy } => artifact(policy)?,
        }
        self.cost.validate()?;
        if let Some(old) = &self.correction_of {
            old.validate()?;
        }
        Ok(())
    }
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum GapReason {
    SourceMissing,
    Rotated,
    Truncated,
    Malformed,
    UnsupportedFormat,
    SpoolOverflow,
    Redacted,
    ReplayUnavailable,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum TextRole {
    Assistant,
    User,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum ObservationKind {
    TurnBoundary {
        in_turn: bool,
    },
    TextReference {
        role: TextRole,
        content: ArtifactRef,
        utf8_bytes: u64,
    },
    ToolStarted {
        call_id: Text,
        tool_name: Text,
    },
    ToolResult {
        call_id: Text,
        content: ArtifactRef,
    },
    Question {
        question_id: Text,
        content: ArtifactRef,
    },
    ProviderError {
        code: Text,
        retryable: bool,
    },
    Usage {
        usage: Box<Usage>,
    },
    Gap {
        reason: GapReason,
        affected: Option<SourceCursor>,
    },
}
impl ObservationKind {
    pub fn is_usage(&self) -> bool {
        matches!(self, Self::Usage { .. })
    }
}
impl Validate for ObservationKind {
    fn validate(&self) -> Result<(), ContractError> {
        match self {
            Self::TurnBoundary { .. } => Ok(()),
            Self::TextReference { content, .. } | Self::Question { content, .. } => {
                artifact(content)
            }
            Self::ToolStarted { call_id, tool_name } => {
                call_id.validate()?;
                tool_name.validate()
            }
            Self::ToolResult { call_id, content } => {
                call_id.validate()?;
                artifact(content)
            }
            Self::ProviderError { code, .. } => code.validate(),
            Self::Usage { usage } => usage.validate(),
            Self::Gap { affected, .. } => {
                if let Some(cursor) = affected {
                    cursor.validate()?;
                }
                Ok(())
            }
        }
    }
}
record!(Observation {
    binding: Binding,
    provenance: Provenance,
    sequence: u64,
    observed_at_ms: u64,
    kind: ObservationKind
});
impl Validate for Observation {
    fn validate(&self) -> Result<(), ContractError> {
        self.binding.validate()?;
        self.provenance.validate()?;
        ensure(self.sequence > 0, "observation sequence")?;
        self.kind.validate()?;
        if let RawRecordEvidence::Unavailable { reason } = &self.provenance.raw_record {
            ensure(
                matches!(&self.kind,ObservationKind::Gap{reason:gap,..} if gap==reason),
                "missing source evidence is an explicit gap",
            )?;
        }
        Ok(())
    }
}
record!(ObservationRecord {
    event_id: Uuid,
    body: Observation,
    body_digest: Digest
});
impl ObservationRecord {
    pub fn seal(event_id: Uuid, body: Observation) -> Result<Self, ContractError> {
        let value = Self {
            event_id,
            body_digest: canonical_digest(&body)?,
            body,
        };
        value.validate()?;
        Ok(value)
    }
}
impl Validate for ObservationRecord {
    fn validate(&self) -> Result<(), ContractError> {
        uuid(self.event_id)?;
        self.body.validate()?;
        ensure(
            canonical_digest(&self.body)? == self.body_digest,
            "observation digest",
        )?;
        if let ObservationKind::Usage { usage } = &self.body.kind {
            ensure(
                usage
                    .correction_of
                    .as_ref()
                    .is_none_or(|old| old.event_id != self.event_id),
                "self correction",
            )?;
        }
        ensure(
            serde_json::to_vec(self)
                .map_err(|e| ContractError::Json(e.to_string()))?
                .len()
                <= MAX_EVENT_BYTES,
            "event bytes",
        )
    }
}
record!(ObservationBatch {node_id:Uuid,boot_id:Uuid,first_sequence:u64,last_sequence:u64,events:Vec<ObservationRecord>});
impl Validate for ObservationBatch {
    fn validate(&self) -> Result<(), ContractError> {
        uuid(self.node_id)?;
        uuid(self.boot_id)?;
        ensure(
            !self.events.is_empty() && self.events.len() <= MAX_EVENTS,
            "event batch count",
        )?;
        let mut ids = std::collections::BTreeSet::new();
        for (i, event) in self.events.iter().enumerate() {
            event.validate()?;
            ensure(ids.insert(event.event_id), "duplicate source event")?;
            let SessionIdentity::Remote {
                node_id, boot_id, ..
            } = &event.body.binding.session
            else {
                return Err(ContractError::Stale);
            };
            if *node_id != self.node_id || *boot_id != self.boot_id {
                return Err(ContractError::Stale);
            }
            ensure(
                self.first_sequence.checked_add(i as u64) == Some(event.body.sequence),
                "batch sequence interval",
            )?;
        }
        ensure(
            self.events
                .first()
                .is_some_and(|e| e.body.sequence == self.first_sequence)
                && self
                    .events
                    .last()
                    .is_some_and(|e| e.body.sequence == self.last_sequence),
            "batch endpoints",
        )
    }
}
