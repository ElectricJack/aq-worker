use aq_lifecycle_protocol::*;
use aq_observation::{Format, Normalizer, Source, framing::Frame};
use aq_observation_redb_prototype::{SeenRow, Store};
use serde_json::json;
use uuid::Uuid;
pub fn binding() -> Binding {
    serde_json::from_value(json!({"project_id":Uuid::from_u128(1),"task_id":Uuid::from_u128(2),"lineage_id":Uuid::from_u128(3),"attempt_id":Uuid::from_u128(4),"attempt_generation":1,"session":{"kind":"remote","node_id":Uuid::from_u128(5),"boot_id":Uuid::from_u128(6),"session_id":Uuid::from_u128(7),"attempt_id":Uuid::from_u128(4),"assignment_generation":1}})).unwrap()
}
pub fn source(id: u128) -> Source {
    Source {
        binding: binding(),
        source_id: Uuid::from_u128(id),
        generation: 1,
        native_session_key: Text::new("native-fixture").unwrap(),
        format: Format::ClaudeV1Jsonl9a9b8e8,
        provider: Text::new("fixture-provider").unwrap(),
        account_reference: Text::new("logical-account").unwrap(),
        registered_session_key: None,
    }
}
pub fn row(call: u64) -> Vec<u8> {
    let mut b=serde_json::to_vec(&json!({"type":"assistant","uuid":format!("row-{call}"),"sessionId":"native-fixture","message":{"id":format!("call-{call}"),"model":"model-a","content":[],"usage":{"input_tokens":12,"output_tokens":5,"cache_read_input_tokens":0,"cache_creation_input_tokens":2}}})).unwrap();
    b.push(b'\n');
    b
}
pub fn normalize(sequence: u64, call: u64) -> (Vec<ObservationRecord>, Vec<(String, SeenRow)>) {
    normalize_at(sequence, call, 0)
}
pub fn normalize_at(
    sequence: u64,
    call: u64,
    start: u64,
) -> (Vec<ObservationRecord>, Vec<(String, SeenRow)>) {
    let mut n = Normalizer::new(source(8), 4096).unwrap();
    n.next_sequence = sequence;
    let (next, facts) = n
        .apply(
            &Frame {
                start,
                bytes: row(call),
                oversized: false,
            },
            100,
        )
        .unwrap();
    let seen = next
        .seen
        .into_iter()
        .map(|(k, v)| {
            (
                k,
                SeenRow {
                    normalized_digest: v.normalized_digest,
                    event: v.event,
                    raw_record_digests: v.raw_record_digests,
                },
            )
        })
        .collect();
    (facts, seen)
}
pub fn append(store: &Store, call: u64) {
    let s = store.source(Uuid::from_u128(8)).unwrap();
    let (facts, seen) = normalize_at(store.snapshot().unwrap().next, call, s.cursor);
    store
        .append(
            s.source.source_id,
            s.cursor,
            s.cursor + row(call).len() as u64,
            &facts,
            &seen,
        )
        .unwrap();
}
pub fn receipt(packet: &Packet) -> Receipt {
    packet.validate().unwrap();
    let Message::Batch(b) = &packet.body.message else {
        panic!()
    };
    Receipt {
        schema_version: SCHEMA_VERSION,
        message_id: packet.body.message_id,
        body_digest: packet.body_digest.clone(),
        disposition: ReceiptDisposition::ObservationRecorded,
        through_sequence: Some(b.last_sequence),
    }
}
pub fn peer() -> ConformancePeer {
    ConformancePeer::new(
        binding(),
        [OBSERVATION_CAPABILITY, USAGE_CAPABILITY]
            .into_iter()
            .map(|s| Capability {
                id: Text::new(s).unwrap(),
                version: 1,
            })
            .collect(),
    )
    .unwrap()
}
