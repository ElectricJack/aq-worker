use aq_worker::config::{RunnerAccess, runner_access_path};
use serde::Deserialize;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};

#[test]
fn runner_access_path_sits_next_to_worker_toml() {
    assert_eq!(
        runner_access_path(Path::new("/config/agent-q/worker.toml")),
        PathBuf::from("/config/agent-q/runner-access.json")
    );
    assert_eq!(
        runner_access_path(Path::new("worker.toml")),
        PathBuf::from("runner-access.json")
    );
}

#[test]
fn runner_access_save_is_private_json() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("agent-q").join("runner-access.json");
    let token = RunnerAccess {
        client_id: "id.access".into(),
        client_secret: "s3cret-value".into(),
    };
    assert!(!format!("{token:?}").contains("s3cret-value"));
    token.save(&path).unwrap();
    let meta = std::fs::metadata(&path).unwrap();
    assert_eq!(meta.mode() & 0o777, 0o600);
    assert_eq!(meta.nlink(), 1);
    assert_eq!(
        std::fs::metadata(path.parent().unwrap()).unwrap().mode() & 0o777,
        0o700
    );
    #[derive(Deserialize)]
    #[serde(deny_unknown_fields)]
    struct Proxy {
        client_id: String,
        client_secret: String,
    }
    let read: Proxy = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
    assert_eq!(read.client_id, "id.access");
    assert_eq!(read.client_secret, "s3cret-value");
    RunnerAccess {
        client_id: "id2".into(),
        client_secret: "other".into(),
    }
    .save(&path)
    .unwrap();
    let read: Proxy = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
    assert_eq!(read.client_id, "id2");
    assert_eq!(
        std::fs::read_dir(path.parent().unwrap()).unwrap().count(),
        1
    );
}
