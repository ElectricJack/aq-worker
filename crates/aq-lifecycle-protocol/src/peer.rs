//! Bounded in-memory fake endpoint. It proves neither durable admission nor the
//! truth of a reported stop, artifact receipt, usage counter or dependency result.
use crate::*;
use std::collections::BTreeMap;
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ReceiptDisposition {
    ObservationRecorded,
    SuspensionRecorded,
    CheckpointRecorded,
    ParkingRecorded,
    ContinuationReceived,
    BootstrapAcknowledgmentRecorded,
}
record!(Receipt {schema_version:u32,message_id:Uuid,body_digest:Digest,disposition:ReceiptDisposition,through_sequence:Option<u64>});
impl Validate for Receipt {
    fn validate(&self) -> Result<(), ContractError> {
        ensure(self.schema_version == SCHEMA_VERSION, "receipt version")?;
        uuid(self.message_id)?;
        ensure(
            self.through_sequence.is_none_or(|v| v > 0),
            "receipt sequence",
        )?;
        ensure(
            self.disposition == ReceiptDisposition::ObservationRecorded
                || self.through_sequence.is_none(),
            "receipt cursor class",
        )
    }
}
impl Receipt {
    pub fn validate_against(&self, packet: &Packet) -> Result<(), ContractError> {
        self.validate()?;
        packet.validate()?;
        if self.message_id != packet.body.message_id || self.body_digest != packet.body_digest {
            return Err(ContractError::Conflict);
        }
        let expected = match &packet.body.message {
            Message::Batch(v) => Some(v.last_sequence),
            _ => None,
        };
        ensure(
            self.through_sequence == expected,
            "receipt cannot skip undelivered cursor",
        )?;
        let disposition = match &packet.body.message {
            Message::Observation(_) | Message::Batch(_) => ReceiptDisposition::ObservationRecorded,
            Message::Checkpoint(_) => ReceiptDisposition::CheckpointRecorded,
            Message::Suspend(_) => ReceiptDisposition::SuspensionRecorded,
            Message::Parking(_) => ReceiptDisposition::ParkingRecorded,
            Message::Continuation(_) => ReceiptDisposition::ContinuationReceived,
            Message::BootstrapAck(_) => ReceiptDisposition::BootstrapAcknowledgmentRecorded,
        };
        ensure(self.disposition == disposition, "receipt disposition class")
    }
}
pub struct ConformancePeer {
    advertised: Vec<Capability>,
    binding: Binding,
    seen: BTreeMap<Uuid, Receipt>,
    events: BTreeMap<Uuid, Digest>,
    last_sequence: u64,
    continuation: Option<(ContinuationAttachment, Digest)>,
    acknowledged: Option<(Uuid, Digest)>,
}
impl ConformancePeer {
    pub fn new(binding: Binding, advertised: Vec<Capability>) -> Result<Self, ContractError> {
        binding.validate()?;
        negotiate(&[], &advertised)?;
        Ok(Self {
            advertised,
            binding,
            seen: BTreeMap::new(),
            events: BTreeMap::new(),
            last_sequence: 0,
            continuation: None,
            acknowledged: None,
        })
    }
    pub fn last_sequence(&self) -> u64 {
        self.last_sequence
    }
    pub fn bootstrap_acknowledged(&self) -> bool {
        self.acknowledged.is_some()
    }
    pub fn receive(&mut self, packet: &Packet) -> Result<Receipt, ContractError> {
        packet.validate()?;
        negotiate(&packet.body.required_capabilities, &self.advertised)?;
        if let Some(old) = self.seen.get(&packet.body.message_id) {
            return if old.body_digest == packet.body_digest {
                Ok(old.clone())
            } else {
                Err(ContractError::Conflict)
            };
        }
        ensure(self.seen.len() < 1024, "fake peer receipt capacity")?;
        let mut new_events = Vec::new();
        let mut through = None;
        let mut attachment = None;
        let mut ack = None;
        let disposition =
            match &packet.body.message {
                Message::Observation(v) => {
                    if v.binding != self.binding {
                        return Err(ContractError::Stale);
                    }
                    let digest = canonical_digest(v)?;
                    if let Some(old) = self.events.get(&packet.body.message_id) {
                        if old != &digest {
                            return Err(ContractError::Conflict);
                        }
                    } else {
                        ensure(self.events.len() < 1024, "fake peer event capacity")?;
                        new_events.push((packet.body.message_id, digest));
                    }
                    ReceiptDisposition::ObservationRecorded
                }
                Message::Batch(batch) => {
                    if batch.first_sequence
                        != self
                            .last_sequence
                            .checked_add(1)
                            .ok_or(ContractError::Stale)?
                    {
                        return Err(ContractError::Stale);
                    }
                    for event in &batch.events {
                        if event.body.binding != self.binding {
                            return Err(ContractError::Stale);
                        }
                        if let Some(old) = self.events.get(&event.event_id) {
                            if old != &event.body_digest {
                                return Err(ContractError::Conflict);
                            }
                        } else {
                            new_events.push((event.event_id, event.body_digest.clone()));
                        }
                    }
                    ensure(
                        self.events.len() + new_events.len() <= 1024,
                        "fake peer event capacity",
                    )?;
                    through = Some(batch.last_sequence);
                    ReceiptDisposition::ObservationRecorded
                }
                Message::Checkpoint(v) => {
                    if v.binding != self.binding {
                        return Err(ContractError::Stale);
                    }
                    ReceiptDisposition::CheckpointRecorded
                }
                Message::Suspend(v) => {
                    if v.binding != self.binding {
                        return Err(ContractError::Stale);
                    }
                    ReceiptDisposition::SuspensionRecorded
                }
                Message::Parking(v) => {
                    if v.binding != self.binding {
                        return Err(ContractError::Stale);
                    }
                    ReceiptDisposition::ParkingRecorded
                }
                Message::Continuation(v) => {
                    if v.successor != self.binding {
                        return Err(ContractError::Stale);
                    }
                    if self.continuation.as_ref().is_some_and(|(old, _)| {
                        old.admission_id == v.admission_id && old != v.as_ref()
                    }) {
                        return Err(ContractError::Conflict);
                    }
                    if self.continuation.is_some() {
                        return Err(ContractError::Stale);
                    }
                    attachment = Some((v.as_ref().clone(), canonical_digest(v.as_ref())?));
                    ReceiptDisposition::ContinuationReceived
                }
                Message::BootstrapAck(v) => {
                    let Some((expected, digest)) = &self.continuation else {
                        return Err(ContractError::Stale);
                    };
                    if v.successor != self.binding
                        || v.admission_id != expected.admission_id
                        || v.continuation != expected.continuation
                        || v.attachment_digest != *digest
                        || v.outcome != expected.outcome
                    {
                        return Err(ContractError::Stale);
                    }
                    if self.acknowledged.is_some() {
                        return Err(ContractError::Conflict);
                    }
                    ack = Some((packet.body.message_id, packet.body_digest.clone()));
                    ReceiptDisposition::BootstrapAcknowledgmentRecorded
                }
            };
        let receipt = Receipt {
            schema_version: SCHEMA_VERSION,
            message_id: packet.body.message_id,
            body_digest: packet.body_digest.clone(),
            disposition,
            through_sequence: through,
        };
        self.events.extend(new_events);
        if let Some(sequence) = through {
            self.last_sequence = sequence;
        }
        if let Some(value) = attachment {
            self.continuation = Some(value);
        }
        if let Some(value) = ack {
            self.acknowledged = Some(value);
        }
        self.seen.insert(receipt.message_id, receipt.clone());
        Ok(receipt)
    }
}
