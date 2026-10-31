use aq_agent_api::Binding;
use aq_agent_client::{
    client::{AgentClient, AgentEgressCredential},
    context::{ContextContent, StartupContext},
    node::NodeClient,
    provision::{Allocator, IssuanceState, Limits},
};
use aq_session_protocol::{Digest, SessionIdentity, Validate, canonical_digest};
use axum::{
    Json, Router,
    extract::State,
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    routing::post,
};
use serde_json::{Value, json};
use std::{
    os::unix::fs::{MetadataExt, PermissionsExt, symlink},
    path::PathBuf,
    sync::{Arc, Mutex},
};
use uuid::Uuid;
fn startup() -> (StartupContext, Value) {
    let base: Value = serde_json::from_str(include_str!("fixtures/agent-base.json")).unwrap();
    let context = base["context"].clone();
    let b: Binding = serde_json::from_value(context["binding"].clone()).unwrap();
    let mut offer: aq_session_protocol::ProfiledOffer =
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
    offer.manifest.bindings.session = session;
    offer.manifest.bindings.attempt_id = b.attempt_id;
    offer.manifest.bindings.assignment_generation = b.generation;
    offer.manifest.bindings.graph_access_profile.digest =
        serde_json::from_value(json!(b.profile_digest)).unwrap();
    offer.manifest.bindings.project_id = b.project_id;
    offer.manifest.bindings.task_id = b.task_id;
    offer.manifest.bindings.graph_id = b.workspace_id;
    let text = "fixture startup".to_owned();
    let d = Digest::of(text.as_bytes());
    offer.manifest.context.references[0].artifact.digest = d.clone();
    offer.manifest.context.references[0].bytes = text.len() as u64;
    offer.binding.launch_manifest_digest = canonical_digest(&offer.manifest).unwrap();
    offer.validate().unwrap();
    let startup = StartupContext {
        binding: b,
        profiled_offer: offer,
        contents: vec![ContextContent { digest: d, text }],
    };
    startup.validate().unwrap();
    (startup, context)
}
fn changed_session(mut v: StartupContext) -> StartupContext {
    let session = Uuid::new_v4();
    let attempt = Uuid::new_v4();
    v.binding.session_id = session;
    v.binding.attempt_id = attempt;
    let identity = SessionIdentity::Remote {
        node_id: v.binding.node_id,
        boot_id: v.binding.boot_id,
        attempt_id: attempt,
        assignment_generation: v.binding.generation,
        session_id: session,
    };
    v.profiled_offer.binding.attempt_id = attempt;
    v.profiled_offer.binding.session = identity.clone();
    v.profiled_offer.manifest.bindings.attempt_id = attempt;
    v.profiled_offer.manifest.bindings.session = identity;
    v.profiled_offer.binding.launch_manifest_digest =
        canonical_digest(&v.profiled_offer.manifest).unwrap();
    v.validate().unwrap();
    v
}
fn private_root(temp: &tempfile::TempDir) -> PathBuf {
    temp.path().join("allocations")
}
struct Peer {
    context: Value,
    requests: Vec<Value>,
    version: u64,
    lose_first: bool,
    wrong_binding: bool,
}
async fn issue(
    State(s): State<Arc<Mutex<Peer>>>,
    headers: HeaderMap,
    Json(r): Json<Value>,
) -> Response {
    assert_eq!(
        headers["authorization"],
        format!("Bearer {}", "n".repeat(40))
    );
    let mut s = s.lock().unwrap();
    s.requests.push(r.clone());
    let b = s.context["binding"].clone();
    assert_eq!(
        r["identity"],
        json!({"node_id":b["node_id"],"boot_id":b["boot_id"]})
    );
    assert_eq!(
        r["attempt"],
        json!({"attempt_id":b["attempt_id"],"session_id":b["session_id"],"generation":b["generation"]})
    );
    assert_eq!(r.as_object().unwrap().len(), 3);
    if r["expected_credential_version"] != s.version {
        return (
            StatusCode::CONFLICT,
            [("X-AQ-Agent-API-Version", "1")],
            Json(json!({"refused":true})),
        )
            .into_response();
    }
    s.version += 1;
    if s.lose_first && s.requests.len() == 1 {
        return (
            StatusCode::BAD_GATEWAY,
            [("X-AQ-Agent-API-Version", "1")],
            Json(json!({"unknown":true})),
        )
            .into_response();
    }
    let mut context = s.context.clone();
    context["credential"]["version"] = s.version.to_string().into();
    if s.wrong_binding {
        context["binding"]["session_id"] = Uuid::new_v4().to_string().into();
    }
    (StatusCode::OK,[("X-AQ-Agent-API-Version","1")],Json(json!({"api_version":1,"token":format!("aqa_{}-{}","t".repeat(40),s.version),"context":context}))).into_response()
}
async fn peer(
    context: Value,
    lose_first: bool,
    wrong_binding: bool,
) -> (NodeClient, Arc<Mutex<Peer>>, tokio::task::JoinHandle<()>) {
    let node_id = context["binding"]["node_id"]
        .as_str()
        .unwrap()
        .parse()
        .unwrap();
    let peer = Arc::new(Mutex::new(Peer {
        context,
        requests: vec![],
        version: 0,
        lose_first,
        wrong_binding,
    }));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let node = NodeClient::new(
        &format!("http://{}", listener.local_addr().unwrap()),
        node_id,
        "n".repeat(40),
        None,
    )
    .unwrap();
    let app = Router::new()
        .route("/v2/worker/agent-session", post(issue))
        .with_state(peer.clone());
    let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    (node, peer, server)
}
fn one_session() -> Limits {
    Limits {
        max_sessions: 1,
        total_reserved_bytes: 66 * 1024 * 1024,
        session_reserved_bytes: 64 * 1024 * 1024,
    }
}
#[tokio::test]
async fn private_atomic_install_has_real_context_and_no_node_credential() {
    let (startup, context) = startup();
    let session = startup.binding.session_id;
    let node_id = startup.binding.node_id;
    let (node, peer, server) = peer(context, false, false).await;
    let temp = tempfile::tempdir().unwrap();
    let root = private_root(&temp);
    let mut a = Allocator::open(&root, node_id, one_session()).unwrap();
    let intent = Uuid::new_v4();
    assert_eq!(
        a.allocate(&node, startup, intent, 0, None).unwrap().state,
        IssuanceState::Prepared
    );
    assert!(a.ready_path(session).is_err());
    assert!(!root.join(session.to_string()).join("client.json").exists());
    let path = a.issue_once(&node, session, intent).await.unwrap();
    assert_eq!(a.issue_once(&node, session, intent).await.unwrap(), path);
    assert_eq!(peer.lock().unwrap().requests.len(), 1);
    assert_eq!(
        a.status(session).unwrap().state,
        IssuanceState::Ready {
            credential_version: 1
        }
    );
    for name in ["context.json", "client.json", "credential.json"] {
        let bytes = std::fs::read(path.join(name)).unwrap();
        assert!(!String::from_utf8_lossy(&bytes).contains(&"n".repeat(40)));
        let m = std::fs::metadata(path.join(name)).unwrap();
        assert_eq!(m.mode() & 0o777, 0o600);
        assert_eq!(m.nlink(), 1);
    }
    let client = AgentClient::open(&path).unwrap();
    assert_eq!(client.cached_context().unwrap()["cached"], true);
    assert!(
        !String::from_utf8_lossy(&std::fs::read(root.join("allocations.json")).unwrap())
            .contains("aqa_")
    );
    server.abort();
}
#[tokio::test]
async fn lost_reply_never_reissues_plaintext_and_explicit_cas_rotation_recovers() {
    let (startup, context) = startup();
    let (session, node_id) = (startup.binding.session_id, startup.binding.node_id);
    let (node, peer, server) = peer(context, true, false).await;
    let temp = tempfile::tempdir().unwrap();
    let root = private_root(&temp);
    let id = Uuid::new_v4();
    let mut a = Allocator::open(&root, node_id, one_session()).unwrap();
    a.allocate(&node, startup.clone(), id, 0, None).unwrap();
    assert!(a.issue_once(&node, session, id).await.is_err());
    assert_eq!(a.status(session).unwrap().state, IssuanceState::Unknown);
    drop(a);
    let mut a = Allocator::open(&root, node_id, one_session()).unwrap();
    assert!(a.recover_local(session).unwrap().is_none());
    assert!(a.issue_once(&node, session, id).await.is_err());
    assert_eq!(peer.lock().unwrap().requests.len(), 1);
    assert!(
        a.prepare_rotation(&node, session, Uuid::new_v4(), 2)
            .is_err()
    );
    let refused = Uuid::new_v4();
    a.prepare_rotation(&node, session, refused, 0).unwrap();
    assert!(a.issue_once(&node, session, refused).await.is_err());
    assert_eq!(a.status(session).unwrap().state, IssuanceState::Unknown);
    assert_eq!(peer.lock().unwrap().requests.len(), 2);
    assert!(a.issue_once(&node, session, refused).await.is_err());
    assert_eq!(peer.lock().unwrap().requests.len(), 2);
    let next = Uuid::new_v4();
    a.prepare_rotation(&node, session, next, 1).unwrap();
    let path = a.issue_once(&node, session, next).await.unwrap();
    let credential: Value =
        serde_json::from_slice(&std::fs::read(path.join("credential.json")).unwrap()).unwrap();
    assert_eq!(credential["context"]["credential"]["version"], "2");
    assert_eq!(peer.lock().unwrap().requests.len(), 3);
    assert!(a.issue_once(&node, session, id).await.is_err());
    server.abort();
}
#[tokio::test]
async fn allocation_quota_and_binding_survive_restart_without_implicit_reclaim() {
    let (startup, context) = startup();
    let (node, _, server) = peer(context, false, false).await;
    let temp = tempfile::tempdir().unwrap();
    let root = private_root(&temp);
    let id = Uuid::new_v4();
    let mut a = Allocator::open(&root, startup.binding.node_id, one_session()).unwrap();
    a.allocate(&node, startup.clone(), id, 0, None).unwrap();
    let reserved = a.reserved_bytes().unwrap();
    assert_eq!(reserved, 66 * 1024 * 1024);
    assert!(
        a.allocate(
            &node,
            changed_session(startup.clone()),
            Uuid::new_v4(),
            0,
            None
        )
        .is_err()
    );
    drop(a);
    let mut a = Allocator::open(&root, startup.binding.node_id, one_session()).unwrap();
    assert_eq!(a.reserved_bytes().unwrap(), reserved);
    assert!(
        a.allocate(&node, changed_session(startup.clone()), id, 0, None)
            .is_err()
    );
    assert!(a.allocate(&node, startup, id, 1, None).is_err());
    server.abort();
}
#[tokio::test]
async fn mismatched_real_response_stays_unknown_and_never_publishes_config() {
    let (startup, context) = startup();
    let (session, node) = (startup.binding.session_id, startup.binding.node_id);
    let (client, peer, server) = peer(context, false, true).await;
    let temp = tempfile::tempdir().unwrap();
    let root = private_root(&temp);
    let mut a = Allocator::open(&root, node, one_session()).unwrap();
    let id = Uuid::new_v4();
    a.allocate(&client, startup, id, 0, None).unwrap();
    assert!(a.issue_once(&client, session, id).await.is_err());
    assert_eq!(a.status(session).unwrap().state, IssuanceState::Unknown);
    assert!(!root.join(session.to_string()).join("client.json").exists());
    assert!(a.issue_once(&client, session, id).await.is_err());
    assert_eq!(peer.lock().unwrap().requests.len(), 1);
    server.abort();
}

