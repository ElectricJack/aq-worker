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
        ensure(
            self.disposition == packet.body.message.disposition(),
            "receipt disposition class",
        )
    }
}
impl Message {
    fn disposition(&self) -> ReceiptDisposition {
        match self {
            Message::Observation(_) | Message::Batch(_) => ReceiptDisposition::ObservationRecorded,
            Message::Checkpoint(_) => ReceiptDisposition::CheckpointRecorded,
            Message::Suspend(_) => ReceiptDisposition::SuspensionRecorded,
            Message::Parking(_) => ReceiptDisposition::ParkingRecorded,
            Message::Continuation(_) => ReceiptDisposition::ContinuationReceived,
            Message::BootstrapAck(_) => ReceiptDisposition::BootstrapAcknowledgmentRecorded,
        }
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
        let through = self.record(packet)?;
        let receipt = Receipt {
            schema_version: SCHEMA_VERSION,
            message_id: packet.body.message_id,
            body_digest: packet.body_digest.clone(),
            disposition: packet.body.message.disposition(),
            through_sequence: through,
        };
        self.seen.insert(receipt.message_id, receipt.clone());
        Ok(receipt)
    }

    fn record(&mut self, packet: &Packet) -> Result<Option<u64>, ContractError> {
        match &packet.body.message {
            Message::Observation(v) => {
                self.check_binding(&v.binding)?;
                let id = packet.body.message_id;
                let digest = canonical_digest(v)?;
                if self.new_event(id, &digest)? {
                    ensure(self.events.len() < 1024, "fake peer event capacity")?;
                    self.events.insert(id, digest);
                }
            }
            Message::Batch(batch) => {
                self.record_batch(batch)?;
                return Ok(Some(batch.last_sequence));
            }
            Message::Checkpoint(v) => self.check_binding(&v.binding)?,
            Message::Suspend(v) => self.check_binding(&v.binding)?,
            Message::Parking(v) => self.check_binding(&v.binding)?,
            Message::Continuation(v) => self.attach(v)?,
            Message::BootstrapAck(v) => {
                self.check_acknowledgment(v)?;
                self.acknowledged = Some((packet.body.message_id, packet.body_digest.clone()));
            }
        }
        Ok(None)
    }

    fn check_binding(&self, binding: &Binding) -> Result<(), ContractError> {
        (binding == &self.binding)
            .then_some(())
            .ok_or(ContractError::Stale)
    }

    fn new_event(&self, id: Uuid, digest: &Digest) -> Result<bool, ContractError> {
        match self.events.get(&id) {
            Some(old) if old != digest => Err(ContractError::Conflict),
            Some(_) => Ok(false),
            None => Ok(true),
        }
    }

    fn record_batch(&mut self, batch: &ObservationBatch) -> Result<(), ContractError> {
        if self.last_sequence.checked_add(1) != Some(batch.first_sequence) {
            return Err(ContractError::Stale);
        }
        let mut events = Vec::new();
        for event in &batch.events {
            self.check_binding(&event.body.binding)?;
            if self.new_event(event.event_id, &event.body_digest)? {
                events.push((event.event_id, event.body_digest.clone()));
            }
        }
        ensure(
            self.events.len() + events.len() <= 1024,
            "fake peer event capacity",
        )?;
        self.events.extend(events);
        self.last_sequence = batch.last_sequence;
        Ok(())
    }

    fn attach(&mut self, value: &ContinuationAttachment) -> Result<(), ContractError> {
        self.check_binding(&value.successor)?;
        if let Some((old, _)) = &self.continuation {
            return Err(if old.admission_id == value.admission_id && old != value {
                ContractError::Conflict
            } else {
                ContractError::Stale
            });
        }
        self.continuation = Some((value.clone(), canonical_digest(value)?));
        Ok(())
    }

    fn check_acknowledgment(&self, ack: &BootstrapAck) -> Result<(), ContractError> {
        let (expected, digest) = self.continuation.as_ref().ok_or(ContractError::Stale)?;
        self.check_binding(&ack.successor)?;
        let attachment = (
            &expected.admission_id,
            &expected.continuation,
            digest,
            &expected.outcome,
        );
        if (
            &ack.admission_id,
            &ack.continuation,
            &ack.attachment_digest,
            &ack.outcome,
        ) != attachment
        {
            return Err(ContractError::Stale);
        }
        if self.acknowledged.is_some() {
            return Err(ContractError::Conflict);
        }
        Ok(())
    }
}
