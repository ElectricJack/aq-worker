//! Channel tests against a stub aq-server and a scripted issuer.
mod rotation;
mod stub;
use super::*;
use axum::{
    Json, Router,
    body::Bytes,
    http::{HeaderMap, StatusCode, Uri},
    response::IntoResponse,
};
use std::{
    os::unix::fs::{FileTypeExt, PermissionsExt},
    sync::{
        Mutex,
        atomic::{AtomicU64, Ordering},
    },
    time::Duration,
};
use stub::{stub, stub_with};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

pub(crate) fn token(n: u64) -> String {
    format!("aqa_{:0>43}", format!("fixture{n}"))
}

/// Issues `token(n)` for version n, each valid for `ttl_ms`.
struct Scripted {
    ttl_ms: u64,
    refuse_from: u64,
    refuse: std::sync::atomic::AtomicBool,
    calls: AtomicU64,
    /// Set to the version issued last, if any.
    live: Option<&'static AtomicU64>,
}
impl CredentialIssuer for Scripted {
    fn issue(&self, expected: u64) -> IssueFuture<'_> {
        Box::pin(async move {
            self.calls.fetch_add(1, Ordering::SeqCst);
            if expected + 1 >= self.refuse_from || self.refuse.load(Ordering::SeqCst) {
                return Err(IssueError::Refused("session_revoked".into()));
            }
            if let Some(live) = self.live {
                live.store(expected + 1, Ordering::SeqCst);
            }
            Ok(Issued {
                token: Zeroizing::new(token(expected + 1)),
                version: expected + 1,
                expires_at_ms: now() + self.ttl_ms,
            })
        })
    }
}
fn scripted(ttl_ms: u64, refuse_from: u64) -> Arc<Scripted> {
    Arc::new(Scripted {
        ttl_ms,
        refuse_from,
        refuse: Default::default(),
        calls: AtomicU64::new(0),
        live: None,
    })
}

fn now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis() as u64
}

fn config(root: &std::path::Path, origin: &str, not_after_ms: u64) -> ChannelConfig {
    ChannelConfig {
        attempt_id: Uuid::from_u128(7),
        directory: root.join(Uuid::from_u128(7).to_string()),
        origin: origin.into(),
        access: None,
        not_after_ms,
    }
}

/// One raw HTTP/1.1 exchange over the socket, as the CLI would send it.
async fn call(socket: &std::path::Path, method: &str, path: &str, extra: &str) -> (u16, String) {
    let mut stream = tokio::net::UnixStream::connect(socket).await.unwrap();
    let body = if method == "GET" { "" } else { "{}" };
    let host = if extra.contains("Host:") {
        ""
    } else {
        "Host: aq\r\n"
    };
    stream
        .write_all(
            format!(
                "{method} {path} HTTP/1.1\r\n{host}Content-Type: application/json\r\n{extra}Content-Length: {}\r\n\r\n{body}",
                body.len()
            )
            .as_bytes(),
        )
        .await
        .unwrap();
    let mut out = Vec::new();
    stream.read_to_end(&mut out).await.unwrap();
    let text = String::from_utf8(out).unwrap();
    let status = text[9..12].parse().unwrap();
    let body = text.split_once("\r\n\r\n").unwrap().1.to_owned();
    (status, body)
}

use uuid::Uuid;
use zeroize::Zeroizing;

#[test]
fn credential_guard_matches_issued_shapes_not_source_text() {
    assert!(holds_agent_credential(token(1).as_bytes()));
    assert!(holds_agent_credential(
        format!("export X={} # leaked", token(9)).as_bytes()
    ));
    assert!(!holds_agent_credential(b"if t.starts_with(\"aqa_\") {}"));
    assert!(!holds_agent_credential(b"aqa_short"));
    assert!(!holds_agent_credential(&token(1).as_bytes()[..30]));
}

#[test]
fn path_rule_is_coarse_prefixes_and_normal_form() {
    for ok in [
        "/api/v1/commands",
        "/api/v1/commands/whoami",
        "/api/v1/commands/task.create",
        "/api/v1/operations/0b7c",
        "/v1/agent/context",
    ] {
        assert_eq!(agent_path(ok), Ok(()), "{ok}");
    }
    for bad in [
        "/v2/worker/agent-session",
        "/v1/worker/heartbeat",
        "/v1/execution/x",
        "/api/v1/actions",
        "/api/human/projects",
        "/",
        "/api/v1/commands/../../v2/worker/agent-session",
        "/api/v1/commands/%2e%2e/%2e%2e/v2/worker/agent-session",
        "/api/v1/commands%2f..%2fv2",
        "/api/v1/commandsx",
    ] {
        assert!(agent_path(bad).is_err(), "{bad}");
    }
}

