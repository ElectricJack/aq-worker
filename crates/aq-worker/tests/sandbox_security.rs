use aq_worker::native::runner::sandbox::{private::RunnerFiles, proxy::AqApiProxy};
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

#[test]
fn runner_private_layout_refuses_unsafe_secret_files() {
    let root = tempfile::tempdir().unwrap();
    let files = RunnerFiles::create(&root.path().join("runner")).unwrap();
    files
        .write_secret("provider.key", b"fixture-provider-key")
        .unwrap();
    let key = files.root().join("provider.key");
    assert_eq!(
        std::fs::metadata(files.root()).unwrap().mode() & 0o777,
        0o700
    );
    assert_eq!(std::fs::metadata(&key).unwrap().mode() & 0o777, 0o600);
    assert_eq!(std::fs::metadata(&key).unwrap().uid(), unsafe {
        libc::geteuid()
    });
    assert_eq!(
        &*RunnerFiles::read_secret(&key).unwrap(),
        b"fixture-provider-key"
    );
    assert!(files.write_secret("../escape", b"bad").is_err());
    assert!(files.write_secret("provider.key", b"overwrite").is_err());
    std::fs::set_permissions(&key, std::fs::Permissions::from_mode(0o644)).unwrap();
    assert!(RunnerFiles::read_secret(&key).is_err());
    std::fs::set_permissions(&key, std::fs::Permissions::from_mode(0o600)).unwrap();
    std::os::unix::fs::symlink(&key, files.root().join("link")).unwrap();
    assert!(RunnerFiles::read_secret(&files.root().join("link")).is_err());
    std::fs::hard_link(&key, files.root().join("hardlink")).unwrap();
    assert!(RunnerFiles::read_secret(&key).is_err());
}

#[tokio::test]
async fn aq_proxy_injects_access_header_without_exposing_token() {
    let root = tempfile::tempdir().unwrap();
    let files = RunnerFiles::create(&root.path().join("runner")).unwrap();
    files
        .write_secret(
            "access.json",
            br#"{"client_id":"runner-only-id","client_secret":"runner-only-secret"}"#,
        )
        .unwrap();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let upstream = format!("http://{}", listener.local_addr().unwrap());
    let peer = tokio::spawn(async move {
        for response in [
            "HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\n{}",
            "HTTP/1.1 200 OK\r\nContent-Length: 18\r\n\r\nrunner-only-secret",
            "HTTP/1.1 200 OK\r\nCf-Access-Client-Secret: runner-only-secret\r\nContent-Length: 2\r\n\r\n{}",
            "HTTP/1.1 302 Found\r\nLocation: https://example.invalid/runner-only-secret\r\nContent-Length: 0\r\n\r\n",
        ] {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut head = Vec::new();
            while !head.ends_with(b"\r\n\r\n") {
                head.push(socket.read_u8().await.unwrap());
            }
            let head = String::from_utf8(head).unwrap().to_lowercase();
            assert!(head.starts_with("get /v1/agent/context?q=1 http/1.1"));
            assert!(head.contains("authorization: bearer agent-credential\r\n"));
            assert!(head.contains("accept-encoding: identity\r\n"));
            assert!(head.contains("cf-access-client-id: runner-only-id\r\n"));
            assert!(head.contains("cf-access-client-secret: runner-only-secret\r\n"));
            assert!(!head.contains("spoof") && !head.contains("hop-secret"));
            socket.write_all(response.as_bytes()).await.unwrap();
        }
    });
    let proxy = AqApiProxy::development_loopback(&upstream, &files.root().join("access.json"))
        .await
        .unwrap();
    let client = reqwest::Client::builder()
        .no_proxy()
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .unwrap();
    for index in 0..4 {
        let response = client
            .get(format!("{}/v1/agent/context?q=1", proxy.origin()))
            .bearer_auth("agent-credential")
            .header("cf-access-client-id", "spoof-id")
            .header("cf-access-client-secret", "spoof-secret")
            .header("Connection", "x-hop")
            .header("x-hop", "hop-secret")
            .send()
            .await
            .unwrap();
        assert_eq!(
            response.status().as_u16(),
            if index == 0 { 200 } else { 502 }
        );
        assert!(response.headers().get("cf-access-client-secret").is_none());
        let body = response.text().await.unwrap();
        assert!(!body.contains("runner-only") && !body.contains("spoof"));
        if index == 0 {
            assert_eq!(body, "{}");
        }
    }
    peer.await.unwrap();
    proxy.stop().await.unwrap();
}

