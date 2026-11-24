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
    aq_finite_responses::completed(model, FINAL_TEXT)
}
fn tool_call(model: &str) -> String {
    aq_finite_responses::exec_command(
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
                let request = aq_finite_responses::request(
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
impl From<InstalledFilePin> for FilePin {
    fn from(pin: InstalledFilePin) -> Self {
        Self {
            path: pin.path,
            artifact: pin.artifact,
        }
    }
}
include!("codex_process_scenarios/installation.rs");
include!("codex_process_scenarios/observations.rs");
include!("codex_process_scenarios/process.rs");
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
