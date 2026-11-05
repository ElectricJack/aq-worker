//! Opt-in real-provider qualification. A local proxy injects faults at the HTTP
//! boundary; every nonfaulted operation reaches the real Boat service.
use super::*;
use axum::{
    Router,
    body::Bytes,
    extract::{OriginalUri, State},
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    routing::any,
};
use std::{
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};
#[derive(Clone)]
struct Proxy {
    client: reqwest::Client,
    fault: Arc<Mutex<Option<Fault>>>,
}
async fn forward(
    State(state): State<Proxy>,
    OriginalUri(uri): OriginalUri,
    method: Method,
    mut headers: HeaderMap,
    body: Bytes,
) -> Response {
    let fault = {
        let mut pending = state.fault.lock().unwrap();
        let applies = match *pending {
            Some(Fault::CrashAfterCreate) => {
                method == Method::POST && uri.path() == "/api/v1/sandboxes"
            }
            Some(Fault::DestroyFailure) => {
                method == Method::DELETE
                    && uri.path().starts_with("/api/v1/sandboxes/")
                    && !uri.path().ends_with("/snapshots")
            }
            _ => false,
        };
        if applies { pending.take() } else { None }
    };
    if matches!(fault, Some(Fault::DestroyFailure)) {
        return (StatusCode::SERVICE_UNAVAILABLE, "injected unavailable").into_response();
    }
    headers.remove("host");
    let response = state
        .client
        .request(method, format!("https://boat.dev{uri}"))
        .headers(headers)
        .body(body)
        .send()
        .await;
    let Ok(response) = response else {
        return (StatusCode::BAD_GATEWAY, "upstream unavailable").into_response();
    };
    let status = response.status();
    let Ok(bytes) = response.bytes().await else {
        return (StatusCode::BAD_GATEWAY, "lost body").into_response();
    };
    if matches!(fault, Some(Fault::CrashAfterCreate)) {
        return (StatusCode::BAD_GATEWAY, "injected lost reply").into_response();
    }
    (status, bytes).into_response()
}
struct LiveFixture {
    backend: BoatBackend,
    fault: Arc<Mutex<Option<Fault>>>,
    node: Uuid,
    boot: Uuid,
    digest: TemplateDigest,
    retention: std::path::PathBuf,
}
impl BackendFixture for LiveFixture {
    type Backend = BoatBackend;
    fn backend(&mut self) -> &mut BoatBackend {
        &mut self.backend
    }
    fn request(&self, case: u128) -> CreateRequest {
        let mut r = request(case);
        r.owner = Ownership {
            node_id: self.node,
            boot_id: self.boot,
            attempt_id: Uuid::new_v4(),
        };
        r.name = r.owner.deterministic_name();
        r.template_digest = self.digest.clone();
        r.hard_deadline_ms = now() + 300_000;
        r
    }
    fn material(&self) -> PrivateMaterial {
        PrivateMaterial::new(b"fixture-private-material".to_vec())
    }
    fn inject(&mut self, f: Fault) {
        assert!(self.fault.lock().unwrap().replace(f).is_none());
    }
    async fn observe_fixture(&mut self, s: &SandboxIdentity) {
        let until = Instant::now() + Duration::from_secs(90);
        loop {
            if self
                .backend
                .read_work_file(s, "result")
                .await
                .is_ok_and(|b| b == b"fixture-complete\n")
            {
                return;
            }
            assert!(Instant::now() < until, "fixture output timeout");
            tokio::time::sleep(Duration::from_millis(500)).await;
        }
    }
    async fn verify_and_retain(&mut self, s: &SandboxIdentity) -> DestroyAuthorization {
        self.observe_fixture(s).await;
        let bytes = self.backend.read_work_file(s, "result").await.unwrap();
        assert_eq!(bytes, b"fixture-complete\n");
        let file = self.retention.join(&s.backend_id);
        std::fs::write(&file, &bytes).unwrap();
        std::fs::File::open(&file).unwrap().sync_all().unwrap();
        assert_eq!(std::fs::read(&file).unwrap(), bytes);
        DestroyAuthorization::retained(s.clone(), format!("boat-live-fixture/{}", s.backend_id))
            .unwrap()
    }
}
async fn catch_panic(future: impl std::future::Future<Output = ()>) -> std::thread::Result<()> {
    let mut future = Box::pin(future);
    std::future::poll_fn(move |cx| {
        match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| future.as_mut().poll(cx))) {
            Ok(std::task::Poll::Ready(())) => std::task::Poll::Ready(Ok(())),
            Ok(std::task::Poll::Pending) => std::task::Poll::Pending,
            Err(e) => std::task::Poll::Ready(Err(e)),
        }
    })
    .await
}
pub async fn run() {
    let path = std::env::var("BOAT_LIVE_CONFIG").expect("BOAT_LIVE_CONFIG required");
    let mut cfg: BoatConfig = serde_json::from_slice(&std::fs::read(path).unwrap()).unwrap();
    cfg.validate().unwrap();
    assert_eq!(
        cfg.api_url, "https://boat.dev/api/v1",
        "live qualification uses the official service"
    );
    // The key is read only from the node-local private file named by the
    // config; it never passes through the environment, argv or logs.
    let key = BoatKey::load(&cfg.key_file).expect("private boat key file required");
    let dir = tempfile::tempdir().unwrap();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let fault = Arc::new(Mutex::new(None));
    let state = Proxy {
        client: reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .timeout(Duration::from_secs(120))
            .build()
            .unwrap(),
        fault: fault.clone(),
    };
    let app = Router::new().fallback(any(forward)).with_state(state);
    cfg.api_url = format!("http://{}/api/v1", listener.local_addr().unwrap());
    cfg.journal_path = dir.path().join("boat-live.json");
    cfg.runner_command=Some(vec!["/bin/sh".into(),"-ec".into(),"test -s /aq/private/material; printf 'fixture-complete\\n' > /aq/work/result; while :; do sleep 60; done".into()]);
    let node = cfg.node_id;
    let digest = cfg.templates[0].digest.clone();
    let backend = BoatBackend::connect(cfg, key).unwrap();
    let proxy = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    let mut fixture = LiveFixture {
        backend,
        fault,
        node,
        boot: Uuid::new_v4(),
        digest,
        retention: dir.path().to_owned(),
    };
    // Each scenario creates and removes one sandbox in sequence, so at most one
    // VM runs at a time. BOAT_LIVE_SCENARIOS selects a subset to fit a budget.
    let selected = std::env::var("BOAT_LIVE_SCENARIOS").unwrap_or_else(|_| "all".into());
    let result = catch_panic(run_selected(&mut fixture, &selected)).await;
    fixture.fault.lock().unwrap().take();
    // Explicit fixture-only operator release; preserve any failed stop/delete
    // as an assertion and rely on the original bounded TTL for compute safety.
    let owned = fixture
        .backend
        .list_owned(node)
        .await
        .expect("live cleanup inventory");
    let mut cleanup = true;
    for e in owned {
        cleanup &= fixture.backend.stop(&e.sandbox).await.is_confirmed();
        let a =
            DestroyAuthorization::operator_released(e.sandbox.clone(), "boat-live-fixture-cleanup")
                .unwrap();
        cleanup &= fixture.backend.destroy(&e.sandbox, &a).await.is_confirmed();
    }
    let empty = fixture
        .backend
        .list_owned(node)
        .await
        .expect("final live inventory")
        .is_empty();
    proxy.abort();
    assert!(
        cleanup && empty,
        "live cleanup incomplete: inspect dedicated node scope"
    );
    if let Err(panic) = result {
        std::panic::resume_unwind(panic);
    }
}

