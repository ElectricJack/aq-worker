//! `aq environment try` (aq-server spec 0017 §6.2): the node pushes the
//! agent's workspace and the channel forwards the pushed commit.
use super::*;
use crate::{CheckpointError, TRY_PATH};

/// `(method, path, body)` of each request the stub saw.
type Forwarded = Arc<Mutex<Vec<(String, String, Vec<u8>)>>>;

async fn recorder() -> (String, Forwarded) {
    let seen: Forwarded = Arc::default();
    let log = seen.clone();
    let app = Router::new().fallback(move |method: axum::http::Method, uri: Uri, body: Bytes| {
        let log = log.clone();
        async move {
            log.lock()
                .unwrap()
                .push((method.to_string(), uri.path().into(), body.to_vec()));
            Json(serde_json::json!({"job_id": Uuid::from_u128(1)}))
        }
    });
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let origin = format!("http://{}", listener.local_addr().unwrap());
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    (origin, seen)
}

async fn open(origin: &str, root: &std::path::Path) -> AgentChannel {
    AgentChannel::open(
        config(root, origin, now() + 600_000),
        scripted(600_000, u64::MAX),
    )
    .await
    .unwrap()
}

async fn send(socket: &std::path::Path, method: &str, path: &str, body: &str) -> (u16, String) {
    let mut stream = tokio::net::UnixStream::connect(socket).await.unwrap();
    let request = format!(
        "{method} {path} HTTP/1.1\r\nHost: aq\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{body}",
        body.len()
    );
    stream.write_all(request.as_bytes()).await.unwrap();
    let mut out = Vec::new();
    stream.read_to_end(&mut out).await.unwrap();
    let text = String::from_utf8(out).unwrap();
    let status = text[9..12].parse().unwrap();
    (status, text.split_once("\r\n\r\n").unwrap().1.to_owned())
}

const PUSHED: &str = "0123456789abcdef0123456789abcdef01234567";

/// A checkpoint that answers `outcome` and counts its calls.
fn fake(outcome: Result<&'static str, CheckpointError>) -> (crate::Checkpoint, Arc<AtomicU64>) {
    let calls = Arc::new(AtomicU64::new(0));
    let counted = calls.clone();
    let checkpoint: crate::Checkpoint = Arc::new(move |_guard| {
        counted.fetch_add(1, Ordering::SeqCst);
        let outcome = outcome.map(str::to_owned);
        Box::pin(async move { outcome })
    });
    (checkpoint, calls)
}

fn json(body: &[u8]) -> serde_json::Value {
    serde_json::from_slice(body).unwrap()
}

#[test]
fn try_pushes_the_workspace_and_forwards_the_pushed_commit() {
    use aq_node_git::test_support::{self as git, Central};
    let central = Central::start();
    let state = tempfile::tempdir().unwrap();
    let node = Arc::new(git::config(state.path()).node(git::remote(&central)));
    let (attempt, path) = (Uuid::from_u128(7), git::at(state.path(), 7));
    let runtime = tokio::runtime::Runtime::new().unwrap();
    runtime.block_on(async {
        let grant = central.grant("e1", None);
        node.start(attempt, path.clone(), grant).await.unwrap();
        let (origin, seen) = recorder().await;
        let root = tempfile::tempdir().unwrap();
        let channel = open(&origin, root.path()).await;
        let checkpoint: crate::Checkpoint = Arc::new(move |guard| {
            let (node, path) = (node.clone(), path.clone());
            Box::pin(async move {
                let live =
                    |tree: &std::path::Path| Ok(std::fs::write(tree.join("b.txt"), "live\n")?);
                let pushed = node.checkpoint(path, "AQ attempt".into(), None, live, guard);
                pushed.await.map_err(|_| CheckpointError::PushFailed)
            })
        });
        channel.set_checkpoint(checkpoint);

        let body = r#"{"api_version":1,"name":"rust","run":"cargo test"}"#;
        let (status, answer) = send(&channel.socket_path(), "POST", TRY_PATH, body).await;
        assert_eq!(status, 200, "{answer}");
        let forwarded = seen.lock().unwrap().clone();
        let [(method, at, sent)] = &forwarded[..] else {
            panic!("{forwarded:?}")
        };
        assert_eq!((method.as_str(), at.as_str()), ("POST", TRY_PATH));
        let sent = json(sent);
        let commit = sent["commit"].as_str().unwrap();
        let try_ref = format!("refs/heads/aq-try/{attempt}");
        assert_eq!(central.head(&try_ref).as_deref(), Some(commit));
        let file = format!("{commit}:b.txt");
        assert_eq!(git::git(&central.remote(), &["show", &file]), "live");
        assert_eq!(
            sent,
            serde_json::json!({"api_version":1,"name":"rust","run":"cargo test","commit":commit})
        );
        assert_eq!(central.head("refs/heads/aq/e1"), None, "never the task ref");
    });
}

