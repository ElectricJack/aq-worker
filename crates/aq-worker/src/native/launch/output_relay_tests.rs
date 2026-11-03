//! Pure tail and synthetic HTTP exchange tests. No live host/authority is forged.
//! Actual host attachment and daemon composition are qualified separately.
use super::*;
use aq_session_protocol::{SessionIdentity, TerminalHost};
use aq_terminal_bridge::http::NodeCredential;
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
    let mut tail = TerminalOutput::tail_only(terminal(), &policy(4, 8)).unwrap();
    tail.accept(&ControlEvent::Bytes(b"abcdefghijklmnopqrst".to_vec()))
        .unwrap();
    let p = prepare(&tail, &peer(), 0, false).unwrap().unwrap();
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
    assert!(prepare(&tail, &peer(), 5, false).unwrap().is_none());
}
#[test]
fn worst_case_json_bytes_are_bounded_and_prefix_never_skips_unselected_frames() {
    let negotiated = policy(65536, 1048576);
    let framing = local_framing(&negotiated);
    assert!(framing.max_frame_bytes < negotiated.max_frame_bytes);
    let mut tail = TerminalOutput::tail_only(terminal(), &framing).unwrap();
    for _ in 0..16 {
        tail.accept(&ControlEvent::Bytes(vec![255; 65536])).unwrap()
    }
    let mut through = 0;
    let mut seen = vec![];
    while let Some(p) = prepare(&tail, &peer(), through, false).unwrap() {
        assert!(serde_json::to_vec(&p.request).unwrap().len() <= MAX_UPLOAD);
        assert_eq!(p.request.frames[0].sequence, through + 1);
        seen.extend(p.request.frames.iter().map(|f| f.sequence));
        through = p.through;
    }
    assert_eq!(seen, (1..tail.next_sequence()).collect::<Vec<_>>());
    assert_eq!(
        tail.frames().map(|f| f.bytes.len()).sum::<usize>(),
        16 * 65536
    );
    assert_eq!(
        MAX_UPLOAD,
        256 * 1024,
        "must fit the actual RelayHttp transport ceiling"
    );
    assert_eq!(tail.next_sequence(), 33);
}
#[test]
fn unknown_producer_discontinuity_does_not_invent_an_exact_gap_or_restart() {
    let mut tail = TerminalOutput::tail_only(terminal(), &policy(8, 16)).unwrap();
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
    policy: Option<TerminalPolicy>,
    mode: AtomicU8,
    registrations: Mutex<Vec<Vec<u8>>>,
    uploads: Mutex<Vec<Vec<u8>>>,
    polls: Mutex<Vec<Vec<u8>>>,
    poll_mode: AtomicU8,
    upload_started: tokio::sync::Notify,
    release_upload: tokio::sync::Notify,
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
        terminal_policy: s.policy.clone().unwrap_or_else(|| policy(4, 8)),
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
    if s.mode.load(Ordering::SeqCst) == 3 {
        s.upload_started.notify_one();
        s.release_upload.notified().await;
    }
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
async fn poll_peer(
    State(s): State<Arc<PeerState>>,
    headers: HeaderMap,
    body: axum::body::Bytes,
) -> Response {
    assert_eq!(headers["authorization"], "Bearer synthetic-node-credential");
    let q: wire::RelayPoll = serde_json::from_slice(&body).unwrap();
    q.validate().unwrap();
    assert_eq!(q.hold_ms, 0);
    assert_eq!(q.active_controllers, 0);
    s.polls.lock().unwrap().push(body.to_vec());
    let effect = if s.poll_mode.load(Ordering::SeqCst) == 1 {
        wire::Effect::Fence(wire::Fence {
            terminal: terminal(),
            fence_id: Uuid::new_v4(),
            generation: 1,
            controller: None,
        })
    } else {
        wire::Effect::Watch(aq_session_protocol::WatchSubscription {
            subscription_id: Uuid::from_u128(88),
            terminal: terminal(),
            principal_id: Text::new("synthetic-viewer").unwrap(),
            expires_at_ms: u64::MAX,
        })
    };
    Json(wire::Envelope {
        version: wire::VERSION,
        capability: q.capability,
        peer: q.peer,
        exchange_id: Uuid::new_v4(),
        cursor: if s.poll_mode.load(Ordering::SeqCst) == 2 {
            0
        } else {
            7
        },
        effects: vec![effect],
    })
    .into_response()
}
// White-box retained transport state only: no LocalPhaseHost or launch token.
async fn transport_fixture() -> (
    PhaseOutputRelay,
    Arc<Mutex<Outputs>>,
    Arc<PeerState>,
    tokio::task::JoinHandle<()>,
) {
    transport_fixture_policy(policy(4, 8)).await
}
async fn transport_fixture_policy(
    negotiated: TerminalPolicy,
) -> (
    PhaseOutputRelay,
    Arc<Mutex<Outputs>>,
    Arc<PeerState>,
    tokio::task::JoinHandle<()>,
) {
    let s = Arc::new(PeerState {
        policy: Some(negotiated.clone()),
        ..PeerState::default()
    });
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    let app = Router::new()
        .route("/v1/terminal-relay/register", post(register_peer))
        .route("/v1/terminal-relay/upload", post(upload_peer))
        .route("/v1/terminal-relay/output-poll", post(poll_peer))
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
        started: RetainedStarted::Frozen(
            serde_json::from_value(f["started_request"].clone()).unwrap(),
        ),
        policy: negotiated.clone(),
        polling: tokio::sync::Mutex::new(OutputPoll {
            http: http.clone(),
            cursor: 0,
            watches: BTreeMap::new(),
        }),
        network: tokio::sync::Mutex::new(Network {
            http,
            registration: Some(q),
            info: None,
            through: 0,
            pending: None,
            rate: OutputRate::default(),
        }),
        draining: AtomicBool::new(false),
        snapshot_requested: AtomicBool::new(false),
        task: Mutex::new(None),
    });
    let outputs = Arc::new(Mutex::new(Outputs::default()));
    {
        let mut o = outputs.lock().unwrap();
        o.tails
            .attach(terminal(), &local_framing(&negotiated))
            .unwrap();
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
    {
        let uploads = peer.uploads.lock().unwrap();
        assert_eq!(uploads.len(), 3);
        assert_eq!(uploads[0], uploads[1]);
        assert_eq!(uploads[1], uploads[2]);
    }
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

#[tokio::test]
async fn high_byte_frames_fit_actual_http_transport_without_losing_sequences() {
    let (relay, outputs, peer, server) = transport_fixture_policy(policy(65536, 1048576)).await;
    peer.mode.store(1, Ordering::SeqCst);
    {
        let mut outputs = outputs.lock().unwrap();
        let tail = outputs.tails.tail_mut(&terminal()).unwrap();
        for _ in 0..4 {
            tail.accept(&ControlEvent::Bytes(vec![255; 65536])).unwrap();
        }
    }
    for _ in 0..16 {
        if relay.upload_once().await.unwrap().is_none() {
            break;
        }
    }
    let uploads = peer.uploads.lock().unwrap();
    let mut bytes = 0;
    let mut sequences = vec![];
    for encoded in uploads.iter() {
        assert!(encoded.len() <= 256 * 1024);
        let request: PeerUpload = serde_json::from_slice(encoded).unwrap();
        for frame in request.frames {
            bytes += frame.bytes.len();
            sequences.push(frame.sequence);
        }
    }
    // The transport fixture already has one eight-byte literal prefix.
    assert_eq!(bytes, 4 * 65536 + 8);
    assert_eq!(sequences, (1..=9).collect::<Vec<_>>());
    server.abort();
}

#[tokio::test]
async fn watch_poll_keeps_pending_upload_bytes_and_independent_cursors_after_409() {
    let (relay, _outputs, peer, server) = transport_fixture().await;
    assert!(relay.upload_once().await.is_err());
    let before = peer.uploads.lock().unwrap()[0].clone();
    assert_eq!(relay.poll_watch_once().await.unwrap(), 1);
    {
        let network = relay.owner.network.lock().await;
        assert_eq!(network.through, 0);
        assert_eq!(
            serde_json::to_vec(&network.pending.as_ref().unwrap().request).unwrap(),
            before
        );
        assert_eq!(network.pending.as_ref().unwrap().request.cursor, 0);
    }
    assert_eq!(relay.owner.polling.lock().await.cursor, 7);
    peer.mode.store(1, Ordering::SeqCst);
    relay.upload_once().await.unwrap().unwrap();
    assert_eq!(peer.uploads.lock().unwrap()[1], before);
    assert_eq!(relay.owner.network.lock().await.through, 2);
    assert_eq!(relay.owner.polling.lock().await.cursor, 7);
    assert_eq!(relay.poll_watch_once().await.unwrap(), 1);
    let polls = peer.polls.lock().unwrap();
    let next: wire::RelayPoll = serde_json::from_slice(&polls[1]).unwrap();
    assert_eq!(next.cursor, 7);
    assert_eq!(next.active_subscriptions, 1);
    server.abort();
}
#[tokio::test]
async fn output_poll_never_consumes_control_or_regressing_cursor() {
    let (relay, _outputs, peer, server) = transport_fixture().await;
    peer.poll_mode.store(1, Ordering::SeqCst);
    assert!(relay.poll_watch_once().await.is_err());
    assert_eq!(relay.owner.polling.lock().await.cursor, 0);
    assert!(relay.owner.polling.lock().await.watches.is_empty());
    peer.poll_mode.store(0, Ordering::SeqCst);
    relay.poll_watch_once().await.unwrap();
    peer.poll_mode.store(2, Ordering::SeqCst);
    assert!(relay.poll_watch_once().await.is_err());
    assert_eq!(relay.owner.polling.lock().await.cursor, 7);
    assert_eq!(relay.owner.network.lock().await.through, 0);
    assert!(
        peer.uploads.lock().unwrap().is_empty(),
        "poll must never ACK an effect"
    );
    server.abort();
}
#[tokio::test]
async fn ordinary_publisher_polls_while_upload_is_blocked_and_drop_aborts_both_tasks() {
    let (relay, _outputs, peer, server) = transport_fixture().await;
    peer.mode.store(3, Ordering::SeqCst);
    let publisher = crate::native::runner::publish_output::OutputPublisher::start(relay.clone());
    tokio::time::timeout(Duration::from_secs(2), peer.upload_started.notified())
        .await
        .unwrap();
    tokio::time::timeout(Duration::from_secs(1), async {
        loop {
            if !peer.polls.lock().unwrap().is_empty() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    assert_eq!(peer.uploads.lock().unwrap().len(), 1);
    assert_eq!(relay.owner.polling.lock().await.cursor, 7);
    drop(publisher);
    tokio::time::sleep(Duration::from_millis(20)).await;
    let count = peer.polls.lock().unwrap().len();
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_eq!(
        peer.polls.lock().unwrap().len(),
        count,
        "dropped publisher must not keep polling"
    );
    peer.release_upload.notify_one();
    server.abort();
}
#[tokio::test]
async fn watch_metadata_is_bounded_expiring_and_never_advances_on_unsupported_effect() {
    let (relay, _outputs, _peer, server) = transport_fixture().await;
    let mut state = relay.owner.polling.lock().await;
    let watch = |id, expires| {
        wire::Effect::Watch(aq_session_protocol::WatchSubscription {
            subscription_id: Uuid::from_u128(id),
            terminal: terminal(),
            principal_id: Text::new("synthetic-viewer").unwrap(),
            expires_at_ms: expires,
        })
    };
    let mut envelope = wire::Envelope {
        version: wire::VERSION,
        capability: Text::new(wire::CAPABILITY).unwrap(),
        peer: peer(),
        exchange_id: Uuid::new_v4(),
        cursor: 1,
        effects: (1..=4).map(|id| watch(id, 100)).collect(),
    };
    assert_eq!(state.accept(&envelope, &terminal(), 1).unwrap(), 4);
    envelope.cursor = 2;
    envelope.effects = vec![watch(33, 100)];
    assert!(state.accept(&envelope, &terminal(), 1).is_err());
    assert_eq!(state.cursor, 1);
    assert_eq!(state.watches.len(), 4);
    envelope.effects = vec![watch(33, 200)];
    assert_eq!(state.accept(&envelope, &terminal(), 100).unwrap(), 1);
    assert_eq!(state.cursor, 2);
    // Watches for this node's other terminals also consume its 32 slots.
    envelope.cursor = 3;
    envelope.effects = (100..131)
        .map(|id| {
            let wire::Effect::Watch(mut w) = watch(id, 200) else {
                unreachable!()
            };
            w.terminal.terminal_id = Uuid::from_u128(1000 + id / 4);
            wire::Effect::Watch(w)
        })
        .collect();
    assert_eq!(state.accept(&envelope, &terminal(), 100).unwrap(), 1);
    assert_eq!(state.watches.len(), 32);
    envelope.cursor = 4;
    let wire::Effect::Watch(mut extra) = watch(999, 200) else {
        unreachable!()
    };
    extra.terminal.terminal_id = Uuid::from_u128(9999);
    envelope.effects = vec![wire::Effect::Watch(extra)];
    assert!(state.accept(&envelope, &terminal(), 100).is_err());
    assert_eq!(state.cursor, 3);
    envelope.cursor = 4;
    envelope.effects = vec![wire::Effect::Fence(wire::Fence {
        terminal: terminal(),
        fence_id: Uuid::new_v4(),
        generation: 1,
        controller: None,
    })];
    assert!(state.accept(&envelope, &terminal(), 100).is_err());
    assert_eq!(state.cursor, 3);
    server.abort();
}