#[tokio::test]
async fn exact_retained_response_recovers_local_install_without_http() {
    let (startup, context) = startup();
    let (session, node_id) = (startup.binding.session_id, startup.binding.node_id);
    let (node, peer, server) = peer(context, false, false).await;
    let temp = tempfile::tempdir().unwrap();
    let root = private_root(&temp);
    let id = Uuid::new_v4();
    let mut a = Allocator::open(&root, node_id, one_session()).unwrap();
    a.allocate(&node, startup, id, 0, None).unwrap();
    let path = a.issue_once(&node, session, id).await.unwrap();
    drop(a);
    // A crash prefix after atomic credential/context fsync and before config/state publication.
    let mut state: Value =
        serde_json::from_slice(&std::fs::read(root.join("allocations.json")).unwrap()).unwrap();
    state["sessions"][session.to_string()]["intents"][0]["state"] = json!({"kind":"unknown"});
    std::fs::write(
        root.join("allocations.json"),
        serde_json::to_vec(&state).unwrap(),
    )
    .unwrap();
    std::fs::remove_file(path.join("client.json")).unwrap();
    let mut a = Allocator::open(&root, node_id, one_session()).unwrap();
    assert_eq!(a.recover_local(session).unwrap(), Some(path.clone()));
    assert!(AgentClient::open(&path).is_ok());
    assert_eq!(peer.lock().unwrap().requests.len(), 1);
    server.abort();
}
#[tokio::test]
async fn symlink_permissions_and_unexpected_temp_refuse_before_http() {
    let (startup, context) = startup();
    let (session, node_id) = (startup.binding.session_id, startup.binding.node_id);
    let (node, peer, server) = peer(context, false, false).await;
    let temp = tempfile::tempdir().unwrap();
    let root = private_root(&temp);
    let mut a = Allocator::open(&root, node_id, one_session()).unwrap();
    let id = Uuid::new_v4();
    a.allocate(&node, startup, id, 0, None).unwrap();
    let private = root.join(session.to_string());
    let victim = temp.path().join("victim");
    std::fs::write(&victim, b"unchanged").unwrap();
    symlink(&victim, private.join("credential.json")).unwrap();
    assert!(a.issue_once(&node, session, id).await.is_err());
    assert_eq!(std::fs::read(&victim).unwrap(), b"unchanged");
    std::fs::remove_file(private.join("credential.json")).unwrap();
    std::fs::set_permissions(
        private.join("context.json"),
        std::fs::Permissions::from_mode(0o644),
    )
    .unwrap();
    assert!(a.issue_once(&node, session, id).await.is_err());
    std::fs::set_permissions(
        private.join("context.json"),
        std::fs::Permissions::from_mode(0o600),
    )
    .unwrap();
    std::fs::write(private.join(".orphan.tmp"), b"retained unknown write").unwrap();
    assert!(a.issue_once(&node, session, id).await.is_err());
    assert_eq!(peer.lock().unwrap().requests.len(), 0);
    assert_eq!(a.status(session).unwrap().state, IssuanceState::Prepared);
    server.abort();
}
#[test]
fn node_owner_lock_and_conservative_minimum_are_enforced() {
    let (startup, _) = startup();
    let temp = tempfile::tempdir().unwrap();
    let root = private_root(&temp);
    let a = Allocator::open(&root, startup.binding.node_id, one_session()).unwrap();
    assert!(Allocator::open(&root, startup.binding.node_id, one_session()).is_err());
    drop(a);
    let mut tiny = one_session();
    tiny.session_reserved_bytes -= 1;
    assert!(Allocator::open(&root, startup.binding.node_id, tiny).is_err());
    let linked = temp.path().join("linked");
    symlink(&root, &linked).unwrap();
    assert!(Allocator::open(&linked, startup.binding.node_id, one_session()).is_err());
}
#[test]
fn explicit_agent_egress_never_copies_node_cf_identity() {
    let (startup, _) = startup();
    let node = NodeClient::new(
        "https://fixture.invalid",
        startup.binding.node_id,
        "n".repeat(40),
        Some(AgentEgressCredential {
            client_id: "node-cf-id".into(),
            client_secret: "node-cf-secret".into(),
        }),
    )
    .unwrap();
    let temp = tempfile::tempdir().unwrap();
    let root = private_root(&temp);
    let mut a = Allocator::open(&root, startup.binding.node_id, one_session()).unwrap();
    assert!(
        a.allocate(&node, startup.clone(), Uuid::new_v4(), 0, None)
            .is_err()
    );
    assert!(
        a.allocate(
            &node,
            startup.clone(),
            Uuid::new_v4(),
            0,
            Some(AgentEgressCredential {
                client_id: "node-cf-id".into(),
                client_secret: "different".into()
            })
        )
        .is_err()
    );
    let session = startup.binding.session_id;
    a.allocate(
        &node,
        startup,
        Uuid::new_v4(),
        0,
        Some(AgentEgressCredential {
            client_id: "agent-cf-id".into(),
            client_secret: "agent-cf-secret".into(),
        }),
    )
    .unwrap();
    let egress =
        std::fs::read_to_string(root.join(session.to_string()).join("agent-egress.json")).unwrap();
    assert!(!egress.contains("node-cf"));
    assert!(egress.contains("agent-cf"));
}

