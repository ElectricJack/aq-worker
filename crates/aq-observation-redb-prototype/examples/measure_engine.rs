//! Synthetic source-shaped input, real file reader/normalizer/storage pipeline.
//! Population executes the real32-record transactional path and is excluded
//! from latency samples. No provider or network coordinator is contacted.
#[allow(dead_code)]
#[path = "../tests/support/mod.rs"]
mod support;
use aq_lifecycle_protocol::*;
use aq_observation::{source::SourceFile, spool::OverflowPolicy};
use aq_observation_engine as engine;
use aq_observation_redb_prototype::{Limits, Store};
use serde_json::{Value, json};
use std::{collections::BTreeMap, io::Write, time::Instant};
use uuid::Uuid;
struct Peer {
    through: u64,
    receipts: BTreeMap<Uuid, Receipt>,
}
impl Peer {
    fn receive(&mut self, packet: &Packet) -> Receipt {
        packet.validate().unwrap();
        if let Some(r) = self.receipts.get(&packet.body.message_id) {
            r.validate_against(packet).unwrap();
            return r.clone();
        }
        let Message::Batch(batch) = &packet.body.message else {
            panic!()
        };
        assert_eq!(batch.first_sequence, self.through + 1);
        assert!(batch.last_sequence <= 16384);
        assert!(self.receipts.len() < 1024);
        let receipt = support::receipt(packet);
        self.through = batch.last_sequence;
        self.receipts
            .insert(packet.body.message_id, receipt.clone());
        receipt
    }
}
fn flush(store: &Store, peer: &mut Peer) {
    while let Some(packet) = store.prepare().unwrap() {
        let receipt = peer.receive(&packet);
        store.acknowledge(&receipt).unwrap();
    }
}
fn sample<T>(store: &Store, run: impl FnOnce() -> T) -> (T, Value) {
    let monitor = store.monitor();
    monitor.reset();
    let before = store.file_bytes().unwrap();
    let start = Instant::now();
    let result = run();
    let ms = start.elapsed().as_secs_f64() * 1000.;
    (
        result,
        json!({"ms":ms,"io":monitor.stats(),"file_before":before,"file_after":store.file_bytes().unwrap()}),
    )
}
fn append(path: &std::path::Path, bytes: &[u8]) {
    let mut file = std::fs::OpenOptions::new().append(true).open(path).unwrap();
    file.write_all(bytes).unwrap();
    file.sync_all().unwrap();
}
fn run(near: bool) -> Value {
    let temp = tempfile::tempdir().unwrap();
    let store = Store::create(&temp.path().join("rows"), Limits::fixture()).unwrap();
    let mut inputs = vec![];
    let mut peer = Peer {
        through: 0,
        receipts: BTreeMap::new(),
    };
    for i in 0..if near { 8 } else { 1 } {
        let source: engine::Source =
            serde_json::from_value(serde_json::to_value(support::source(8 + i)).unwrap()).unwrap();
        let root = temp.path().join(format!("native-{i}"));
        std::fs::create_dir(&root).unwrap();
        let path = root.join("native.jsonl");
        std::fs::write(&path, b"").unwrap();
        let permit = SourceFile::inspect(
            source.source_id,
            &root,
            "native.jsonl",
            source.native_session_key.clone(),
        )
        .unwrap();
        let file = SourceFile::open(permit.clone()).unwrap();
        store
            .attach_file_engine(source.clone(), 4096, permit)
            .unwrap();
        for call in 0..if near { 640 } else { 4 } {
            let mut bytes = vec![];
            for variant in 0..if near { 32 } else { 1 } {
                let mut row: Value = serde_json::from_slice(&support::row(call)).unwrap();
                row["synthetic_capture_hint"] = json!(variant);
                bytes.extend(serde_json::to_vec(&row).unwrap());
                bytes.push(b'\n');
            }
            append(&path, &bytes);
            let collected = store
                .collect_file(source.source_id, &file, 100, OverflowPolicy::Hold)
                .unwrap();
            assert_eq!(collected.facts_added, 1);
            assert!(!collected.blocked);
            if call % 32 == 31 {
                flush(&store, &mut peer);
            }
        }
        flush(&store, &mut peer);
        eprintln!(
            "populated {near} source {i}; logical{} file{}",
            store.snapshot().unwrap().logical_bytes,
            store.file_bytes().unwrap()
        );
        inputs.push((source.source_id, file, path));
    }
    store.audit().unwrap();
    let initial = store.snapshot().unwrap();
    let initial_file = store.file_bytes().unwrap();
    let (id, file, path) = &inputs[0];
    let mut samples = vec![];
    let repetitions = std::env::var("AQ_OBSERVATION_BENCH_REPETITIONS")
        .ok()
        .map(|v| v.parse::<u64>().unwrap())
        .unwrap_or(35);
    assert!((35..=108).contains(&repetitions));
    for rep in 0..repetitions {
        let mut bytes = vec![];
        for call in 0..32 {
            bytes.extend(support::row(10000 + rep * 32 + call));
        }
        append(path, &bytes);
        let (result, collect) = sample(&store, || {
            store
                .collect_file(*id, file, 200 + rep, OverflowPolicy::Hold)
                .unwrap()
        });
        assert_eq!(result.facts_added, 32);
        assert!(!result.blocked);
        assert_eq!(collect["io"]["syncs"], 2);
        let (packet, prepare) = sample(&store, || store.prepare().unwrap().unwrap());
        let (sent, send) = sample(&store, || {
            store.begin_send(1000 + rep * 1000).unwrap().unwrap()
        });
        assert_eq!(packet, sent);
        let receipt = peer.receive(&packet);
        let (_, ack) = sample(&store, || store.acknowledge(&receipt).unwrap());
        samples.push(json!({"collect32":collect,"prepare32":prepare,"send32":send,"ack32":ack,"source_read_bytes":bytes.len(),"facts":result.facts_added,"indexed_lookups":result.indexed_lookups}));
    }
    store.audit().unwrap();
    json!({"fixture":"synthetic Claude-shaped records,real file/normalizer/row-store;32distinct raw fingerprints per retained call forlargecase; no actualCLI/provider qualification","near":near,"population":"real bounded32-record collection/ACK path; excluded fromsamples","initial":initial,"initial_file_bytes":initial_file,"final_file_bytes":store.file_bytes().unwrap(),"physical_cap":Limits::fixture().physical_bytes,"cache_bytes":Limits::fixture().cache_bytes,"repetitions":samples.len(),"samples":samples})
}
fn main() {
    println!(
        "{}",
        serde_json::to_string_pretty(&json!({"small":run(false),"near":run(true)})).unwrap()
    );
}
