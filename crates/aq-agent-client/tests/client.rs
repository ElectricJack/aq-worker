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
    /// The fixture task thread, oldest first.
    comments: Mutex<Vec<Value>>,
}
fn agent_error(status: StatusCode, code: &str) -> Response {
    (
        status,
        [("X-AQ-Agent-API-Version", "1")],
        axum::Json(
            json!({"api_version":1,"request_id":Uuid::new_v4(),"operation_id":null,
            "code":code,"message":"agent access request refused","effect":"not_applied",
            "retryable":false,"recovery":null}),
        ),
    )
        .into_response()
}
/// The agent-comments v1 routes as aq-server serves them (protocol/agent-comments/v1).
#[allow(clippy::result_large_err)] // test fixture: the error is the reply
fn comment_route(s: &Peer, path: &str, raw: &[u8]) -> Result<Value, Response> {
    let request: Value = serde_json::from_slice(raw).unwrap();
    let binding = &s.base["context"]["binding"];
    if s.mode == 32 {
        return Err(agent_error(StatusCode::UNAUTHORIZED, "session_revoked"));
    }
    let mut thread = s.comments.lock().unwrap();
    if path == "/v1/agent/comments" {
        assert_eq!(
            request.as_object().unwrap().len(),
            3,
            "only api_version, operation_id and body: {request}"
        );
        s.posts.lock().unwrap().push(raw.to_vec());
        let op = &request["operation_id"];
        let comment = match thread.iter().find(|c| &c["comment_id"] == op) {
            Some(c) if c["body"] == request["body"] => c.clone(),
            Some(_) => return Err(agent_error(StatusCode::CONFLICT, "id_conflict")),
            None => {
                let c = json!({"comment_id":op,"task_id":binding["task_id"],
                    "author":{"kind":"agent","user_id":null,"display_name":null,
                        "session_id":binding["session_id"],"attempt_id":binding["attempt_id"]},
                    "body":request["body"],"created_at":format!("2026-11-06T12:00:{:02}.000Z", thread.len()),
                    "edited_at":null,"deleted_at":null,"revision":"1","can_edit":false});
                thread.push(c.clone());
                c
            }
        };
        if s.mode == 31 && s.posts.lock().unwrap().len() == 1 {
            // Recorded, but the response is lost on the way back.
            return Err((
                StatusCode::BAD_GATEWAY,
                [("X-AQ-Agent-API-Version", "1")],
                "lost",
            )
                .into_response());
        }
        let mut comment = comment;
        if s.mode == 30 {
            comment["author"]["attempt_id"] = json!(Uuid::new_v4());
        }
        return Ok(json!({"api_version":1,"comment":comment}));
    }
    assert_eq!(path, "/v1/agent/comments/list");
    let limit = request["limit"].as_u64().unwrap_or(50) as usize;
    let before = request["cursor"]
        .as_str()
        .map(|c| c.strip_prefix("tc1.").unwrap().parse::<usize>().unwrap())
        .unwrap_or(thread.len() + 1);
    let mut items: Vec<(usize, Value)> = thread
        .iter()
        .cloned()
        .enumerate()
        .map(|(i, c)| (i + 1, c))
        .filter(|(seq, _)| *seq < before)
        .rev()
        .collect();
    let more = items.len() > limit;
    items.truncate(limit);
    let next = more.then(|| format!("tc1.{}", items.last().unwrap().0));
    let mut items: Vec<Value> = items.into_iter().map(|(_, c)| c).collect();
    if s.mode == 33 {
        items[0]["task_id"] = json!(Uuid::new_v4());
    }
    Ok(json!({"api_version":1,"task_id":binding["task_id"],"items":items,"next_cursor":next}))
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
    let csrf = req.headers().get("X-AQ-Request").cloned();
    let raw = to_bytes(req.into_body(), 1_048_576).await.unwrap();
    if path.starts_with("/v1/agent/comments") {
        assert_eq!(csrf.unwrap(), "1");
        return match comment_route(&s, &path, &raw) {
            Ok(value) => (
                [
                    ("X-AQ-Agent-API-Version", "1"),
                    ("Cache-Control", "no-store"),
                ],
                axum::Json(value),
            )
                .into_response(),
            Err(response) => response,
        };
    }
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
        match s.mode {
            20 | 22 => value["messages"][0]["sender"] = supervisor_sender(),
            21 => {
                value["messages"][0]["sender"] =
                    json!({"kind":"owner","user_id":Uuid::from_u128(7)})
            }
            _ => {}
        }
        if s.mode == 22 && request["limit"] != 1 {
            value = messaging("omitted-body-explicit-chunk-request");
        }
        value["recipient"] = request["recipient"].clone();
        value["after"] = request["after"].clone();
        value
    } else if path == "/v1/agent/messages/body" {
        let request: Value = serde_json::from_slice(&raw).unwrap();
        let mut value = messaging("bounded-body-response");
        value["message_id"] = request["message_id"].clone();
        if request["offset"] != "0" {
            value["offset"] = request["offset"].clone();
            value["next_offset"] = json!("6");
            value["body"] = json!("!");
            value["complete"] = json!(true);
            if s.mode == 23 {
                value["payload_digest"] = json!("b".repeat(64));
            }
        }
        value
    } else if path == "/v1/agent/messages/send" {
        let request: Value = serde_json::from_slice(&raw).unwrap();
        s.posts.lock().unwrap().push(raw.to_vec());
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
        comments: Mutex::new(vec![]),
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
const CONVERSATION: Uuid = Uuid::from_u128(0xc0);
fn supervisor_sender() -> Value {
    json!({"kind":"central_supervisor","actor_id":Uuid::from_u128(0xa0),"generation":3,
        "session_id":Uuid::from_u128(0xa1),"conversation_id":CONVERSATION})
}
async fn reply(mode: usize, body: &str) -> (anyhow::Result<Value>, Vec<Value>) {
    let (url, peer, server) = peer(mode).await;
    let root = TempDir::new().unwrap();
    install(root.path(), &url, base()["context"].clone());
    let client = AgentClient::open(root.path()).unwrap();
    let original: Uuid = messaging("mailbox-page")["messages"][0]["message_id"]
        .as_str()
        .unwrap()
        .parse()
        .unwrap();
    let out = client
        .reply(
            false,
            original,
            body.to_owned(),
            Uuid::from_u128(0x0e),
            Uuid::from_u128(0x0f),
        )
        .await
        .map(|o| {
            assert_eq!(o.exit_code, 0);
            o.value
        });
    let sends = peer
        .posts
        .lock()
        .unwrap()
        .iter()
        .map(|b| serde_json::from_slice(b).unwrap())
        .collect();
    server.abort();
    (out, sends)
}
#[tokio::test]
async fn agent_reply_reaches_supervisor_mailbox() {
    let thread = messaging("mailbox-page")["messages"][0]["thread_id"].clone();
    for mode in [20, 22] {
        let (out, sends) = reply(mode, "Done: café 世界.").await;
        let receipt = out.unwrap();
        assert_eq!(sends.len(), 1, "exactly one send");
        let send = &sends[0];
        assert_eq!(
            send["recipient"],
            json!({"kind":"global_supervisor","conversation_id":CONVERSATION})
        );
        assert_eq!(send["thread_id"], thread);
        assert_eq!(send["body"], "Done: café 世界.");
        assert_eq!(send["operation_id"], json!(Uuid::from_u128(0x0e)));
        assert_eq!(send["message_id"], json!(Uuid::from_u128(0x0f)));
        assert_eq!(receipt["recipient"], send["recipient"]);
    }
    // A session sender's reply goes back to that session's mailbox.
    let (out, sends) = reply(0, "ack").await;
    out.unwrap();
    let sender = &messaging("mailbox-page")["messages"][0]["sender"]["binding"];
    assert_eq!(sends[0]["recipient"]["kind"], "session");
    for key in [
        "workspace_id",
        "project_id",
        "task_id",
        "session_id",
        "attempt_id",
        "generation",
    ] {
        assert_eq!(sends[0]["recipient"][key], sender[key]);
    }
    // No owner mailbox exists in agent-access v1, and body bounds hold before any send.
    for (mode, body) in [
        (21, "hi".to_owned()),
        (20, String::new()),
        (20, "x".repeat(16_385)),
    ] {
        let (out, sends) = reply(mode, &body).await;
        assert!(out.is_err());
        assert!(sends.is_empty());
    }
    let (out, _) = reply(20, &"é".repeat(8192)).await;
    out.unwrap();
}
#[tokio::test]
async fn agent_body_follows_chunks_under_one_digest() {
    let id = Uuid::from_u128(0x1f6);
    for (mode, ok) in [(0, true), (23, false)] {
        let (url, _peer, server) = peer(mode).await;
        let root = TempDir::new().unwrap();
        install(root.path(), &url, base()["context"].clone());
        let client = AgentClient::open(root.path()).unwrap();
        let out = client.body(false, id).await;
        if ok {
            let value = out.unwrap().value;
            assert_eq!(value["body"], "é界!");
            assert_eq!(value["payload_digest"], "a".repeat(64));
        } else {
            assert!(out.is_err(), "digest change between chunks is refused");
        }
        server.abort();
    }
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

async fn comment_client(mode: usize) -> (TempDir, Arc<Peer>, tokio::task::JoinHandle<()>) {
    let (url, peer, server) = peer(mode).await;
    let root = TempDir::new().unwrap();
    install(root.path(), &url, base()["context"].clone());
    (root, peer, server)
}
#[tokio::test]
async fn comment_add_replays_and_conflicts_by_operation_id() {
    let (root, peer, server) = comment_client(0).await;
    let client = AgentClient::open(root.path()).unwrap();
    let binding = &base()["context"]["binding"];
    let op = Uuid::from_u128(0xc1);
    let body = "Finding: the adapter drops `executions`.\n\n- café 世界";
    let added = client.comment_add(op, body).await.unwrap();
    assert_eq!(added.exit_code, 0);
    let c = &added.value["comment"];
    assert_eq!(c["comment_id"], json!(op));
    assert_eq!(c["task_id"], binding["task_id"]);
    assert_eq!(c["author"]["kind"], "agent");
    assert_eq!(c["author"]["attempt_id"], binding["attempt_id"]);
    assert!(aq_agent_client::comments::valid_comment(c));
    // The request names no task: the credential binding decides the thread.
    let sent: Value = serde_json::from_slice(&peer.posts.lock().unwrap()[0]).unwrap();
    assert_eq!(sent, json!({"api_version":1,"operation_id":op,"body":body}));
    // Same ID and body: the same comment. A different body: id_conflict, exit 11.
    let replay = client.comment_add(op, body).await.unwrap();
    assert_eq!((replay.exit_code, &replay.value), (0, &added.value));
    let conflict = client.comment_add(op, "Something else").await.unwrap();
    assert_eq!(conflict.exit_code, 11);
    assert_eq!(conflict.value["code"], "id_conflict");
    assert_eq!(peer.comments.lock().unwrap().len(), 1);
    server.abort();
}
#[tokio::test]
async fn comment_bodies_are_checked_before_any_request() {
    let (root, peer, server) = comment_client(0).await;
    let client = AgentClient::open(root.path()).unwrap();
    for bad in [
        String::new(),
        " \n\t".into(),
        "bell\u{7}".into(),
        "x".repeat(16_385),
    ] {
        let error = client
            .comment_add(Uuid::new_v4(), &bad)
            .await
            .err()
            .unwrap();
        let failure = error.downcast_ref::<Failure>().unwrap();
        assert_eq!(
            (failure.code, failure.effect),
            ("invalid_comment_body", "not_applied")
        );
    }
    assert!(client.comment_add(Uuid::nil(), "ok").await.is_err());
    for (cursor, limit) in [
        (None, Some(0)),
        (None, Some(101)),
        (Some(""), None),
        (Some("a b"), None),
    ] {
        assert!(client.comment_list(cursor, limit).await.is_err());
    }
    assert_eq!(peer.calls.load(Ordering::SeqCst), 0);
    // Tab, LF, CR and a full 16 KiB of multibyte text are accepted.
    let ok = client
        .comment_add(Uuid::new_v4(), "a\tb\r\nc")
        .await
        .unwrap();
    assert_eq!(ok.exit_code, 0);
    let full = client
        .comment_add(Uuid::new_v4(), &"é".repeat(8192))
        .await
        .unwrap();
    assert_eq!(full.exit_code, 0);
    server.abort();
}
#[tokio::test]
async fn comment_list_pages_newest_first_and_checks_the_thread() {
    let (root, _peer, server) = comment_client(0).await;
    let client = AgentClient::open(root.path()).unwrap();
    for n in 1..=3u128 {
        let added = client
            .comment_add(Uuid::from_u128(n), &format!("note {n}"))
            .await;
        assert_eq!(added.unwrap().exit_code, 0);
    }
    let first = client.comment_list(None, Some(2)).await.unwrap();
    assert_eq!(first.exit_code, 0);
    let bodies = |v: &Value| -> Vec<String> {
        v["items"]
            .as_array()
            .unwrap()
            .iter()
            .map(|c| c["body"].as_str().unwrap().to_owned())
            .collect()
    };
    assert_eq!(bodies(&first.value), ["note 3", "note 2"]);
    let cursor = first.value["next_cursor"].as_str().unwrap().to_owned();
    let text = aq_agent_client::comments::render_page(&first.value);
    assert!(text.contains("2 comments (newest first)"), "{text}");
    assert!(text.contains("    note 3"), "{text}");
    assert!(
        text.contains(&format!("aq-agent comment list --cursor {cursor}")),
        "{text}"
    );
    let older = client.comment_list(Some(&cursor), Some(2)).await.unwrap();
    assert_eq!(bodies(&older.value), ["note 1"]);
    assert!(older.value["next_cursor"].is_null());
    server.abort();
}
#[tokio::test]
async fn comment_responses_outside_the_attempt_or_lost_stay_uncertain() {
    // Another attempt's author, or another task's comment, is never accepted.
    for mode in [30, 33] {
        let (root, _peer, server) = comment_client(mode).await;
        let client = AgentClient::open(root.path()).unwrap();
        let op = Uuid::new_v4();
        if mode == 30 {
            let error = client.comment_add(op, "x").await.err().unwrap();
            let failure = error.downcast_ref::<Failure>().unwrap();
            assert_eq!(
                (failure.operation_id, failure.effect),
                (Some(op), "unknown")
            );
        } else {
            client.comment_add(op, "x").await.unwrap();
            let error = client.comment_list(None, None).await.err().unwrap();
            assert_eq!(error.downcast_ref::<Failure>().unwrap().effect, "read_only");
        }
        server.abort();
    }
    // A lost response reports the ID as safe to retry; the retry returns the
    // comment recorded by the first send.
    let (root, peer, server) = comment_client(31).await;
    let client = AgentClient::open(root.path()).unwrap();
    let op = Uuid::new_v4();
    let error = client.comment_add(op, "lost once").await.err().unwrap();
    let failure = error.downcast_ref::<Failure>().unwrap();
    assert_eq!(failure.code, "remote_response_unknown");
    assert_eq!((failure.operation_id, failure.retryable), (Some(op), true));
    let retry = client.comment_add(op, "lost once").await.unwrap();
    assert_eq!(retry.value["comment"]["comment_id"], json!(op));
    assert_eq!(peer.comments.lock().unwrap().len(), 1);
    server.abort();
    // A revoked credential is a definite refusal, exit 14.
    let (root, _peer, server) = comment_client(32).await;
    let client = AgentClient::open(root.path()).unwrap();
    let refused = client.comment_list(None, None).await.unwrap();
    assert_eq!(
        (refused.exit_code, refused.value["code"].as_str()),
        (14, Some("session_revoked"))
    );
    server.abort();
}
#[tokio::test]
async fn actual_cli_comment_add_and_list() {
    use std::process::Stdio;
    use tokio::{io::AsyncWriteExt, process::Command};
    let (root, peer, server) = comment_client(0).await;
    let cli = |args: &[&str]| {
        let mut command = Command::new(env!("CARGO_BIN_EXE_aq-agent"));
        command
            .arg("--state-dir")
            .arg(root.path())
            .arg("comment")
            .args(args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        command
    };
    let op = Uuid::from_u128(0xa1).to_string();
    let output = cli(&["add", "Finding: **one**", "--operation-id", &op])
        .output()
        .await
        .unwrap();
    assert_eq!(output.status.code(), Some(0));
    let text = String::from_utf8(output.stdout).unwrap();
    assert!(
        text.starts_with(&format!("Comment {op} recorded on task ")),
        "{text}"
    );
    // Body from stdin; the trailing newline is dropped. --json prints the response.
    let mut child = cli(&["add", "--json"]).spawn().unwrap();
    let mut stdin = child.stdin.take().unwrap();
    stdin.write_all("Second\nline\n".as_bytes()).await.unwrap();
    drop(stdin);
    let output = child.wait_with_output().await.unwrap();
    assert_eq!(output.status.code(), Some(0));
    let added: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(added["comment"]["body"], "Second\nline");
    // Body from a file.
    let file = root.path().join("note.md");
    std::fs::write(&file, "From a file\n").unwrap();
    let output = cli(&["add", "--file", file.to_str().unwrap()])
        .output()
        .await
        .unwrap();
    assert_eq!(output.status.code(), Some(0));
    // Same ID, different body: conflict on stderr, exit 11.
    let output = cli(&["add", "changed", "--operation-id", &op])
        .output()
        .await
        .unwrap();
    assert_eq!(output.status.code(), Some(11));
    assert!(output.stdout.is_empty());
    assert!(String::from_utf8_lossy(&output.stderr).contains("id_conflict"));
    // A blank body never reaches the server: exit 15.
    let posts = peer.posts.lock().unwrap().len();
    let output = cli(&["add", "  "]).output().await.unwrap();
    assert_eq!(output.status.code(), Some(15));
    assert_eq!(peer.posts.lock().unwrap().len(), posts);
    let output = cli(&["list", "--limit", "101"]).output().await.unwrap();
    assert_eq!(output.status.code(), Some(2), "clap rejects the limit");
    // Text and JSON listings.
    let output = cli(&["list", "--limit", "2"]).output().await.unwrap();
    assert_eq!(output.status.code(), Some(0));
    let text = String::from_utf8(output.stdout).unwrap();
    assert!(text.contains("2 comments (newest first)"), "{text}");
    assert!(text.contains("    From a file"), "{text}");
    assert!(text.contains("agent (attempt "), "{text}");
    assert!(
        text.contains("aq-agent comment list --cursor tc1.2"),
        "{text}"
    );
    let output = cli(&["list", "--json", "--cursor", "tc1.2"])
        .output()
        .await
        .unwrap();
    let page: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(page["items"][0]["body"], "Finding: **one**");
    for stream in [&output.stdout, &output.stderr] {
        assert!(!String::from_utf8_lossy(stream).contains("aqa_"));
    }
    server.abort();
}