#[tokio::test]
async fn an_agent_supplied_commit_is_overwritten() {
    let (origin, seen) = recorder().await;
    let root = tempfile::tempdir().unwrap();
    let channel = open(&origin, root.path()).await;
    let socket = channel.socket_path();
    let agent = format!(
        r#"{{"api_version":1,"name":"rust","commit":"{}"}}"#,
        "f".repeat(40)
    );
    // Without a checkpoint the agent's value is dropped: the server answers
    // `workspace_not_pushed`.
    assert_eq!(send(&socket, "POST", TRY_PATH, &agent).await.0, 200);
    let (checkpoint, calls) = fake(Ok(PUSHED));
    channel.set_checkpoint(checkpoint);
    assert_eq!(send(&socket, "POST", TRY_PATH, &agent).await.0, 200);
    let bodies: Vec<_> = seen
        .lock()
        .unwrap()
        .iter()
        .map(|(_, _, b)| json(b))
        .collect();
    assert_eq!(
        bodies,
        [
            serde_json::json!({"api_version":1,"name":"rust"}),
            serde_json::json!({"api_version":1,"name":"rust","commit":PUSHED}),
        ]
    );
    assert_eq!(calls.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn a_failed_checkpoint_push_is_reported_and_not_forwarded() {
    let (origin, seen) = recorder().await;
    let root = tempfile::tempdir().unwrap();
    let channel = open(&origin, root.path()).await;
    let socket = channel.socket_path();
    let body = r#"{"api_version":1,"name":"rust"}"#;
    for (outcome, status, code) in [
        (
            Err(CheckpointError::PushFailed),
            502,
            "checkpoint_push_failed",
        ),
        (Err(CheckpointError::Refused), 422, "checkpoint_refused"),
        // A checkpoint answer that is not a full object id is never forwarded.
        (Ok("HEAD"), 502, "checkpoint_push_failed"),
    ] {
        channel.set_checkpoint(fake(outcome).0);
        let (got, answer) = send(&socket, "POST", TRY_PATH, body).await;
        assert_eq!(got, status, "{answer}");
        let refusal = json(answer.as_bytes());
        assert_eq!(
            (refusal["code"].as_str(), refusal["effect"].as_str()),
            (Some(code), Some("not_applied"))
        );
        let message = refusal["message"].as_str().unwrap();
        assert!(
            !message.contains(&origin) && !message.contains("aqa_"),
            "{message}"
        );
    }
    // A body that is not a JSON object is refused before any push.
    let (checkpoint, calls) = fake(Ok(PUSHED));
    channel.set_checkpoint(checkpoint);
    assert_eq!(send(&socket, "POST", TRY_PATH, "[1]").await.0, 400);
    assert_eq!(calls.load(Ordering::SeqCst), 0);
    assert!(seen.lock().unwrap().is_empty(), "nothing was forwarded");
}

#[tokio::test]
async fn other_agent_routes_are_forwarded_unchanged() {
    let (origin, seen) = recorder().await;
    let root = tempfile::tempdir().unwrap();
    let channel = open(&origin, root.path()).await;
    let socket = channel.socket_path();
    let (checkpoint, calls) = fake(Ok(PUSHED));
    channel.set_checkpoint(checkpoint);
    let surface = r#"{"params":{"name":"rust","commit":"agent"}}"#;
    let job = format!("{TRY_PATH}/{}", Uuid::from_u128(1));
    for (method, path, body) in [
        ("POST", "/api/v1/commands/environment.try", surface),
        ("GET", job.as_str(), ""),
        ("POST", "/v1/agent/context", r#"{"commit":"agent"}"#),
    ] {
        assert_eq!(send(&socket, method, path, body).await.0, 200, "{path}");
    }
    let seen = seen.lock().unwrap().clone();
    let bodies: Vec<_> = seen
        .iter()
        .map(|(m, p, b)| (m.as_str(), p.as_str(), b.as_slice()))
        .collect();
    assert_eq!(
        bodies,
        [
            (
                "POST",
                "/api/v1/commands/environment.try",
                surface.as_bytes()
            ),
            ("GET", job.as_str(), &b""[..]),
            ("POST", "/v1/agent/context", &br#"{"commit":"agent"}"#[..]),
        ]
    );
    assert_eq!(calls.load(Ordering::SeqCst), 0);
}

/// One snapshot per attempt: a try while another still holds the guard is
/// refused before its checkpoint runs (no Docker read), and the first one
/// completes. The guard ends with the work, not with the waiting request.
#[tokio::test]
async fn a_second_concurrent_try_is_refused_and_the_first_completes() {
    let (origin, seen) = recorder().await;
    let root = tempfile::tempdir().unwrap();
    let channel = open(&origin, root.path()).await;
    let socket = channel.socket_path();
    let (calls, release) = (
        Arc::new(AtomicU64::new(0)),
        Arc::new(tokio::sync::Notify::new()),
    );
    let (counted, waited) = (calls.clone(), release.clone());
    channel.set_checkpoint(Arc::new(move |guard| {
        counted.fetch_add(1, Ordering::SeqCst);
        let waited = waited.clone();
        Box::pin(async move {
            waited.notified().await;
            drop(guard);
            Ok(PUSHED.to_owned())
        })
    }));
    let body = r#"{"api_version":1,"name":"rust"}"#;
    let first = tokio::spawn({
        let socket = socket.clone();
        async move { send(&socket, "POST", TRY_PATH, body).await }
    });
    while calls.load(Ordering::SeqCst) == 0 {
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    let (status, answer) = send(&socket, "POST", TRY_PATH, body).await;
    assert_eq!(status, 502, "{answer}");
    let refusal = json(answer.as_bytes());
    assert_eq!(refusal["code"], "checkpoint_push_failed");
    assert!(
        refusal["message"]
            .as_str()
            .unwrap()
            .contains("already in progress")
    );
    assert_eq!(
        calls.load(Ordering::SeqCst),
        1,
        "the second try did no work"
    );
    release.notify_one();
    assert_eq!(first.await.unwrap().0, 200);
    let bodies: Vec<_> = seen
        .lock()
        .unwrap()
        .iter()
        .map(|(_, _, b)| json(b))
        .collect();
    assert_eq!(
        bodies,
        [serde_json::json!({"api_version":1,"name":"rust","commit":PUSHED})]
    );
    // The guard is free again once the first try's work ended.
    let next = tokio::spawn(async move { send(&socket, "POST", TRY_PATH, body).await });
    while calls.load(Ordering::SeqCst) < 2 {
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    release.notify_one();
    assert_eq!(next.await.unwrap().0, 200);
}

#[tokio::test]
async fn an_encoded_environments_path_is_refused() {
    let (origin, seen) = recorder().await;
    let root = tempfile::tempdir().unwrap();
    let channel = open(&origin, root.path()).await;
    let socket = channel.socket_path();
    let (checkpoint, calls) = fake(Ok(PUSHED));
    channel.set_checkpoint(checkpoint);
    let agent = r#"{"api_version":1,"name":"rust","commit":"agent"}"#;
    for (method, path) in [
        ("POST", "/v1/agent/environments/tr%79"),
        ("POST", "/v1/agent/environments/%74ry"),
        ("GET", "/v1/agent/environments/try/%31"),
    ] {
        let (status, answer) = send(&socket, method, path, agent).await;
        assert_eq!(status, 403, "{path}: {answer}");
        assert_eq!(json(answer.as_bytes())["code"], "channel_refused");
    }
    assert!(seen.lock().unwrap().is_empty());
    assert_eq!(calls.load(Ordering::SeqCst), 0);
}
