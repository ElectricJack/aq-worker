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
const DOWN: &str = "http://127.0.0.1:9";
fn worker(url: &str, node_id: Uuid) -> WorkerConfig {
    let (server_url, credential) = (url.into(), "synthetic".into());
    WorkerConfig {
        server_url,
        node_id,
        credential,
        cf_client_id: None,
        cf_client_secret: None,
    }
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
        let url = format!("http://{}", listener.local_addr().unwrap());
        let client = NativeClient::from_config(&worker(&url, node_id)).unwrap();
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
    let client = NativeClient::from_config(&worker(DOWN, Uuid::from_u128(1))).unwrap();
    let a = terminal_attempt(false, false);
    let error = replay(&client, &a, |_| panic!("no receipt without stop proof"))
        .await
        .unwrap_err();
    assert!(error.to_string().contains("retained physical stop"));
}

#[tokio::test]
async fn recovery_never_creates_missing_history_or_replaces_original_identity() {
    let (dir, c) = super::super::tests::config();
    let worker = worker(DOWN, Uuid::new_v4());
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
    let original = q.original();
    assert!(matches!(
        q,
        crate::native::journal::launch::ClosureRequest::Frozen(_)
    ));
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

/// The same closure for a registry-refresh profile attempt uses the refresh
/// envelope (the server's launch-closure route dispatches on negotiation).
#[test]
fn refresh_profile_unstarted_launch_closes_with_the_refresh_envelope() {
    let mut a = terminal_attempt(true, false);
    let h = a.native_host.as_mut().unwrap();
    h.running = None;
    h.refresh_terminal = None;
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
    let q = closure_request(&a, 8, &state, Uuid::new_v4()).unwrap();
    q.validate().unwrap();
    assert!(matches!(
        q,
        crate::native::journal::launch::ClosureRequest::Refresh(_)
    ));
    assert!(matches!(
        q.original().evidence,
        aq_admission::granted_closure::Evidence::Suppressed { .. }
    ));
}

// W4 salvage. A coherent result journal needs a live launch, so these pin each
// seam: the copy-back and claim, the journal salvage record and the dispositions.
use crate::native::{journal::salvage_history, result::history::ResultHistory};
use {ReconcileDisposition as R, SalvageDisposition as S};
fn salvage_fixture() -> (tempfile::TempDir, ResultHistory, Attempt) {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("kept.txt"), b"base").unwrap();
    let mut h = crate::native::result::tests::history();
    h.scope.workspace.path = dir.path().to_path_buf();
    let mut a = stopped_attempt(false);
    let t = a.native_host.as_mut().unwrap().terminal.as_mut().unwrap();
    let receipt = fixture(false)["stopped_receipt"].clone();
    t.stopped.as_mut().unwrap().receipt = Some(serde_json::from_value(receipt).unwrap());
    (dir, h, a)
}
type Files<'a> = &'a [(&'a str, &'a [u8])];
/// salvage_claim over one FakeBackend sandbox, stopped after it ran, whose /aq/work holds `files`.
async fn claim(h: &ResultHistory, files: Files<'_>) -> Result<local::Submission, &'static str> {
    use crate::backend::{CreateRequest, FakeBackend, Ownership, PlacementBackend, TemplateDigest};
    let (node_id, boot_id, attempt_id) = (Uuid::new_v4(), Uuid::new_v4(), Uuid::new_v4());
    let mut backend = FakeBackend::default();
    let template = TemplateDigest::parse(format!("sha256:{}", "1".repeat(64))).unwrap();
    let owner = Ownership {
        node_id,
        boot_id,
        attempt_id,
    };
    let request = CreateRequest::new(owner, template, 1 << 40, vec![]);
    let sandbox = backend.create(&request).await.unwrap();
    let material = crate::backend::PrivateMaterial::new(b"fixture".to_vec());
    backend
        .deliver_private_material(&sandbox, material)
        .await
        .unwrap();
    backend.start_runner(&sandbox).await.unwrap();
    let running = stopped_sandboxes(&mut backend, node_id, attempt_id).await;
    assert!(running.unwrap().is_empty() && backend.stop(&sandbox).await.is_confirmed());
    let other = stopped_sandboxes(&mut backend, node_id, Uuid::new_v4()).await;
    assert!(other.unwrap().is_empty());
    let sandboxes = stopped_sandboxes(&mut backend, node_id, attempt_id)
        .await
        .unwrap();
    let file = |&(path, data): &(&str, &[u8])| (path.to_string(), data.to_vec());
    let entry = |(path, data)| TreeEntry::File {
        path,
        mode: 0o644,
        data,
    };
    let entries: Vec<_> = files.iter().map(file).map(entry).collect();
    let archive = tar::tree_archive("work", &entries).unwrap(); // members are work/...
    let restart = aq_protocol::HandoffReason::Restart;
    let claimed = salvage_claim(h, restart, &sandboxes, 1 << 20, async |_| Ok(archive));
    claimed.await.unwrap()
}

#[tokio::test]
async fn salvage_reports_failed_result_with_saved_workspace() {
    let (dir, mut h, a) = salvage_fixture();
    // Histories written before W4 serialize (and digest) exactly as before.
    assert!(serde_json::to_value(&h).unwrap().get("salvaged").is_none());
    let note = claim(&h, &[("kept.txt", b"base"), ("notes.txt", b"saved")]).await;
    // The checkpoint and export read this workspace: it now holds the sandbox's file.
    assert_eq!(
        std::fs::read(dir.path().join("notes.txt")).unwrap(),
        b"saved"
    );
    salvage_history(&a, &mut h, Some(note.unwrap()), 5).unwrap();
    let conclusion = h.claim.as_ref().unwrap().submission().unwrap().conclusion;
    let c = conclusion.as_str();
    assert!(
        c.starts_with("Handoff (node restart)") && c.contains("Changed: notes.txt."),
        "{c}"
    );
    // The report's outcome must equal this anchor's (ResultHistory::validate).
    let outcome = h.stop.as_ref().unwrap().outcome;
    assert!(matches!(
        outcome,
        aq_admission::transport::ProcessOutcome::Failed
    ));
    assert!(h.intake_closed && h.salvaged == Some(5));
}

/// Review Focus 2: a recorded report gives AlreadyReported; a resumed salvage keeps its
/// claim and stop; a report intent refuses a new salvage record.
#[tokio::test]
async fn salvage_is_idempotent_after_report() {
    assert_eq!(settled(R::ResultRecorded), Some(S::AlreadyReported));
    assert_eq!(settled(R::StopReported), None);
    let (_dir, mut h, a) = salvage_fixture();
    let note = claim(&h, &[]).await.unwrap();
    let mut first = h.clone();
    salvage_history(&a, &mut first, Some(note.clone()), 5).unwrap();
    let mut again = first.clone();
    salvage_history(&a, &mut again, Some(note.clone()), 6).unwrap();
    assert_eq!(again.digest().unwrap(), first.digest().unwrap());
    let fixture: Value = serde_json::from_slice(include_bytes!(
        "../../../../../../vendor/aq-admission-0.1.10/artifacts/result-record.fixture.json"
    ))
    .unwrap();
    h.report = Some(crate::native::result::history::ReportIntent {
        request: serde_json::from_value(fixture["report"].clone()).unwrap(),
        bytes: vec![],
        digest: aq_session_protocol::Digest::of(b"report"),
        recorded_revision: 4,
    });
    let error = salvage_history(&a, &mut h, Some(note), 5).unwrap_err();
    assert!(error.to_string().contains("report intent"), "{error:#}");
}

#[tokio::test]
async fn salvage_without_sandbox_is_unsalvageable() {
    let (dir, mut h, a) = salvage_fixture();
    let restart = aq_protocol::HandoffReason::Restart;
    let never = async |_: &SandboxIdentity| -> Result<Vec<u8>> { panic!("no sandbox to archive") };
    let refused = salvage_claim(&h, restart, &[], 1 << 20, never).await;
    assert_eq!(refused.unwrap().err(), Some("sandbox_absent"));
    assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 1);
    // Nor can the journal record a salvage without a claim or without a stop proof.
    assert!(salvage_history(&a, &mut h, None, 5).is_err());
    let mut no_stop = a.clone();
    no_stop.native_host.as_mut().unwrap().stopped = None;
    let note = claim(&h, &[]).await.unwrap();
    assert!(salvage_history(&no_stop, &mut h, Some(note), 5).is_err());
}

