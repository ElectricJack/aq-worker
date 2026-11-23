fn binary_source_fixture(
    fixture: &source_repository::Fixture,
    root: &Path,
    git_root: &Path,
    binding: SourceBinding,
) {
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .unwrap();
    runtime.block_on(async {
        let (server, url) = start_source_cli(binding).await;
        let config = write_source_config(root, url);
        let state = root.join("cli-journal");
        let output = launch_source_cli(fixture, git_root, &config, &state).await;
        assert_pending_stdout(output);
        assert_pending_journal(&state);
        server.abort();
    });
}

async fn start_source_cli(binding: SourceBinding) -> (tokio::task::JoinHandle<()>, String) {
    use axum::{Json, Router, extract::State, routing::post};
    let mut seed = offer();
    seed.attempt.attempt_id = Uuid::new_v4();
    seed.attempt.session_id = Uuid::new_v4();
    seed.spec.bindings.as_mut().unwrap().source = Some(binding);
    seed.spec = seed.spec.seal();
    let shared = Arc::new(Mutex::new(FakeCoordinator::new(seed, 100)));
    async fn register(
        State(p): State<Arc<Mutex<FakeCoordinator>>>,
        Json(r): Json<RegisterRequest>,
    ) -> Json<RegisterResponse> {
        assert!(r.capabilities.contains(&SOURCE_CAPABILITY.into()));
        assert!(!r.capabilities.contains(&SOURCE_TRANSFER_CAPABILITY.into()));
        {
            let mut p = p.lock().unwrap();
            p.offer.identity = r.identity.clone();
            p.active_identity = r.identity.clone();
        }
        Json(RegisterResponse {
            identity: r.identity,
            protocol_version: 2,
            capabilities: r.capabilities,
        })
    }
    async fn poll(
        State(p): State<Arc<Mutex<FakeCoordinator>>>,
        Json(_): Json<PollRequest>,
    ) -> Json<PollResponse> {
        Json(PollResponse {
            server_time_ms: 100,
            offers: vec![p.lock().unwrap().offer.clone()],
            directives: vec![],
        })
    }
    async fn accept(
        State(p): State<Arc<Mutex<FakeCoordinator>>>,
        Json(r): Json<AcceptRequest>,
    ) -> Json<AcceptResponse> {
        Json(p.lock().unwrap().accept(&r).unwrap())
    }
    async fn observe(
        State(p): State<Arc<Mutex<FakeCoordinator>>>,
        Json(r): Json<Observation>,
    ) -> Json<Ack> {
        Json(p.lock().unwrap().observe(&r).unwrap())
    }
    let app = Router::new()
        .route("/v2/worker/register", post(register))
        .route("/v2/worker/poll", post(poll))
        .route("/v2/worker/accept", post(accept))
        .route("/v2/worker/observe", post(observe))
        .with_state(shared);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    (server, url)
}

fn write_source_config(root: &Path, url: String) -> std::path::PathBuf {
    let config = root.join("cli.toml");
    aq_worker::config::WorkerConfig {
        server_url: url,
        node_id: Uuid::from_u128(1),
        credential: "synthetic-node-secret".into(),
        cf_client_id: None,
        cf_client_secret: None,
    }
    .save(&config)
    .unwrap();
    config
}

async fn launch_source_cli(
    fixture: &source_repository::Fixture,
    git_root: &Path,
    config: &Path,
    state: &Path,
) -> std::process::Output {
    tokio::process::Command::new(env!("CARGO_BIN_EXE_aq-worker"))
        .arg("--config")
        .arg(config)
        .arg("run-execution")
        .arg("--state-dir")
        .arg(state)
        .arg("--fixture-agent")
        .arg(env!("CARGO_BIN_EXE_aq-fixture-agent"))
        .arg("--aq-git-binary")
        .arg(&fixture.exe)
        .arg("--aq-git-sha256")
        .arg(&fixture.pin)
        .arg("--aq-git-state-dir")
        .arg(git_root)
        .args(["--exit-after-result", "--idle-timeout-secs", "5"])
        .output()
        .await
        .unwrap()
}

fn assert_pending_stdout(output: std::process::Output) {
    assert!(
        output.status.success(),
        "{} {}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let text = String::from_utf8(output.stdout).unwrap();
    assert!(text.contains("source_pending"));
    assert!(!text.contains("result_acknowledged"));
    assert!(!text.contains("synthetic-node-secret"));
}

fn assert_pending_journal(state: &Path) {
    let journal: aq_worker::journal::State =
        serde_json::from_slice(&std::fs::read(state.join("journal.json")).unwrap()).unwrap();
    let r = journal.attempts.values().next().unwrap();
    assert_eq!(r.stop_certainty, StopCertainty::ConfirmedStopped);
    assert_eq!(r.phase, Phase::Reporting);
    let source = r.source.as_ref().unwrap();
    assert!(source.checkpoint.is_some() && source.exported.is_some() && source.receipt.is_none());
    assert!(source.workspace.as_ref().unwrap().path.exists());
    assert!(!r.report.as_ref().unwrap().claimed_success);
    assert_eq!(r.report.as_ref().unwrap().artifacts.len(), 1);
    assert!(r.result.is_none());
}
