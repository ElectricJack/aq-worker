//! Public synthetic history/loopback peer only; never creates a live host permit.
use super::*;
use crate::native::{
    journal::launch::NativeStopped, test_support::profile_history::terminal_attempt,
};
use axum::{
    Json, Router,
    extract::{Path, State},
    http::StatusCode,
    response::{IntoResponse, Response},
    routing::post,
};
use serde_json::Value;
use std::sync::{
    Mutex,
    atomic::{AtomicBool, Ordering},
};

fn fixture(refresh: bool) -> Value {
    serde_json::from_slice(if refresh {
        include_bytes!("../../../../../../vendor/aq-phase-terminal-protocol-0.1.1/artifacts/refresh-v1/fixtures.json")
    } else {
        include_bytes!("../../../../../../vendor/aq-phase-terminal-protocol-0.1.0/artifacts/fixtures.json")
    }).unwrap()
}
fn stopped_attempt(refresh: bool) -> Attempt {
    let mut a = terminal_attempt(refresh, false);
    let f = fixture(refresh);
    let h = a.native_host.as_mut().unwrap();
    h.stopped = Some(NativeStopped {
        observation: serde_json::from_value(
            f["stopped_request"]["original"]["proof"]["observation"].clone(),
        )
        .unwrap(),
        proof_digest: aq_session_protocol::Digest::of(b"synthetic historical fixture only"),
        recorded_revision: 4,
    });
    if refresh {
        h.refresh_terminal.as_mut().unwrap().stopped = Some(
            crate::native::journal::refresh_terminal::StoppedPublication {
                request: serde_json::from_value(f["stopped_request"].clone()).unwrap(),
                recorded_revision: 4,
                receipt: None,
            },
        );
    } else {
        h.terminal.as_mut().unwrap().stopped =
            Some(crate::native::journal::phase_terminal::StoppedPublication {
                request: serde_json::from_value(f["stopped_request"].clone()).unwrap(),
                recorded_revision: 4,
                receipt: None,
            });
    }
    a
}
struct Peer {
    fixture: Value,
    calls: Mutex<Vec<(String, Value)>>,
    lose_stop: AtomicBool,
}
async fn report(
    State(p): State<Arc<Peer>>,
    Path(path): Path<String>,
    Json(request): Json<Value>,
) -> Response {
    p.calls
        .lock()
        .unwrap()
        .push((path.clone(), request.clone()));
    assert_eq!(request, p.fixture[format!("{path}_request")]);
    if path == "stopped" && p.lose_stop.swap(false, Ordering::SeqCst) {
        return StatusCode::SERVICE_UNAVAILABLE.into_response();
    }
    (
        [("aq-phase-terminal-api-version", "1")],
        Json(p.fixture[format!("{path}_receipt")].clone()),
    )
        .into_response()
}
fn save_receipt(path: &std::path::Path, receipt: Receipt) -> Result<()> {
    let mut a: Attempt = serde_json::from_slice(&std::fs::read(path)?)?;
    let h = a.native_host.as_mut().unwrap();
    match receipt {
        Receipt::Started(r) => h.terminal.as_mut().unwrap().started.receipt = Some(r),
        Receipt::Stopped(r) => {
            h.terminal
                .as_mut()
                .unwrap()
                .stopped
                .as_mut()
                .unwrap()
                .receipt = Some(r)
        }
        Receipt::RefreshStarted(r) => {
            h.refresh_terminal.as_mut().unwrap().started.receipt = Some(r)
        }
        Receipt::RefreshStopped(r) => {
            h.refresh_terminal
                .as_mut()
                .unwrap()
                .stopped
                .as_mut()
                .unwrap()
                .receipt = Some(r)
        }
        Receipt::Result(..) => panic!("no invented result report"),
    }
    std::fs::write(path, serde_json::to_vec(&a)?)?;
    Ok(())
}

#[tokio::test]
async fn preserved_stopped_attempt_reconciles_without_a_new_seeded_task() {
    for refresh in [false, true] {
        let attempt = stopped_attempt(refresh);
        let original_binding = attempt.reservation.offer.offered.offer.binding.clone();
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("preserved-attempt.json");
        std::fs::write(&path, serde_json::to_vec(&attempt).unwrap()).unwrap();
        drop(attempt);
        let peer = Arc::new(Peer {
            fixture: fixture(refresh),
            calls: Mutex::new(vec![]),
            lose_stop: AtomicBool::new(true),
        });
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let aq_session_protocol::SessionIdentity::Remote { node_id, .. } = original_binding.session
        else {
            panic!()
        };
        let client = NativeClient::from_config(&WorkerConfig {
            server_url: format!("http://{}", listener.local_addr().unwrap()),
            node_id,
            credential: "synthetic-recovery".into(),
            cf_client_id: None,
            cf_client_secret: None,
        })
        .unwrap();
        // There is no registration, offer/poll or task-seeding route at this peer.
        let app = Router::new()
            .route("/v1/worker/profiled-v2/{path}", post(report))
            .with_state(peer.clone());
        let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let load = || serde_json::from_slice::<Attempt>(&std::fs::read(&path).unwrap()).unwrap();
        assert!(
            replay(&client, &load(), |r| save_receipt(&path, r))
                .await
                .is_err()
        );
        assert_eq!(
            replay(&client, &load(), |r| save_receipt(&path, r))
                .await
                .unwrap(),
            ReconcileDisposition::StopReported
        );
        let retained = load();
        assert_eq!(
            retained.reservation.offer.offered.offer.binding,
            original_binding
        );
        assert!(retained.result.is_none());
        assert!(
            TerminalProfile::for_attempt(&retained)
                .unwrap()
                .complete(&retained)
                .unwrap()
        );
        assert_eq!(
            replay(&client, &load(), |r| save_receipt(&path, r))
                .await
                .unwrap(),
            ReconcileDisposition::StopReported
        );
        let calls = peer.calls.lock().unwrap();
        assert_eq!(
            calls.iter().map(|(p, _)| p.as_str()).collect::<Vec<_>>(),
            ["started", "stopped", "stopped"]
        );
        assert_eq!(calls[1], calls[2]);
        server.abort();
    }
}

