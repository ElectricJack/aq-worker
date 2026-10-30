use aq_protocol::{CompatibilityError, HeartbeatRequest, PROTOCOL_VERSION};
use aq_worker::client::{CallError, Client};
use axum::Router;
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::IntoResponse;
use axum::routing::post;

async fn serve(app: Router) -> String {
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = l.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(l, app).await.unwrap() });
    format!("http://{addr}")
}

fn hb() -> HeartbeatRequest {
    HeartbeatRequest {
        worker_version: "0.1.0".into(),
        protocol_version: PROTOCOL_VERSION,
    }
}

fn client(url: &str) -> Client {
    Client::new(url, Some("aqn_x".into()), None, None).unwrap()
}

async fn hb_err(app: Router) -> CallError {
    let url = serve(app).await;
    client(&url).heartbeat(&hb()).await.unwrap_err()
}

const ACCESS_MSG: &str = "Cloudflare Access rejected the request (check service token)";

#[tokio::test]
async fn redirect_classified_as_access_rejected() {
    let app = Router::new().route(
        "/v1/worker/heartbeat",
        post(|| async {
            (
                StatusCode::FOUND,
                [(
                    header::LOCATION,
                    "https://team.cloudflareaccess.com/cdn-cgi/access/login",
                )],
            )
        }),
    );
    let e = hb_err(app).await;
    assert!(matches!(e, CallError::AccessRejected), "{e:?}");
    assert_eq!(e.to_string(), ACCESS_MSG);
}

#[tokio::test]
async fn html_login_page_classified_as_access_rejected() {
    for status in [
        StatusCode::OK,
        StatusCode::FORBIDDEN,
        StatusCode::UNAUTHORIZED,
    ] {
        let app = Router::new().route(
            "/v1/worker/heartbeat",
            post(move || async move {
                (
                    status,
                    [(header::CONTENT_TYPE, "text/html")],
                    "<html>login</html>",
                )
            }),
        );
        assert!(matches!(hb_err(app).await, CallError::AccessRejected));
    }
}

#[tokio::test]
async fn non_json_401_is_access_rejected() {
    let app = Router::new().route(
        "/v1/worker/heartbeat",
        post(|| async { (StatusCode::UNAUTHORIZED, "nope") }),
    );
    assert!(matches!(hb_err(app).await, CallError::AccessRejected));
}

#[tokio::test]
async fn unexpected_401_code_is_other() {
    let app = Router::new().route(
        "/v1/worker/heartbeat",
        post(|| async {
            (
                StatusCode::UNAUTHORIZED,
                [(header::CONTENT_TYPE, "application/json")],
                r#"{"error":"weird","message":"x"}"#,
            )
        }),
    );
    assert!(matches!(hb_err(app).await, CallError::Other(_)));
}

#[tokio::test]
async fn server_down_is_transient() {
    let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = l.local_addr().unwrap().port();
    drop(l);
    let e = client(&format!("http://127.0.0.1:{port}"))
        .heartbeat(&hb())
        .await
        .unwrap_err();
    assert!(matches!(e, CallError::Transient(_)), "{e:?}");
}

#[tokio::test]
async fn server_error_is_transient() {
    let app = Router::new().route(
        "/v1/worker/heartbeat",
        post(|| async { StatusCode::BAD_GATEWAY }),
    );
    assert!(matches!(hb_err(app).await, CallError::Transient(_)));
}

fn json_resp(status: StatusCode, body: &'static str) -> impl IntoResponse {
    (status, [(header::CONTENT_TYPE, "application/json")], body)
}

#[tokio::test]
async fn revoked_is_fatal() {
    let app = Router::new().route(
        "/v1/worker/heartbeat",
        post(|| async {
            json_resp(
                StatusCode::UNAUTHORIZED,
                r#"{"error":"credential_revoked","message":"revoked"}"#,
            )
        }),
    );
    assert!(matches!(hb_err(app).await, CallError::Revoked));
}

