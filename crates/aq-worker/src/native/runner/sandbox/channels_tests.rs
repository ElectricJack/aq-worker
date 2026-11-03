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