#[tokio::test]
async fn uncertain_stop_is_preserved_without_reporting_or_admission() {
    let client = NativeClient::from_config(&WorkerConfig {
        server_url: "http://127.0.0.1:9".into(),
        node_id: Uuid::from_u128(1),
        credential: "synthetic".into(),
        cf_client_id: None,
        cf_client_secret: None,
    })
    .unwrap();
    let a = terminal_attempt(false, false);
    let error = replay(&client, &a, |_| panic!("no receipt without stop proof"))
        .await
        .unwrap_err();
    assert!(error.to_string().contains("retained physical stop"));
}

#[tokio::test]
async fn recovery_never_creates_missing_history_or_replaces_original_identity() {
    let (dir, c) = super::super::tests::config();
    let worker = WorkerConfig {
        server_url: "http://127.0.0.1:9".into(),
        node_id: Uuid::new_v4(),
        credential: "synthetic".into(),
        cf_client_id: None,
        cf_client_secret: None,
    };
    assert!(reconcile_native(&worker, &c, Uuid::new_v4()).await.is_err());
    assert!(!dir.path().join("journal").exists());
    let journal = Journal::create(
        &dir.path().join("journal"),
        worker.node_id,
        c.capacity.clone(),
    )
    .unwrap();
    let identity = journal.state().identity.clone();
    let bytes = std::fs::read(dir.path().join("journal/journal.json")).unwrap();
    drop(journal);
    assert!(
        reconcile_native(&worker, &c, Uuid::new_v4())
            .await
            .unwrap_err()
            .to_string()
            .contains("attempt absent")
    );
    assert_eq!(
        std::fs::read(dir.path().join("journal/journal.json")).unwrap(),
        bytes
    );
    let journal = Journal::open(
        &dir.path().join("journal"),
        worker.node_id,
        c.capacity.clone(),
    )
    .unwrap();
    assert_eq!(journal.state().identity, identity);
    assert!(journal.recovery_required());
    assert!(journal.available().is_err());
    assert!(!dir.path().join("materials").exists());
}

/// B5b/AQ-08: a granted launch with no host process closes with a durable
/// suppression bound to the exact phase history; a launch with Running history
/// is never treated as unstarted.
#[test]
fn unstarted_launch_closes_by_suppression_and_started_never_does() {
    let mut a = terminal_attempt(false, false);
    assert!(
        !unstarted(&a),
        "Running history is never closed as unstarted"
    );
    let h = a.native_host.as_mut().unwrap();
    h.running = None;
    h.terminal = None;
    h.start_intended = false;
    assert!(unstarted(&a));
    let state = crate::native::journal::State {
        format: 1,
        identity: aq_admission::transport::WorkerIdentity {
            node_id: Uuid::from_u128(1),
            boot_id: Uuid::from_u128(2),
        },
        kernel_boot_id: Uuid::from_u128(3),
        revision: 7,
        attempts: Default::default(),
    };
    let op = Uuid::new_v4();
    let q = closure_request(&a, 8, &state, op).unwrap();
    q.validate().unwrap();
    let original = &q.original;
    assert_eq!(original.operation_id, op);
    assert_eq!(original.journal.revision, 8);
    let aq_admission::granted_closure::Evidence::Suppressed { record, .. } = &original.evidence
    else {
        panic!("no host process: suppression evidence");
    };
    assert_eq!(record.previous.revision, 7);
    assert_eq!(record.terminal_revision, 8);
    assert_eq!(record.recording_kernel_boot_id, Uuid::from_u128(3));
    // A started guardian without a retained stopped proof has no evidence yet.
    let h = a.native_host.as_mut().unwrap();
    h.start_intended = true;
    h.guardian = Some(
        serde_json::from_value(
            fixture(false)["stopped_request"]["original"]["proof"]["observation"]["guardian"]
                .clone(),
        )
        .unwrap(),
    );
    assert!(closure_request(&a, 8, &state, op).is_err());
}
