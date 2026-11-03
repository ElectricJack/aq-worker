//! The production run command must report startup failures before B2 admission.
use aq_worker::config::WorkerConfig;
use std::process::Command;
use uuid::Uuid;

#[test]
fn run_reports_missing_worker_config_and_malformed_controller_config() {
    let dir = tempfile::tempdir().unwrap();
    let worker = dir.path().join("worker.toml");
    let controller = dir.path().join("node.json");
    let run = || {
        Command::new(env!("CARGO_BIN_EXE_aq-worker"))
            .arg("--config")
            .arg(&worker)
            .arg("run")
            .arg("--controller-config")
            .arg(&controller)
            .output()
            .unwrap()
    };
    let missing = run();
    assert_eq!(missing.status.code(), Some(2));
    let stderr = String::from_utf8(missing.stderr).unwrap();
    assert!(stderr.contains(&worker.display().to_string()), "{stderr}");
    assert!(stderr.contains("No such file"), "{stderr}");
    assert!(!stderr.contains("B2"), "{stderr}");

    WorkerConfig {
        server_url: "http://127.0.0.1:1".into(),
        node_id: Uuid::from_u128(1),
        credential: "test-credential".into(),
        cf_client_id: None,
        cf_client_secret: None,
    }
    .save(&worker)
    .unwrap();
    std::fs::write(&controller, b"{invalid-json").unwrap();
    let malformed = run();
    assert_eq!(malformed.status.code(), Some(2));
    let stderr = String::from_utf8(malformed.stderr).unwrap();
    assert!(stderr.contains("key must be a string"), "{stderr}");
    assert!(!stderr.contains("B2"), "{stderr}");
    assert!(!stderr.contains("test-credential"), "{stderr}");
}
