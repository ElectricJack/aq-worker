/// A ChatGPT login document built at run time (no credential-shaped literal).
fn codex_login() -> (Vec<u8>, String) {
    let token = |name: &str| format!("fixture-{name}-{}", "z".repeat(24));
    let refresh = token("refresh");
    let auth = json!({
        "auth_mode": "chatgpt",
        "tokens": {"id_token": token("id"), "access_token": token("access"),
            "refresh_token": refresh, "account_id": "fixture-account"},
    });
    (serde_json::to_vec(&auth).unwrap(), refresh)
}

struct CodexDriverRun {
    /// What the pane showed, and what reached the sandbox terminal's input.
    screen: Vec<u8>,
    typed: Vec<u8>,
    attach: String,
    code: i32,
    docker: ScriptedDocker,
    dir: tempfile::TempDir,
    auth: Vec<u8>,
}
/// Runs the node driver's Codex launch (harness `codex`, the docker-local Codex
/// recipe) against the scripted double: the TUI stays running, its `notify`
/// program has left `final_message` in the stop marker, the sandbox terminal
/// prints `tui` and the owner types `keys`. `extra` are files the runner writes.
async fn run_codex_driver(final_message: &str, extra: &[(&str, Vec<u8>)]) -> CodexDriverRun {
    run_codex_driver_on(final_message, extra, b"codex tui\r\n", b"").await
}
async fn run_codex_driver_on(
    final_message: &str,
    extra: &[(&str, Vec<u8>)],
    tui: &[u8],
    keys: &[u8],
) -> CodexDriverRun {
    run_driver(final_message, extra, (tui, keys), |_| {}).await
}
/// `prepare` edits the driver configuration (and the files it names) before the run.
async fn run_driver(
    final_message: &str,
    extra: &[(&str, Vec<u8>)],
    streams: (&[u8], &[u8]),
    prepare: impl FnOnce(&mut aq_worker::sandbox_driver::DriverConfig),
) -> CodexDriverRun {
    use aq_worker::sandbox_driver::DriverHarness;
    use std::os::unix::fs::PermissionsExt;
    let socket = owned_socket();
    let (cache, scratch) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
    let mut docker_local = base_config(&socket, &cache, &scratch);
    let docker = ScriptedDocker::new().runner(Runner::StaysRunning);
    let marker = json!({"type":"agent-turn-complete","last-assistant-message":final_message});
    docker.on_start_write("/aq/out/stop.json", marker.to_string().into_bytes());
    for (path, data) in extra {
        docker.on_start_write(path, data.clone());
    }

    let dir = tempfile::tempdir().unwrap();
    for d in ["work", "session", "private", "private/transcript"] {
        std::fs::create_dir(dir.path().join(d)).unwrap();
        std::fs::set_permissions(dir.path().join(d), std::fs::Permissions::from_mode(0o700))
            .unwrap();
    }
    let work = dir.path().join("work");
    let sessions = DriverHarness::Codex.transcript_dir();
    docker_local.transcript = Some((dir.path().join("private/transcript"), sessions.into()));
    docker.on_start_write(&format!("{sessions}/2026/rollout.jsonl"), b"{}\n".to_vec());
    std::fs::write(work.join("README.md"), "# scratch\n").unwrap();
    std::fs::write(
        dir.path().join("session/context-0.bin"),
        "Write the fixture.",
    )
    .unwrap();
    let (auth, _) = codex_login();
    let token_file = dir.path().join("private/token");
    write_private_fixture(&token_file, &auth);

    let mut config = fixture_driver_config(docker_local, work, token_file.clone());
    prepare(&mut config);
    let config_file = dir.path().join("private/driver.json");
    write_private_fixture(&config_file, &serde_json::to_vec(&config).unwrap());
    let session = dir.path().join("session");
    let (screen, typed, attach, code) =
        fixture_terminal(&docker, &config_file, &session, streams).await;
    assert!(!token_file.exists(), "the leased login never stays on disk");
    CodexDriverRun {
        screen,
        typed,
        attach,
        code,
        docker,
        dir,
        auth,
    }
}

