//! Real pinned Codex 0.166 + real tmux; synthetic admission and loopback provider.
//! These observations grant no production runtime/model qualification.
use super::*;
use crate::native::{
    adapter::{
        AdapterManifest, ExpectedAdapter, FilePin, HandoffKind,
        codex::CodexExecProfile,
        named_exec::{NamedExecInstallation, NamedExecKind, NamedExecManifest},
        native_exec::{NativeExecInstallation, NativeExecKind, NativeExecManifest},
    },
    launch::NativeCodexHostProfile,
    runner::{CodexExecutor, NativeExecutor},
};
use aq_session_host::observed::{NamedReadonlyEnvironment, NamedReadonlyExecPool, NativeExecPool};
use axum::http::{HeaderMap, StatusCode};

#[path = "codex_process_guard.rs"]
mod confinement;

const CODEX_SHA256: &str = "b8a11fedf24b2dfb59a70c9e52b030f9a508dcd49cd2dd0d41956f4c785156b8";
const CODEX_BYTES: u64 = 319_346_888;
const FINAL_TEXT: &str = "aq-local-response-proof";
struct Provider {
    origin: String,
    requests: Arc<Mutex<Vec<Value>>>,
    entered: Arc<Notify>,
    release: Arc<Semaphore>,
    task: tokio::task::JoinHandle<()>,
}
impl Drop for Provider {
    fn drop(&mut self) {
        self.task.abort();
    }
}
fn completed(model: &str) -> String {
    crate::finite_responses_fixture::completed(model, FINAL_TEXT)
}
fn tool_call(model: &str) -> String {
    crate::finite_responses_fixture::exec_command(
        model,
        "resp_aq_tool",
        "fc_aq_local",
        "call_aq_local",
        "/usr/bin/printf aq-local-tool-proof | /usr/bin/tee aq-local-tool-proof.txt",
    )
}
async fn provider() -> Provider {
    provider_mode(false).await
}
async fn provider_mode(tool_flow: bool) -> Provider {
    confinement::require_confined_fixture().expect("refuse unconfined real CLI qualification");
    let requests = Arc::new(Mutex::new(Vec::new()));
    let entered = Arc::new(Notify::new());
    let release = Arc::new(Semaphore::new(0));
    let saved = requests.clone();
    let signal = entered.clone();
    let gate = release.clone();
    let app = Router::new().route(
        "/v1/responses",
        post(move |headers: HeaderMap, body: axum::body::Bytes| {
            let saved = saved.clone();
            let signal = signal.clone();
            let gate = gate.clone();
            async move {
                assert_eq!(headers["authorization"], "Bearer synthetic-upstream-key");
                assert!(
                    !headers.contains_key("content-encoding"),
                    "fixture expects uncompressed JSON"
                );
                let request = crate::finite_responses_fixture::request(
                    &body,
                    None,
                    4096,
                    &["synthetic-key", "synthetic-upstream-key", "synthetic-node-credential"],
                ).unwrap();
                let model = request["model"].as_str().unwrap().to_owned();
                eprintln!("actual CLI request structure: {}", json!({
                    "input_types":request["input"].as_array().unwrap().iter().map(|v|v["type"].clone()).collect::<Vec<_>>(),
                    "tools":request["tools"].as_array().unwrap().iter().map(|v|json!({"type":v["type"],"name":v["name"]})).collect::<Vec<_>>()
                }));
                let index = {
                    let mut all = saved.lock().unwrap();
                    let index = all.len();
                    assert!(index < if tool_flow {2} else {1}, "finite provider request count");
                    all.push(request);
                    index
                };
                eprintln!("fixture provider request index={index} boot_ns={}", crate::native::http::boot_ns().unwrap());
                signal.notify_one();
                let _permit = gate.acquire().await.unwrap();
                if index == 0 {record_observation(&saved.lock().unwrap()[0], body.len(), tool_flow);}
                (
                    StatusCode::OK,
                    [
                        ("content-type", "text/event-stream"),
                        ("cache-control", "no-store"),
                    ],
                    if tool_flow && index == 0 {tool_call(&model)} else {completed(&model)},
                )
            }
        }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let origin = format!("http://{}/", listener.local_addr().unwrap());
    let task = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    Provider {
        origin,
        requests,
        entered,
        release,
        task,
    }
}
fn record_observation(request: &Value, upstream_bytes: usize, tool_flow: bool) {
    let input = request["input"].as_array().unwrap();
    let implicit = &input[..input.len() - 1];
    let tools = &request["tools"];
    let summary = json!({
        "schema_version":1,
        "authority":"observation only; no runtime/model/tokenizer qualification",
        "codex_sha256":CODEX_SHA256,
        "model":request["model"],
        "max_output_tokens":request["max_output_tokens"],
        "upstream_request_bytes":upstream_bytes,
        "request_fields":request.as_object().unwrap().keys().collect::<Vec<_>>(),
        "inputs":input.iter().map(|v| json!({"type":v["type"],"role":v["role"],
            "encoded_bytes":serde_json::to_vec(v).unwrap().len(),
            "text_bytes":v["content"].as_array().unwrap().iter().map(|c|c["text"].as_str().unwrap().len()).sum::<usize>()})).collect::<Vec<_>>(),
        "sealed_payload_bytes":b"instructions\n\ntask-context".len(),
        "implicit_input_encoded_bytes":serde_json::to_vec(implicit).unwrap().len(),
        "implicit_input_digest":Digest::of(&serde_json::to_vec(implicit).unwrap()),
        "tool_definition_bytes":serde_json::to_vec(tools).unwrap().len(),
        "tool_definition_digest":Digest::of(&serde_json::to_vec(tools).unwrap()),
        "tools":tools.as_array().unwrap().iter().map(|v|json!({"type":v["type"],"name":v["name"]})).collect::<Vec<_>>()
    });
    eprintln!("actual CLI request observation: {summary}");
    if let Some(directory) = std::env::var_os("AQ_NATIVE_CODEX_EVIDENCE_DIR") {
        use std::io::Write;
        use std::os::unix::fs::OpenOptionsExt;
        let root = PathBuf::from(directory);
        assert_eq!(root.canonicalize().unwrap(), root);
        for (name, value) in [
            ("request-observation.json", summary),
            (
                "implicit-inputs.observed.json",
                Value::Array(implicit.to_vec()),
            ),
            ("tools.observed.json", tools.clone()),
        ] {
            let mut f = fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .mode(0o600)
                .open(root.join(if tool_flow {
                    format!("tool-flow-{name}")
                } else {
                    name.to_owned()
                }))
                .unwrap();
            f.write_all(&serde_json::to_vec_pretty(&value).unwrap())
                .unwrap();
            f.sync_all().unwrap();
        }
    }
}
fn installed_pin(binary: &str, hash: &str, id: &str) -> FilePin {
    let p = pin(binary, hash, id);
    FilePin {
        path: p.path,
        artifact: p.artifact,
    }
}
enum FixtureImage {
    Sealed,
    NamedReadonly,
}
enum FixtureInstallation {
    Sealed(NativeExecInstallation),
    NamedReadonly(NamedExecInstallation),
}
fn preflight_fixture(kind: FixtureImage) -> FixtureInstallation {
    // Independently selected fixture pins are installed before any admission
    // deadline. A named case never retries/falls back through a sealed owner.
    let original = offer();
    let m = &original.offered.offer.manifest;
    let runtime = qualification();
    let executable = installed_pin(
        "AQ_NATIVE_CODEX_BINARY",
        "AQ_NATIVE_CODEX_SHA256",
        "aq.fixture.codex-0.166.0",
    );
    assert_eq!(executable.artifact.digest.as_str(), CODEX_SHA256);
    assert_eq!(fs::metadata(&executable.path).unwrap().len(), CODEX_BYTES);
    let installation = AdapterManifest {
        schema_version: 1,
        adapter: m.harness.adapter.clone(),
        runtime: runtime.runtime.clone(),
        qualification: runtime.qualification.clone(),
        renderer: original.startup.renderer_qualification.clone(),
        handoff: HandoffKind::OpaquePrivateFileV1,
        executable,
        helper: installed_pin(
            "AQ_NATIVE_HOST_BINARY",
            "AQ_NATIVE_HOST_SHA256",
            "aq.fixture.source-host",
        ),
        tmux: installed_pin(
            "AQ_NATIVE_TMUX_BINARY",
            "AQ_NATIVE_TMUX_SHA256",
            "aq.fixture.tmux-3.4",
        ),
    };
    match kind {
        FixtureImage::Sealed => {
            let manifest = NativeExecManifest {
                schema_version: 1,
                profile: NativeExecKind::Codex0166ExecStdinV1,
                executable_bytes: CODEX_BYTES,
                installation,
            };
            let pool = NativeExecPool::new(512 * 1024 * 1024, 1).unwrap();
            FixtureInstallation::Sealed(
                NativeExecInstallation::open(
                    manifest.clone(),
                    ExpectedAdapter {
                        manifest_digest: &manifest.digest().unwrap(),
                        adapter: &m.harness.adapter,
                        runtime: &runtime.runtime,
                        qualification: &runtime.qualification,
                        renderer: &original.startup.renderer_qualification,
                    },
                    &pool,
                )
                .unwrap(),
            )
        }
        FixtureImage::NamedReadonly => {
            let manifest = NamedExecManifest {
                schema_version: 1,
                profile: NamedExecKind::Codex0166NamedReadonlyExecStdinV1,
                executable_bytes: CODEX_BYTES,
                environment: m.harness.environment.clone(),
                installation,
            };
            let environment =
                NamedReadonlyEnvironment::inspect(m.harness.environment.clone()).unwrap();
            let pool = NamedReadonlyExecPool::new(512 * 1024 * 1024, 1, environment).unwrap();
            FixtureInstallation::NamedReadonly(
                NamedExecInstallation::open(
                    manifest.clone(),
                    ExpectedAdapter {
                        manifest_digest: &manifest.digest().unwrap(),
                        adapter: &m.harness.adapter,
                        runtime: &runtime.runtime,
                        qualification: &runtime.qualification,
                        renderer: &original.startup.renderer_qualification,
                    },
                    &m.harness.environment,
                    &pool,
                )
                .unwrap(),
            )
        }
    }
}
async fn codex_prepared(
    peer: &Provider,
    kind: FixtureImage,
) -> (
    Inputs,
    GitPeer,
    DurablePhaseExecution,
    Arc<NativeCodexHostProfile>,
    aq_session_protocol::TerminalPolicy,
) {
    eprintln!(
        "fixture preflight begin boot_ns={}",
        crate::native::http::boot_ns().unwrap()
    );
    let installed = preflight_fixture(kind);
    eprintln!(
        "fixture preflight complete boot_ns={}",
        crate::native::http::boot_ns().unwrap()
    );
    let export = synthetic();
    let source_manifest = export.manifest.clone();
    let h = Inputs::new_phase_codex(export).await;
    h.host_report_mode.store(1, Ordering::SeqCst);
    h.grant_window_ms.store(20000, Ordering::SeqCst);
    let git = GitPeer::new(h.root.clone(), source_manifest);
    let snapshot = h.pipeline().snapshot().await.unwrap();
    let a = &snapshot.attempts[&h.id];
    let m = &a.reservation.offer.offered.offer.manifest;
    let renderer = a.reservation.offer.startup.renderer_qualification.clone();
    let mapper = CodexExecProfile {
        harness: m.harness.clone(),
        model: m.model.clone(),
        permissions_digest: crate::native::journal::canonical_digest(&m.permissions).unwrap(),
        renderer,
        confinement: None,
    };
    let policy = m.terminal.clone();
    let host_root = h._dir.path().join("codex-host");
    fs::create_dir(&host_root).unwrap();
    fs::set_permissions(&host_root, fs::Permissions::from_mode(0o700)).unwrap();
    let profile = match installed {
        FixtureInstallation::Sealed(installation) => {
            NativeCodexHostProfile::new(host_root, installation, mapper, 160, 40)
        }
        FixtureInstallation::NamedReadonly(installation) => {
            NativeCodexHostProfile::new_named(host_root, installation, mapper, 160, 40)
        }
    }
    .unwrap();
    let inputs = phase_join(&h, git.client.clone()).await;
    // The finite Git socket peer provisions a placeholder workspace. Codex exec
    // requires an actual repository, so initialize only this owned fixture path.
    assert!(
        std::process::Command::new("/usr/bin/git")
            .args(["init", "--quiet"])
            .arg(&inputs.workspace().workspace().path)
            .env_clear()
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .status()
            .unwrap()
            .success()
    );
    let state = h.pipeline().snapshot().await.unwrap();
    let intent = state.attempts[&h.id].phase_acceptance.as_ref().unwrap();
    let observed = h
        .client
        .prepare_execution(
            h.proposed.as_ref().unwrap(),
            &intent.request,
            h.phase.as_ref().unwrap(),
            inputs.request(),
            None,
        )
        .await
        .unwrap();
    let PhaseExecutionCommit::Usable(grant) = h
        .pipeline()
        .record_phase_execution(inputs, observed)
        .await
        .unwrap()
    else {
        panic!("actual CLI fixture grant expired")
    };
    let profile = Arc::new(profile.with_development_provider(&peer.origin).unwrap());
    (h, git, grant, profile, policy)
}

async fn begin_execution(
    peer: &Provider,
    slow_output: bool,
    delayed_started: bool,
) -> (
    Inputs,
    GitPeer,
    watch::Sender<bool>,
    tokio::task::JoinHandle<anyhow::Result<()>>,
    OutputPeer,
) {
    begin_fixture_execution(peer, slow_output, delayed_started, FixtureImage::Sealed).await
}
async fn begin_fixture_execution(
    peer: &Provider,
    slow_output: bool,
    delayed_started: bool,
    kind: FixtureImage,
) -> (
    Inputs,
    GitPeer,
    watch::Sender<bool>,
    tokio::task::JoinHandle<anyhow::Result<()>>,
    OutputPeer,
) {
    let (h, git, grant, profile, policy) = codex_prepared(peer, kind).await;

    if delayed_started {
        h.host_report_mode.store(4, Ordering::SeqCst);
    }
    let (http, mut output) = output_peer(policy, slow_output).await;
    output.execution_deadline_ns = Some(grant.deadline_ns());
    eprintln!(
        "fixture execute begin boot_ns={} original_deadline_ns={}",
        crate::native::http::boot_ns().unwrap(),
        grant.deadline_ns()
    );
    let mut executor = CodexExecutor::development_preflighted(http, profile).unwrap();
    let (tx, rx) = watch::channel(false);
    let pipeline = h.pipeline().clone();
    let task = tokio::spawn(async move { executor.execute(pipeline, grant, rx).await });
    (h, git, tx, task, output)
}
async fn assert_private_auth(h: &Inputs) -> String {
    let state = h.pipeline().snapshot().await.unwrap();
    let history = state.attempts[&h.id].native_host.as_ref().unwrap();
    assert!(history.baseline.is_some() && history.running.is_some());
    eprintln!(
        "fixture physical Running observed_boot_ms={} started_ack={}",
        history
            .running
            .as_ref()
            .unwrap()
            .observation
            .observed_boot_ms,
        history
            .terminal
            .as_ref()
            .is_some_and(|v| v.started.receipt.is_some())
    );
    let auth = history
        .intent
        .host_root
        .join(history.intent.launch_id.to_string())
        .join("codex-home/auth.json");
    assert_eq!(
        fs::metadata(&auth).unwrap().permissions().mode() & 0o777,
        0o600
    );
    let private: Value = serde_json::from_slice(&fs::read(auth).unwrap()).unwrap();
    let capability = private["OPENAI_API_KEY"].as_str().unwrap().to_owned();
    assert!(capability.len() >= 16 && capability.bytes().all(|b| b.is_ascii_graphic()));
    assert!(
        capability != "synthetic-upstream-key" && capability != "synthetic-key",
        "child capability must be fresh, not an upstream/source credential"
    );
    capability
}

fn user_input(request: &Value) -> Vec<&str> {
    request["input"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|v| v["role"] == "user")
        .flat_map(|v| v["content"].as_array().unwrap())
        .filter_map(|v| v["text"].as_str())
        .collect()
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires exact pinned Codex0.166, helper0.1.5 and actual tmux; loopback only"]
async fn actual_codex_finite_response_has_exact_auth_startup_readiness_output_and_stopped_acks() {
    let peer = provider().await;
    let (h, _git, tx, task, output) = begin_execution(&peer, false, false).await;
    if tokio::time::timeout(std::time::Duration::from_secs(12), peer.entered.notified())
        .await
        .is_err()
    {
        let bytes: Vec<u8> = output
            .uploads
            .lock()
            .unwrap()
            .iter()
            .flat_map(|b| {
                serde_json::from_slice::<PeerUpload>(b)
                    .unwrap()
                    .frames
                    .into_iter()
                    .flat_map(|f| f.bytes)
                    .collect::<Vec<_>>()
            })
            .collect();
        let _ = tx.send(true);
        let driver_result = tokio::time::timeout(std::time::Duration::from_secs(3), task).await;
        eprintln!("actual CLI driver cleanup: {driver_result:?}");
        panic!(
            "actual CLI made no loopback request; observed physical output: {}",
            String::from_utf8_lossy(&bytes)
        );
    }
    assert_private_auth(&h).await;
    {
        let requests = peer.requests.lock().unwrap();
        let user = user_input(&requests[0]);
        assert_eq!(
            user.len(),
            2,
            "only observed CLI environment and exact stdin user messages"
        );
        assert!(user[0].starts_with("<environment_context>\n"));
        assert!(
            user[0].contains("<permission_profile type=\"managed\">")
                && user[0].contains("<file_system type=\"restricted\">")
        );
        assert_eq!(user[1], "instructions\n\ntask-context");
        assert_eq!(
            user.iter()
                .filter(|s| **s == "instructions\n\ntask-context")
                .count(),
            1
        );
    }
    tokio::time::timeout(
        std::time::Duration::from_secs(12),
        output.entered.notified(),
    )
    .await
    .unwrap();
    assert!(
        !task.is_finished(),
        "unwatched output409 must not stop actual Codex"
    );
    {
        let uploads = output.uploads.lock().unwrap();
        let q: PeerUpload = serde_json::from_slice(&uploads[0]).unwrap();
        let bytes: Vec<u8> = q.frames.iter().flat_map(|f| f.bytes.clone()).collect();
        let literal = String::from_utf8_lossy(&bytes);
        assert!(
            literal.contains("thread.started") && literal.contains("turn.started"),
            "genuine CLI JSONL readiness must be observed through physical tmux bytes: {literal}"
        );
        assert!(
            !literal.contains("synthetic-key"),
            "scoped private auth must not appear in terminal output"
        );
    }
    peer.release.add_permits(1);
    tokio::time::timeout(std::time::Duration::from_secs(5), task)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    stopped(&h, true).await;
    assert_eq!(peer.requests.lock().unwrap().len(), 1);
    let state = h.pipeline().snapshot().await.unwrap();
    let history = state.attempts[&h.id].native_host.as_ref().unwrap();
    assert!(history.baseline.is_some() && history.running.is_some());
    assert_eq!(
        history.stopped.as_ref().unwrap().observation.root_status,
        Some(0),
        "actual Codex completed the finite successful response"
    );
    let directory = history
        .intent
        .host_root
        .join(history.intent.launch_id.to_string());
    assert!(
        !directory.join("codex-home/auth.json").exists(),
        "auth removed only after physical stopped proof"
    );
    assert_eq!(h.pipeline().retained_input_stats().unwrap().attempts, 1);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires exact pinned Codex0.166, helper0.1.5 and actual tmux; loopback only"]
async fn actual_codex_shutdown_during_blocked_output_stops_without_provider_response() {
    let peer = provider().await;
    let (h, _git, tx, task, output) = begin_execution(&peer, true, false).await;
    tokio::time::timeout(
        std::time::Duration::from_secs(12),
        output.entered.notified(),
    )
    .await
    .unwrap();
    tx.send(true).unwrap();
    tokio::time::timeout(std::time::Duration::from_secs(3), task)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    stopped(&h, true).await;
    assert_eq!(output.uploads.lock().unwrap().len(), 1);
    assert!(peer.requests.lock().unwrap().len() <= 1);
    assert_eq!(
        peer.release.available_permits(),
        0,
        "provider response was never released"
    );
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires exact pinned Codex0.166, helper0.1.5 and actual tmux; loopback only"]
async fn actual_codex_shutdown_during_delayed_started_keeps_unknown_receipt_after_physical_stop() {
    let peer = provider().await;
    let (h, _git, tx, task, output) = begin_execution(&peer, false, true).await;
    tokio::time::timeout(std::time::Duration::from_secs(12), async {
        while h.phase_terminal_calls.lock().unwrap().is_empty() {
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    tx.send(true).unwrap();
    assert!(
        tokio::time::timeout(std::time::Duration::from_secs(3), task)
            .await
            .unwrap()
            .unwrap()
            .is_err()
    );
    stopped(&h, false).await;
    assert!(output.uploads.lock().unwrap().is_empty());
    assert!(peer.requests.lock().unwrap().len() <= 1);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "qualification probe: unified exec currently ENOENT; requires independently confined runtime and admitted helper route"]
async fn actual_codex_exec_command_round_trip_uses_confined_workspace_and_actual_tool_output() {
    let peer = provider_mode(true).await;
    let (h, _git, _tx, task, _output) = begin_execution(&peer, false, false).await;
    tokio::time::timeout(std::time::Duration::from_secs(12), peer.entered.notified())
        .await
        .unwrap();
    assert_private_auth(&h).await;
    {
        let requests = peer.requests.lock().unwrap();
        assert!(
            requests[0]["tools"]
                .as_array()
                .unwrap()
                .iter()
                .any(|v| v["type"] == "function" && v["name"] == "exec_command")
        );
    }
    peer.release.add_permits(1);
    tokio::time::timeout(std::time::Duration::from_secs(5), async {
        while peer.requests.lock().unwrap().len() < 2 {
            peer.entered.notified().await;
        }
    })
    .await
    .unwrap();
    {
        let requests = peer.requests.lock().unwrap();
        let outputs: Vec<_> = requests[1]["input"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|v| v["type"] == "function_call_output" && v["call_id"] == "call_aq_local")
            .collect();
        assert_eq!(
            outputs.len(),
            1,
            "actual CLI must return the exact controlled tool call"
        );
        let output = serde_json::to_string(&outputs[0]["output"]).unwrap();
        assert!(
            output.contains("aq-local-tool-proof"),
            "actual local tool did not produce expected output: {output}"
        );
    }
    let state = h.pipeline().snapshot().await.unwrap();
    let created = state.attempts[&h.id].source.as_ref().unwrap().operations["create"]
        .result
        .as_ref()
        .unwrap();
    let workspace: crate::source::Workspace = serde_json::from_value(created.clone()).unwrap();
    assert_eq!(
        fs::read(workspace.path.join("aq-local-tool-proof.txt")).unwrap(),
        b"aq-local-tool-proof"
    );
    peer.release.add_permits(1);
    tokio::time::timeout(std::time::Duration::from_secs(5), task)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    stopped(&h, true).await;
    let state = h.pipeline().snapshot().await.unwrap();
    assert_eq!(
        state.attempts[&h.id]
            .native_host
            .as_ref()
            .unwrap()
            .stopped
            .as_ref()
            .unwrap()
            .observation
            .root_status,
        Some(0)
    );
    assert_eq!(peer.requests.lock().unwrap().len(), 2);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "explicit named-readonly qualification; requires private fixture, exact pins and root-approved paired source"]
async fn actual_named_readonly_codex_exec_command_round_trip_has_owned_cap_and_physical_cleanup() {
    let peer = provider_mode(true).await;
    let (h, _git, tx, mut task, output) =
        begin_fixture_execution(&peer, false, false, FixtureImage::NamedReadonly).await;
    let deadline = output.execution_deadline_ns.unwrap();
    // Every qualification stage shares the original Grant deadline. No new
    // wall-clock budget starts when an earlier stage happens to finish.
    let mut joined = false;
    let progress = async {
        original_deadline_stage(deadline, "first provider request", async {
            while peer.requests.lock().unwrap().is_empty() {
                peer.entered.notified().await;
            }
        })
        .await?;
        let capability = assert_private_auth(&h).await;
        {
            let requests = peer.requests.lock().unwrap();
            anyhow::ensure!(
                !serde_json::to_string(&requests[0])
                    .unwrap()
                    .contains(&capability),
                "child capability entered model input"
            );
        }
        {
            let requests = peer.requests.lock().unwrap();
            anyhow::ensure!(
                requests[0]["tools"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .any(|v| v["type"] == "function" && v["name"] == "exec_command")
            );
        }
        peer.release.add_permits(1);
        original_deadline_stage(deadline, "real tool result request", async {
            while peer.requests.lock().unwrap().len() < 2 {
                peer.entered.notified().await;
            }
        })
        .await?;
        {
            let requests = peer.requests.lock().unwrap();
            let outputs: Vec<_> = requests[1]["input"]
                .as_array()
                .unwrap()
                .iter()
                .filter(|v| v["type"] == "function_call_output" && v["call_id"] == "call_aq_local")
                .collect();
            anyhow::ensure!(
                outputs.len() == 1,
                "actual CLI must return the exact controlled tool call"
            );
            let output = serde_json::to_string(&outputs[0]["output"]).unwrap();
            anyhow::ensure!(
                output.contains("aq-local-tool-proof"),
                "actual local tool did not produce expected output: {output}"
            );
        }
        let state = h.pipeline().snapshot().await.unwrap();
        let created = state.attempts[&h.id].source.as_ref().unwrap().operations["create"]
            .result
            .as_ref()
            .unwrap();
        let workspace: crate::source::Workspace = serde_json::from_value(created.clone()).unwrap();
        anyhow::ensure!(
            fs::read(workspace.path.join("aq-local-tool-proof.txt"))? == b"aq-local-tool-proof",
            "real workspace tool file differs"
        );
        // Output delivery is independent of inference. Hold the second response
        // while checking it; do not demand an upload before releasing the tool call.
        original_deadline_stage(deadline, "terminal readiness upload", async {
            while output.uploads.lock().unwrap().is_empty() {
                output.entered.notified().await;
            }
        })
        .await?;
        anyhow::ensure!(
            !task.is_finished(),
            "no current Watch must not stop a named agent"
        );
        {
            let uploads = output.uploads.lock().unwrap();
            let request: PeerUpload = serde_json::from_slice(&uploads[0]).unwrap();
            let literal: Vec<u8> = request
                .frames
                .iter()
                .flat_map(|f| f.bytes.clone())
                .collect();
            let literal = String::from_utf8_lossy(&literal);
            anyhow::ensure!(
                literal.contains("thread.started") && literal.contains("turn.started"),
                "genuine named CLI readiness must be observed"
            );
            assert!(!literal.contains(&capability) && !literal.contains("synthetic-upstream-key"));
        }
        peer.release.add_permits(1);
        let completion =
            original_deadline_stage(deadline, "natural physical completion", &mut task).await?;
        joined = true;
        completion??;
        Ok::<String, anyhow::Error>(capability)
    }
    .await;
    if let Err(error) = &progress {
        // Always ask the real owner to stop and persist proof on a stage timeout.
        // Cleanup is allowed to finish after the Grant has stopped admission.
        let _ = tx.send(true);
        let cleanup_joined = joined
            || tokio::time::timeout(std::time::Duration::from_secs(10), &mut task)
                .await
                .is_ok();
        let state = h.pipeline().snapshot().await.unwrap();
        let history = state.attempts[&h.id].native_host.as_ref();
        eprintln!(
            "fixture failure: {error}; cleanup_joined={} provider_requests={} registrations={} uploads={} running_observed_boot_ms={:?} echild={} started_ack={} stopped_ack={}",
            cleanup_joined,
            peer.requests.lock().unwrap().len(),
            output.registrations.lock().unwrap().len(),
            output.uploads.lock().unwrap().len(),
            history
                .and_then(|v| v.running.as_ref())
                .map(|v| v.observation.observed_boot_ms),
            history
                .and_then(|v| v.stopped.as_ref())
                .is_some_and(|v| v.observation.kernel_wait_echild),
            history
                .and_then(|v| v.terminal.as_ref())
                .is_some_and(|v| v.started.receipt.is_some()),
            history
                .and_then(|v| v.terminal.as_ref())
                .and_then(|v| v.stopped.as_ref())
                .is_some_and(|v| v.receipt.is_some())
        );
    }
    let capability = progress.unwrap();
    stopped(&h, true).await;
    let state = h.pipeline().snapshot().await.unwrap();
    assert_eq!(
        state.attempts[&h.id]
            .native_host
            .as_ref()
            .unwrap()
            .stopped
            .as_ref()
            .unwrap()
            .observation
            .root_status,
        Some(0)
    );
    assert_eq!(peer.requests.lock().unwrap().len(), 2);
    let history = state.attempts[&h.id].native_host.as_ref().unwrap();
    let auth = history
        .intent
        .host_root
        .join(history.intent.launch_id.to_string())
        .join("codex-home/auth.json");
    assert!(
        !auth.exists(),
        "matching private capability cleanup follows physical stop"
    );
    assert_eq!(h.pipeline().retained_input_stats().unwrap().attempts, 1);
    for request in peer.requests.lock().unwrap().iter() {
        assert!(
            !serde_json::to_string(request)
                .unwrap()
                .contains(&capability)
        );
    }
}

async fn original_deadline_stage<T>(
    deadline_ns: u64,
    stage: &str,
    operation: impl std::future::Future<Output = T>,
) -> anyhow::Result<T> {
    let now = crate::native::http::boot_ns()?;
    eprintln!(
        "fixture stage={stage} boot_ns={now} remaining_ns={}",
        deadline_ns.saturating_sub(now)
    );
    anyhow::ensure!(now < deadline_ns, "original Grant expired before {stage}");
    let result = tokio::time::timeout(
        std::time::Duration::from_nanos(deadline_ns - now),
        operation,
    )
    .await
    .map_err(|_| anyhow::anyhow!("original Grant expired during {stage}"))?;
    eprintln!(
        "fixture stage={stage} complete boot_ns={}",
        crate::native::http::boot_ns()?
    );
    Ok(result)
}

#[tokio::test]
async fn qualification_stage_does_not_reset_or_poll_expired_original_deadline() {
    let polled = std::sync::atomic::AtomicBool::new(false);
    let expired = crate::native::http::boot_ns().unwrap().saturating_sub(1);
    assert!(
        original_deadline_stage(expired, "expired", async {
            polled.store(true, Ordering::SeqCst);
        })
        .await
        .is_err()
    );
    assert!(!polled.load(Ordering::SeqCst));
    let deadline = crate::native::http::boot_ns().unwrap() + 5_000_000;
    assert!(
        original_deadline_stage(deadline, "pending", std::future::pending::<()>())
            .await
            .is_err()
    );
    assert!(crate::native::http::boot_ns().unwrap() >= deadline);
}
