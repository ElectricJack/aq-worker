//! MS1 live check of the node-side sandbox driver against the dedicated
//! sandbox daemon and a real Claude Code session (subscription token).
//! Ignored by default. Inputs:
//! `AQ_MS1_CLAUDE_TOKEN_FILE` (0600 token file), `AQ_SANDBOX_SOCKET`,
//! `AQ_MS1_TEMPLATE_IMAGE` (pullable `...@sha256:...`).
use aq_worker::backend::docker_local::{AllowedTemplate, DockerLocalConfig};
use aq_worker::backend::{Ownership, ResourceLimits, TemplateDigest};
use aq_worker::sandbox_driver::{DriverConfig, run};
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};

fn env(name: &str) -> String {
    std::env::var(name).unwrap_or_else(|_| panic!("{name} must be set"))
}

#[tokio::test]
#[ignore = "live: real sandbox daemon and a real Claude session"]
async fn driver_runs_claude_in_docker_local_and_copies_the_edit_back() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    for d in ["work", "session", "private"] {
        std::fs::create_dir(root.join(d)).unwrap();
        std::fs::set_permissions(root.join(d), std::fs::Permissions::from_mode(0o700)).unwrap();
    }
    let work = root.join("work");
    std::fs::write(work.join("README.md"), "# scratch\n").unwrap();
    std::fs::create_dir(work.join(".git")).unwrap();
    std::fs::write(work.join(".git/HEAD"), "ref: refs/heads/main\n").unwrap();
    std::fs::write(
        root.join("session/context-0.bin"),
        std::env::var("AQ_MS1_PROMPT").unwrap_or_else(|_| {
            "Append exactly one line `hello from the MS1 driver test` to README.md. \
             Change nothing else and do not run shell commands."
                .into()
        }),
    )
    .unwrap();
    // Copy the token into a private file the driver deletes; never print it.
    let token = std::fs::read(env("AQ_MS1_CLAUDE_TOKEN_FILE")).unwrap();
    let token_file = root.join("private/token");
    let mut f = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&token_file)
        .unwrap();
    std::io::Write::write_all(&mut f, &token).unwrap();
    drop(f);
    let image = env("AQ_MS1_TEMPLATE_IMAGE");
    let digest = image.rsplit_once('@').unwrap().1.to_string();
    let mut docker = DockerLocalConfig::new(env("AQ_SANDBOX_SOCKET").into());
    docker.templates = vec![AllowedTemplate {
        image: image.clone(),
        digest: TemplateDigest::parse(digest.clone()).unwrap(),
    }];
    docker.shared_root_fs = true;
    let home = "/aq/home";
    let config = DriverConfig {
        docker_local: docker,
        template: TemplateDigest::parse(digest).unwrap(),
        owner: Ownership {
            node_id: uuid::Uuid::new_v4(),
            boot_id: uuid::Uuid::new_v4(),
            attempt_id: uuid::Uuid::new_v4(),
        },
        hard_deadline_ms: std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_millis() as u64
            + 600_000,
        resources: ResourceLimits {
            cpu_millis: 2000,
            memory_bytes: 2 << 30,
            pids: 256,
            disk_bytes: None,
        },
        workspace: work.clone(),
        token_file: token_file.clone(),
        executable: "/opt/aq/bin/claude".into(),
        arguments: [
            "-p",
            "--output-format",
            "stream-json",
            "--verbose",
            "--input-format",
            "text",
            "--permission-mode",
            "bypassPermissions",
            "--setting-sources",
            "user",
            "--settings",
            "/aq/home/settings.json",
            "--strict-mcp-config",
            "--no-session-persistence",
            "--disable-slash-commands",
            "--max-turns",
            "8",
        ]
        .map(String::from)
        .to_vec(),
        environment: [
            ("HOME", home),
            ("CLAUDE_CONFIG_DIR", home),
            ("PATH", "/usr/local/bin:/usr/bin:/bin"),
            ("DISABLE_AUTOUPDATER", "1"),
            ("DISABLE_TELEMETRY", "1"),
            ("DISABLE_ERROR_REPORTING", "1"),
            ("CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC", "1"),
        ]
        .into_iter()
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect(),
        settings_json: r#"{"permissions":{"defaultMode":"bypassPermissions"},"sandbox":{"enabled":false},"disableAllHooks":true,"enableAllProjectMcpServers":false,"cleanupPeriodDays":1,"forceLoginMethod":"claudeai"}"#.into(),
        result_dir: None,
        max_workspace_bytes: 64 * 1024 * 1024,
        agent_channel: false,
    };
    let config_file = root.join("private/driver.json");
    std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&config_file)
        .and_then(|mut f| std::io::Write::write_all(&mut f, &serde_json::to_vec(&config).unwrap()))
        .unwrap();
    unsafe { std::env::set_var("AQ_SESSION_DIRECTORY", root.join("session")) };
    let code = run(&config_file).await.unwrap();
    if let Ok(out) = std::env::var("AQ_MS1_STREAM_OUT") {
        std::fs::copy(root.join("private/claude.jsonl"), out).unwrap();
    }
    let readme = std::fs::read_to_string(work.join("README.md")).unwrap();
    assert!(!token_file.exists(), "token file wiped");
    assert_eq!(code, 0, "driver exit");
    if std::env::var("AQ_MS1_PROMPT").is_ok() {
        return;
    }
    assert!(
        readme.contains("hello from the MS1 driver test"),
        "{readme}"
    );
    assert_eq!(
        std::fs::read_to_string(work.join(".git/HEAD")).unwrap(),
        "ref: refs/heads/main\n"
    );
}