fn write_private_fixture(path: &std::path::Path, data: &[u8]) {
    use std::os::unix::fs::OpenOptionsExt;
    let mut f = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(path)
        .unwrap();
    std::io::Write::write_all(&mut f, data).unwrap();
}
fn driver_recipe() -> aq_codex_mapper::Recipe {
    use aq_session_protocol::{Capability, HarnessSettings, ModelSettings, Text};
    let artifact = |id: &str| aq_session_protocol::ArtifactRef {
        id: Text::new(id).unwrap(),
        version: Text::new("1").unwrap(),
        digest: aq_session_protocol::Digest::of(id.as_bytes()),
    };
    let harness = HarnessSettings {
        harness: Text::new("codex").unwrap(),
        cli_version: Text::new(aq_codex_mapper::CODEX_CLI_VERSION).unwrap(),
        adapter: artifact("aq.codex-docker-result-local-adapter"),
        environment: artifact("environment"),
        required_capabilities: vec![Capability {
            id: Text::new(aq_session_protocol::CONTAINER_CONFINEMENT_CAPABILITY).unwrap(),
            version: 1,
        }],
    };
    let model = ModelSettings {
        provider: Text::new("openai").unwrap(),
        model: Text::new("gpt-5.5").unwrap(),
        reasoning: Text::new("high").unwrap(),
        max_output_tokens: 4096,
        spending_budget: None,
    };
    aq_codex_mapper::docker_local::map_docker_local(
        aq_codex_mapper::Settings {
            harness: &harness,
            model: &model,
            terminal_input_enabled: false,
        },
        aq_codex_mapper::Paths {
            executable: std::path::Path::new("/opt/aq/bin/codex"),
            workspace: std::path::Path::new("/aq/work"),
            private_home: std::path::Path::new("/aq/home/codex"),
        },
        None,
    )
    .unwrap()
}
fn fixture_driver_config(
    docker_local: DockerLocalConfig,
    work: PathBuf,
    token_file: PathBuf,
) -> aq_worker::sandbox_driver::DriverConfig {
    use aq_worker::sandbox_driver::{DriverConfig, DriverHarness};
    let recipe = driver_recipe();
    DriverConfig {
        docker_local,
        template: digest(),
        owner: owner(1, 2, 3),
        hard_deadline_ms: std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_millis() as u64
            + 600_000,
        resources: LIMITS,
        workspace: work,
        token_file,
        executable: recipe.executable.to_str().unwrap().into(),
        arguments: recipe.arguments,
        environment: recipe.environment,
        settings_json: String::new(),
        result_dir: None,
        max_workspace_bytes: 1 << 20,
        agent_channel: false,
        conversation_outbox: None,
        transcript_bound: None,
        harness: DriverHarness::Codex,
        codex_runtime_bundle: None,
        build_cache: None,
    }
}

#[tokio::test]
async fn a_create_timeout_is_named_and_never_triggers_an_image_pull_or_second_create() {
    let h = harness().await;
    let daemon = h.docker.clone();
    daemon.script(Rule {
        method: DockerMethod::Post,
        path_prefix: "/v1.44/containers/create".into(),
        effect: Effect::StallBeforeApply,
    });
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("context-0.bin"), "Run the fixture.").unwrap();
    let token = dir.path().join("token");
    write_private_fixture(&token, &codex_login().0);
    let mut docker = h.backend.config().clone();
    docker.api_timeout_ms = 20;
    let config = fixture_driver_config(docker, dir.path().to_path_buf(), token);
    let path = dir.path().join("driver.json");
    write_private_fixture(&path, &serde_json::to_vec(&config).unwrap());
    let error = bounded(aq_worker::sandbox_driver::run_with(
        &path,
        Some(dir.path()),
        |config| DockerLocalBackend::connect(config, daemon.clone()),
        |_, _| async {
            Err::<(tokio::io::DuplexStream, aq_worker::sandbox_driver::Pane), _>(
                anyhow::anyhow!("an uncertain create must not attach a terminal"),
            )
        },
    ))
    .await
    .unwrap_err();
    assert_eq!(error.to_string(), "create sandbox");
    assert_eq!(
        error.downcast_ref::<BackendError>(),
        Some(&BackendError::Uncertain(UnconfirmedReason::Timeout))
    );
    let posts = daemon
        .requests()
        .into_iter()
        .filter(|(method, _)| *method == DockerMethod::Post)
        .map(|(_, path)| path)
        .collect::<Vec<_>>();
    assert_eq!(posts.len(), 2);
    assert!(posts[0].starts_with("/v1.44/volumes/create"));
    assert!(posts[1].starts_with("/v1.44/containers/create"));
}

async fn fixture_terminal(
    docker: &ScriptedDocker,
    config_file: &std::path::Path,
    session: &std::path::Path,
    (tui, keys): (&[u8], &[u8]),
) -> (Vec<u8>, Vec<u8>, String, i32) {
    use aq_worker::sandbox_driver::run_with;
    let connect = |c| DockerLocalBackend::connect(c, docker.clone());
    // The sandbox terminal (a duplex stream) and the pane (two pipes).
    use std::io::{Read, Write};
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let (ours, mut sandbox) = tokio::io::duplex(1 << 16);
    let (input, mut keyboard) = std::io::pipe().unwrap();
    let (mut shown, output) = std::io::pipe().unwrap();
    keyboard.write_all(keys).unwrap();
    drop(keyboard);
    sandbox.write_all(tui).await.unwrap();
    let attached = std::sync::Arc::new(std::sync::Mutex::new(String::new()));
    let seen = attached.clone();
    let terminal = |_socket: std::path::PathBuf, path: String| async move {
        *seen.lock().unwrap() = path;
        let pane = aq_worker::sandbox_driver::Pane {
            input: Box::new(input),
            output: Box::new(output),
            tty: false,
        };
        Ok((ours, pane))
    };
    let code = tokio::time::timeout(
        Duration::from_secs(60),
        run_with(config_file, Some(session), connect, terminal),
    )
    .await
    .expect("driver hung")
    .unwrap();
    let mut typed = vec![0u8; keys.len()];
    sandbox.read_exact(&mut typed).await.unwrap();
    let mut screen = Vec::new();
    shown.read_to_end(&mut screen).unwrap();
    let attach = attached.lock().unwrap().clone();
    (screen, typed, attach, code)
}