#[tokio::test]
async fn salvage_refuses_workspace_holding_agent_credential() {
    let (dir, h, _) = salvage_fixture();
    let secret = format!("token=aqa_{}", "x".repeat(40));
    let refused = claim(
        &h,
        &[("kept.txt", b"changed"), ("leak.env", secret.as_bytes())],
    )
    .await;
    assert_eq!(refused.err(), Some("credential_in_workspace"));
    // Nothing the sandbox produced reaches the workspace.
    assert_eq!(std::fs::read(dir.path().join("kept.txt")).unwrap(), b"base");
    assert!(!dir.path().join("leak.env").exists());
}

#[test]
fn salvage_of_unstarted_launch_is_not_started() {
    assert_eq!(settled(R::LaunchClosed), Some(S::NotStarted));
    let mut a = terminal_attempt(false, false);
    let h = a.native_host.as_mut().unwrap();
    (h.running, h.terminal, h.start_intended) = (None, None, false);
    assert!(unstarted(&a), "reconcile_native closes it: NotStarted");
    let (_dir, c) = super::super::tests::config();
    let worker = worker(DOWN, Uuid::new_v4());
    fn send<T: Send>(_: T) {}
    send(salvage_interrupted(&worker, &c, Uuid::new_v4())); // the service runs it on its tasks
}
