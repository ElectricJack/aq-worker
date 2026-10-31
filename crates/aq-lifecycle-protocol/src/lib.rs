//! Wire claims and pure conformance only. No runtime authentication, stop proof,
//! ledger, source verifier, process control or durable outbox is implemented here.
pub use aq_session_protocol::{
    ArtifactRef, Capability, ContractError, Digest, SessionIdentity, Text, Validate,
    canonical_bytes, canonical_digest,
};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;
use uuid::Uuid;
mod continuation;
mod observation;
mod peer;
pub use continuation::*;
pub use observation::*;
pub use peer::*;
pub const SCHEMA_VERSION: u32 = 1;
pub const MAX_EVENT_BYTES: usize = 65536;
pub const MAX_BATCH_BYTES: usize = 262144;
pub const MAX_EVENTS: usize = 32;
pub const MAX_REFS: usize = 32;
pub const OBSERVATION_CAPABILITY: &str = "aq.lifecycle.observation";
pub const USAGE_CAPABILITY: &str = "aq.lifecycle.usage";
pub const SUSPENSION_CAPABILITY: &str = "aq.lifecycle.suspension";
pub const CONTINUATION_CAPABILITY: &str = "aq.lifecycle.continuation";
macro_rules! record {($name:ident {$($field:ident:$ty:ty),* $(,)?})=>{#[derive(Clone,Debug,PartialEq,Eq,Serialize,Deserialize,JsonSchema)] #[serde(deny_unknown_fields)] pub struct $name {$(pub $field:$ty),*}}}
pub(crate) use record;
pub fn ensure(ok: bool, why: &'static str) -> Result<(), ContractError> {
    if ok {
        Ok(())
    } else {
        Err(ContractError::Invalid(why))
    }
}
pub fn artifact(reference: &ArtifactRef) -> Result<(), ContractError> {
    reference.id.validate()?;
    reference.version.validate()
}
pub fn uuid(id: Uuid) -> Result<(), ContractError> {
    ensure(!id.is_nil(), "nil identity")
}
pub fn unique_ids(ids: &[Uuid], maximum: usize) -> Result<(), ContractError> {
    ensure(ids.len() <= maximum, "identity collection bound")?;
    let mut seen = BTreeSet::new();
    for id in ids {
        uuid(*id)?;
        ensure(seen.insert(*id), "duplicate identity")?;
    }
    Ok(())
}
record!(Binding {
    project_id: Uuid,
    task_id: Uuid,
    lineage_id: Uuid,
    attempt_id: Uuid,
    attempt_generation: u64,
    session: SessionIdentity
});
impl Validate for Binding {
    fn validate(&self) -> Result<(), ContractError> {
        for id in [
            self.project_id,
            self.task_id,
            self.lineage_id,
            self.attempt_id,
        ] {
            uuid(id)?;
        }
        self.session.validate()?;
        ensure(self.attempt_generation > 0, "attempt generation")?;
        if let SessionIdentity::Remote {
            attempt_id,
            assignment_generation,
            ..
        } = &self.session
        {
            ensure(
                *attempt_id == self.attempt_id && *assignment_generation == self.attempt_generation,
                "remote attempt binding",
            )?;
        }
        Ok(())
    }
}
impl Binding {
    pub fn session_id(&self) -> Uuid {
        match self.session {
            SessionIdentity::Remote { session_id, .. }
            | SessionIdentity::Central { session_id, .. } => session_id,
        }
    }
}
record!(WaitRef {
    wait_id: Uuid,
    generation: u64,
    predicate_digest: Digest
});
impl Validate for WaitRef {
    fn validate(&self) -> Result<(), ContractError> {
        uuid(self.wait_id)?;
        ensure(self.generation > 0, "wait generation")
    }
}
record!(OutcomeRef {
    outcome_id: Uuid,
    wait: WaitRef,
    revision: u64,
    digest: Digest
});
impl Validate for OutcomeRef {
    fn validate(&self) -> Result<(), ContractError> {
        uuid(self.outcome_id)?;
        self.wait.validate()?;
        ensure(self.revision > 0, "outcome revision")
    }
}
record!(EventRef {
    event_id: Uuid,
    digest: Digest
});
impl Validate for EventRef {
    fn validate(&self) -> Result<(), ContractError> {
        uuid(self.event_id)
    }
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(
    tag = "kind",
    content = "body",
    rename_all = "snake_case",
    deny_unknown_fields
)]
pub enum Message {
    Observation(Observation),
    Batch(ObservationBatch),
    Checkpoint(Checkpoint),
    Suspend(SuspensionIntent),
    Parking(ParkingReport),
    Continuation(Box<ContinuationAttachment>),
    BootstrapAck(BootstrapAck),
}
impl Message {
    pub fn capabilities(&self) -> Vec<Capability> {
        let mut names = match self {
            Self::Observation(_) | Self::Batch(_) => vec![OBSERVATION_CAPABILITY],
            Self::Checkpoint(_) | Self::Suspend(_) | Self::Parking(_) => {
                vec![SUSPENSION_CAPABILITY]
            }
            Self::Continuation(_) | Self::BootstrapAck(_) => vec![CONTINUATION_CAPABILITY],
        };
        let usage = match self {
            Self::Observation(v) => v.kind.is_usage(),
            Self::Batch(v) => v.events.iter().any(|e| e.body.kind.is_usage()),
            _ => false,
        };
        if usage {
            names.push(USAGE_CAPABILITY);
        }
        names
            .into_iter()
            .map(|id| Capability {
                id: Text::new(id).expect("constant capability"),
                version: 1,
            })
            .collect()
    }
    fn max_bytes(&self) -> usize {
        if matches!(self, Self::Batch(_)) {
            MAX_BATCH_BYTES
        } else {
            MAX_EVENT_BYTES
        }
    }
}
impl Validate for Message {
    fn validate(&self) -> Result<(), ContractError> {
        match self {
            Self::Observation(v) => v.validate(),
            Self::Batch(v) => v.validate(),
            Self::Checkpoint(v) => v.validate(),
            Self::Suspend(v) => v.validate(),
            Self::Parking(v) => v.validate(),
            Self::Continuation(v) => v.validate(),
            Self::BootstrapAck(v) => v.validate(),
        }
    }
}
record!(PacketBody {schema_version:u32,message_id:Uuid,required_capabilities:Vec<Capability>,message:Message});
impl Validate for PacketBody {
    fn validate(&self) -> Result<(), ContractError> {
        ensure(self.schema_version == SCHEMA_VERSION, "schema version")?;
        uuid(self.message_id)?;
        self.message.validate()?;
        if let Message::Observation(Observation {
            kind: ObservationKind::Usage { usage },
            ..
        }) = &self.message
        {
            ensure(
                usage
                    .correction_of
                    .as_ref()
                    .is_none_or(|old| old.event_id != self.message_id),
                "self correction",
            )?;
        }
        let required = self.message.capabilities();
        ensure(self.required_capabilities.len() <= 8, "capability bound")?;
        let mut seen = BTreeSet::new();
        for c in &self.required_capabilities {
            c.validate()?;
            ensure(seen.insert(c.id.clone()), "duplicate capability")?;
        }
        if self.required_capabilities.len() != required.len()
            || !required
                .iter()
                .all(|c| self.required_capabilities.contains(c))
        {
            return Err(ContractError::Unsupported);
        }
        Ok(())
    }
}
record!(Packet {
    body: PacketBody,
    body_digest: Digest
});
impl Packet {
    pub fn seal(message_id: Uuid, message: Message) -> Result<Self, ContractError> {
        let body = PacketBody {
            schema_version: SCHEMA_VERSION,
            message_id,
            required_capabilities: message.capabilities(),
            message,
        };
        let body_digest = canonical_digest(&body)?;
        let value = Self { body, body_digest };
        value.validate()?;
        Ok(value)
    }
}
impl Validate for Packet {
    fn validate(&self) -> Result<(), ContractError> {
        self.body.validate()?;
        ensure(
            canonical_digest(&self.body)? == self.body_digest,
            "packet digest",
        )?;
        ensure(
            serde_json::to_vec(self)
                .map_err(|e| ContractError::Json(e.to_string()))?
                .len()
                <= self.body.message.max_bytes(),
            "packet byte bound",
        )
    }
}
pub fn decode(bytes: &[u8]) -> Result<Packet, ContractError> {
    ensure(bytes.len() <= MAX_BATCH_BYTES, "input byte bound")?;
    let value: Packet =
        serde_json::from_slice(bytes).map_err(|e| ContractError::Json(e.to_string()))?;
    value.validate()?;
    Ok(value)
}
pub fn negotiate(required: &[Capability], advertised: &[Capability]) -> Result<(), ContractError> {
    ensure(
        required.len() <= 8 && advertised.len() <= 16,
        "capability bound",
    )?;
    for list in [required, advertised] {
        let mut ids = BTreeSet::new();
        for c in list {
            c.validate()?;
            ensure(ids.insert(c.id.clone()), "duplicate capability")?;
        }
    }
    for c in required {
        if c.version != 1
            || !matches!(
                c.id.as_str(),
                OBSERVATION_CAPABILITY
                    | USAGE_CAPABILITY
                    | SUSPENSION_CAPABILITY
                    | CONTINUATION_CAPABILITY
            )
            || !advertised.contains(c)
        {
            return Err(ContractError::Unsupported);
        }
    }
    Ok(())
}
fn schema_for<T: JsonSchema>() -> serde_json::Value {
    fn bounds(value: &mut serde_json::Value) {
        if let Some(object) = value.as_object_mut() {
            match object.get("format").and_then(|v| v.as_str()) {
                Some("uint64") => {
                    object.insert("maximum".into(), serde_json::json!(u64::MAX));
                }
                Some("uint32") => {
                    object.insert("maximum".into(), serde_json::json!(u32::MAX));
                }
                _ => {}
            }
            if let Some(properties) = object.get_mut("properties").and_then(|p| p.as_object_mut()) {
                for (field, shape) in properties {
                    if matches!(
                        field.as_str(),
                        "required_capabilities"
                            | "events"
                            | "context_references"
                            | "pending_operation_ids"
                            | "operation_ids"
                            | "central_receipts"
                            | "retained"
                            | "held_task_message_ids"
                    ) {
                        shape["maxItems"] =
                            serde_json::json!(if field == "required_capabilities" {
                                8
                            } else if field == "events" {
                                MAX_EVENTS
                            } else {
                                MAX_REFS
                            });
                        if field == "required_capabilities" {
                            shape["minItems"] = serde_json::json!(1);
                        }
                        if field == "events" {
                            shape["minItems"] = serde_json::json!(1);
                        }
                    }
                    if matches!(
                        field.as_str(),
                        "attempt_generation"
                            | "generation"
                            | "source_generation"
                            | "counter_generation"
                            | "revision"
                            | "sequence"
                            | "first_sequence"
                            | "last_sequence"
                    ) {
                        shape["minimum"] = serde_json::json!(1);
                    }
                    if field == "schema_version" {
                        shape["const"] = serde_json::json!(SCHEMA_VERSION);
                    }
                    if field == "writer_count" {
                        shape["maximum"] = serde_json::json!(65536);
                    }
                    if field == "currency" {
                        shape["pattern"] = serde_json::json!("^[A-Z]{3}$");
                    }
                    if field == "max_summary_bytes" {
                        shape["minimum"] = serde_json::json!(1);
                        shape["maximum"] = serde_json::json!(16384);
                    }
                }
            }
            for child in object.values_mut() {
                bounds(child);
            }
        } else if let Some(array) = value.as_array_mut() {
            for child in array {
                bounds(child);
            }
        }
    }
    let mut schema = serde_json::to_value(schemars::schema_for!(T)).expect("schema serialization");
    bounds(&mut schema);
    if let Some(defs) = schema["$defs"].as_object_mut() {
        if let Some(text) = defs.get_mut("Text") {
            text["x-utf8-maxBytes"] = serde_json::json!(512);
        }
        if let Some(text) = defs.get_mut("SummaryText") {
            text["x-utf8-maxBytes"] = serde_json::json!(16384);
        }
        if let Some(cap) = defs.get_mut("Capability") {
            cap["properties"]["version"]["const"] = serde_json::json!(1);
            cap["properties"]["id"]["enum"] = serde_json::json!([
                OBSERVATION_CAPABILITY,
                USAGE_CAPABILITY,
                SUSPENSION_CAPABILITY,
                CONTINUATION_CAPABILITY
            ]);
        }
    }
    schema["x-max-wire-bytes"] = serde_json::json!(MAX_BATCH_BYTES);
    schema
}

pub fn contract_schema() -> serde_json::Value {
    schema_for::<Packet>()
}
pub fn receipt_schema() -> serde_json::Value {
    let mut schema = schema_for::<Receipt>();
    schema["x-max-wire-bytes"] = serde_json::json!(4096);
    schema
}
