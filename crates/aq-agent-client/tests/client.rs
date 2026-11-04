use aq_agent_api::{Binding, canonical, digest};
use aq_agent_client::{
    client::{AgentClient, ClientConfig, Failure, MessageRoute},
    context::{ContextContent, StartupContext},
};
use aq_session_protocol::{
    ContextDelivery, ContextRef, Digest, ProfiledOffer, SessionIdentity, canonical_digest,
};
use axum::{
    Router,
    body::{Body, to_bytes},
    extract::State,
    http::{Request, StatusCode},
    response::{IntoResponse, Response},
};
use serde_json::{Value, json};
use std::{
    os::unix::fs::PermissionsExt,
    path::Path,
    sync::{
        Arc, Mutex,
        atomic::{AtomicUsize, Ordering},
    },
};
use tempfile::TempDir;
use uuid::Uuid;
fn base() -> Value {
    serde_json::from_str(include_str!("fixtures/agent-base.json")).unwrap()
}
fn startup(context: &Value) -> StartupContext {
    let b: Binding = serde_json::from_value(context["binding"].clone()).unwrap();
    let mut offer: ProfiledOffer =
        serde_json::from_str(include_str!("fixtures/profiled-offer.json")).unwrap();
    let session = SessionIdentity::Remote {
        node_id: b.node_id,
        boot_id: b.boot_id,
        attempt_id: b.attempt_id,
        assignment_generation: b.generation,
        session_id: b.session_id,
    };
    offer.binding.session = session.clone();
    offer.binding.attempt_id = b.attempt_id;
    offer.binding.assignment_generation = b.generation;
    offer.binding.execution_spec_digest =
        serde_json::from_value(json!(b.resolved_spec_digest)).unwrap();
    offer.binding.graph_access_profile_digest =
        serde_json::from_value(json!(b.profile_digest)).unwrap();
    let m = &mut offer.manifest;
    m.bindings.session = session;
    m.bindings.project_id = b.project_id;
    m.bindings.graph_id = b.workspace_id;
    m.bindings.task_id = b.task_id;
    m.bindings.attempt_id = b.attempt_id;
    m.bindings.assignment_generation = b.generation;
    m.bindings.graph_access_profile.digest = offer.binding.graph_access_profile_digest.clone();
    let text = "Required instructions: café 世界.".to_owned();
    let content = ContextContent {
        digest: Digest::of(text.as_bytes()),
        text,
    };
    let mut artifact = m.agent_profile.clone();
    artifact.digest = content.digest.clone();
    m.context.references = vec![ContextRef {
        artifact,
        provenance: "fixture revision 1".try_into().unwrap(),
        delivery: ContextDelivery::Startup,
        bytes: content.text.len() as u64,
        tokens: 16,
        required: true,
    }];
    m.context.startup_byte_budget = 1024;
    m.context.startup_token_budget = 1024;
    m.context.context_window_tokens = 32768;
    offer.binding.launch_manifest_digest = canonical_digest(m).unwrap();
    let startup = StartupContext {
        binding: b,
        profiled_offer: offer,
        contents: vec![content],
    };
    startup.validate().unwrap();
    startup
}
fn private_write(root: &Path, name: &str, value: &Value) {
    let path = root.join(name);
    std::fs::write(&path, serde_json::to_vec(value).unwrap()).unwrap();
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600)).unwrap();
}
fn install(root: &Path, url: &str, context: Value) {
    std::fs::set_permissions(root, std::fs::Permissions::from_mode(0o700)).unwrap();
    let bundle = startup(&context)
        .with_graph_context(context.clone())
        .unwrap();
    let cfg = ClientConfig {
        format_version: 1,
        server_url: url.into(),
        binding: bundle.binding.clone(),
        edge_authentication: false,
    };
    private_write(root, "client.json", &serde_json::to_value(cfg).unwrap());
    private_write(root, "context.json", &serde_json::to_value(bundle).unwrap());
    private_write(
        root,
        "credential.json",
        &json!({"api_version":1,"token":format!("aqa_{}","q".repeat(40)),"context":context}),
    );
}
fn messaging(name: &str) -> Value {
    let all: Vec<Value> = serde_json::from_str(include_str!(
        "../../../vendor/aq-agent-api-0.1.0/artifacts/fixtures/messaging.json"
    ))
    .unwrap();
    all.into_iter().find(|f| f["name"] == name).unwrap()["value"].clone()
}
struct Peer {
    base: Value,
    calls: AtomicUsize,
    posts: Mutex<Vec<Vec<u8>>>,
    mode: usize,
}
async fn endpoint(State(s): State<Arc<Peer>>, req: Request<Body>) -> Response {
    assert_eq!(
        req.headers()["authorization"],
        format!("Bearer aqa_{}", "q".repeat(40))
    );
    assert_eq!(req.headers()["X-AQ-Agent-API-Version"], "1");
    assert!(req.headers().get("cf-access-client-secret").is_none());
    s.calls.fetch_add(1, Ordering::SeqCst);
    let path = req.uri().path().to_owned();
    let raw = to_bytes(req.into_body(), 1_048_576).await.unwrap();
    let mut value = if path == "/v1/agent/context" {
        s.base["context"].clone()
    } else if path == "/v1/agent/graph/read" {
        let request: Value = serde_json::from_slice(&raw).unwrap();
        assert_eq!(request["binding"], s.base["context"]["binding"]);
        let mut value =
            s.base["read_responses"][request["query"]["kind"].as_str().unwrap()].clone();
        value["request_id"] = request["request_id"].clone();
        value
    } else if path == "/v1/agent/graph/commands" {
        let request: Value = serde_json::from_slice(&raw).unwrap();
        let mut posts = s.posts.lock().unwrap();
        if let Some(first) = posts.first() {
            assert_eq!(first.as_slice(), raw.as_ref());
        }
        posts.push(raw.to_vec());
        if s.mode == 1 && posts.len() == 1 {
            return (
                StatusCode::BAD_GATEWAY,
                [("X-AQ-Agent-API-Version", "1")],
                "private-secret-marker invalid intermediary response",
            )
                .into_response();
        }
        let mut value = s.base["operation_statuses"]["confirmed"].clone();
        value["operation_id"] = request["operation_id"].clone();
        value["payload_digest"] = json!(digest(&request).unwrap());
        value
    } else if path.starts_with("/v1/agent/operations/") {
        let request = s
            .posts
            .lock()
            .unwrap()
            .last()
            .map(|b| serde_json::from_slice::<Value>(b).unwrap())
            .unwrap_or_else(|| s.base["command_requests"]["correct_record"].clone());
        let mut value = s.base["operation_statuses"]["confirmed"].clone();
        value["operation_id"] = request["operation_id"].clone();
        value["payload_digest"] = json!(digest(&request).unwrap());
        value
    } else if path == "/v1/agent/messages/inbox" {
        let request: Value = serde_json::from_slice(&raw).unwrap();
        let mut value = messaging("mailbox-page");
        value["recipient"] = request["recipient"].clone();
        value["after"] = request["after"].clone();
        value
    } else if path == "/v1/agent/messages/send" {
        let request: Value = serde_json::from_slice(&raw).unwrap();
        let mut value = messaging("accepted-is-queued");
        for key in ["operation_id", "message_id", "thread_id", "recipient"] {
            value[key] = request[key].clone();
        }
        value
    } else {
        panic!("unexpected finite route");
    };
    if s.mode == 4 {
        return (
            StatusCode::ACCEPTED,
            [("X-AQ-Agent-API-Version", "1")],
            axum::Json(value),
        )
            .into_response();
    }
    if s.mode == 5 {
        value["receipt"]["workspace_id"] = json!(Uuid::new_v4());
    }
    if (10..=14).contains(&s.mode) {
        let name = ["confirmed", "pending", "conflict", "rejected", "unknown"][s.mode - 10];
        let mut result = s.base["operation_statuses"][name].clone();
        result["payload_digest"] = value["payload_digest"].clone();
        result["operation_id"] = value["operation_id"].clone();
        let code = [200, 202, 409, 422, 202][s.mode - 10];
        return (
            StatusCode::from_u16(code).unwrap(),
            [("X-AQ-Agent-API-Version", "1")],
            axum::Json(result),
        )
            .into_response();
    }
    if s.mode == 2 {
        value["binding"]["generation"] = json!(999);
    }
    if s.mode == 3 {
        return (
            StatusCode::OK,
            [("X-AQ-Agent-API-Version", "2")],
            axum::Json(value),
        )
            .into_response();
    }
    (
        [
            ("X-AQ-Agent-API-Version", "1"),
            ("Cache-Control", "no-store"),
        ],
        axum::Json(value),
    )
        .into_response()
}
async fn peer(mode: usize) -> (String, Arc<Peer>, tokio::task::JoinHandle<()>) {
    let state = Arc::new(Peer {
        base: base(),
        calls: AtomicUsize::new(0),
        posts: Mutex::new(vec![]),
        mode,
    });
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    let app = Router::new().fallback(endpoint).with_state(state.clone());
    let handle = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    (url, state, handle)
}
#[tokio::test]
async fn published_read_shapes_and_live_context_use_only_session_authority() {
    let (url, peer, server) = peer(0).await;
    let root = TempDir::new().unwrap();
    install(root.path(), &url, base()["context"].clone());
    let client = AgentClient::open(root.path()).unwrap();
    assert_eq!(client.context().await.unwrap().exit_code, 0);
    for request in base()["read_requests"].as_object().unwrap().values() {
        assert_eq!(client.read(request.clone()).await.unwrap().exit_code, 0);
    }
    assert_eq!(peer.calls.load(Ordering::SeqCst), 7);
    server.abort();
}
#[tokio::test]
async fn message_inbox_and_send_stay_in_bound_mailbox() {
    let (url, peer, server) = peer(0).await;
    let root = TempDir::new().unwrap();
    install(root.path(), &url, base()["context"].clone());
    let client = AgentClient::open(root.path()).unwrap();
    let inbox = client.inbox_request(false, "0", 32, None);
    assert_eq!(
        inbox["recipient"],
        messaging("scoped-task-send")["recipient"]
    );
    let page = client.message(MessageRoute::Inbox, inbox).await.unwrap();
    assert_eq!(page.exit_code, 0);
    assert_eq!(page.value["next_cursor"], "1");
    let session = client.inbox_request(true, "0", 8, None);
    assert_eq!(
        session["recipient"],
        messaging("immutable-session-send")["recipient"]
    );
    assert_eq!(
        client
            .message(MessageRoute::Inbox, session)
            .await
            .unwrap()
            .exit_code,
        0
    );
    let send = messaging("scoped-task-send");
    let receipt = client
        .message(MessageRoute::Send, send.clone())
        .await
        .unwrap();
    assert_eq!(receipt.exit_code, 0);
    assert_eq!(receipt.value["disposition"], "queued");
    assert_eq!(peer.calls.load(Ordering::SeqCst), 3);
    // A mailbox in another project or task is refused before any request.
    let mut foreign = send.clone();
    foreign["recipient"]["project_id"] = json!(Uuid::new_v4());
    assert!(client.message(MessageRoute::Send, foreign).await.is_err());
    let mut other_task = client.inbox_request(false, "0", 32, None);
    other_task["recipient"]["task_id"] = json!(Uuid::new_v4());
    assert!(
        client
            .message(MessageRoute::Inbox, other_task)
            .await
            .is_err()
    );
    assert_eq!(peer.calls.load(Ordering::SeqCst), 3);
    server.abort();
}
#[tokio::test]
async fn lost_response_survives_process_reopen_with_exact_id_bytes_and_receipt() {
    let (url, peer, server) = peer(1).await;
    let root = TempDir::new().unwrap();
    install(root.path(), &url, base()["context"].clone());
    let request = base()["command_requests"]["correct_record"].clone();
    let id: Uuid = request["operation_id"].as_str().unwrap().parse().unwrap();
    let mut client = AgentClient::open(root.path()).unwrap();
    let error = client.command(request.clone()).await.err().unwrap();
    let failure = error.downcast_ref::<Failure>().unwrap();
    assert_eq!(failure.operation_id, Some(id));
    assert_eq!(failure.effect, "unknown");
    assert!(!error.to_string().contains("private-secret-marker"));
    drop(client);
    let bytes = std::fs::read(root.path().join("operations.json")).unwrap();
    assert!(!String::from_utf8_lossy(&bytes).contains("aqa_"));
    let state: Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(state["operations"][id.to_string()]["attempts"], 1);
    let mut client = AgentClient::open(root.path()).unwrap();
    let mut changed = request.clone();
    changed["reason"] = json!("changed request");
    assert!(client.command(changed).await.is_err());
    tokio::time::sleep(std::time::Duration::from_millis(260)).await;
    let response = client.command(request.clone()).await.unwrap();
    assert_eq!(response.exit_code, 0);
    assert_eq!(peer.posts.lock().unwrap().len(), 2);
    assert_eq!(peer.posts.lock().unwrap()[0], canonical(&request).unwrap());
    assert_eq!(client.command(request).await.unwrap().exit_code, 0);
    assert_eq!(peer.posts.lock().unwrap().len(), 2);
    server.abort();
}
#[tokio::test]
async fn response_version_binding_and_offline_state_never_become_applied() {
    for mode in [2, 3, 4, 5] {
        let (url, peer, server) = peer(mode).await;
        let root = TempDir::new().unwrap();
        install(root.path(), &url, base()["context"].clone());
        let mut client = AgentClient::open(root.path()).unwrap();
        if mode == 2 || mode == 3 || mode == 4 {
            assert!(client.context().await.is_err());
        }
        let error = client
            .command(base()["command_requests"]["correct_record"].clone())
            .await
            .err()
            .unwrap();
        assert_eq!(error.downcast_ref::<Failure>().unwrap().effect, "unknown");
        assert_eq!(peer.posts.lock().unwrap().len(), 1);
        server.abort();
    }
    let root = TempDir::new().unwrap();
    install(root.path(), "http://127.0.0.1:1", base()["context"].clone());
    let mut client = AgentClient::open(root.path()).unwrap();
    let cached = client.cached_context().unwrap();
    assert_eq!(cached["cached"], true);
    assert_eq!(cached["live_authority"], false);
    assert!(
        client
            .command(base()["command_requests"]["correct_record"].clone())
            .await
            .err()
            .unwrap()
            .downcast_ref::<Failure>()
            .is_some()
    );
}
#[test]
fn required_context_and_access_profile_binding_cannot_be_silently_changed() {
    let context = base()["context"].clone();
    let mut bundle = startup(&context).with_graph_context(context).unwrap();
    assert_eq!(
        bundle.startup().unwrap(),
        vec!["Required instructions: café 世界."]
    );
    bundle.contents.clear();
    assert!(bundle.validate().is_err());
    let context = base()["context"].clone();
    let mut bundle = startup(&context).with_graph_context(context).unwrap();
    bundle.contents[0].text.push('!');
    assert!(bundle.validate().is_err());
    let context = base()["context"].clone();
    let mut bundle = startup(&context).with_graph_context(context).unwrap();
    bundle.profiled_offer.binding.graph_access_profile_digest = bundle
        .profiled_offer
        .manifest
        .execution_profile
        .digest
        .clone();
    assert!(bundle.validate().is_err());
}
#[tokio::test]
async fn node_credential_symlink_permissions_and_expiry_are_refused_before_network() {
    for variant in 0..4 {
        let (url, peer, server) = peer(0).await;
        let root = TempDir::new().unwrap();
        install(root.path(), &url, base()["context"].clone());
        let path = root.path().join("credential.json");
        match variant {
            0 => {
                let mut v: Value = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
                v["token"] = json!("aqw_private_node_credential_must_not_be_sent");
                private_write(root.path(), "credential.json", &v);
            }
            1 => std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap(),
            2 => {
                std::fs::rename(&path, root.path().join("secret")).unwrap();
                std::os::unix::fs::symlink(root.path().join("secret"), &path).unwrap();
            }
            _ => {
                let mut v: Value = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
                v["context"]["credential"]["expires_at"] = json!("2000-01-01T00:00:00Z");
                private_write(root.path(), "credential.json", &v);
            }
        }
        let client = AgentClient::open(root.path()).unwrap();
        assert!(client.context().await.is_err());
        assert_eq!(peer.calls.load(Ordering::SeqCst), 0);
        server.abort();
    }
}

