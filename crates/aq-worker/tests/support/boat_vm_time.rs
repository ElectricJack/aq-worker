use super::*;
use aq_worker::client::Client;
use axum::{
    Router,
    extract::State,
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    routing::post,
};
use std::{
    collections::BTreeMap,
    sync::{Arc, Mutex},
};

#[derive(Default)]
struct Ledger {
    facts: BTreeMap<Uuid, VmTimeFact>,
    requests: Vec<VmTimeFact>,
    unavailable: bool,
    lose_reply: bool,
    reject_receipt: bool,
}
struct Server {
    url: String,
    data: Arc<Mutex<Ledger>>,
    task: tokio::task::JoinHandle<()>,
}
impl Drop for Server {
    fn drop(&mut self) {
        self.task.abort();
    }
}
impl Server {
    async fn start() -> Self {
        async fn report(
            State(state): State<Arc<Mutex<Ledger>>>,
            headers: HeaderMap,
            axum::Json(fact): axum::Json<VmTimeFact>,
        ) -> Response {
            assert_eq!(
                headers["authorization"],
                "Bearer fixture-server-node-credential"
            );
            assert_eq!(headers["cf-access-client-id"], "fixture-access-id");
            assert_eq!(headers["cf-access-client-secret"], "fixture-access-secret");
            assert!(!headers.contains_key("x-boat-org"));
            let mut d = state.lock().unwrap();
            d.requests.push(fact.clone());
            if d.unavailable {
                return StatusCode::SERVICE_UNAVAILABLE.into_response();
            }
            if let Some(old) = d.facts.get(&fact.attempt_id) {
                assert_eq!(old.sandbox_id, fact.sandbox_id);
                assert_eq!(old.size, fact.size);
                assert_eq!(old.started_at_ms, fact.started_at_ms);
                if old.stopped_at_ms.is_some() {
                    assert_eq!(old.stopped_at_ms, fact.stopped_at_ms);
                }
            }
            d.facts.insert(fact.attempt_id, fact);
            if d.lose_reply {
                return (StatusCode::OK, "truncated response").into_response();
            }
            axum::Json(json!({"recorded":!d.reject_receipt})).into_response()
        }
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let data = Arc::new(Mutex::new(Ledger::default()));
        let router = Router::new()
            .route("/v1/worker/observations/vm-time", post(report))
            .with_state(data.clone());
        let task = tokio::spawn(async move {
            axum::serve(listener, router).await.unwrap();
        });
        Self { url, data, task }
    }
    fn client(&self) -> Client {
        Client::new(
            &self.url,
            Some("fixture-server-node-credential".into()),
            Some("fixture-access-id".into()),
            Some("fixture-access-secret".into()),
        )
        .unwrap()
    }
}

#[tokio::test]
async fn vm_time_creation_stop_and_duplicate_keep_one_immutable_interval() {
    let api = BoatDouble::start().await;
    let server = Server::start().await;
    let dir = tempfile::tempdir().unwrap();
    let mut b = backend(&api, dir.path())
        .await
        .with_vm_time_client(server.client());
    let mut r = request(200);
    r.resources.as_mut().unwrap().cpu_millis = 3000;
    let s = b.create(&r).await.unwrap();
    let start = server.data.lock().unwrap().facts[&r.owner.attempt_id].clone();
    assert_eq!(start.size, VmSize::Default);
    assert_eq!(start.sandbox_id, s.backend_id);
    assert_eq!(start.started_at_ms, 1767225600000); // actual createdAt in the provider double
    assert_eq!(start.stopped_at_ms, None);
    assert_eq!(b.pending_vm_time_reports(), 0);
    assert_eq!(b.create(&r).await.unwrap(), s);
    assert!(b.stop(&s).await.is_confirmed());
    let end = server.data.lock().unwrap().facts[&r.owner.attempt_id].clone();
    assert!(end.stopped_at_ms.is_some());
    assert_eq!(end.started_at_ms, start.started_at_ms);
    assert!(b.archive(&s).await.is_confirmed());
    b.list_owned(r.owner.node_id).await.unwrap();
    assert_eq!(server.data.lock().unwrap().facts[&r.owner.attempt_id], end);
    assert_eq!(server.data.lock().unwrap().facts.len(), 1);
    assert_eq!(server.data.lock().unwrap().requests.len(), 2);
    // Changed start/size/committed stop cannot be imported over the fact.
    for bad in [
        VmTimeFact {
            started_at_ms: start.started_at_ms + 1,
            ..end.clone()
        },
        VmTimeFact {
            size: VmSize::Small,
            ..end.clone()
        },
        VmTimeFact {
            stopped_at_ms: None,
            ..end.clone()
        },
    ] {
        assert!(b.backfill_vm_time(bad).is_err());
    }
}

