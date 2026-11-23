//! Explicit CLI integration: cold base, concurrent control and retained handoff.
use super::*;
use axum::{
    Router,
    body::{Body, to_bytes},
    extract::State,
    http::{Request, StatusCode},
    response::{IntoResponse, Response},
};
use std::{
    collections::BTreeMap,
    sync::atomic::{AtomicBool, AtomicUsize, Ordering},
};
use tokio::sync::Notify;
#[path = "source_cli/control.rs"]
mod control;
#[path = "source_cli/download.rs"]
mod download;
#[path = "source_cli/outcome.rs"]
mod outcome;
#[path = "source_cli/report.rs"]
mod report;
use super::source_repository::{self as repository, Fixture};
#[path = "source_cli/upload.rs"]
mod upload;
struct Peer {
    fixture: Arc<Fixture>,
    coordinator: Mutex<FakeCoordinator>,
    grant: Mutex<Option<Grant>>,
    upload: Mutex<Option<(TransferManifest, TransferStatus)>>,
    operations: Mutex<BTreeMap<Uuid, (Value, Value)>>,
    held: AtomicBool,
    release: Notify,
    control_polls: AtomicUsize,
    directive_acked: AtomicBool,
    cancel: bool,
    report: Mutex<Option<ResultReport>>,
    report_lost: AtomicBool,
}
fn json_response(v: impl serde::Serialize) -> Response {
    axum::Json(v).into_response()
}
async fn endpoint(State(s): State<Arc<Peer>>, r: Request<Body>) -> Response {
    assert_eq!(r.headers()["authorization"], "Bearer fixture-cli-token");
    let path = r.uri().path().to_owned();
    let query = r.uri().query().unwrap_or("").to_owned();
    let body = to_bytes(r.into_body(), MAX_CHUNK_BYTES + 65536)
        .await
        .unwrap();
    if path.starts_with("/v1/execution/sources/base/") {
        return s.base_download(&path, &query).await;
    }
    let has_raw_chunk = path.contains("/sources/") && !query.is_empty();
    let value: Value = if body.is_empty() || has_raw_chunk {
        Value::Null
    } else {
        serde_json::from_slice(&body).unwrap()
    };
    match path.as_str() {
        "/v2/worker/register" => s.register(value),
        "/v2/worker/poll" => s.poll(value),
        "/v2/worker/accept" => s.accept(value),
        "/v2/worker/renew" => s.renew(value),
        "/v2/worker/observe" => s.observe(value),
        "/v2/worker/directive-ack" => s.ack_directive(value),
        "/v2/worker/reconcile" => s.reconcile(value),
        "/v2/worker/result" => s.result(value),
        "/v1/execution/sources/begin" => s.begin(value),
        "/v1/execution/sources/finalize" => s.finalize(value),
        _ if path.starts_with("/v1/execution/sources/") && !query.is_empty() => {
            s.chunk(&path, &query, &body)
        }
        _ => panic!("unexpected CLI request {path}: {value}"),
    }
}