#[tokio::test]
async fn unknown_credential_is_invalid() {
    let app = Router::new().route(
        "/v1/worker/heartbeat",
        post(|| async {
            json_resp(
                StatusCode::UNAUTHORIZED,
                r#"{"error":"invalid_credential","message":"?"}"#,
            )
        }),
    );
    assert!(matches!(hb_err(app).await, CallError::InvalidCredential));
}

#[tokio::test]
async fn upgrade_required_is_incompatible() {
    let app = Router::new().route(
        "/v1/worker/heartbeat",
        post(|| async {
            json_resp(
                StatusCode::UPGRADE_REQUIRED,
                r#"{"error":"unsupported_protocol","message":"upgrade the worker","supported_min":2,"supported_max":3}"#,
            )
        }),
    );
    match hb_err(app).await {
        CallError::Incompatible(CompatibilityError {
            message,
            supported_min,
            ..
        }) => {
            assert_eq!(message, "upgrade the worker");
            assert_eq!(supported_min, 2);
        }
        e => panic!("{e:?}"),
    }
}

#[tokio::test]
async fn sends_bearer_and_cf_headers() {
    let app = Router::new().route(
        "/v1/worker/heartbeat",
        post(|h: HeaderMap| async move {
            assert_eq!(h["authorization"], "Bearer aqn_x");
            assert_eq!(h["cf-access-client-id"], "cid");
            assert_eq!(h["cf-access-client-secret"], "csec");
            json_resp(
                StatusCode::OK,
                r#"{"server_version":"s","heartbeat_interval_secs":15}"#,
            )
        }),
    );
    let url = serve(app).await;
    let c = Client::new(
        &url,
        Some("aqn_x".into()),
        Some("cid".into()),
        Some("csec".into()),
    )
    .unwrap();
    assert_eq!(
        c.heartbeat(&hb()).await.unwrap().heartbeat_interval_secs,
        15
    );
}

#[test]
fn malformed_server_urls_are_rejected() {
    for bad in [
        "htps://example.com",
        "example.com",
        "ftp://example.com",
        "http://",
    ] {
        assert!(Client::new(bad, None, None, None).is_err(), "{bad}");
    }
    assert!(Client::new("https://example.com/", None, None, None).is_ok());
}

#[tokio::test]
async fn html_429_and_408_and_502_are_transient() {
    for status in [
        StatusCode::TOO_MANY_REQUESTS,
        StatusCode::REQUEST_TIMEOUT,
        StatusCode::BAD_GATEWAY,
    ] {
        let app = Router::new().route(
            "/v1/worker/heartbeat",
            post(move || async move {
                (
                    status,
                    [(header::CONTENT_TYPE, "text/html")],
                    "<html>slow down</html>",
                )
            }),
        );
        assert!(
            matches!(hb_err(app).await, CallError::Transient(_)),
            "{status}"
        );
    }
    let app = Router::new().route(
        "/v1/worker/heartbeat",
        post(|| async { StatusCode::TOO_MANY_REQUESTS }),
    );
    assert!(matches!(hb_err(app).await, CallError::Transient(_)));
}

#[tokio::test]
async fn connection_refused_error_includes_the_cause() {
    // Bind then drop to get a port nothing listens on.
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = l.local_addr().unwrap().port();
    drop(l);
    let e = client(&format!("http://127.0.0.1:{port}"))
        .heartbeat(&hb())
        .await
        .unwrap_err();
    let CallError::Transient(msg) = e else {
        panic!("expected transient, got {e:?}")
    };
    let lower = msg.to_lowercase();
    assert!(
        lower.contains("refused") || lower.contains("connect"),
        "{msg}"
    );
    assert!(lower.contains("refused"), "{msg}");
    assert!(!msg.contains("127.0.0.1"), "url must not appear: {msg}");
}

#[tokio::test]
async fn not_found_hints_at_the_worker_port() {
    for body in ["", r#"{"error":"not_found","message":"no such route"}"#] {
        let app = Router::new().route(
            "/v1/worker/heartbeat",
            post(move || async move { (StatusCode::NOT_FOUND, body) }),
        );
        let CallError::Other(m) = hb_err(app).await else {
            panic!("expected Other")
        };
        assert!(m.contains("worker port (8081)"), "{m}");
    }
}