#[tokio::test]
async fn runner_responses_proxy_keeps_provider_key_at_upstream() {
    use aq_responses_transport::ResponsesPolicy;
    use aq_session_protocol::{Digest, Text};
    use aq_worker::native::runner::sandbox::responses::RunnerResponsesProxy;
    let root = tempfile::tempdir().unwrap();
    let files = RunnerFiles::create(&root.path().join("runner")).unwrap();
    files
        .write_secret("provider.key", b"provider-only-secret")
        .unwrap();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let upstream = format!("http://{}", listener.local_addr().unwrap());
    let peer = tokio::spawn(async move {
        let (mut socket, _) = listener.accept().await.unwrap();
        let mut head = Vec::new();
        while !head.ends_with(b"\r\n\r\n") {
            head.push(socket.read_u8().await.unwrap());
        }
        let head = String::from_utf8(head).unwrap().to_lowercase();
        assert!(head.contains("authorization: bearer provider-only-secret\r\n"));
        let length: usize = head
            .lines()
            .find_map(|line| line.strip_prefix("content-length: "))
            .unwrap()
            .trim()
            .parse()
            .unwrap();
        let mut body = vec![0; length];
        socket.read_exact(&mut body).await.unwrap();
        assert_eq!(
            serde_json::from_slice::<serde_json::Value>(&body).unwrap()["max_output_tokens"],
            100
        );
        socket.write_all(b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: 12\r\n\r\ndata: done\n\n").await.unwrap();
    });
    let model = aq_session_protocol::ModelSettings {
        provider: Text::new("openai").unwrap(),
        model: Text::new("fixture-model").unwrap(),
        reasoning: Text::new("medium").unwrap(),
        max_output_tokens: 100,
        spending_budget: None,
    };
    let mut now = libc::timespec {
        tv_sec: 0,
        tv_nsec: 0,
    };
    assert_eq!(
        unsafe { libc::clock_gettime(libc::CLOCK_BOOTTIME, &mut now) },
        0
    );
    let policy = ResponsesPolicy::new(
        Digest::of(b"fixture-attempt"),
        &model,
        now.tv_sec as u64 * 1_000_000_000 + now.tv_nsec as u64 + 30_000_000_000,
    )
    .unwrap();
    let proxy = RunnerResponsesProxy::development_loopback(
        policy,
        &files.root().join("provider.key"),
        &upstream,
    )
    .await
    .unwrap();
    let local = proxy.local_capability();
    assert_ne!(&*local, b"provider-only-secret");
    let response = reqwest::Client::new().post(format!("{}/v1/responses", proxy.origin()))
        .bearer_auth(std::str::from_utf8(&local).unwrap()).header("content-type", "application/json")
        .body(r#"{"model":"fixture-model","stream":true,"store":false,"input":[],"reasoning":{"effort":"medium","summary":"auto"},"tools":[],"tool_choice":"auto","parallel_tool_calls":true,"text":{"verbosity":"low"},"include":[]}"#)
        .send().await.unwrap();
    assert_eq!(response.status(), 200);
    assert_eq!(response.text().await.unwrap(), "data: done\n\n");
    peer.await.unwrap();
    proxy.stop().await.unwrap();
}

// A fresh dedicated executable performs transitions before starting any threads.
#[test]
fn agent_user_cannot_read_permit_or_proxy_key() {
    let wrapper = std::env::var("AQ_B6B_ROOT_WRAPPER").unwrap_or_else(|_| {
        concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../scripts/test-b6b-root.sh"
        )
        .into()
    });
    let output = std::process::Command::new(wrapper)
        .arg(env!("CARGO_BIN_EXE_aq-b6b-user-probe"))
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "stdout={} stderr={}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

#[tokio::test]
async fn aq_proxy_refuses_non_machine_origins_and_unsafe_access_files() {
    let dir = tempfile::tempdir().unwrap();
    let files = RunnerFiles::create(&dir.path().join("runner")).unwrap();
    files
        .write_secret(
            "access.json",
            br#"{"client_id":"id","client_secret":"secret"}"#,
        )
        .unwrap();
    for origin in [
        "http://machine.example",
        "https://127.0.0.1",
        "https://[::1]",
        "https://[2001:db8::1]",
        "https://user:pass@machine.example",
        "https://machine.example/route",
        "https://machine.example/?x=1",
    ] {
        assert!(
            AqApiProxy::start(origin, &files.root().join("access.json"))
                .await
                .is_err()
        );
    }
    for origin in [
        "http://0.0.0.0:1234",
        "http://localhost:1234",
        "http://127.0.0.1:1234/route",
    ] {
        assert!(
            AqApiProxy::development_loopback(origin, &files.root().join("access.json"))
                .await
                .is_err()
        );
    }
    std::fs::set_permissions(
        files.root().join("access.json"),
        std::fs::Permissions::from_mode(0o644),
    )
    .unwrap();
    assert!(
        AqApiProxy::development_loopback(
            "http://127.0.0.1:1234",
            &files.root().join("access.json")
        )
        .await
        .is_err()
    );
}

#[tokio::test]
async fn aq_proxy_shutdown_cancels_concurrent_incomplete_requests() {
    let root = tempfile::tempdir().unwrap();
    let files = RunnerFiles::create(&root.path().join("runner")).unwrap();
    files
        .write_secret(
            "access.json",
            br#"{"client_id":"fixture-id","client_secret":"fixture-secret"}"#,
        )
        .unwrap();
    let proxy =
        AqApiProxy::development_loopback("http://127.0.0.1:9", &files.root().join("access.json"))
            .await
            .unwrap();
    let address = proxy.origin().strip_prefix("http://").unwrap().to_owned();
    let mut sockets = Vec::new();
    for _ in 0..8 {
        let mut socket = tokio::net::TcpStream::connect(&address).await.unwrap();
        socket.write_all(b"GET / HTTP/1.1\r\n").await.unwrap();
        sockets.push(socket);
    }
    tokio::time::timeout(std::time::Duration::from_secs(1), proxy.stop())
        .await
        .unwrap()
        .unwrap();
    assert!(tokio::net::TcpStream::connect(&address).await.is_err());
    for mut socket in sockets {
        let mut byte = [0];
        let result =
            tokio::time::timeout(std::time::Duration::from_secs(1), socket.read(&mut byte))
                .await
                .unwrap();
        assert!(matches!(result, Ok(0) | Err(_)));
    }
}

#[test]
fn runner_private_files_refuse_oversized_and_malformed_secrets() {
    let root = tempfile::tempdir().unwrap();
    let files = RunnerFiles::create(&root.path().join("runner")).unwrap();
    assert!(
        files
            .write_secret("provider.key", &vec![b'x'; 4 * 1024 * 1024 + 1])
            .is_err()
    );
    files.write_secret("provider.key", b"key").unwrap();
    let file = std::fs::OpenOptions::new()
        .write(true)
        .open(files.root().join("provider.key"))
        .unwrap();
    file.set_len(4 * 1024 * 1024 + 1).unwrap();
    assert!(RunnerFiles::read_secret(&files.root().join("provider.key")).is_err());
}

#[tokio::test]
async fn aq_proxy_refuses_compressed_access_echo() {
    let root = tempfile::tempdir().unwrap();
    let files = RunnerFiles::create(&root.path().join("runner")).unwrap();
    files
        .write_secret(
            "access.json",
            br#"{"client_id":"runner-only-id","client_secret":"runner-only-secret"}"#,
        )
        .unwrap();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let upstream = format!("http://{}", listener.local_addr().unwrap());
    let peer = tokio::spawn(async move {
        let (mut socket, _) = listener.accept().await.unwrap();
        let mut head = Vec::new();
        while !head.ends_with(b"\r\n\r\n") {
            head.push(socket.read_u8().await.unwrap());
        }
        // gzip({"echo":"runner-only-secret"}), containing no literal token bytes.
        let compressed: &[u8] = &[
            31, 139, 8, 0, 0, 0, 0, 0, 2, 3, 171, 86, 74, 77, 206, 200, 87, 178, 82, 42, 42, 205,
            203, 75, 45, 210, 205, 207, 203, 169, 212, 45, 78, 77, 46, 74, 45, 81, 170, 5, 0, 124,
            32, 219, 126, 29, 0, 0, 0,
        ];
        socket
            .write_all(
                format!(
                    "HTTP/1.1 500 Error\r\nContent-Encoding: gzip\r\nContent-Length: {}\r\n\r\n",
                    compressed.len()
                )
                .as_bytes(),
            )
            .await
            .unwrap();
        socket.write_all(compressed).await.unwrap();
    });
    let proxy = AqApiProxy::development_loopback(&upstream, &files.root().join("access.json"))
        .await
        .unwrap();
    let response = reqwest::Client::new()
        .get(format!("{}/echo", proxy.origin()))
        .header("Accept-Encoding", "gzip")
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 502);
    assert!(response.bytes().await.unwrap().is_empty());
    peer.await.unwrap();
    proxy.stop().await.unwrap();
}

// C6a: the provider key stays in the runner's private file. The agent's harness
// homes, argv and environment carry at most the proxy's attempt-local capability.
#[tokio::test]
async fn api_key_never_visible_to_agent_user() {
    use aq_responses_transport::ResponsesPolicy;
    use aq_session_protocol::{ArtifactRef, Capability, Digest, HarnessSettings, Text};
    use aq_worker::native::adapter::claude::{
        CLAUDE_CLI_VERSION, ClaudePaths, FORBIDDEN_ENVIRONMENT,
    };
    use aq_worker::native::runner::sandbox::{
        confinement::*,
        harness_config::{ClaudeAgentHome, CodexAgentHome},
        responses::RunnerResponsesProxy,
    };
    const KEY: &[u8] = b"sk-provider-only-secret-c6a";
    let root = tempfile::tempdir().unwrap();
    let files = RunnerFiles::create(&root.path().join("runner")).unwrap();
    files.write_secret("provider.key", KEY).unwrap();
    let text = |s: &str| Text::new(s).unwrap();
    let harness = |name: &str, version: &str, confinement: &str| {
        let artifact = ArtifactRef {
            id: text("fixture"),
            version: text("1"),
            digest: Digest::of(b"fixture"),
        };
        HarnessSettings {
            harness: text(name),
            cli_version: text(version),
            adapter: artifact.clone(),
            environment: artifact,
            required_capabilities: ["aq.harness.startup-context", confinement, "aq.harness.stop"]
                .map(|id| Capability {
                    id: text(id),
                    version: 1,
                })
                .to_vec(),
        }
    };
    let model = |provider: &str| aq_session_protocol::ModelSettings {
        provider: text(provider),
        model: text("fixture-model"),
        reasoning: text("medium"),
        max_output_tokens: 100,
        spending_budget: None,
    };
    let mut now = libc::timespec {
        tv_sec: 0,
        tv_nsec: 0,
    };
    assert_eq!(
        unsafe { libc::clock_gettime(libc::CLOCK_BOOTTIME, &mut now) },
        0
    );
    let policy = ResponsesPolicy::new(
        Digest::of(b"fixture-attempt"),
        &model("openai"),
        now.tv_sec as u64 * 1_000_000_000 + now.tv_nsec as u64 + 30_000_000_000,
    )
    .unwrap();
    let proxy = RunnerResponsesProxy::development_loopback(
        policy,
        &files.root().join("provider.key"),
        "http://127.0.0.1:9",
    )
    .await
    .unwrap();
    let port = reqwest::Url::parse(proxy.origin()).unwrap().port().unwrap();
    let endpoint = aq_codex_mapper::LoopbackResponsesEndpoint::new(port).unwrap();
    let capability = proxy.local_capability();
    let separation = AttemptSeparation {
        runner_uid: 1001,
        agent: aq_session_host::users::AgentIdentity {
            uid: 1002,
            gid: 1002,
        },
        credentials_attempt_scoped: true,
    };
    let permit = ExecutionConfinement {
        permit_container: true,
        require_native: false,
    };
    let vm = QualificationRecord::from_native_probe(
        "aq-rust",
        "vm",
        BackendBoundary::Container,
        NativeProbe {
            outside_write_refused: true,
        },
        "fixture",
    );
    let docker = QualificationRecord::docker_local("aq-rust");
    let workspace = root.path().join("work");
    let contains = |hay: &[u8]| hay.windows(KEY.len()).any(|w| w == KEY);
    let assert_clean = |home: &std::path::Path,
                        args: &[String],
                        env: &std::collections::BTreeMap<String, String>| {
        for name in FORBIDDEN_ENVIRONMENT {
            assert!(!env.contains_key(*name), "{name} in agent env");
        }
        assert!(
            !args
                .iter()
                .chain(env.values())
                .any(|v| contains(v.as_bytes()))
        );
        assert!(
            !args
                .iter()
                .chain(env.values())
                .any(|v| v.contains("provider.key"))
        );
        assert_eq!(std::fs::metadata(home).unwrap().mode() & 0o777, 0o700);
        for entry in std::fs::read_dir(home).unwrap() {
            let path = entry.unwrap().path();
            assert_eq!(std::fs::metadata(&path).unwrap().mode() & 0o777, 0o600);
            assert!(!contains(&std::fs::read(&path).unwrap()), "{path:?}");
        }
    };
    for (mode, record) in [
        (ConfinementMode::Native, &vm),
        (ConfinementMode::Container, &docker),
    ] {
        let authorized = authorize(mode, &permit, record, &separation).unwrap();
        let confinement = match mode {
            ConfinementMode::Native => "aq.harness.confined-worktree",
            ConfinementMode::Container => "aq.harness.container-confinement",
        };
        // Codex: private CODEX_HOME, proxy provider, capability-only auth.json.
        let codex_home = root.path().join(format!("codex-{}", mode.as_str()));
        let codex_harness = harness("codex", aq_codex_mapper::CODEX_CLI_VERSION, confinement);
        let openai = model("openai");
        let recipe = CodexAgentHome::install(
            &authorized,
            aq_codex_mapper::Settings {
                harness: &codex_harness,
                model: &openai,
                terminal_input_enabled: false,
            },
            aq_codex_mapper::Paths {
                executable: std::path::Path::new("/opt/aq/bin/codex"),
                workspace: &workspace,
                private_home: &codex_home,
            },
            endpoint,
            &capability,
        )
        .unwrap();
        assert_eq!(
            recipe.environment.keys().collect::<Vec<_>>(),
            ["CODEX_HOME"]
        );
        assert!(
            recipe
                .arguments
                .iter()
                .any(|a| a == "model_provider=\"aq-runner-proxy\"")
        );
        assert!(
            recipe
                .arguments
                .iter()
                .any(|a| a == "cli_auth_credentials_store=\"file\"")
        );
        assert!(
            !recipe
                .arguments
                .iter()
                .any(|a| a.starts_with("openai_base_url="))
        );
        assert_clean(&codex_home, &recipe.arguments, &recipe.environment);
        let auth: serde_json::Value =
            serde_json::from_slice(&std::fs::read(codex_home.join("auth.json")).unwrap()).unwrap();
        assert_eq!(
            auth,
            serde_json::json!({ "OPENAI_API_KEY": std::str::from_utf8(&capability).unwrap() })
        );
        // The provider key itself is refused as an auth.json value.
        assert!(
            CodexAgentHome::install(
                &authorized,
                aq_codex_mapper::Settings {
                    harness: &codex_harness,
                    model: &openai,
                    terminal_input_enabled: false
                },
                aq_codex_mapper::Paths {
                    executable: std::path::Path::new("/opt/aq/bin/codex"),
                    workspace: &workspace,
                    private_home: &root.path().join(format!("codex-key-{}", mode.as_str())),
                },
                endpoint,
                KEY,
            )
            .is_err()
        );
        // A home inside the workspace would let workspace-write rewrite auth.json.
        let nested = workspace.join(format!("codex-{}", mode.as_str()));
        assert!(
            CodexAgentHome::install(
                &authorized,
                aq_codex_mapper::Settings {
                    harness: &codex_harness,
                    model: &openai,
                    terminal_input_enabled: false
                },
                aq_codex_mapper::Paths {
                    executable: std::path::Path::new("/opt/aq/bin/codex"),
                    workspace: &workspace,
                    private_home: &nested,
                },
                endpoint,
                &capability,
            )
            .is_err()
                && !nested.exists()
        );
        // Claude: subscription only; settings.json and nothing else.
        let claude_home = root.path().join(format!("claude-{}", mode.as_str()));
        let recipe = ClaudeAgentHome::install(
            &authorized,
            &harness("claude", CLAUDE_CLI_VERSION, confinement),
            &model("anthropic"),
            ClaudePaths {
                executable: std::path::Path::new("/opt/aq/bin/claude"),
                workspace: &workspace,
                private_home: &claude_home,
            },
        )
        .unwrap();
        assert_clean(&claude_home, &recipe.arguments, &recipe.environment);
        let listed: Vec<_> = std::fs::read_dir(&claude_home)
            .unwrap()
            .map(|e| e.unwrap().file_name())
            .collect();
        assert_eq!(listed, ["settings.json"]);
        let settings: serde_json::Value =
            serde_json::from_slice(&std::fs::read(claude_home.join("settings.json")).unwrap())
                .unwrap();
        assert!(settings.get("apiKeyHelper").is_none() && settings.get("env").is_none());
        // OpenCode (I3): container only; capability-only `openai` API entry.
        use aq_worker::native::adapter::opencode::{self, OpenCodePaths, OpenCodeRoute};
        use aq_worker::native::runner::sandbox::harness_config::OpenCodeAgentHome;
        let oc_home = root.path().join(format!("opencode-{}", mode.as_str()));
        let installed = OpenCodeAgentHome::install(
            &authorized,
            &harness("opencode", opencode::OPENCODE_CLI_VERSION, confinement),
            &openai,
            OpenCodePaths {
                executable: std::path::Path::new("/usr/local/bin/opencode"),
                workspace: &workspace,
                private_home: &oc_home,
            },
            OpenCodeRoute::ResponsesProxy { port },
            Some(&capability),
        );
        if mode == ConfinementMode::Native {
            assert!(installed.is_err() && !oc_home.exists());
            continue;
        }
        let recipe = installed.unwrap();
        for name in opencode::FORBIDDEN_ENVIRONMENT {
            assert!(
                !recipe.environment.contains_key(*name),
                "{name} in agent env"
            );
        }
        assert!(
            !recipe
                .arguments
                .iter()
                .chain(recipe.environment.values())
                .any(|v| contains(v.as_bytes()) || v.contains("aq-responses-"))
        );
        let mode_of = |p: &std::path::Path| std::fs::metadata(p).unwrap().mode() & 0o777;
        assert_eq!(mode_of(&oc_home), 0o700);
        assert_eq!(mode_of(&oc_home.join("data/opencode")), 0o700);
        for file in ["opencode.json", opencode::AUTH_FILE] {
            let path = oc_home.join(file);
            assert_eq!(mode_of(&path), 0o600);
            assert!(!contains(&std::fs::read(&path).unwrap()), "{path:?}");
        }
        let auth: serde_json::Value =
            serde_json::from_slice(&std::fs::read(oc_home.join(opencode::AUTH_FILE)).unwrap())
                .unwrap();
        assert_eq!(
            auth,
            serde_json::json!({"openai":{"type":"api",
                "key": std::str::from_utf8(&capability).unwrap()}})
        );
        // A provider-shaped key is refused before anything is created.
        let refused = root.path().join("opencode-refused");
        assert!(
            OpenCodeAgentHome::install(
                &authorized,
                &harness("opencode", opencode::OPENCODE_CLI_VERSION, confinement),
                &openai,
                OpenCodePaths {
                    executable: std::path::Path::new("/usr/local/bin/opencode"),
                    workspace: &workspace,
                    private_home: &refused,
                },
                OpenCodeRoute::ResponsesProxy { port },
                Some(KEY),
            )
            .is_err()
                && !refused.exists()
        );
    }
    // The key never left the runner's private file.
    assert_eq!(
        &*RunnerFiles::read_secret(&files.root().join("provider.key")).unwrap(),
        KEY
    );
    proxy.stop().await.unwrap();
}

// I2: the Gemini home holds only system settings; the agent sees only the
// proxy capability, and a native confinement is refused before any file exists.
#[tokio::test]
async fn gemini_home_carries_only_the_proxy_capability() {
    use aq_session_protocol::{ArtifactRef, Capability, Digest, HarnessSettings, Text};
    use aq_worker::native::adapter::gemini::{
        FORBIDDEN_ENVIRONMENT, GEMINI_CLI_VERSION, GeminiAuth, GeminiPaths,
    };
    use aq_worker::native::runner::sandbox::{
        confinement::*, gemini_proxy::GeminiApiProxy, harness_config::GeminiAgentHome,
    };
    const KEY: &str = "AIzaGeminiProviderOnlySecret_i2";
    let root = tempfile::tempdir().unwrap();
    let files = RunnerFiles::create(&root.path().join("runner")).unwrap();
    files.write_secret("provider.key", KEY.as_bytes()).unwrap();
    let proxy = GeminiApiProxy::development_loopback(
        "http://127.0.0.1:9",
        &files.root().join("provider.key"),
    )
    .await
    .unwrap();
    let text = |s: &str| Text::new(s).unwrap();
    let artifact = ArtifactRef {
        id: text("fixture"),
        version: text("1"),
        digest: Digest::of(b"fixture"),
    };
    let harness = |confinement: &str| HarnessSettings {
        harness: text("gemini"),
        cli_version: text(GEMINI_CLI_VERSION),
        adapter: artifact.clone(),
        environment: artifact.clone(),
        required_capabilities: ["aq.harness.startup-context", confinement, "aq.harness.stop"]
            .map(|id| Capability {
                id: text(id),
                version: 1,
            })
            .to_vec(),
    };
    let model = aq_session_protocol::ModelSettings {
        provider: text("google"),
        model: text("gemini-2.5-flash"),
        reasoning: text("medium"),
        max_output_tokens: 100,
        spending_budget: None,
    };
    let separation = AttemptSeparation {
        runner_uid: 1001,
        agent: aq_session_host::users::AgentIdentity {
            uid: 1002,
            gid: 1002,
        },
        credentials_attempt_scoped: true,
    };
    let permit = ExecutionConfinement {
        permit_container: true,
        require_native: false,
    };
    let workspace = root.path().join("work");
    let paths = |home: &'static str| GeminiPaths {
        node: std::path::Path::new("/opt/aq/node/bin/node"),
        script: std::path::Path::new("/opt/aq/harnesses/gemini/bundle/gemini.js"),
        workspace: Box::leak(workspace.clone().into_boxed_path()),
        private_home: Box::leak(root.path().join(home).into_boxed_path()),
    };
    let auth = GeminiAuth::ApiKeyProxy {
        origin: proxy.origin(),
        capability: proxy.local_capability(),
    };
    let container = authorize(
        ConfinementMode::Container,
        &permit,
        &QualificationRecord::docker_local("aq-rust"),
        &separation,
    )
    .unwrap();
    let recipe = GeminiAgentHome::install(
        &container,
        &harness("aq.harness.container-confinement"),
        &model,
        paths("gemini-home"),
        auth,
    )
    .unwrap();
    let home = root.path().join("gemini-home");
    assert_eq!(std::fs::metadata(&home).unwrap().mode() & 0o777, 0o700);
    let listed: Vec<_> = std::fs::read_dir(&home)
        .unwrap()
        .map(|e| e.unwrap().file_name())
        .collect();
    assert_eq!(listed, ["system-settings.json"]);
    let settings = home.join("system-settings.json");
    assert_eq!(std::fs::metadata(&settings).unwrap().mode() & 0o777, 0o600);
    let all: Vec<&str> = recipe
        .arguments
        .iter()
        .chain(recipe.environment.values())
        .map(String::as_str)
        .collect();
    assert!(
        !all.iter()
            .any(|v| v.contains(KEY) || v.contains("provider.key"))
    );
    assert!(
        !String::from_utf8(std::fs::read(&settings).unwrap())
            .unwrap()
            .contains(KEY)
    );
    assert_eq!(
        recipe.environment["GEMINI_API_KEY"],
        proxy.local_capability()
    );
    for name in FORBIDDEN_ENVIRONMENT {
        if !matches!(*name, "GEMINI_API_KEY" | "GOOGLE_GEMINI_BASE_URL") {
            assert!(!recipe.environment.contains_key(*name), "{name}");
        }
    }
    let native = authorize(
        ConfinementMode::Native,
        &permit,
        &QualificationRecord::from_native_probe(
            "aq-rust",
            "vm",
            BackendBoundary::Container,
            NativeProbe {
                outside_write_refused: true,
            },
            "fixture",
        ),
        &separation,
    )
    .unwrap();
    assert!(
        GeminiAgentHome::install(
            &native,
            &harness("aq.harness.confined-worktree"),
            &model,
            paths("gemini-native"),
            auth,
        )
        .is_err()
    );
    assert!(!root.path().join("gemini-native").exists());
    proxy.stop().await.unwrap();
}
