#[allow(dead_code)]
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
        },
        128,
    )
    .unwrap()
}
fn rows() -> Vec<Value> {
    let f: Value = serde_json::from_str(include_str!("fixtures/native-synthetic.json")).unwrap();
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

// Spawn the test executable itself so the durable-temp boundary survives an
// actual process exit. This is not a machine/power-loss simulation.
#[test]
fn durable_temp_child() {
    let Ok(root) = std::env::var("AQ_OBSERVATION_CRASH_FIXTURE") else {
        return;
    };
    let root = std::path::Path::new(&root);
    let mut spool = Spool::open(&root.join("spool"), node(), Limits::default()).unwrap();
    let permit = SourceFile::inspect(
        Uuid::from_u128(8),
        root,
        "native.jsonl",
        Text::new("native-fixture").unwrap(),
    )
    .unwrap();
    let file = SourceFile::open(permit).unwrap();
    spool.inject_write_fault(WriteFault::BeforeRenameRetainedTemp);
    assert!(
        spool
            .collect(Uuid::from_u128(8), &file, 100, OverflowPolicy::Hold)
            .is_err()
    );
    assert!(spool.path().join("pending.tmp").is_file());
    std::process::exit(77);
}
#[test]
fn retained_fsynced_temp_survives_process_exit_and_reopen_refuses_without_deletion() {
    let temp = tempfile::tempdir().unwrap();
    let (spool, _, _) = fixture(&temp, &lines(&rows()), Limits::default());
    let committed = std::fs::read(spool.path().join("spool.json")).unwrap();
    drop(spool);
    let status = std::process::Command::new(std::env::current_exe().unwrap())
        .args(["--exact", "durable_temp_child", "--nocapture"])
        .env("AQ_OBSERVATION_CRASH_FIXTURE", temp.path())
        .status()
        .unwrap();
    assert_eq!(status.code(), Some(77));
    let pending = temp.path().join("spool/pending.tmp");
    let retained = std::fs::read(&pending).unwrap();
    assert_ne!(retained, committed);
    let candidate: Value = serde_json::from_slice(&retained).unwrap();
    assert!(!candidate["state"]["outbox"].as_array().unwrap().is_empty());
    assert!(Spool::open(&temp.path().join("spool"), node(), Limits::default()).is_err());
    assert_eq!(std::fs::read(&pending).unwrap(), retained);
    assert_eq!(
        std::fs::read(temp.path().join("spool/spool.json")).unwrap(),
        committed
    );
    let old: Value = serde_json::from_slice(&committed).unwrap();
    assert_eq!(
        old["state"]["sources"][Uuid::from_u128(8).to_string()]["cursor"]["offset"],
        0
    );
}