#[tokio::test]
async fn status_lookup_without_local_intent_still_validates_receipt_scope() {
    let (url, _, server) = peer(5).await;
    let root = TempDir::new().unwrap();
    install(root.path(), &url, base()["context"].clone());
    let mut client = AgentClient::open(root.path()).unwrap();
    let id = base()["command_requests"]["correct_record"]["operation_id"]
        .as_str()
        .unwrap()
        .parse()
        .unwrap();
    let error = client.operation(id).await.err().unwrap();
    assert_eq!(
        error.downcast_ref::<Failure>().unwrap().operation_id,
        Some(id)
    );
    server.abort();
}
#[tokio::test]
async fn actual_cli_preserves_operation_states_and_safe_error_output() {
    use std::process::Stdio;
    use tokio::{io::AsyncWriteExt, process::Command};
    for (mode, expected) in [(10, 0), (11, 10), (12, 11), (13, 12), (14, 13), (1, 13)] {
        let (url, _, server) = peer(mode).await;
        let root = TempDir::new().unwrap();
        install(root.path(), &url, base()["context"].clone());
        let request = base()["command_requests"]["correct_record"].clone();
        let mut child = Command::new(env!("CARGO_BIN_EXE_aq-agent"))
            .env("HTTP_PROXY", "http://127.0.0.1:1")
            .env("HTTPS_PROXY", "http://127.0.0.1:1")
            .env("ALL_PROXY", "http://127.0.0.1:1")
            .env("NO_PROXY", "")
            .arg("--state-dir")
            .arg(root.path())
            .args(["graph", "command"])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        child
            .stdin
            .take()
            .unwrap()
            .write_all(&canonical(&request).unwrap())
            .await
            .unwrap();
        let output = child.wait_with_output().await.unwrap();
        assert_eq!(output.status.code(), Some(expected));
        let bytes = if mode == 1 {
            &output.stderr
        } else {
            &output.stdout
        };
        let result: Value = serde_json::from_slice(bytes).unwrap();
        assert_eq!(result["operation_id"], request["operation_id"]);
        for stream in [&output.stdout, &output.stderr] {
            let text = String::from_utf8_lossy(stream);
            assert!(!text.contains("private-secret-marker"));
            assert!(!text.contains("aqa_"));
        }
        if mode == 1 {
            assert_eq!(result["effect"], "unknown");
            assert!(output.stdout.is_empty());
        }
        server.abort();
    }
    let root = TempDir::new().unwrap();
    install(root.path(), "http://127.0.0.1:1", base()["context"].clone());
    let output = Command::new(env!("CARGO_BIN_EXE_aq-agent"))
        .arg("--state-dir")
        .arg(root.path())
        .args(["context", "--cached"])
        .output()
        .await
        .unwrap();
    assert!(output.status.success());
    let value: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(value["cached"], true);
    assert_eq!(value["live_authority"], false);
}
