//! Pure tail and synthetic HTTP exchange tests. No live host/authority is forged.
//! Actual host attachment and daemon composition are qualified separately.
use super::*;
use aq_session_protocol::{SessionIdentity, TerminalHost};
use aq_terminal_bridge::{http::NodeCredential, output::OutputTail};
use axum::{
    Json, Router,
    extract::State,
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    routing::post,
};
use std::sync::atomic::AtomicU8;
fn terminal() -> TerminalIdentity {
    TerminalIdentity {
        session: SessionIdentity::Remote {
            node_id: Uuid::from_u128(1),
            boot_id: Uuid::from_u128(2),
            attempt_id: Uuid::from_u128(3),
            assignment_generation: 1,
            session_id: Uuid::from_u128(4),
        },
        terminal_id: Uuid::from_u128(5),
        epoch: 1,
    }
}
fn policy(frame: u32, replay: u32) -> TerminalPolicy {
    TerminalPolicy {
        host: TerminalHost::Tmux,
        input_enabled: false,
        max_input_bytes: 1,
        max_pending_input_bytes: 1,
        max_frame_bytes: frame,
        replay_bytes: replay,
    }
}
fn peer() -> Peer {
    Peer::Remote {
        node_id: Uuid::from_u128(1),
        boot_id: Uuid::from_u128(2),
    }
}
#[test]
fn bounded_tail_packet_preserves_literal_sequences_and_explicit_eviction_gap() {
    let mut tail = OutputTail::new(terminal(), &policy(4, 8)).unwrap();
    tail.accept(&ControlEvent::Bytes(b"abcdefghijklmnopqrst".to_vec()))
        .unwrap();
    let p = prepare(&tail, &peer(), 0).unwrap().unwrap();
    assert_eq!(p.through, 5);
    assert_eq!(p.request.gaps[0].first_missing, 1);
    assert_eq!(p.request.gaps[0].last_missing, 3);
    assert_eq!(
        p.request
            .frames
            .iter()
            .map(|f| f.sequence)
            .collect::<Vec<_>>(),
        vec![4, 5]
    );
    assert_eq!(p.request.frames[0].bytes, b"mnop");
    assert!(
        p.request.snapshots.is_empty()
            && p.request.receipts.is_empty()
            && p.request.fence_acks.is_empty()
    );
    assert!(prepare(&tail, &peer(), 5).unwrap().is_none());
}
#[test]
fn worst_case_json_bytes_are_bounded_and_prefix_never_skips_unselected_frames() {
    let mut tail = OutputTail::new(terminal(), &policy(65536, 1048576)).unwrap();
    for _ in 0..16 {
        tail.accept(&ControlEvent::Bytes(vec![255; 65536])).unwrap()
    }
    let mut through = 0;
    let mut seen = vec![];
    while let Some(p) = prepare(&tail, &peer(), through).unwrap() {
        assert!(serde_json::to_vec(&p.request).unwrap().len() <= MAX_UPLOAD);
        assert_eq!(p.request.frames[0].sequence, through + 1);
        seen.extend(p.request.frames.iter().map(|f| f.sequence));
        through = p.through;
    }
    assert_eq!(seen, (1..=16).collect::<Vec<_>>());
    assert_eq!(tail.next_sequence(), 17);
}
#[test]
fn unknown_producer_discontinuity_does_not_invent_an_exact_gap_or_restart() {
    let mut tail = OutputTail::new(terminal(), &policy(8, 16)).unwrap();
    tail.accept(&ControlEvent::Bytes(b"literal".to_vec()))
        .unwrap();
    tail.accept(&ControlEvent::Paused).unwrap();
    assert!(
        tail.accept(&ControlEvent::Bytes(b"not-restarted".to_vec()))
            .is_err()
    );
    assert_eq!(tail.next_sequence(), 2);
    assert_eq!(tail.gaps().count(), 0);
    assert_eq!(tail.state(), TailState::Discontinuous);
}
#[derive(Default)]
struct PeerState {
    mode: AtomicU8,
    registrations: Mutex<Vec<Vec<u8>>>,
    uploads: Mutex<Vec<Vec<u8>>>,
}
async fn register_peer(
    State(s): State<Arc<PeerState>>,
    headers: HeaderMap,
    body: axum::body::Bytes,
) -> Response {
    assert_eq!(headers["authorization"], "Bearer synthetic-node-credential");
    let q: Register = serde_json::from_slice(&body).unwrap();
    q.validate().unwrap();
    s.registrations.lock().unwrap().push(body.to_vec());
    let info = aq_terminal_relay::api::TerminalInfo {
        terminal: q.terminal.clone(),
        session_revision: q.expected_authority_revision,
        input_mode: q.input_mode,
        terminal_policy: policy(4, 8),
        control: aq_terminal_relay::draft::ControlStatus {
            terminal: q.terminal,
            session_revision: q.expected_authority_revision,
            generation: 0,
            lease: None,
            fence_acknowledged: false,
            input_paused: false,
        },
        last_peer_observed_at_ms: 1,
        last_output_observed_at_ms: 0,
        disconnected: false,
    };
    Json(aq_terminal_relay::api::MutationReply::Registered {
        operation_id: q.operation_id,
        info,
    })
    .into_response()
}
async fn upload_peer(State(s): State<Arc<PeerState>>, body: axum::body::Bytes) -> Response {
    let q: PeerUpload = serde_json::from_slice(&body).unwrap();
    q.validate().unwrap();
    assert!(
        q.snapshots.is_empty()
            && q.receipts.is_empty()
            && q.epoch_receipts.is_empty()
            && q.resize_receipts.is_empty()
            && q.fence_acks.is_empty()
    );
    s.uploads.lock().unwrap().push(body.to_vec());
    match s.mode.load(Ordering::SeqCst) {
        0 => (StatusCode::CONFLICT, "no current watch").into_response(),
        2 => ([("content-type", "application/json")], "{").into_response(),
        _ => Json(UploadAck {
            version: wire::VERSION,
            capability: q.capability,
            peer: q.peer,
            exchange_id: q.exchange_id,
            cursor: q.cursor,
            retirements: vec![],
            dropped_output: vec![],
        })
        .into_response(),
    }
}
// White-box retained transport state only: no LocalPhaseHost or launch token.
async fn transport_fixture() -> (
    PhaseOutputRelay,
    Arc<Mutex<Outputs>>,
    Arc<PeerState>,
    tokio::task::JoinHandle<()>,
) {
    let s = Arc::new(PeerState::default());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    let app = Router::new()
        .route("/v1/terminal-relay/register", post(register_peer))
        .route("/v1/terminal-relay/upload", post(upload_peer))
        .with_state(s.clone());
    let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    let http = RelayHttp::development_loopback(
        &url,
        &NodeCredential::new("synthetic-node-credential".into()).unwrap(),
    )
    .unwrap();
    let q = Register {
        version: wire::VERSION,
        capability: Text::new(wire::CAPABILITY).unwrap(),
        operation_id: Uuid::from_u128(9),
        peer: peer(),
        terminal: terminal(),
        expected_authority_revision: 77,
        input_mode: wire::InputMode::FiniteV1,
        advertised_capabilities: vec![aq_session_protocol::Capability {
            id: Text::new(wire::CAPABILITY).unwrap(),
            version: wire::VERSION,
        }],
    };
    let f: serde_json::Value = serde_json::from_slice(include_bytes!(
        "../../../../../vendor/aq-phase-terminal-protocol-0.1.0/artifacts/fixtures.json"
    ))
    .unwrap();
    let relay = Arc::new(Relay {
        terminal: terminal(),
        attempt: Uuid::from_u128(3),
        journal: Weak::new(),
        peer: peer(),
        started: serde_json::from_value(f["started_request"].clone()).unwrap(),
        policy: policy(4, 8),
        network: tokio::sync::Mutex::new(Network {
            http,
            registration: Some(q),
            info: None,
            through: 0,
            pending: None,
        }),
        draining: AtomicBool::new(false),
        task: Mutex::new(None),
    });
    let outputs = Arc::new(Mutex::new(Outputs::default()));
    {
        let mut o = outputs.lock().unwrap();
        o.tails.attach(terminal(), &policy(4, 8)).unwrap();
        o.tails
            .tail_mut(&terminal())
            .unwrap()
            .accept(&ControlEvent::Bytes(b"abcdefgh".to_vec()))
            .unwrap();
        o.owners.insert(Uuid::from_u128(3), relay.clone());
    }
    (
        PhaseOutputRelay {
            owner: relay,
            outputs: Arc::downgrade(&outputs),
        },
        outputs,
        s,
        server,
    )
}
#[tokio::test]
async fn no_watch_and_ambiguous_reply_keep_exact_pending_bytes_while_tail_continues() {
    let (handle, outputs, peer, server) = transport_fixture().await;
    let info = handle.register().await.unwrap();
    assert_eq!(info.session_revision, 77);
    assert_eq!(info.terminal, terminal());
    assert_eq!(peer.registrations.lock().unwrap().len(), 1);
    assert!(handle.upload_once().await.is_err());
    outputs
        .lock()
        .unwrap()
        .tails
        .tail_mut(&terminal())
        .unwrap()
        .accept(&ControlEvent::Bytes(b"ijklmnopqrst".to_vec()))
        .unwrap();
    assert_eq!(handle.status().unwrap().next_sequence, 6);
    peer.mode.store(2, Ordering::SeqCst);
    assert!(handle.upload_once().await.is_err());
    peer.mode.store(1, Ordering::SeqCst);
    handle.upload_once().await.unwrap().unwrap();
    let uploads = peer.uploads.lock().unwrap();
    assert_eq!(uploads.len(), 3);
    assert_eq!(uploads[0], uploads[1]);
    assert_eq!(uploads[1], uploads[2]);
    drop(uploads);
    handle.upload_once().await.unwrap().unwrap();
    let uploads = peer.uploads.lock().unwrap();
    let next: PeerUpload = serde_json::from_slice(&uploads[3]).unwrap();
    assert_eq!(next.gaps[0].first_missing, 3);
    assert_eq!(next.gaps[0].last_missing, 3);
    assert_eq!(next.frames[0].sequence, 4);
    drop(uploads);
    assert_eq!(peer.registrations.lock().unwrap().len(), 1);
    assert_eq!(handle.status().unwrap().next_sequence, 6);
    server.abort();
}
#[tokio::test]
async fn dropped_pipeline_tail_owner_cannot_be_reconstructed_from_transport_history() {
    let (handle, outputs, _peer, server) = transport_fixture().await;
    drop(outputs);
    assert!(handle.register().await.is_err());
    assert!(handle.upload_once().await.is_err());
    assert!(handle.status().is_err());
    server.abort();
}
