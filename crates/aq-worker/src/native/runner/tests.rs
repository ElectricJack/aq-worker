use super::*;
use crate::native::adapter::{AdapterManifest, FilePin, HandoffKind};
use std::os::unix::fs::{PermissionsExt, symlink};
#[path = "../../../tests/support/native_runner_config.rs"]
mod fixture;
pub(crate) use fixture::config;
use fixture::write;

#[tokio::test]
async fn unavailable_executor_refuses_before_any_journal_or_network_admission() {
    let (dir, c) = config();
    let (_, shutdown) = watch::channel(false);
    let worker = WorkerConfig {
        server_url: "http://127.0.0.1:9".into(),
        node_id: Uuid::new_v4(),
        credential: "test-node-credential".into(),
        cf_client_id: None,
        cf_client_secret: None,
    };
    let error = run_once(&worker, c, &mut UnavailableExecutor, shutdown)
        .await
        .unwrap_err();
    assert!(
        error
            .to_string()
            .contains("no qualified native executor installed")
    );
    assert!(!dir.path().join("journal").exists());
    assert!(!dir.path().join("materials").exists());
    // The absent inventory file/Git socket would fail if any admission setup had run.
}
#[test]
fn private_config_refuses_symlink_public_mode_and_oversize() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("config");
    write(&path, b"{}", 0o600);
    let link = dir.path().join("link");
    symlink(&path, &link).unwrap();
    assert!(config::private_file(&link, 10).is_err());
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
    assert!(config::private_file(&path, 10).is_err());
    write(&path, b"too many bytes", 0o600);
    assert!(config::private_file(&path, 10).is_err());
}
#[test]
fn existing_native_state_is_opened_for_validated_recovery() {
    let (dir, c) = config();
    let node = Uuid::new_v4();
    let root = dir.path().join("journal");
    let initial = Journal::create(&root, node, c.capacity.clone()).unwrap();
    let old = initial.state().identity.boot_id;
    drop(initial);
    let reopened = open_journal(&c, node).unwrap();
    assert!(reopened.recovery_required());
    assert_eq!(reopened.state().identity.boot_id, old);
}

#[test]
fn retained_attempts_is_empty_without_journal() {
    let (_dir, c) = config();
    assert!(retained_attempts(&c, Uuid::new_v4()).unwrap().is_empty());
}