const SCENARIOS: [&str; 6] = [
    "create_crash_before_record",
    "unknown_sandbox_in_inventory",
    "destroy_failure_keeps_resources_protected",
    "preserve_before_destroy_with_unretained_work",
    "deadline_sweep_stops_rather_than_deletes",
    "create_provision_run_observe_handoff_destroy",
];
async fn run_selected(fixture: &mut LiveFixture, selected: &str) {
    let names: Vec<&str> = if selected == "all" {
        SCENARIOS.to_vec()
    } else {
        selected.split(',').map(str::trim).collect()
    };
    for name in names {
        assert!(SCENARIOS.contains(&name), "unknown live scenario {name}");
        let started = Instant::now();
        match name {
            "create_crash_before_record" => conformance::create_crash_before_record(fixture).await,
            "unknown_sandbox_in_inventory" => {
                conformance::unknown_sandbox_in_inventory(fixture).await
            }
            "destroy_failure_keeps_resources_protected" => {
                conformance::destroy_failure_keeps_resources_protected(fixture).await
            }
            "preserve_before_destroy_with_unretained_work" => {
                conformance::preserve_before_destroy_with_unretained_work(fixture).await
            }
            "deadline_sweep_stops_rather_than_deletes" => {
                conformance::deadline_sweep_stops_rather_than_deletes(fixture).await
            }
            _ => conformance::create_provision_run_observe_handoff_destroy(fixture).await,
        }
        eprintln!(
            "LIVE PASS {name} in {:.1}s",
            started.elapsed().as_secs_f64()
        );
    }
}
