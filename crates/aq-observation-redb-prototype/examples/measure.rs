#[allow(dead_code)]
#[path = "../tests/support/mod.rs"]
mod support;
use aq_lifecycle_protocol::*;
use aq_observation_redb_prototype::{Limits, Store};
use serde_json::{Value, json};
use std::time::Instant;
use uuid::Uuid;
fn sample<T>(store: &Store, f: impl FnOnce() -> T) -> (T, Value) {
    let monitor = store.monitor();
    monitor.reset();
    let before = store.file_bytes().unwrap();
    let start = Instant::now();
    let result = f();
    let ms = start.elapsed().as_secs_f64() * 1000.;
    let stats = monitor.stats();
    (
        result,
        json!({"ms":ms,"io":stats,"file_before":before,"file_after":store.file_bytes().unwrap()}),
    )
}
fn run(near: bool, quick: bool) -> Value {
    let t = tempfile::tempdir().unwrap();
    let mut limits = Limits::fixture();
    limits.quick_repair = quick;
    let store = Store::create(&t.path().join("store"), limits).unwrap();
    let (_, template) = support::normalize(1, 1);
    let template = &template[0].1;
    for source_index in 0..if near { 8 } else { 1 } {
        let source = support::source(8 + source_index);
        store.attach(source.clone()).unwrap();
        let size = if near { 640 } else { 4 };
        for start in (0..size).step_by(128) {
            let mut seen = vec![];
            for index in start..(start + 128).min(size) {
                let mut row = template.clone();
                row.raw_record_digests =
                    vec![Digest::of(format!("{index}").as_bytes()); if near { 32 } else { 1 }];
                seen.push((format!("synthetic-retained-{index}"), row));
            }
            let cursor = store.source(source.source_id).unwrap().cursor;
            store
                .append(source.source_id, cursor, cursor + 1, &[], &seen)
                .unwrap();
        }
    }
    store.audit().unwrap();
    let initial = store.snapshot().unwrap();
    let initial_file = store.file_bytes().unwrap();
    let mut samples = vec![];
    let mut peer = support::peer();
    for i in 0..35 {
        let (_, append) = sample(&store, || support::append(&store, 10000 + i));
        let (packet, prepare) = sample(&store, || store.prepare().unwrap().unwrap());
        let (sent, send) = sample(&store, || {
            store.begin_send(1000 + i * 1000).unwrap().unwrap()
        });
        assert_eq!(sent, packet);
        let receipt = peer.receive(&packet).unwrap();
        let (_, ack) = sample(&store, || store.acknowledge(&receipt).unwrap());
        samples.push(json!({"append":append,"prepare":prepare,"send":send,"ack":ack}));
    }
    store.audit().unwrap();
    json!({"fixture":"synthetic retained identity rows,real normalizer-produced single-event batches; history-fill excluded from timing","near":near,"quick_repair":quick,"physical_cap":64*1024*1024,"cache_bytes":4*1024*1024,"initial":initial,"initial_file_bytes":initial_file,"final_file_bytes":store.file_bytes().unwrap(),"measured_repetitions":samples.len(),"samples":samples,"active_source":Uuid::from_u128(8)})
}
fn main() {
    println!("{}",serde_json::to_string_pretty(&json!({"small":run(false,false),"near":run(true,false),"small_quick":run(false,true),"near_quick":run(true,true)})).unwrap());
}