#[tokio::test]
async fn explicit_refresh_atomically_replaces_credential_while_agent_journal_is_open() {
    let (startup, context) = startup();
    let (session, node_id) = (startup.binding.session_id, startup.binding.node_id);
    let (node, peer, server) = peer(context, false, false).await;
    let temp = tempfile::tempdir().unwrap();
    let root = private_root(&temp);
    let mut a = Allocator::open(&root, node_id, one_session()).unwrap();
    let first = Uuid::new_v4();
    a.allocate(&node, startup, first, 0, None).unwrap();
    let path = a.issue_once(&node, session, first).await.unwrap();
    let old = std::fs::read(path.join("credential.json")).unwrap();
    let old_inode = std::fs::metadata(path.join("credential.json"))
        .unwrap()
        .ino();
    let _running_agent = AgentClient::open(&path).unwrap();
    let second = Uuid::new_v4();
    a.prepare_rotation(&node, session, second, 1).unwrap();
    assert!(a.ready_path(session).is_err());
    a.issue_once(&node, session, second).await.unwrap();
    let new = std::fs::read(path.join("credential.json")).unwrap();
    assert_ne!(old, new);
    assert_ne!(
        old_inode,
        std::fs::metadata(path.join("credential.json"))
            .unwrap()
            .ino()
    );
    assert_eq!(
        serde_json::from_slice::<Value>(&new).unwrap()["context"]["credential"]["version"],
        "2"
    );
    assert_eq!(a.ready_path(session).unwrap(), path);
    assert_eq!(peer.lock().unwrap().requests.len(), 2);
    server.abort();
}
#[tokio::test]
async fn changed_startup_bytes_and_impossible_disk_reservation_refuse() {
    let (startup, context) = startup();
    let (node, peer, server) = peer(context, false, false).await;
    let temp = tempfile::tempdir().unwrap();
    let root = private_root(&temp);
    let mut a = Allocator::open(&root, startup.binding.node_id, one_session()).unwrap();
    let id = Uuid::new_v4();
    a.allocate(&node, startup.clone(), id, 0, None).unwrap();
    let mut changed = startup.clone();
    changed.contents[0].text.push('!');
    let digest = Digest::of(changed.contents[0].text.as_bytes());
    changed.contents[0].digest = digest.clone();
    changed.profiled_offer.manifest.context.references[0]
        .artifact
        .digest = digest;
    changed.profiled_offer.manifest.context.references[0].bytes =
        changed.contents[0].text.len() as u64;
    changed.profiled_offer.binding.launch_manifest_digest =
        canonical_digest(&changed.profiled_offer.manifest).unwrap();
    changed.validate().unwrap();
    assert!(a.allocate(&node, changed, id, 0, None).is_err());
    assert_eq!(peer.lock().unwrap().requests.len(), 0);
    let impossible = Limits {
        max_sessions: 1,
        session_reserved_bytes: u64::MAX / 2,
        total_reserved_bytes: u64::MAX,
    };
    let mut huge = Allocator::open(
        &temp.path().join("huge"),
        startup.binding.node_id,
        impossible,
    )
    .unwrap();
    assert!(
        huge.allocate(&node, startup, Uuid::new_v4(), 0, None)
            .is_err()
    );
    assert_eq!(huge.reserved_bytes().unwrap(), 2 * 1024 * 1024);
    server.abort();
}

