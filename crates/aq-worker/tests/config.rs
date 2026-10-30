use std::os::unix::fs::PermissionsExt;

use aq_worker::config::WorkerConfig;
use uuid::Uuid;

fn sample() -> WorkerConfig {
    WorkerConfig {
        server_url: "https://worker.example.com".into(),
        node_id: Uuid::new_v4(),
        credential: "aqn_secret".into(),
        cf_client_id: Some("id".into()),
        cf_client_secret: Some("cfsecret".into()),
    }
}

#[test]
fn config_saved_with_0600() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("sub/worker.toml");
    let cfg = sample();
    cfg.save(&path).unwrap();
    let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
    assert_eq!(mode, 0o600);
    let dmode = std::fs::metadata(path.parent().unwrap())
        .unwrap()
        .permissions()
        .mode()
        & 0o777;
    assert_eq!(dmode, 0o700);
    let back = WorkerConfig::load(&path).unwrap();
    assert_eq!(back.node_id, cfg.node_id);
    assert_eq!(back.credential, "aqn_secret");
    // No temp files left behind.
    assert_eq!(
        std::fs::read_dir(path.parent().unwrap()).unwrap().count(),
        1
    );
}

#[test]
fn load_refuses_world_readable_config() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("worker.toml");
    sample().save(&path).unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
    let err = WorkerConfig::load(&path).unwrap_err().to_string();
    assert_eq!(
        err,
        format!(
            "{p} is readable by other users; run chmod 600 {p}",
            p = path.display()
        )
    );
}

#[test]
fn debug_redacts_secrets() {
    let s = format!("{:?}", sample());
    assert!(!s.contains("aqn_secret"));
    assert!(!s.contains("cfsecret"));
}
