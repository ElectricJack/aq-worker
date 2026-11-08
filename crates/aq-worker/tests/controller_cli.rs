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

fn aq_worker() -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_aq-worker"));
    command.stdin(std::process::Stdio::null());
    command
}

#[test]
fn run_requires_exactly_one_config_kind() {
    assert_eq!(aq_worker().arg("run").status().unwrap().code(), Some(2));
    let both = aq_worker()
        .args(["run", "--controller-config", "x", "--service-config", "y"])
        .output()
        .unwrap();
    assert_eq!(both.status.code(), Some(2));
    let missing = aq_worker()
        .args(["run", "--service-config", "/nonexistent/service.toml"])
        .output()
        .unwrap();
    assert_eq!(missing.status.code(), Some(2));
    let stderr = String::from_utf8(missing.stderr).unwrap();
    assert!(stderr.contains("/nonexistent/service.toml"), "{stderr}");
}

#[test]
fn service_enroll_never_accepts_secrets_in_argv() {
    let output = aq_worker()
        .args([
            "service",
            "enroll",
            "--profile",
            "p",
            "--server",
            "http://127.0.0.1:9",
        ])
        .args([
            "--native-template",
            "t",
            "--aq-git",
            "g",
            "--repositories",
            "r",
        ])
        .args(["--code", "secret-code"])
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(2));
    let stderr = String::from_utf8(output.stderr).unwrap();
    assert!(stderr.contains("unexpected argument '--code'"), "{stderr}");
}

#[test]
fn service_enroll_refuses_an_invalid_machine_key_before_any_request() {
    let output = aq_worker()
        .args([
            "service",
            "enroll",
            "--profile",
            "p",
            "--server",
            "http://127.0.0.1:9",
        ])
        .args([
            "--native-template",
            "t",
            "--aq-git",
            "g",
            "--repositories",
            "r",
        ])
        .args(["--machine-key", "Desk"])
        .output()
        .unwrap();
    assert_ne!(output.status.code(), Some(0));
    let stderr = String::from_utf8(output.stderr).unwrap();
    assert!(stderr.contains("machine key"), "{stderr}");
    assert!(!stderr.contains("connect"), "{stderr}");
}

#[test]
fn service_status_without_install_reports_not_installed() {
    let home = tempfile::tempdir().unwrap();
    let output = aq_worker()
        .args(["service", "status", "--profile", "local"])
        .env("XDG_CONFIG_HOME", home.path().join("config"))
        .env("XDG_STATE_HOME", home.path().join("state"))
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(0), "{output:?}");
    let stdout = String::from_utf8(output.stdout).unwrap();
    assert!(stdout.contains("not installed"), "{stdout}");
}