/// 0006 §5.1/§5.2: a caller reaches `whoami` with no credential of its own;
/// the stub sees only the attempt credential (spoofed Authorization and
/// Access headers are removed), node routes are refused without reaching the
/// server, and a response that echoes the credential is refused.
#[tokio::test]
async fn channel_injects_the_attempt_credential_and_refuses_node_paths() {
    let (origin, seen) = stub().await;
    let root = tempfile::tempdir().unwrap();
    let channel = AgentChannel::open(
        config(root.path(), &origin, now() + 600_000),
        scripted(600_000, u64::MAX),
    )
    .await
    .unwrap();
    let socket = channel.socket_path();
    let dir_mode = std::fs::metadata(channel.directory())
        .unwrap()
        .permissions()
        .mode()
        & 0o777;
    let meta = std::fs::symlink_metadata(&socket).unwrap();
    assert!(meta.file_type().is_socket());
    assert_eq!(
        (dir_mode, meta.permissions().mode() & 0o777),
        (0o711, 0o666)
    );

    let (status, body) = call(
        &socket,
        "POST",
        "/api/v1/commands/whoami",
        "Authorization: Bearer aqa_spoofed\r\ncf-access-client-id: spoof\r\n",
    )
    .await;
    assert_eq!(status, 200, "{body}");
    assert!(body.contains("\"principal\":\"agent\"") && !body.contains("aqa_"));

    // The CLI's own request form: `Host: localhost`, `X-AQ-Request: 1`, no
    // Authorization, a query string on discovery.
    let (status, _) = call(
        &socket,
        "GET",
        "/api/v1/commands?all=true",
        "Host: localhost\r\nX-AQ-Request: 1\r\n",
    )
    .await;
    assert_eq!(
        status, 404,
        "the stub has no discovery route; it was reached"
    );
    assert_refused_paths(&socket).await;
    let (status, body) = call(&socket, "POST", "/api/v1/commands/echo", "").await;
    assert_eq!(status, 502);
    assert!(!body.contains("aqa_"));

    let seen = seen.lock().unwrap().clone();
    assert_eq!(
        seen.iter().map(|s| s.path.as_str()).collect::<Vec<_>>(),
        [
            "/api/v1/commands/whoami",
            "/api/v1/commands?all=true x-aq-request",
            "/api/v1/commands/echo"
        ],
        "refused paths never reach the server"
    );
    for saw in &seen {
        assert_eq!(saw.auth, format!("Bearer {}", token(1)));
        assert_eq!(saw.access, None);
    }
    let dir = channel.directory().to_owned();
    channel.stop().await.unwrap();
    assert!(!dir.exists(), "stop removes the socket directory");
}

async fn assert_refused_paths(socket: &std::path::Path) {
    for path in [
        "/v2/worker/agent-session",
        "/v1/worker/x",
        "/api/v1/actions",
        "/api/v1/commands/../../v2/worker/agent-session",
    ] {
        let (status, body) = call(socket, "POST", path, "").await;
        assert_eq!(status, 403, "{path}");
        let refusal: serde_json::Value = serde_json::from_str(&body).unwrap();
        assert_eq!(
            refusal.as_object().unwrap().keys().collect::<Vec<_>>(),
            [
                "code",
                "command",
                "effect",
                "grant",
                "held",
                "message",
                "request_id"
            ],
            "a commands-1 Refusal"
        );
        assert_eq!(
            (refusal["code"].as_str(), refusal["effect"].as_str()),
            (Some("channel_refused"), Some("not_applied"))
        );
    }
}

