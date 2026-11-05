//! B7: scoped AQ access for agents inside sandboxes.
use aq_agent_client::{
    client::{AgentClient, ClientConfig, Failure},
    context::{ContextContent, StartupContext},
    contract::{Binding, digest},
    provision::SessionDelivery,
};
use aq_session_protocol::{
    ContextDelivery, ContextRef, Digest, ProfiledOffer, SessionIdentity, canonical_digest,
};
use aq_worker::native::runner::sandbox::{
    agent_access::{AgentAccess, AgentGrant},
    private::RunnerFiles,
    proxy::AqApiProxy,
};
use serde_json::{Value, json};
use std::{
    os::unix::fs::PermissionsExt,
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use uuid::Uuid;

const ACCESS_SECRET: &str = "runner-only-access-secret";
fn token() -> String {
    format!("aqa_{}", "t".repeat(40))
}
fn base() -> Value {
    serde_json::from_str(include_str!(
        "../../aq-agent-client/tests/fixtures/agent-base.json"
    ))
    .unwrap()
}
fn startup(context: &Value) -> StartupContext {
    let b: Binding = serde_json::from_value(context["binding"].clone()).unwrap();
    let mut offer: ProfiledOffer = serde_json::from_str(include_str!(
        "../../aq-agent-client/tests/fixtures/profiled-offer.json"
    ))
    .unwrap();
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
    let text = "Required instructions.".to_owned();
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
    StartupContext {
        binding: b,
        profiled_offer: offer,
        contents: vec![content],
    }
}
/// What the node's `Allocator::delivery` hands over: the node origin, never
/// Access egress, and a credential expiring at `expires_at`.
fn delivery(expires_at: &str) -> SessionDelivery {
    let mut context = base()["context"].clone();
    context["credential"]["expires_at"] = json!(expires_at);
    let bundle = startup(&context)
        .with_graph_context(context.clone())
        .unwrap();
    let config = ClientConfig {
        format_version: 1,
        server_url: "https://aq.example.invalid".into(),
        binding: bundle.binding.clone(),
        edge_authentication: false,
    };
    SessionDelivery {
        client: serde_json::to_vec(&config).unwrap(),
        context: serde_json::to_vec(&bundle).unwrap(),
        credential: serde_json::to_vec(&json!({"api_version":1,"token":token(),"context":context}))
            .unwrap(),
    }
}
fn ms(rfc3339: &str) -> u64 {
    chrono::DateTime::parse_from_rfc3339(rfc3339)
        .unwrap()
        .timestamp_millis() as u64
}
fn grant(not_after: &str) -> AgentGrant {
    let b = &base()["context"]["binding"];
    AgentGrant {
        attempt_id: b["attempt_id"].as_str().unwrap().parse().unwrap(),
        node_id: b["node_id"].as_str().unwrap().parse().unwrap(),
        not_after_ms: ms(not_after),
        live: true,
    }
}
fn agent_root() -> (tempfile::TempDir, PathBuf) {
    let root = tempfile::tempdir().unwrap();
    std::fs::set_permissions(root.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
    let state = std::fs::canonicalize(root.path()).unwrap().join("agent");
    (root, state)
}
fn has_credential(state: &Path) -> bool {
    state.join("credential.json").exists()
}

#[tokio::test]
async fn agent_credential_expires_with_attempt() {
    let (_root, state) = agent_root();
    let access = AgentAccess::new(state.clone(), "http://127.0.0.1:9");
    let now = ms("2020-01-01T00:00:00Z");
    let attempt = grant("2021-01-01T00:00:00Z");

    // A credential outliving the attempt grant is refused, not clamped.
    let long = delivery("2099-01-01T00:00:00Z");
    assert!(format!("{long:?}").contains("redacted"));
    assert!(!format!("{long:?}").contains("aqa_"));
    let refused = access.provision(&long, &attempt, now).unwrap_err();
    assert!(refused.to_string().contains("outlives the attempt grant"));
    assert!(!format!("{refused:#}").contains(&token()));
    assert!(!has_credential(&state));

    // A credential already expired at delivery is refused.
    let stale = delivery("2019-12-31T00:00:00Z");
    assert!(access.provision(&stale, &attempt, now).is_err());

    // Access egress material is never installed for a sandbox agent.
    let mut edged = delivery("2020-06-01T00:00:00Z");
    let mut cfg: Value = serde_json::from_slice(&edged.client).unwrap();
    cfg["edge_authentication"] = json!(true);
    edged.client = serde_json::to_vec(&cfg).unwrap();
    assert!(access.provision(&edged, &attempt, now).is_err());

    // A delivery for another attempt or node is refused before installation.
    let inside = delivery("2020-06-01T00:00:00Z");
    for other in [
        AgentGrant {
            attempt_id: Uuid::new_v4(),
            ..attempt
        },
        AgentGrant {
            node_id: Uuid::new_v4(),
            ..attempt
        },
    ] {
        assert!(access.provision(&inside, &other, now).is_err());
    }
    assert!(!has_credential(&state));

    // Expired grants and lost authority stop provisioning.
    assert!(
        access
            .provision(&inside, &attempt, attempt.not_after_ms)
            .is_err()
    );
    let lost = AgentGrant {
        live: false,
        ..attempt
    };
    assert!(access.provision(&inside, &lost, now).is_err());

    // An in-grant credential installs; its expiry never exceeds the grant.
    let installed = access.provision(&inside, &attempt, now).unwrap();
    assert_eq!(installed.path, state);
    assert_eq!(installed.expires_at_ms, ms("2020-06-01T00:00:00Z"));
    assert!(installed.expires_at_ms <= attempt.not_after_ms);
    assert_eq!(
        std::fs::metadata(&state).unwrap().permissions().mode() & 0o777,
        0o700
    );
    let config: Value =
        serde_json::from_slice(&std::fs::read(state.join("client.json")).unwrap()).unwrap();
    assert_eq!(config["server_url"], "http://127.0.0.1:9");
    assert_eq!(config["edge_authentication"], false);
    assert!(!state.join("agent-egress.json").exists());

    // The installed credential (2020-06-01) is now past expires_at by the
    // real clock: the client refuses it before any request (aq-agent exits 14).
    let Err(refused) = AgentClient::open(&state)
        .unwrap()
        .command(base()["command_requests"]["correct_record"].clone())
        .await
    else {
        panic!("expired credential was used");
    };
    assert_eq!(
        refused.downcast_ref::<Failure>().unwrap().code,
        "authentication_or_authority_refused"
    );

    // Stop (cancel/expiry/revocation) removes the credential and is permanent.
    access.stop().unwrap();
    assert!(access.stopped());
    assert!(!has_credential(&state));
    assert!(access.provision(&inside, &attempt, now).is_err());
    assert!(!has_credential(&state));
    access.stop().unwrap();
}

#[derive(Default)]
struct Seen {
    heads: Vec<String>,
    bodies: Vec<Value>,
}
/// Raw loopback AQ upstream behind the proxy: answers context and commands.
async fn upstream(seen: Arc<Mutex<Seen>>) -> String {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let origin = format!("http://{}", listener.local_addr().unwrap());
    tokio::spawn(async move {
        loop {
            let (socket, _) = listener.accept().await.unwrap();
            let seen = seen.clone();
            tokio::spawn(async move {
                let mut socket = BufReader::new(socket);
                loop {
                    let mut head = String::new();
                    loop {
                        let mut line = String::new();
                        if socket.read_line(&mut line).await.unwrap() == 0 {
                            return;
                        }
                        head.push_str(&line);
                        if line == "\r\n" {
                            break;
                        }
                    }
                    let lower = head.to_lowercase();
                    let length: usize = lower
                        .lines()
                        .find_map(|l| l.strip_prefix("content-length: "))
                        .map(|v| v.trim().parse().unwrap())
                        .unwrap_or(0);
                    let mut body = vec![0; length];
                    socket.read_exact(&mut body).await.unwrap();
                    let fixture = base();
                    let value = if lower.starts_with("get /v1/agent/context ") {
                        fixture["context"].clone()
                    } else if lower.starts_with("post /v1/agent/graph/commands ") {
                        let request: Value = serde_json::from_slice(&body).unwrap();
                        let mut value = fixture["operation_statuses"]["confirmed"].clone();
                        value["operation_id"] = request["operation_id"].clone();
                        value["payload_digest"] = json!(digest(&request).unwrap());
                        seen.lock().unwrap().bodies.push(request);
                        value
                    } else {
                        panic!("unexpected upstream route");
                    };
                    seen.lock().unwrap().heads.push(lower);
                    let body = serde_json::to_vec(&value).unwrap();
                    let head = format!(
                        "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nX-AQ-Agent-API-Version: 1\r\nCache-Control: no-store\r\nContent-Length: {}\r\n\r\n",
                        body.len()
                    );
                    let socket = socket.get_mut();
                    socket.write_all(head.as_bytes()).await.unwrap();
                    socket.write_all(&body).await.unwrap();
                }
            });
        }
    });
    origin
}

#[tokio::test]
async fn agent_reads_scoped_context_and_writes_versioned_mutation() {
    let root = tempfile::tempdir().unwrap();
    let files = RunnerFiles::create(&root.path().join("runner")).unwrap();
    files
        .write_secret(
            "access.json",
            format!(r#"{{"client_id":"runner-only-id","client_secret":"{ACCESS_SECRET}"}}"#)
                .as_bytes(),
        )
        .unwrap();
    let seen = Arc::new(Mutex::new(Seen::default()));
    let origin = upstream(seen.clone()).await;
    let proxy = AqApiProxy::development_loopback(&origin, &files.root().join("access.json"))
        .await
        .unwrap();

    let (_agent_root, state) = agent_root();
    let access = AgentAccess::new(state.clone(), proxy.origin());
    let now = chrono::Utc::now().timestamp_millis() as u64;
    let attempt = grant("2099-01-01T00:00:00Z");
    access
        .provision(&delivery("2099-01-01T00:00:00Z"), &attempt, now)
        .unwrap();
    let config: Value =
        serde_json::from_slice(&std::fs::read(state.join("client.json")).unwrap()).unwrap();
    assert_eq!(config["server_url"], json!(proxy.origin()));

    let mut client = AgentClient::open(&state).unwrap();
    let context = client.context().await.unwrap();
    assert_eq!(context.exit_code, 0);
    assert_eq!(context.value["binding"], base()["context"]["binding"]);

    let request = base()["command_requests"]["correct_record"].clone();
    assert_eq!(request["commands"][0]["expected_version"], "2");
    let receipt = client.command(request.clone()).await.unwrap();
    assert_eq!(receipt.exit_code, 0);

    let seen = std::mem::take(&mut *seen.lock().unwrap());
    assert_eq!(seen.heads.len(), 2);
    for head in &seen.heads {
        assert!(head.contains(&format!("authorization: bearer {}\r\n", token())));
        assert!(head.contains("cf-access-client-id: runner-only-id\r\n"));
        assert!(head.contains(&format!("cf-access-client-secret: {ACCESS_SECRET}\r\n")));
    }
    assert_eq!(seen.bodies, vec![request]);
    assert_eq!(seen.bodies[0]["commands"][0]["expected_version"], "2");
    // What the agent sees never carries the Access secret or its token.
    for out in [&context.value, &receipt.value] {
        let text = out.to_string();
        assert!(!text.contains(ACCESS_SECRET) && !text.contains(&token()));
    }
    for name in ["client.json", "context.json"] {
        let bytes = std::fs::read_to_string(state.join(name)).unwrap();
        assert!(!bytes.contains(ACCESS_SECRET) && !bytes.contains("runner-only-id"));
    }
    drop(seen);
    access.stop().unwrap();
    proxy.stop().await.unwrap();
}