impl Peer {
    fn new(fixture: Arc<Fixture>, cancel: bool) -> Arc<Self> {
        let mut seed = offer();
        seed.spec.bindings.as_mut().unwrap().source = Some(fixture.binding.clone());
        seed.spec
            .required_capabilities
            .push(SOURCE_TRANSFER_CAPABILITY.into());
        seed.spec = seed.spec.seal();
        Arc::new(Self {
            fixture,
            coordinator: Mutex::new(FakeCoordinator::new(seed, 100)),
            grant: Mutex::new(None),
            upload: Mutex::new(None),
            operations: Mutex::new(BTreeMap::new()),
            held: AtomicBool::new(false),
            release: Notify::new(),
            control_polls: AtomicUsize::new(0),
            directive_acked: AtomicBool::new(false),
            cancel,
            report: Mutex::new(None),
            report_lost: AtomicBool::new(false),
        })
    }
}
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "requires explicit AQ_GIT_TEST_BINARY pinned public binary; actual CLI HTTP integration"]
async fn pinned_git_cli_cold_download_handoff_and_held_download_cancel() {
    let exe = std::env::var_os("AQ_GIT_TEST_BINARY").expect("set pinned executable");
    let exe = Path::new(&exe);
    let temp = TempDir::new().unwrap();
    let fixture = repository::create_fixture(exe, temp.path());
    let worker = temp.path().join("worker");
    repository::register_worker(exe, &worker, fixture.rid);
    run_case(fixture.clone(), temp.path(), &worker, false).await;
    let cancelled_worker = temp.path().join("worker-cancel");
    repository::register_worker(exe, &cancelled_worker, fixture.rid);
    run_case(fixture, temp.path(), &cancelled_worker, true).await;
}
struct Case {
    peer: Arc<Peer>,
    config_path: std::path::PathBuf,
    state: std::path::PathBuf,
    worker: std::path::PathBuf,
    server: tokio::task::JoinHandle<()>,
}
impl Case {
    async fn start(fixture: Arc<Fixture>, root: &Path, worker: &Path, cancel: bool) -> Self {
        std::fs::write(worker.join("artifact_limits.json"),serde_json::to_vec(&json!({"max_bundle_bytes":67108864,"max_expanded_bytes":536870912,"disk_reserve_bytes":16777216})).unwrap()).unwrap();
        let peer = Peer::new(fixture, cancel);
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let config = aq_worker::config::WorkerConfig {
            server_url: format!("http://{}", listener.local_addr().unwrap()),
            node_id: Uuid::from_u128(1),
            credential: "fixture-cli-token".into(),
            cf_client_id: None,
            cf_client_secret: None,
        };
        let app = Router::new().fallback(endpoint).with_state(peer.clone());
        let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let label = if cancel { "cancel" } else { "complete" };
        let config_path = root.join(format!("cli-{label}.toml"));
        config.save(&config_path).unwrap();
        let state = root.join(format!("cli-{label}-journal"));
        Self {
            peer,
            config_path,
            state,
            worker: worker.to_owned(),
            server,
        }
    }

    fn journal(&self) -> aq_worker::journal::State {
        serde_json::from_slice(&std::fs::read(self.state.join("journal.json")).unwrap()).unwrap()
    }

    async fn launch(&self) -> std::process::Output {
        launch_cli(
            &self.peer.fixture,
            &self.config_path,
            &self.state,
            &self.worker,
        )
        .await
    }
}
async fn run_case(fixture: Arc<Fixture>, root: &Path, worker: &Path, cancel: bool) {
    let case = Case::start(fixture, root, worker, cancel).await;
    let output = case.launch().await;
    let journal = case.journal();
    assert_eq!(
        journal.attempts.len(),
        1,
        "no new offers admitted during bulk transfer"
    );
    let r = journal.attempts.values().next().unwrap();
    assert!(case.peer.held.load(Ordering::SeqCst));
    assert!(
        case.peer.control_polls.load(Ordering::SeqCst) > 0,
        "held download must not block control"
    );
    assert!(!String::from_utf8_lossy(&output.stderr).contains("fixture-cli-token"));
    if cancel {
        let incoming = case.assert_cancelled(r, &output);
        case.assert_lost_release(&incoming).await;
        case.assert_recovered(&incoming).await;
    } else {
        case.assert_completed(r, &output);
    }
    case.server.abort();
}

async fn launch_cli(
    fixture: &Fixture,
    config_path: &Path,
    state: &Path,
    worker: &Path,
) -> std::process::Output {
    tokio::time::timeout(
        std::time::Duration::from_secs(30),
        tokio::process::Command::new(env!("CARGO_BIN_EXE_aq-worker"))
            .arg("--config")
            .arg(config_path)
            .kill_on_drop(true)
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
            .arg(worker)
            .args([
                "--source-transfer",
                "--exit-after-result",
                "--idle-timeout-secs",
                "10",
            ])
            .output(),
    )
    .await
    .expect("CLI timeout")
    .unwrap()
}
