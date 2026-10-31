#[allow(dead_code)]
mod support;
use aq_lifecycle_protocol::*;
use aq_observation_engine as engine;
use aq_observation_redb_prototype::{Limits, Store};
use serde_json::json;
use uuid::Uuid;
fn source() -> engine::Source {
    let mut value = serde_json::to_value(support::source(8)).unwrap();
    value["format"] = "open_code_session_dto6c24ec9".into();
    serde_json::from_value(value).unwrap()
}
fn record(id: &str, output: u64) -> Vec<u8> {
    serde_json::to_vec(&json!({"id":id,"type":"assistant","model":{"id":"model-a","providerID":"fixture-provider"},"content":[],"tokens":{"input":12,"output":output,"reasoning":2,"cache":{"read":0,"write":1}},"cost":0.123,"finish":"stop"})).unwrap()
}
#[test]
fn scoped_public_record_cursor_cas_and_provenance_use_no_private_database() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("rows");
    let store = Store::create(&root, Limits::fixture()).unwrap();
    assert!(store.attach_engine(source(), 4096, false).is_err());
    store.attach_engine(source(), 4096, true).unwrap();
    let bytes = record("msg_a", 5);
    let first = store
        .ingest_record(
            Uuid::from_u128(8),
            None,
            Text::new("page1").unwrap(),
            bytes.clone(),
            100,
        )
        .unwrap();
    assert!(first.facts_added > 0);
    let snapshot = store.snapshot().unwrap();
    assert!(
        store
            .ingest_record(
                Uuid::from_u128(8),
                None,
                Text::new("foreign-cursor").unwrap(),
                bytes.clone(),
                101
            )
            .is_err()
    );
    assert_eq!(store.snapshot().unwrap(), snapshot);
    assert_eq!(
        store
            .ingest_record(
                Uuid::from_u128(8),
                Some("page1"),
                Text::new("page2").unwrap(),
                bytes,
                101
            )
            .unwrap()
            .facts_added,
        0
    );
    let frozen = store.prepare().unwrap().unwrap();
    let Message::Batch(batch) = &frozen.body.message else {
        panic!()
    };
    assert!(batch.events.iter().all(|e| e.body.provenance.source_kind
        == SourceKind::StructuredProviderEvent
        && matches!(&e.body.provenance.cursor,SourceCursor::Record{id}if id.as_str()=="msg_a")));
    assert!(
        batch
            .events
            .iter()
            .filter_map(|e| if let ObservationKind::Usage { usage } = &e.body.kind {
                Some(&usage.cost)
            } else {
                None
            })
            .all(|c| matches!(c, Cost::Unknown { .. }))
    );
    store
        .ingest_record(
            Uuid::from_u128(8),
            Some("page2"),
            Text::new("page3").unwrap(),
            record("msg_a", 8),
            102,
        )
        .unwrap();
    assert_eq!(store.prepare().unwrap().unwrap(), frozen);
    store.audit().unwrap();
    drop(store);
    let store = Store::open(&root, Limits::fixture()).unwrap();
    assert_eq!(
        store
            .source(Uuid::from_u128(8))
            .unwrap()
            .record_cursor
            .unwrap()
            .as_str(),
        "page3"
    );
    assert_eq!(store.prepare().unwrap().unwrap(), frozen);
    store.acknowledge(&support::receipt(&frozen)).unwrap();
    let correction = store.prepare().unwrap().unwrap();
    let Message::Batch(batch) = correction.body.message else {
        panic!()
    };
    assert!(batch.events.iter().any(
        |e| matches!(&e.body.kind,ObservationKind::Usage{usage}if usage.correction_of.is_some())
    ));
    store.audit().unwrap();
}
