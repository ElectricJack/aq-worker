#[allow(dead_code)]
mod support;
use aq_observation_redb_prototype::{
    Limits, Snapshot, SourceRow, Store,
    backend::{Action, Fault},
};
use std::{
    path::Path,
    process::{Command, Stdio},
};
use uuid::Uuid;
fn projection(store: &Store) -> (Snapshot, SourceRow) {
    (
        store.snapshot().unwrap(),
        store.source(Uuid::from_u128(8)).unwrap(),
    )
}
fn apply(store: &Store, stage: &str) -> anyhow::Result<()> {
    match stage {
        "append" => {
            let s = store.source(Uuid::from_u128(8))?;
            let (events, seen) = support::normalize_at(store.snapshot()?.next, 2, s.cursor);
            store.append(
                s.source.source_id,
                s.cursor,
                s.cursor + support::row(2).len() as u64,
                &events,
                &seen,
            )
        }
        "prepare" => store.prepare().map(|_| ()),
        "send" => store.begin_send(1000).map(|_| ()),
        "ack" => {
            let packet = store.snapshot()?.delivery.unwrap().packet;
            store.acknowledge(&support::receipt(&packet))
        }
        _ => panic!(),
    }
}
fn copy_db(from: &Path, to: &Path) {
    use std::os::unix::fs::DirBuilderExt;
    std::fs::DirBuilder::new().mode(0o700).create(to).unwrap();
    std::fs::copy(from.join("store.redb"), to.join("store.redb")).unwrap();
}
fn setup(base: &Path, stage: &str) {
    let store = Store::create(base, Limits::fixture()).unwrap();
    store.attach(support::source(8)).unwrap();
    if stage != "append" {
        support::append(&store, 1);
    }
    if stage == "send" || stage == "ack" {
        store.prepare().unwrap();
    }
    if stage == "ack" {
        store.begin_send(500).unwrap();
    }
}
#[test]
fn crash_child() {
    let Ok(path) = std::env::var("AQ_REDB_CRASH_ROOT") else {
        return;
    };
    let stage = std::env::var("AQ_REDB_STAGE").unwrap();
    let op = std::env::var("AQ_REDB_OP").unwrap().parse().unwrap();
    let after = std::env::var("AQ_REDB_AFTER").unwrap() == "true";
    let store = Store::open(Path::new(&path), Limits::fixture()).unwrap();
    store.monitor().reset();
    store.monitor().fault(Fault {
        operation: op,
        after,
        action: Action::Exit,
    });
    apply(&store, &stage).unwrap();
    panic!("target boundary was not reached");
}
// A concurrent fork can briefly inherit another test's flock before exec
// closes CLOEXEC descriptors. Serialize the two process/IO matrices so a
// reopen tests storage recovery rather than that unrelated fork window.
static MATRIX_OWNER: std::sync::Mutex<()> = std::sync::Mutex::new(());
fn walk(process_exit: bool) -> u64 {
    let _exclusive = MATRIX_OWNER.lock().unwrap();
    let mut cuts = 0;
    for stage in ["append", "prepare", "send", "ack"] {
        let temp = tempfile::tempdir().unwrap();
        let base = temp.path().join("base");
        setup(&base, stage);
        let before_store = Store::open(&base, Limits::fixture()).unwrap();
        let before = projection(&before_store);
        drop(before_store);
        let successful = temp.path().join("successful");
        copy_db(&base, &successful);
        let store = Store::open(&successful, Limits::fixture()).unwrap();
        store.monitor().reset();
        apply(&store, stage).unwrap();
        let operations = store.monitor().stats().operations;
        let after = projection(&store);
        drop(store);
        assert!(operations > 2);
        for op in 1..=operations {
            for is_after in [false, true] {
                let path = temp.path().join(format!("cut-{op}-{is_after}"));
                copy_db(&base, &path);
                if process_exit {
                    let status = Command::new(std::env::current_exe().unwrap())
                        .args(["--exact", "crash_child", "--nocapture"])
                        .env("AQ_REDB_CRASH_ROOT", &path)
                        .env("AQ_REDB_STAGE", stage)
                        .env("AQ_REDB_OP", op.to_string())
                        .env("AQ_REDB_AFTER", is_after.to_string())
                        .stdout(Stdio::null())
                        .stderr(Stdio::null())
                        .status()
                        .unwrap();
                    assert_eq!(status.code(), Some(77), "{stage} op{op} after{is_after}");
                } else {
                    let store = Store::open(&path, Limits::fixture()).unwrap();
                    store.monitor().reset();
                    store.monitor().fault(Fault {
                        operation: op,
                        after: is_after,
                        action: Action::Error,
                    });
                    assert!(
                        apply(&store, stage).is_err(),
                        "{stage} op{op} after{is_after}"
                    );
                    assert!(store.prepare().is_err(), "uncertain packet prepared");
                    assert!(store.begin_send(5000).is_err(), "uncertain store reused");
                    drop(store);
                }
                let reopened = Store::open(&path, Limits::fixture()).unwrap();
                reopened.audit().unwrap();
                let actual = projection(&reopened);
                assert!(
                    actual == before || actual == after,
                    "partial projection {stage} op{op} after{is_after}: {actual:?}"
                );
                assert!(reopened.file_bytes().unwrap() <= Limits::fixture().physical_bytes);
                cuts += 1;
            }
        }
    }
    cuts
}
#[test]
fn every_mutating_backend_boundary_io_error_reopens_to_old_or_new_projection() {
    let count = walk(false);
    eprintln!("deterministic IO-error boundary cuts: {count}");
}
#[test]
fn every_mutating_backend_boundary_process_exit_reopens_to_old_or_new_projection() {
    let count = walk(true);
    eprintln!("process-exit boundary cuts: {count}");
}