/// The production issuer: request shape, the version fence and error classes.
#[tokio::test]
async fn node_issuer_uses_the_agent_session_route_with_the_version_fence() {
    let requests: Arc<Mutex<Vec<(String, serde_json::Value)>>> = Arc::default();
    let log = requests.clone();
    let app = Router::new().route(
        "/v2/worker/agent-session",
        axum::routing::post(move |headers: HeaderMap, Json(body): Json<serde_json::Value>| {
            let log = log.clone();
            async move {
                let auth = headers["authorization"].to_str().unwrap().to_owned();
                log.lock().unwrap().push((auth, body.clone()));
                let expected = body["expected_credential_version"].as_u64().unwrap();
                match expected {
                    0 => Json(serde_json::json!({"api_version":1,"token":token(1),"context":{
                        "credential":{"kind":"agent_session","version":"1","expires_at":"2099-01-01T00:00:00Z"}}}))
                        .into_response(),
                    1 => Json(serde_json::json!({"api_version":1,"token":token(5),"context":{
                        "credential":{"version":"5","expires_at":"2099-01-01T00:00:00Z"}}}))
                        .into_response(),
                    2 => (StatusCode::UNAUTHORIZED, Json(serde_json::json!({"code":"session_revoked"}))).into_response(),
                    _ => StatusCode::SERVICE_UNAVAILABLE.into_response(),
                }
            }
        }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let origin = format!("http://{}", listener.local_addr().unwrap());
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    let attempt = AttemptIdentity {
        node_id: Uuid::from_u128(1),
        boot_id: Uuid::from_u128(2),
        attempt_id: Uuid::from_u128(3),
        session_id: Uuid::from_u128(4),
        generation: 5,
    };
    assert!(
        NodeIssuer::new(&origin, &token(1), None, attempt).is_err(),
        "never an agent token"
    );
    let issuer =
        NodeIssuer::new(&origin, "node-credential-0123456789abcdef", None, attempt).unwrap();
    let issued = issuer.issue(0).await.unwrap();
    assert_eq!(
        (issued.version, issued.token.as_str()),
        (1, token(1).as_str())
    );
    assert!(!format!("{issued:?}").contains("aqa_"));
    assert_eq!(
        issuer.issue(1).await.unwrap_err(),
        IssueError::Refused("invalid_response".into()),
        "a version that skips the fence is refused"
    );
    assert_eq!(
        issuer.issue(2).await.unwrap_err(),
        IssueError::Refused("session_revoked".into())
    );
    assert_eq!(issuer.issue(3).await.unwrap_err(), IssueError::Unavailable);
    let (auth, body) = requests.lock().unwrap()[0].clone();
    assert_eq!(auth, "Bearer node-credential-0123456789abcdef");
    assert_eq!(
        body,
        serde_json::json!({
            "identity": {"node_id": attempt.node_id, "boot_id": attempt.boot_id},
            "attempt": {"attempt_id": attempt.attempt_id, "session_id": attempt.session_id, "generation": 5},
            "expected_credential_version": 0,
        })
    );
}

/// The full response text of one raw request.
async fn raw(socket: &std::path::Path, request: &str) -> String {
    let mut stream = tokio::net::UnixStream::connect(socket).await.unwrap();
    stream.write_all(request.as_bytes()).await.unwrap();
    let mut out = Vec::new();
    stream.read_to_end(&mut out).await.unwrap();
    String::from_utf8(out).unwrap()
}

async fn wait_gone(path: &std::path::Path) {
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    while path.exists() {
        assert!(
            std::time::Instant::now() < deadline,
            "{path:?} still exists"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

const NODE_CREDENTIAL: &str = "node-credential-0123456789abcdef";

fn attempt() -> AttemptIdentity {
    AttemptIdentity {
        node_id: Uuid::from_u128(1),
        boot_id: Uuid::from_u128(2),
        attempt_id: Uuid::from_u128(7),
        session_id: Uuid::from_u128(4),
        generation: 5,
    }
}

/// Finding 1: the channel serves the node's own enrollment under the rule
/// of the node's native transport (aq-worker `NativeClient::from_config`):
/// HTTPS to any host, plain HTTP only to a numeric loopback address. Hosts
/// the node itself refuses for plaintext stay refused. `docker_local_backend`
/// checks the same table against `NativeClient`.
#[test]
fn node_origins_follow_the_native_transport_rule() {
    for ok in [
        "https://aq.example.com",
        "https://aq.example.com:8443/",
        "https://10.0.0.5",
        "http://127.0.0.1:32839",
        "http://127.0.0.1",
        "http://127.0.0.2:8080",
        "http://[::1]:8080",
    ] {
        assert!(node_origin(ok).is_ok(), "{ok}");
        assert!(
            NodeLink::new(ok, NODE_CREDENTIAL, None, None).is_ok(),
            "{ok}"
        );
    }
    for bad in [
        "http://host.docker.internal:32839",
        "http://localhost:8080",
        "http://aq-server:8080",
        "http://192.168.1.5:8080",
        "http://0.0.0.0:8080",
        "ftp://aq.example.com",
        "https://user:pw@aq.example.com",
        "https://aq.example.com/prefix",
        "https://aq.example.com/?q=1",
    ] {
        assert!(node_origin(bad).is_err(), "{bad}");
        assert!(
            NodeLink::new(bad, NODE_CREDENTIAL, None, None).is_err(),
            "{bad}"
        );
    }
}

/// Finding 2: the node's Access service token (from its enrollment) reaches
/// issuance and every forwarded request; a caller's own Access headers are
/// replaced, and an answer that echoes the secret is refused.
#[tokio::test]
async fn access_headers_reach_issuance_and_every_forwarded_request() {
    let (origin, seen) = stub().await;
    assert!(
        NodeLink::new(&origin, NODE_CREDENTIAL, Some("svc.access"), None).is_err(),
        "half a service token"
    );
    let link = NodeLink::new(
        &origin,
        NODE_CREDENTIAL,
        Some("svc.access"),
        Some("svc-secret-0123456789"),
    )
    .unwrap();
    assert!(!format!("{link:?}").contains("svc-secret"));
    let root = tempfile::tempdir().unwrap();
    let directory = root.path().join(attempt().attempt_id.to_string());
    let channel = link
        .open(attempt(), directory, now() + 60_000)
        .await
        .unwrap();
    let socket = channel.socket_path();
    let (status, _) = call(
        &socket,
        "POST",
        "/api/v1/commands/whoami",
        "cf-access-client-id: spoofed\r\ncf-access-client-secret: spoofed\r\n",
    )
    .await;
    assert_eq!(status, 200);
    let (status, body) = call(&socket, "POST", "/api/v1/commands/echo-access", "").await;
    assert_eq!(status, 502);
    assert!(!body.contains("svc-secret"));
    let seen = seen.lock().unwrap().clone();
    assert_eq!(
        seen.iter().map(|s| s.path.as_str()).collect::<Vec<_>>(),
        [
            "/v2/worker/agent-session",
            "/api/v1/commands/whoami",
            "/api/v1/commands/echo-access"
        ]
    );
    assert_eq!(seen[0].auth, format!("Bearer {NODE_CREDENTIAL}"));
    assert_eq!(seen[1].auth, format!("Bearer {}", token(1)));
    for saw in &seen {
        assert_eq!(saw.access.as_deref(), Some("svc.access"), "{}", saw.path);
    }
    channel.stop().await.unwrap();
}

/// Finding 7: only allowlisted request headers reach aq-server (no cookies,
/// forwarding or proxy identity, `cf-*`, development identity or `Origin`),
/// and no `Set-Cookie` reaches the sandbox.
#[tokio::test]
async fn only_allowlisted_headers_pass_and_no_cookie_comes_back() {
    let (origin, seen) = stub().await;
    let root = tempfile::tempdir().unwrap();
    let channel = AgentChannel::open(
        config(root.path(), &origin, now() + 60_000),
        scripted(60_000, u64::MAX),
    )
    .await
    .unwrap();
    let socket = channel.socket_path();
    let spoofed = [
        "Cookie: CF_Authorization=stolen",
        "X-Forwarded-For: 10.0.0.1",
        "X-Forwarded-Host: evil.example",
        "Forwarded: for=10.0.0.1",
        "Cf-Connecting-Ip: 10.0.0.1",
        "Cf-Ray: 1",
        "Cf-Access-Jwt-Assertion: forged",
        "X-Real-Ip: 10.0.0.1",
        "True-Client-Ip: 10.0.0.1",
        "X-AQ-Dev-Email: owner@example.test",
        "Origin: http://evil.example",
        "Proxy-Authorization: Basic eA==",
        "Accept: application/json",
        "X-AQ-Request: 1",
    ]
    .map(|h| format!("{h}\r\n"))
    .concat();
    let (status, _) = call(&socket, "POST", "/api/v1/commands/whoami", &spoofed).await;
    assert_eq!(status, 200);
    assert_eq!(
        seen.lock().unwrap()[0].names,
        [
            "accept",
            "accept-encoding",
            "authorization",
            "content-length",
            "content-type",
            "host",
            "x-aq-request"
        ]
    );
    let text = raw(
        &socket,
        "GET /api/v1/commands/cookie HTTP/1.1\r\nHost: localhost\r\n\r\n",
    )
    .await;
    assert!(text.starts_with("HTTP/1.1 200"), "{text}");
    assert!(!text.to_ascii_lowercase().contains("set-cookie"), "{text}");
    assert!(!text.contains("CF_Authorization"), "{text}");
    channel.stop().await.unwrap();
}

/// Follow-up 2: a sweep holding a handle to a directory another sweep has
/// already removed must not remove the live directory a claim has since
/// created at the same path. A stale directory still at its path is removed.
#[tokio::test]
async fn a_sweep_never_removes_a_directory_it_did_not_lock() {
    let (origin, _) = stub().await;
    let root = tempfile::tempdir().unwrap();
    let path = root.path().join(Uuid::from_u128(7).to_string());
    std::fs::create_dir(&path).unwrap();
    let old = crate::channel::open_dir(&path).unwrap();
    std::fs::remove_dir(&path).unwrap(); // the other sweep
    let channel = AgentChannel::open(
        config(root.path(), &origin, now() + 60_000),
        scripted(60_000, u64::MAX),
    )
    .await
    .unwrap();
    assert!(!crate::channel::sweep_one(&path, &old).unwrap());
    let (status, _) = call(
        &channel.socket_path(),
        "POST",
        "/api/v1/commands/whoami",
        "",
    )
    .await;
    assert_eq!(status, 200);
    channel.stop().await.unwrap();

    let stale = root.path().join(Uuid::from_u128(8).to_string());
    std::fs::create_dir(&stale).unwrap();
    let dir = crate::channel::open_dir(&stale).unwrap();
    assert!(crate::channel::sweep_one(&stale, &dir).unwrap());
    assert!(!stale.exists());
}

/// aq-server's issuance as it is: one version per attempt, the expected
/// version fence, no idempotency. `lose` drops the next answer after commit.
struct Server {
    version: Mutex<u64>,
    lose: std::sync::atomic::AtomicBool,
    calls: AtomicU64,
}
impl CredentialIssuer for Server {
    fn issue(&self, expected: u64) -> IssueFuture<'_> {
        Box::pin(async move {
            self.calls.fetch_add(1, Ordering::SeqCst);
            let mut version = self.version.lock().unwrap();
            if expected != *version {
                return Err(IssueError::Refused("id_conflict".into()));
            }
            *version += 1;
            if self.lose.swap(false, Ordering::SeqCst) {
                return Err(IssueError::Unavailable);
            }
            Ok(Issued {
                token: Zeroizing::new(token(*version)),
                version: *version,
                expires_at_ms: now() + 1_500,
            })
        })
    }
}

/// Finding 4: the server commits a rotation and its answer is lost. The
/// retry (fenced on the old version) gets `id_conflict`; because an
/// unavailable issuance preceded it, the node fences on the committed version
/// and the channel stays open. An `id_conflict` with no lost answer before it
/// (another issuer moved the version) still closes the channel.
#[tokio::test]
async fn a_lost_rotation_answer_resyncs_and_a_foreign_rotation_closes() {
    let (origin, seen) = stub().await;
    let root = tempfile::tempdir().unwrap();
    let server = Arc::new(Server {
        version: Mutex::new(0),
        lose: Default::default(),
        calls: AtomicU64::new(0),
    });
    let channel = AgentChannel::open(config(root.path(), &origin, now() + 60_000), server.clone())
        .await
        .unwrap();
    server.lose.store(true, Ordering::SeqCst);
    // Rotation at 1 s commits 2 and loses the answer; the retry 200 ms later
    // gets id_conflict and the next issuance, fenced on 2, gets 3.
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    while channel.credential_version() < 3 {
        assert!(std::time::Instant::now() < deadline, "no re-sync");
        assert!(!channel.is_closed(), "a lost answer closed the channel");
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert_eq!(channel.credential_version(), 3);
    assert_eq!(server.calls.load(Ordering::SeqCst), 4);
    let (status, _) = call(
        &channel.socket_path(),
        "POST",
        "/api/v1/commands/whoami",
        "",
    )
    .await;
    assert_eq!(status, 200);
    assert_eq!(
        seen.lock().unwrap().last().unwrap().auth,
        format!("Bearer {}", token(3))
    );
    *server.version.lock().unwrap() += 1;
    tokio::time::timeout(Duration::from_secs(5), channel.closed())
        .await
        .unwrap();
}

/// Finding 9: a sweep running while channels open never removes a directory
/// an open is creating; every channel opens with a live socket.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_concurrent_sweep_never_breaks_an_opening_channel() {
    let (origin, _) = stub().await;
    let root = tempfile::tempdir().unwrap();
    let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let sweeper = {
        let (stop, root) = (stop.clone(), root.path().to_owned());
        std::thread::spawn(move || {
            let mut swept = 0;
            while !stop.load(Ordering::SeqCst) {
                swept += sweep(&root).unwrap_or(0);
            }
            swept
        })
    };
    for _ in 0..100 {
        let channel = AgentChannel::open(
            config(root.path(), &origin, now() + 60_000),
            scripted(60_000, u64::MAX),
        )
        .await
        .unwrap();
        let (status, _) = call(
            &channel.socket_path(),
            "POST",
            "/api/v1/commands/whoami",
            "",
        )
        .await;
        assert_eq!(status, 200);
        channel.stop().await.unwrap();
    }
    stop.store(true, Ordering::SeqCst);
    sweeper.join().unwrap();
}

#[cfg(feature = "environment-try")]
mod checkpoint;