#[tokio::test]
async fn vm_time_stop_intent_202_and_deadline_never_close_interval() {
    let api = BoatDouble::start().await;
    let server = Server::start().await;
    let dir = tempfile::tempdir().unwrap();
    let mut cfg = config(&api, dir.path());
    cfg.poll_timeout_ms = 1;
    let mut b = BoatBackend::connect(cfg, BoatKey::new("fixture-node-key".into()).unwrap())
        .unwrap()
        .with_vm_time_client(server.client());
    let r = request(201);
    let s = b.create(&r).await.unwrap();
    api.data.lock().unwrap().lie_stop = true;
    assert!(!b.stop(&s).await.is_confirmed());
    assert_eq!(
        server.data.lock().unwrap().facts[&r.owner.attempt_id].stopped_at_ms,
        None
    );
    // Archived without a completed snapshot is still unconfirmed.
    api.data
        .lock()
        .unwrap()
        .sandboxes
        .get_mut(&s.backend_id)
        .unwrap()["state"] = json!("archived");
    b.inspect(&s).await.unwrap();
    assert_eq!(
        server.data.lock().unwrap().facts[&r.owner.attempt_id].stopped_at_ms,
        None
    );
    api.data.lock().unwrap().lie_stop = false;
    assert!(b.stop(&s).await.is_confirmed());
    assert!(
        server.data.lock().unwrap().facts[&r.owner.attempt_id]
            .stopped_at_ms
            .is_some()
    );
}

#[tokio::test]
async fn vm_time_replays_lost_ack_and_stopped_deleted_vm_after_restart() {
    let api = BoatDouble::start().await;
    let server = Server::start().await;
    let dir = tempfile::tempdir().unwrap();
    server.data.lock().unwrap().lose_reply = true;
    let r = request(202);
    let mut b = backend(&api, dir.path())
        .await
        .with_vm_time_client(server.client());
    let s = b.create(&r).await.unwrap();
    assert_eq!(b.pending_vm_time_reports(), 1);
    let original = server.data.lock().unwrap().facts[&r.owner.attempt_id].clone();
    drop(b);
    server.data.lock().unwrap().lose_reply = false;
    let mut b = backend(&api, dir.path())
        .await
        .with_vm_time_client(server.client());
    b.list_owned(r.owner.node_id).await.unwrap();
    assert_eq!(
        server.data.lock().unwrap().facts[&r.owner.attempt_id],
        original
    );
    assert_eq!(b.pending_vm_time_reports(), 0);
    server.data.lock().unwrap().unavailable = true;
    let auth = DestroyAuthorization::retained(s.clone(), "retained").unwrap();
    assert!(b.destroy(&s, &auth).await.is_confirmed());
    assert_eq!(b.pending_vm_time_reports(), 1);
    let disk: serde_json::Value =
        serde_json::from_slice(&std::fs::read(dir.path().join("boat.json")).unwrap()).unwrap();
    let stopped = disk["vm_time"][r.owner.attempt_id.to_string()]["fact"]["stopped_at_ms"]
        .as_u64()
        .unwrap();
    drop(b);
    server.data.lock().unwrap().unavailable = false;
    let mut b = backend(&api, dir.path())
        .await
        .with_vm_time_client(server.client());
    api.fault(Method::GET, "/sandboxes", 503, false);
    assert!(b.list_owned(r.owner.node_id).await.is_err());
    assert_eq!(b.pending_vm_time_reports(), 0);
    assert_eq!(
        server.data.lock().unwrap().facts[&r.owner.attempt_id].stopped_at_ms,
        Some(stopped)
    );
    drop(b);
    let mut b = backend(&api, dir.path())
        .await
        .with_vm_time_client(server.client());
    let count = server.data.lock().unwrap().requests.len();
    b.replay_vm_time().await.unwrap();
    assert_eq!(server.data.lock().unwrap().requests.len(), count);
    assert_eq!(server.data.lock().unwrap().facts.len(), 1);
}