#[tokio::test]
async fn context_bundle_bound_is_distinct_from_one_megabyte_wire_bound() {
    let (mut startup, context) = startup();
    for ch in ['x', 'y'] {
        let text = ch.to_string().repeat(600_000);
        let digest = Digest::of(text.as_bytes());
        let mut reference = startup.profiled_offer.manifest.context.references[0].clone();
        reference.artifact.digest = digest.clone();
        reference.bytes = text.len() as u64;
        reference.required = false;
        reference.delivery = aq_session_protocol::ContextDelivery::OnDemand;
        startup
            .profiled_offer
            .manifest
            .context
            .references
            .push(reference);
        startup.contents.push(ContextContent { digest, text });
    }
    startup.profiled_offer.binding.launch_manifest_digest =
        canonical_digest(&startup.profiled_offer.manifest).unwrap();
    startup.validate().unwrap();
    assert!(serde_json::to_vec(&startup).unwrap().len() > aq_agent_api::MAX_DOCUMENT_BYTES);
    let (session, node_id) = (startup.binding.session_id, startup.binding.node_id);
    let (node, _, server) = peer(context, false, false).await;
    let temp = tempfile::tempdir().unwrap();
    let mut a = Allocator::open(&private_root(&temp), node_id, one_session()).unwrap();
    let id = Uuid::new_v4();
    a.allocate(&node, startup, id, 0, None).unwrap();
    let path = a.issue_once(&node, session, id).await.unwrap();
    assert!(AgentClient::open(&path).is_ok());
    assert_eq!(a.ready_path(session).unwrap(), path);
    server.abort();
}

