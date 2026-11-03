//! Per-sandbox terminal long poll: held only on demand, one at a time, bounded.
use super::auth_tests::client;
use super::channels::{SandboxChannels, TerminalDemand};
use axum::{
    Router,
    body::Bytes,
    extract::State,
    http::{HeaderMap, Uri, header},
    response::{IntoResponse, Response},
    routing::any,
};
use serde_json::{Value, json};
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicBool, Ordering},
};
use tokio::sync::{Notify, Semaphore};
use uuid::Uuid;

type Seen = (String, Option<String>, Value);
#[derive(Clone)]
struct Relay {
    seen: Arc<Mutex<Vec<Seen>>>,
    arrived: Arc<Notify>,
    release: Arc<Semaphore>,
    oversize: Arc<AtomicBool>,
}
impl Default for Relay {
    fn default() -> Self {
        Self {
            seen: Default::default(),
            arrived: Default::default(),
            release: Arc::new(Semaphore::new(0)),
            oversize: Default::default(),
        }
    }
}
async fn relay(State(r): State<Relay>, uri: Uri, headers: HeaderMap, body: Bytes) -> Response {
    let poll: Value = serde_json::from_slice(&body).unwrap_or(Value::Null);
    r.seen.lock().unwrap().push((
        uri.to_string(),
        headers
            .get(header::AUTHORIZATION)
            .map(|v| v.to_str().unwrap().into()),
        poll.clone(),
    ));
    r.arrived.notify_one();
    // Stands in for the server-side hold; the test releases it explicitly.
    r.release.acquire().await.unwrap().forget();
    if r.oversize.load(Ordering::SeqCst) {
        return vec![b' '; 64 * 1024 + 1].into_response();
    }
    let envelope = json!({"version":1,"capability":"aq.terminal-relay","peer":poll["peer"],
        "exchange_id":Uuid::new_v4(),"cursor":poll["cursor"].as_u64().unwrap() + 1,"effects":[]});
    (
        [(header::CONTENT_TYPE, "application/json")],
        envelope.to_string(),
    )
        .into_response()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn idle_sandbox_holds_no_terminal_poll() {
    let _clock = crate::native::http::test_clock::TestClock::new();
    let state = Relay::default();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let origin = format!("http://{}", listener.local_addr().unwrap());
    let router = Router::new().fallback(any(relay)).with_state(state.clone());
    let server = tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
    let (_dir, http, permit) = client(&origin, false);
    let channels = Arc::new(SandboxChannels::new(&http).unwrap());
    assert!(
        channels
            .set_demand(TerminalDemand {
                subscriptions: 5,
                controllers: 0
            })
            .is_err()
    );

    // Idle: neither a direct poll nor the long-poll loop sends anything.
    assert!(channels.poll_terminal().await.unwrap().is_none());
    let (tx, mut delivered) = tokio::sync::mpsc::unbounded_channel();
    let runner = {
        let channels = channels.clone();
        tokio::spawn(async move { channels.run_terminal(|e| tx.send(e).unwrap()).await })
    };
    for _ in 0..16 {
        tokio::task::yield_now().await;
    }
    assert!(state.seen.lock().unwrap().is_empty(), "idle sandbox polled");
    assert!(!channels.terminal_held());

    // Demand: exactly one held poll, on the sandbox channel, permit-authenticated.
    channels
        .set_demand(TerminalDemand {
            subscriptions: 1,
            controllers: 0,
        })
        .unwrap();
    state.arrived.notified().await;
    assert!(channels.terminal_held());
    assert!(
        channels.poll_terminal().await.is_err(),
        "second concurrent terminal poll for one sandbox"
    );
    {
        let seen = state.seen.lock().unwrap();
        assert_eq!(seen.len(), 1);
        let (path, authorization, poll) = &seen[0];
        assert_eq!(path, "/v1/terminal-relay/poll");
        assert_eq!(
            authorization.as_deref(),
            Some("Bearer fixture_only_not_a_real_secret_00000000")
        );
        assert_eq!(poll["hold_ms"], 20_000);
        assert_eq!(poll["active_subscriptions"], 1);
        assert_eq!(
            poll["peer"],
            json!({"kind":"remote","node_id":permit.binding.node.node_id,
                "boot_id":permit.binding.node.boot_id})
        );
    }

    // Back to idle before the server answers: the loop delivers and holds nothing.
    channels.set_demand(TerminalDemand::default()).unwrap();
    state.release.add_permits(1);
    let envelope = delivered.recv().await.unwrap();
    assert_eq!(envelope.cursor, 1);
    for _ in 0..16 {
        tokio::task::yield_now().await;
    }
    assert!(!channels.terminal_held());
    assert_eq!(state.seen.lock().unwrap().len(), 1);
    runner.abort();

    // Frames are bounded at 64 KiB.
    channels
        .set_demand(TerminalDemand {
            subscriptions: 0,
            controllers: 1,
        })
        .unwrap();
    state.oversize.store(true, Ordering::SeqCst);
    state.release.add_permits(1);
    assert!(channels.poll_terminal().await.is_err());
    assert!(!channels.terminal_held());
    assert!(
        !http.authority_lost(),
        "a bad frame is not a permit refusal"
    );
    server.abort();
}

#[tokio::test]
async fn output_runtime_rejects_unrelated_authority_and_releases_held_poll_on_stop() {
    use super::output_runtime::OutputRuntime;
    let _clock = crate::native::http::test_clock::TestClock::new();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let origin = format!("http://{}", listener.local_addr().unwrap());
    let arrived = std::sync::Arc::new(tokio::sync::Notify::new());
    let notified = arrived.clone();
    let router = axum::Router::new().fallback(move || {
        let notified = notified.clone();
        async move {
            notified.notify_one();
            std::future::pending::<axum::http::StatusCode>().await
        }
    });
    let server = tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
    let (_dir, http, _) = super::auth_tests::client(&origin, false);
    let (_other_dir, other, _) = super::auth_tests::client(&origin, false);
    let channels = std::sync::Arc::new(SandboxChannels::new(&http).unwrap());
    channels.validate_authority(&http).unwrap();
    assert!(channels.validate_authority(&other).is_err());
    channels
        .set_demand(TerminalDemand {
            subscriptions: 1,
            controllers: 0,
        })
        .unwrap();
    let lane = channels
        .output_from(
            crate::native::terminal_output::TerminalOutput::from_blank(
                aq_session_protocol::TerminalIdentity {
                    session: http.launch_binding().session,
                    terminal_id: uuid::Uuid::new_v4(),
                    epoch: 1,
                },
                &aq_session_protocol::TerminalPolicy {
                    host: aq_session_protocol::TerminalHost::Tmux,
                    input_enabled: false,
                    max_input_bytes: 1,
                    max_pending_input_bytes: 1,
                    max_frame_bytes: 4,
                    replay_bytes: 8,
                },
                aq_session_protocol::ArtifactRef {
                    id: aq_session_protocol::Text::new("aq-terminal-renderer").unwrap(),
                    version: aq_session_protocol::Text::new("0.1.0").unwrap(),
                    digest: aq_session_protocol::Digest::of(b"fixture"),
                },
                40,
                4,
            )
            .unwrap(),
        )
        .unwrap();
    let (_cancel, shutdown) = tokio::sync::watch::channel(false);
    let runtime = OutputRuntime::start(channels.clone(), lane, shutdown);
    arrived.notified().await;
    // A stalled terminal long poll is held by runtime, never by renewal.
    while !channels.terminal_held() {
        tokio::task::yield_now().await;
    }
    runtime.stop().await;
    assert!(!channels.terminal_held());
    server.abort();
}

#[tokio::test]
async fn foreign_output_watch_never_advances_sandbox_cursor() {
    use aq_session_protocol::{TerminalHost, TerminalIdentity, TerminalPolicy};
    let _clock = crate::native::http::test_clock::TestClock::new();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let origin = format!("http://{}", listener.local_addr().unwrap());
    let (_dir, http, _) = client(&origin, false);
    let terminal = TerminalIdentity {
        session: http.launch_binding().session,
        terminal_id: Uuid::new_v4(),
        epoch: 1,
    };
    let mut foreign = terminal.clone();
    foreign.epoch += 1;
    let cursors = Arc::new(Mutex::new(Vec::new()));
    let seen = cursors.clone();
    let router = Router::new().fallback(move |axum::Json(q): axum::Json<Value>| {
        seen.lock().unwrap().push(q["cursor"].as_u64().unwrap());
        let foreign = foreign.clone();
        async move { axum::Json(json!({"version":1,"capability":"aq.terminal-relay","peer":q["peer"],
            "exchange_id":Uuid::new_v4(),"cursor":q["cursor"].as_u64().unwrap()+1,
            "effects":[{"kind":"watch","body":{"terminal":foreign,"subscription_id":Uuid::new_v4(),"principal_id":"viewer","expires_at_ms":10000}}]})) }
    });
    let server = tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
    let channels = SandboxChannels::new(&http).unwrap();
    channels
        .set_demand(TerminalDemand {
            subscriptions: 1,
            controllers: 0,
        })
        .unwrap();
    let lane = channels
        .output_from(
            crate::native::terminal_output::TerminalOutput::tail_only(
                terminal,
                &TerminalPolicy {
                    host: TerminalHost::Tmux,
                    input_enabled: false,
                    max_input_bytes: 1,
                    max_pending_input_bytes: 1,
                    max_frame_bytes: 4,
                    replay_bytes: 8,
                },
            )
            .unwrap(),
        )
        .unwrap();
    assert!(channels.run_output(lane.clone()).await.is_err());
    assert!(channels.run_output(lane).await.is_err());
    assert_eq!(*cursors.lock().unwrap(), vec![0, 0]);
    server.abort();
}