#[tokio::test]
async fn vm_time_lost_create_response_recovers_actual_start() {
    let api = BoatDouble::start().await;
    let server = Server::start().await;
    let dir = tempfile::tempdir().unwrap();
    let r = request(203);
    let mut b = backend(&api, dir.path())
        .await
        .with_vm_time_client(server.client());
    api.fault(Method::POST, "/sandboxes", 0, true);
    assert!(b.create(&r).await.is_err());
    assert!(server.data.lock().unwrap().facts.is_empty());
    drop(b);
    let mut b = backend(&api, dir.path())
        .await
        .with_vm_time_client(server.client());
    let entries = b.list_owned(r.owner.node_id).await.unwrap();
    assert_eq!(entries.len(), 1);
    assert_eq!(
        server.data.lock().unwrap().facts[&r.owner.attempt_id].started_at_ms,
        1767225600000
    );
    assert_eq!(
        server.data.lock().unwrap().facts[&r.owner.attempt_id].stopped_at_ms,
        None
    );
    assert_eq!(
        b.record_not_created(&r, now()).await.unwrap_err(),
        BackendError::Conflict
    );
    assert!(b.stop(&entries[0].sandbox).await.is_confirmed());
}

#[tokio::test]
async fn vm_time_old_journal_backfill_and_no_create_proof() {
    let api = BoatDouble::start().await;
    let server = Server::start().await;
    let dir = tempfile::tempdir().unwrap();
    let r = request(204);
    let mut b = backend(&api, dir.path()).await;
    let s = b.create(&r).await.unwrap();
    drop(b);
    let path = dir.path().join("boat.json");
    let mut disk: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
    disk.as_object_mut().unwrap().remove("vm_time"); // pre-H2 journal
    std::fs::write(&path, serde_json::to_vec(&disk).unwrap()).unwrap();
    let mut b = backend(&api, dir.path())
        .await
        .with_vm_time_client(server.client());
    b.list_owned(r.owner.node_id).await.unwrap();
    assert_eq!(
        server.data.lock().unwrap().facts[&r.owner.attempt_id].started_at_ms,
        1767225600000
    );
    // Historical destroyed VM has no remaining provider metadata: explicit evidence import.
    let historical = VmTimeFact {
        attempt_id: Uuid::from_u128(205),
        sandbox_id: "bx_historical".into(),
        size: VmSize::Xlarge,
        started_at_ms: 1000,
        stopped_at_ms: Some(2000),
    };
    b.backfill_vm_time(historical.clone()).unwrap();
    server.data.lock().unwrap().reject_receipt = true;
    b.replay_vm_time().await.unwrap();
    assert_eq!(b.pending_vm_time_reports(), 1);
    drop(b);
    server.data.lock().unwrap().reject_receipt = false;
    let mut b = backend(&api, dir.path())
        .await
        .with_vm_time_client(server.client());
    b.replay_vm_time().await.unwrap();
    assert_eq!(
        server.data.lock().unwrap().facts[&historical.attempt_id],
        historical
    );
    let never = request(206);
    b.record_not_created(&never, now()).await.unwrap();
    let zero = server.data.lock().unwrap().facts[&never.owner.attempt_id].clone();
    assert_eq!(
        zero.sandbox_id,
        format!("not-created/{}", never.owner.attempt_id)
    );
    assert_eq!(Some(zero.started_at_ms), zero.stopped_at_ms);
    b.record_not_created(&never, now()).await.unwrap();
    assert_eq!(
        server.data.lock().unwrap().facts[&never.owner.attempt_id],
        zero
    );
    assert!(b.create(&never).await.is_err());
    assert!(b.backfill_vm_time(zero).is_err());
    assert!(b.stop(&s).await.is_confirmed());
}