#[tokio::test]
async fn explicit_new_intent_can_recover_loss_before_server_commit_at_same_cas_version() {
    use std::sync::atomic::{AtomicUsize, Ordering};
    async fn before_commit(
        State(state): State<Arc<(AtomicUsize, Value)>>,
        Json(request): Json<Value>,
    ) -> Response {
        assert_eq!(request["expected_credential_version"], 0);
        if state.0.fetch_add(1, Ordering::SeqCst) == 0 {
            return (
                StatusCode::BAD_GATEWAY,
                [("X-AQ-Agent-API-Version", "1")],
                Json(json!({"unknown":true})),
            )
                .into_response();
        }
        (
            StatusCode::OK,
            [("X-AQ-Agent-API-Version", "1")],
            Json(
                json!({"api_version":1,"token":format!("aqa_{}","b".repeat(40)),"context":state.1}),
            ),
        )
            .into_response()
    }
    let (startup, context) = startup();
    let (session, node_id) = (startup.binding.session_id, startup.binding.node_id);
    let state = Arc::new((AtomicUsize::new(0), context));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let node = NodeClient::new(
        &format!("http://{}", listener.local_addr().unwrap()),
        node_id,
        "n".repeat(40),
        None,
    )
    .unwrap();
    let app = Router::new()
        .route("/v2/worker/agent-session", post(before_commit))
        .with_state(state.clone());
    let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    let temp = tempfile::tempdir().unwrap();
    let root = private_root(&temp);
    let first = Uuid::new_v4();
    let mut a = Allocator::open(&root, node_id, one_session()).unwrap();
    a.allocate(&node, startup, first, 0, None).unwrap();
    assert!(a.issue_once(&node, session, first).await.is_err());
    assert_eq!(state.0.load(Ordering::SeqCst), 1);
    assert!(a.issue_once(&node, session, first).await.is_err());
    assert_eq!(state.0.load(Ordering::SeqCst), 1);
    let next = Uuid::new_v4();
    a.prepare_rotation(&node, session, next, 0).unwrap();
    let path = a.issue_once(&node, session, next).await.unwrap();
    assert_eq!(state.0.load(Ordering::SeqCst), 2);
    assert_eq!(
        a.status(session).unwrap().state,
        IssuanceState::Ready {
            credential_version: 1
        }
    );
    assert!(AgentClient::open(&path).is_ok());
    drop(a);
    let a = Allocator::open(&root, node_id, one_session()).unwrap();
    assert_eq!(a.ready_path(session).unwrap(), path);
    server.abort();
}
