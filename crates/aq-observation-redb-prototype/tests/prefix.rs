//! Synthetic wire-boundary values, never provider pricing evidence.
#[allow(dead_code)]
mod support;
use aq_lifecycle_protocol::*;
use aq_observation_redb_prototype::{Limits, Store};
use sha2::{Digest as _, Sha256};
use uuid::Uuid;
fn id(batch: &ObservationBatch) -> Uuid {
    let hash = Sha256::digest(canonical_bytes(batch).unwrap());
    let mut b = [0; 16];
    b.copy_from_slice(&hash[..16]);
    b[6] = (b[6] & 15) | 128;
    b[8] = (b[8] & 63) | 128;
    Uuid::from_bytes(b)
}
#[test]
fn exact_escaped_envelope_sizes_choose_same_maximal_prefix_as_reference_algorithm() {
    let temp = tempfile::tempdir().unwrap();
    let store = Store::create(&temp.path().join("rows"), Limits::fixture()).unwrap();
    let long = Text::new("\"".repeat(512)).unwrap();
    let mut source = support::source(8);
    source.native_session_key = long.clone();
    source.provider = long.clone();
    source.account_reference = long.clone();
    store.attach(source.clone()).unwrap();
    let artifact = ArtifactRef {
        id: long.clone(),
        version: long.clone(),
        digest: Digest::of(b"synthetic-untrusted-reference"),
    };
    let (mut template, _) = support::normalize(1, 1);
    let mut events = vec![];
    for i in 0..32u64 {
        let mut event = template.remove(0);
        event.event_id = Uuid::from_u128(1000 + i as u128);
        event.body.sequence = i + 1;
        event.body.provenance.native_session_key = long.clone();
        let ObservationKind::Usage { usage } = &mut event.body.kind else {
            panic!()
        };
        usage.call.provider = long.clone();
        usage.call.account_reference = long.clone();
        usage.call.model = long.clone();
        usage.call.native_call_id = long.clone();
        usage.basis = CounterBasis::Cumulative {
            counter_id: long.clone(),
            counter_generation: 1,
        };
        usage.category_semantics = CategorySemantics::MayOverlap {
            policy: artifact.clone(),
        };
        usage.cost = Cost::NonInvoiceEstimate {
            amount_microunits: 1,
            currency: Text::new("USD").unwrap(),
            rate_card: artifact.clone(),
            allocation_policy: artifact.clone(),
            basis: EstimateBasis::TokenRates,
        };
        event.body_digest = canonical_digest(&event.body).unwrap();
        event.validate().unwrap();
        template.push(event.clone());
        events.push(event);
    }
    store.append(source.source_id, 0, 1, &events, &[]).unwrap();
    let mut selected = vec![];
    let mut expected = None;
    for event in &events {
        let mut candidate = selected.clone();
        candidate.push(event.clone());
        let batch = ObservationBatch {
            node_id: Uuid::from_u128(5),
            boot_id: Uuid::from_u128(6),
            first_sequence: 1,
            last_sequence: event.body.sequence,
            events: candidate,
        };
        match Packet::seal(id(&batch), Message::Batch(batch)) {
            Ok(packet) => {
                let Message::Batch(batch) = &packet.body.message else {
                    panic!()
                };
                selected = batch.events.clone();
                expected = Some(packet)
            }
            Err(_) => break,
        }
    }
    assert!(
        selected.len() < 32 && selected.len() > 1,
        "exercise actual packet byte boundary"
    );
    let packet = store.prepare().unwrap().unwrap();
    assert_eq!(Some(packet.clone()), expected);
    assert!(serde_json::to_vec(&packet).unwrap().len() <= MAX_BATCH_BYTES);
    store.acknowledge(&support::receipt(&packet)).unwrap();
    assert!(store.prepare().unwrap().is_some());
    store.audit().unwrap();
}
