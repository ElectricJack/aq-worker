#[allow(dead_code)]
#[path = "../tests/support/mod.rs"]
mod support;
use aq_observation::{lifecycle::*, source::*, spool::*, *};
use serde_json::{Value, json};
use std::path::PathBuf;
use uuid::Uuid;
fn binding() -> Binding {
    serde_json::from_value(json!({"project_id":Uuid::from_u128(1),"task_id":Uuid::from_u128(2),"lineage_id":Uuid::from_u128(3),"attempt_id":Uuid::from_u128(4),"attempt_generation":1,"session":{"kind":"remote","node_id":Uuid::from_u128(5),"boot_id":Uuid::from_u128(6),"session_id":Uuid::from_u128(7),"attempt_id":Uuid::from_u128(4),"assignment_generation":1}})).unwrap()
}
fn node() -> Node {
    Node {
        node_id: Uuid::from_u128(5),
        boot_id: Uuid::from_u128(6),
    }
}
fn normalizer(format: Format) -> Normalizer {
    Normalizer::new(
        Source {
            binding: binding(),
            source_id: Uuid::from_u128(8),
            generation: 1,
            native_session_key: Text::new("native-fixture").unwrap(),
            format,
            provider: Text::new("fixture-provider").unwrap(),
            account_reference: Text::new("logical-account").unwrap(),
            registered_session_key: None,
        },
        128,
    )
    .unwrap()
}
fn rows() -> Vec<Value> {
    let f: Value =
        serde_json::from_str(include_str!("../tests/fixtures/native-synthetic.json")).unwrap();
    f["claude"].as_array().unwrap().clone()
}
fn lines(rows: &[Value]) -> Vec<u8> {
    let mut out = vec![];
    for r in rows {
        out.extend(serde_json::to_vec(r).unwrap());
        out.push(b'\n');
    }
    out
}
fn fixture(temp: &tempfile::TempDir, bytes: &[u8], limits: Limits) -> (Spool, SourceFile, PathBuf) {
    let path = temp.path().join("native.jsonl");
    std::fs::write(&path, bytes).unwrap();
    let permit = SourceFile::inspect(
        Uuid::from_u128(8),
        temp.path(),
        "native.jsonl",
        Text::new("native-fixture").unwrap(),
    )
    .unwrap();
    let source = SourceFile::open(permit.clone()).unwrap();
    let mut spool = support::initialize(&temp.path().join("spool"), node(), limits);
    spool
        .attach_file(normalizer(Format::ClaudeV1Jsonl9a9b8e8), permit)
        .unwrap();
    (spool, source, path)
}
fn peer() -> ConformancePeer {
    ConformancePeer::new(
        binding(),
        [OBSERVATION_CAPABILITY, USAGE_CAPABILITY]
            .iter()
            .map(|id| Capability {
                id: Text::new(*id).unwrap(),
                version: 1,
            })
            .collect(),
    )
    .unwrap()
}
fn elapsed<T>(f: impl FnOnce() -> T) -> (T, f64) {
    let start = std::time::Instant::now();
    let value = f();
    (value, start.elapsed().as_secs_f64() * 1000.0)
}
fn run(near: bool) -> Value {
    let temp = tempfile::tempdir().unwrap();
    let limits = Limits {
        journal_bytes: 16 * 1024 * 1024,
        ordinary_bytes: 8 * 1024 * 1024,
        ..Limits::default()
    };
    let (mut spool, source, path) = fixture(&temp, b"", limits.clone());
    // Populate one valid fact through the real parser, then synthesize bounded retained history.
    std::fs::write(&path, lines(&rows()[..1])).unwrap();
    spool
        .collect(Uuid::from_u128(8), &source, 100, OverflowPolicy::Hold)
        .unwrap();
    let journal = spool.path().join("spool.json");
    drop(spool);
    let mut envelope: Value = serde_json::from_slice(&std::fs::read(&journal).unwrap()).unwrap();
    let state = &mut envelope["state"];
    let key = Uuid::from_u128(8).to_string();
    let template = state["sources"][&key].clone();
    let seen = template["normalizer"]["seen"]
        .as_object()
        .unwrap()
        .values()
        .next()
        .unwrap()
        .clone();
    let event = state["outbox"][0].clone();
    let sources = if near { 8 } else { 1 };
    let entries = if near { 640 } else { 4 };
    let queued = if near { 192 } else { 4 };
    for source_index in 0..sources {
        let id = Uuid::from_u128(8 + source_index as u128).to_string();
        let mut entry = template.clone();
        entry["normalizer"]["source"]["source_id"] = json!(id);
        entry["normalizer"]["maximum_tracked"] = json!(4096);
        entry["normalizer"]["next_sequence"] = json!(queued + 1);
        entry["cursor"] = json!(Cursor::default());
        if source_index > 0 {
            entry["permit"] = Value::Null;
        }
        let mut records = serde_json::Map::new();
        for index in 0..entries {
            let mut record = seen.clone();
            record["raw_record_digests"] = json!(vec![
                Digest::of(format!("{index}").as_bytes());
                if near { 32 } else { 1 }
            ]);
            records.insert(format!("synthetic-retained-{index}"), record);
        }
        entry["normalizer"]["seen"] = Value::Object(records);
        state["sources"][&id] = entry;
    }
    let mut out = vec![];
    for index in 0..queued {
        let mut e = event.clone();
        e["event_id"] = json!(Uuid::from_u128(1000 + index as u128));
        e["body"]["sequence"] = json!(index + 1);
        e["body_digest"] = json!(Digest::of(&serde_json::to_vec(&e["body"]).unwrap()));
        out.push(e);
    }
    state["outbox"] = json!(out);
    state["next_sequence"] = json!(queued + 1);
    envelope["digest"] = json!(Digest::of(&serde_json::to_vec(&envelope["state"]).unwrap()));
    std::fs::write(&journal, serde_json::to_vec(&envelope).unwrap()).unwrap();
    std::fs::write(&path, b"").unwrap();
    let initial_bytes = std::fs::metadata(&journal).unwrap().len();
    let mut spool = Spool::open(&temp.path().join("spool"), node(), limits).unwrap();
    let mut idle = vec![];
    for _ in 0..5 {
        idle.push(
            elapsed(|| {
                spool
                    .collect(Uuid::from_u128(8), &source, 101, OverflowPolicy::Hold)
                    .unwrap()
            })
            .1,
        );
    }
    std::fs::write(&path, b"{").unwrap();
    let mut partial = vec![];
    for _ in 0..5 {
        partial.push(
            elapsed(|| {
                spool
                    .collect(Uuid::from_u128(8), &source, 102, OverflowPolicy::Hold)
                    .unwrap()
            })
            .1,
        );
    }
    let mut burst = vec![];
    for i in 0..32 {
        let mut row = rows()[0].clone();
        row["uuid"] = json!(format!("burst-{i}"));
        row["message"]["id"] = json!(format!("burst-{i}"));
        row["message"]["content"] = json!([]);
        burst.push(row);
    }
    std::fs::write(&path, lines(&burst)).unwrap();
    let (collected, burst_ms) = elapsed(|| {
        spool
            .collect(Uuid::from_u128(8), &source, 103, OverflowPolicy::Hold)
            .unwrap()
    });
    let (packet, prepare_ms) = elapsed(|| spool.prepare_packet().unwrap().unwrap());
    let (_, send_ms) = elapsed(|| spool.begin_send(1000).unwrap());
    let ack = peer().receive(&packet).unwrap();
    let (_, ack_ms) = elapsed(|| spool.acknowledge(&ack).unwrap());
    json!({"fixture":"synthetic valid bounded journal snapshot; excludes fill cost; warm local filesystem", "near":near,"initial_journal_bytes":initial_bytes,"sources":sources,"seen_per_source":entries,"raw_digests_per_seen":if near {32}else{1},"queued":queued,"idle_ms":idle,"partial_ms":partial,"burst_records":32,"burst_facts":collected.facts_added,"burst_ms":burst_ms,"prepare_ms":prepare_ms,"begin_send_ms":send_ms,"ack_ms":ack_ms})
}
fn main() {
    println!(
        "{}",
        serde_json::to_string_pretty(&json!({"small":run(false),"near":run(true)})).unwrap()
    );
}
