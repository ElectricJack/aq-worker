//! Unit checks of the node driver’s public pieces.

use aq_handoff_note::Secrets;
use aq_worker::backend::docker_local::{DockerLocalConfig, tar::TreeEntry};
use aq_worker::sandbox_driver::{
    CHANNEL_NOTE, DriverConfig, channel_ready, input_entries, run, token_leak,
};
use std::io::Write;

/// MS1 review S3a: the token file is read and removed before anything that
/// can fail, even when the driver config itself is unreadable.
#[tokio::test]
async fn token_file_is_removed_even_when_the_driver_config_is_bad() {
    use std::os::unix::fs::OpenOptionsExt;
    let dir = tempfile::tempdir().unwrap();
    let config = dir.path().join("driver.json");
    let token = dir.path().join("token");
    for (path, data) in [
        (&config, &b"{not a config"[..]),
        (&token, b"fixture-login-0123456789"),
    ] {
        std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(path)
            .unwrap()
            .write_all(data)
            .unwrap();
    }
    assert!(run(&config).await.is_err());
    assert!(!token.exists(), "the leased token must not stay on disk");
}

/// 0006 §5.4/§8: no channel socket, no launch; the env names the socket;
/// an `aqa_` credential anywhere that leaves the sandbox is a leak.
#[test]
fn channel_is_required_named_in_env_and_its_credential_guarded() {
    let root = tempfile::tempdir().unwrap();
    let mut config: DriverConfig = serde_json::from_value(serde_json::json!({
        "docker_local": DockerLocalConfig::new("/run/x.sock".into()),
        "template": format!("sha256:{}", "a".repeat(64)),
        "owner": {"node_id": uuid::Uuid::from_u128(1), "boot_id": uuid::Uuid::from_u128(2),
            "attempt_id": uuid::Uuid::from_u128(3)},
        "hard_deadline_ms": 1, "resources": {"cpu_millis": 1000, "memory_bytes": 1u64 << 30, "pids": 64},
        "workspace": "/w", "token_file": "/t", "executable": "/e", "arguments": [], "environment": {},
        "settings_json": "{}", "result_dir": null, "max_workspace_bytes": 1, "agent_channel": true,
    }))
    .unwrap();
    let error = channel_ready(&config).unwrap_err().to_string();
    assert!(error.starts_with("agent_channel_unavailable"), "{error}");
    config.docker_local.channel_root = Some(root.path().into());
    let dir = root.path().join(uuid::Uuid::from_u128(3).to_string());
    std::fs::create_dir(&dir).unwrap();
    assert!(
        channel_ready(&config).is_err(),
        "a directory without a socket"
    );
    let _socket = std::os::unix::net::UnixListener::bind(dir.join("aq.sock")).unwrap();
    channel_ready(&config).unwrap();
    config.environment = [("PATH".into(), "/usr/bin:/bin".into())].into();
    let inputs = input_entries(&config, b"p").unwrap();
    let text = |name: &str| match inputs.iter().find(|e| e.path() == name) {
        Some(TreeEntry::File { data, .. }) => String::from_utf8(data.clone()).unwrap(),
        _ => panic!("{name}"),
    };
    let env = "PATH=/opt/aq/bin:/usr/bin:/bin\nAQ_CHANNEL=unix:/aq/channel/aq.sock\n";
    assert_eq!(text("env"), env);

    assert!(text("prompt").ends_with(CHANNEL_NOTE));
    let credential = format!("aqa_{}", "Q".repeat(43));
    let file = |data: &str| TreeEntry::File {
        path: "f".into(),
        mode: 0o644,
        data: data.into(),
    };
    let token = &Secrets::new(b"fixture-login-0123456789abcdef", None);
    assert!(token_leak(&[file(&credential)], b"", token).is_some());
    assert!(token_leak(&[], credential.as_bytes(), token).is_some());
    assert!(token_leak(&[file("starts_with(\"aqa_\")")], b"", token).is_none());
    // A shape the base tree already held (a fixture in the repository) is no leak.
    let kept = &Secrets::new(b"fixture-login-0123456789abcdef", [credential.as_bytes()]);
    assert!(token_leak(&[file(&credential)], credential.as_bytes(), kept).is_none());
    assert!(token_leak(&[file(&format!("{credential}x"))], b"", kept).is_some());
    // A variable that would replace the subscription login is refused.
    config.environment = [("ANTHROPIC_API_KEY".into(), "x".into())].into();
    assert!(input_entries(&config, b"p").is_err(), "API key env refused");
}

/// Spec 0016 §3 (W3): conflicts that merging main left go into the agent's task context.
#[test]
fn a_returned_tasks_merge_conflicts_are_named_in_the_prompt() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("worktree");
    let repository = uuid::Uuid::from_u128(7);
    let mut worktree = aq_node_git::Worktree {
        attempt: uuid::Uuid::from_u128(3),
        repository: dir.path().join("repo.git"),
        path: path.clone(),
        grant: aq_node_git::Grant {
            endpoint: format!("/v1/git/{repository}"),
            repository_id: repository,
            base: "a".repeat(40),
            task_ref: "refs/heads/aq/t".into(),
            lease: Some("b".repeat(40)),
            update_from_main: true,
        },
        start: "b".repeat(40),
        synced: aq_node_git::Synced::Fetched,
        conflicts: vec!["src/a.rs".into()],
    };
    let record = path.with_extension("grant.json");
    std::fs::write(&record, serde_json::to_vec(&worktree).unwrap()).unwrap();
    let mut config: DriverConfig = serde_json::from_value(serde_json::json!({
        "docker_local": DockerLocalConfig::new("/run/x.sock".into()),
        "template": format!("sha256:{}", "a".repeat(64)),
        "owner": {"node_id": uuid::Uuid::from_u128(1), "boot_id": uuid::Uuid::from_u128(2),
            "attempt_id": uuid::Uuid::from_u128(3)},
        "hard_deadline_ms": 1, "resources": {"cpu_millis": 1000, "memory_bytes": 1u64 << 30, "pids": 64},
        "workspace": "/w", "token_file": "/t", "executable": "/e", "arguments": [], "environment": {},
        "settings_json": "{}", "result_dir": null, "max_workspace_bytes": 1,
    }))
    .unwrap();
    let prompt = |config: &DriverConfig| match input_entries(config, b"p")
        .unwrap()
        .into_iter()
        .find(|e| e.path() == "prompt")
    {
        Some(TreeEntry::File { data, .. }) => String::from_utf8(data).unwrap(),
        _ => panic!("prompt"),
    };
    assert!(!prompt(&config).contains("conflict markers"));
    config.workspace = path;
    assert!(prompt(&config).contains("conflict markers in: src/a.rs"));
    worktree.conflicts.clear();
    std::fs::write(record, serde_json::to_vec(&worktree).unwrap()).unwrap();
    assert!(prompt(&config).contains("The merge was clean"));
}