#[tokio::test]
async fn vm_time_controller_backfills_abandonment_only_before_create_intent() {
    let api = BoatDouble::start().await;
    let server = Server::start().await;
    let dir = tempfile::tempdir().unwrap();
    let placeholder = WorkerIdentity {
        node_id: Uuid::from_u128(1),
        boot_id: Uuid::from_u128(2),
    };
    let launch_request = launch(&placeholder);
    let root = dir.path().join("controller");
    let mut c = Controller::open(
        &root,
        placeholder.node_id,
        node(&launch_request),
        SelectedBackend::Boat(Box::new(
            backend(&api, dir.path())
                .await
                .with_vm_time_client(server.client()),
        )),
        Provider,
    )
    .unwrap();
    c.register().await.unwrap();
    let id = c.reserve(launch(c.identity())).unwrap();
    c.accept_with(id, |_| async { Ok(()) }).await.unwrap();
    drop(c);
    let mut c = Controller::open(
        &root,
        placeholder.node_id,
        node(&launch_request),
        SelectedBackend::Boat(Box::new(
            backend(&api, dir.path())
                .await
                .with_vm_time_client(server.client()),
        )),
        Provider,
    )
    .unwrap();
    c.reconcile_at(now()).await.unwrap();
    let fact = server.data.lock().unwrap().facts[&id].clone();
    assert_eq!(fact.sandbox_id, format!("not-created/{id}"));
    assert_eq!(fact.stopped_at_ms, Some(fact.started_at_ms));
    assert_eq!(api.running(), 0);
    c.reconcile_at(now()).await.unwrap();
    assert_eq!(server.data.lock().unwrap().facts[&id], fact);
}

#[tokio::test]
async fn vm_time_backfill_cli_uses_node_enrollment_and_preserves_pending_delivery() {
    let api = BoatDouble::start().await;
    let server = Server::start().await;
    let dir = tempfile::tempdir().unwrap();
    let worker_path = dir.path().join("worker.toml");
    let controller_path = dir.path().join("node.json");
    let facts_path = dir.path().join("facts.json");
    aq_worker::config::WorkerConfig {
        server_url: server.url.clone(),
        node_id: Uuid::from_u128(1),
        credential: "fixture-server-node-credential".into(),
        cf_client_id: Some("fixture-access-id".into()),
        cf_client_secret: Some("fixture-access-secret".into()),
    }
    .save(&worker_path)
    .unwrap();
    let cfg = config(&api, dir.path());
    std::fs::write(&cfg.key_file, "fixture-node-key").unwrap();
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(&cfg.key_file, std::fs::Permissions::from_mode(0o600)).unwrap();
    let identity = WorkerIdentity {
        node_id: cfg.node_id,
        boot_id: Uuid::from_u128(2),
    };
    let mut node_config = node(&launch(&identity));
    node_config.templates.clear();
    std::fs::write(
        &controller_path,
        serde_json::to_vec(&json!({
            "state_dir":dir.path().join("controller"), "node":node_config,
            "boat": {
                "api_url":cfg.api_url, "node_id":cfg.node_id,
                "attempt_org":cfg.attempt_org, "template_org":cfg.template_org,
                "journal_path":cfg.journal_path, "key_file":cfg.key_file,
                "templates":cfg.templates
            }
        }))
        .unwrap(),
    )
    .unwrap();
    let fact = VmTimeFact {
        attempt_id: Uuid::from_u128(207),
        sandbox_id: "bx_old_deleted".into(),
        size: VmSize::Small,
        started_at_ms: 1000,
        stopped_at_ms: Some(3000),
    };
    std::fs::write(
        &facts_path,
        serde_json::to_vec(&vec![fact.clone()]).unwrap(),
    )
    .unwrap();
    let run = || {
        let mut command = tokio::process::Command::new(env!("CARGO_BIN_EXE_aq-worker"));
        command
            .arg("--config")
            .arg(&worker_path)
            .arg("boat-vm-time-backfill")
            .arg("--controller-config")
            .arg(&controller_path)
            .arg("--facts")
            .arg(&facts_path);
        command
    };
    server.data.lock().unwrap().unavailable = true;
    let pending = run().output().await.unwrap();
    assert!(!pending.status.success());
    assert!(String::from_utf8_lossy(&pending.stderr).contains("delivery pending"));
    server.data.lock().unwrap().unavailable = false;
    let delivered = run().output().await.unwrap();
    assert!(
        delivered.status.success(),
        "{}",
        String::from_utf8_lossy(&delivered.stderr)
    );
    assert_eq!(server.data.lock().unwrap().facts[&fact.attempt_id], fact);
    assert!(run().output().await.unwrap().status.success());
    assert_eq!(server.data.lock().unwrap().facts.len(), 1);
    assert!(
        api.data.lock().unwrap().requests.is_empty(),
        "historical import must not recreate VMs"
    );
}
