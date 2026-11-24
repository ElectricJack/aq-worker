use super::*;

/// One request the stub saw.
#[derive(Clone, Debug)]
pub(super) struct Saw {
    pub(super) path: String,
    pub(super) auth: String,
    /// `cf-access-client-id`, if any.
    pub(super) access: Option<String>,
    /// Every header name, sorted.
    pub(super) names: Vec<String>,
}
pub(super) type Seen = Arc<Mutex<Vec<Saw>>>;

/// The highest credential version a [`Scripted`] issuer has issued, which
/// the stub treats as the one live verifier (aq-server keeps one per attempt).
static NO_LIVE: AtomicU64 = AtomicU64::new(0);

/// Stub aq-server: `whoami` answers the principal for the presented token;
/// `echo` returns the authorization header; `revoked` answers 401;
/// `unauthorized` answers the server's unknown-verifier 401; `live/<n>`
/// answers 200 only for the version `live` holds, after waiting for it to
/// reach n (a request that raced a rotation); `cookie` sets a cookie.
pub(super) async fn stub() -> (String, Seen) {
    stub_with(&NO_LIVE).await
}
fn record_request(uri: &Uri, headers: &HeaderMap) -> Saw {
    let auth = headers
        .get("authorization")
        .and_then(|v| v.to_str().ok())
        .unwrap_or_default()
        .to_owned();
    let access = headers
        .get("cf-access-client-id")
        .map(|v| v.to_str().unwrap().to_owned());
    let path = match (
        uri.query(),
        headers.get("x-aq-request"),
        headers.get("origin"),
    ) {
        (Some(q), Some(x), None) if x == "1" => format!("{}?{q} x-aq-request", uri.path()),
        _ => uri.path().to_owned(),
    };
    let mut names: Vec<String> = headers.keys().map(|k| k.as_str().to_owned()).collect();
    names.sort();
    Saw {
        path,
        auth,
        access,
        names,
    }
}

async fn stub_response(
    uri: &Uri,
    headers: &HeaderMap,
    auth: String,
    live: &AtomicU64,
) -> axum::response::Response {
    fn unauthorized() -> axum::response::Response {
        (
            StatusCode::UNAUTHORIZED,
            Json(serde_json::json!({"code": "unauthorized", "effect": "not_applied"})),
        )
            .into_response()
    }
    if let Some(n) = uri.path().strip_prefix("/api/v1/commands/live/") {
        let n: u64 = n.parse().unwrap();
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        while live.load(Ordering::SeqCst) < n && std::time::Instant::now() < deadline {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        // Let the channel store what the issuer just returned.
        tokio::time::sleep(Duration::from_millis(200)).await;
        let current = format!("Bearer {}", token(live.load(Ordering::SeqCst)));
        return if auth == current {
            StatusCode::OK.into_response()
        } else {
            unauthorized()
        };
    }
    match uri.path() {
        "/api/v1/commands/unauthorized" => unauthorized(),
        "/api/v1/commands/cookie" => (
            [("set-cookie", "CF_Authorization=edge-session; Path=/")],
            "ok",
        )
            .into_response(),
        "/api/v1/commands/whoami" => Json(serde_json::json!({
            "principal": "agent",
            "version": auth.trim_start_matches("Bearer aqa_").trim_start_matches('0'),
        }))
        .into_response(),
        "/api/v1/commands/echo" => auth.into_response(),
        "/api/v1/commands/echo-access" => headers
            .get("cf-access-client-secret")
            .map(|v| v.to_str().unwrap().to_owned())
            .unwrap_or_default()
            .into_response(),
        "/v2/worker/agent-session" => Json(serde_json::json!({"api_version":1,"token":token(1),
                    "context":{"credential":{"version":"1","expires_at":"2099-01-01T00:00:00Z"}}}))
        .into_response(),
        "/api/v1/commands/revoked" => (
            StatusCode::UNAUTHORIZED,
            Json(serde_json::json!({"code": "session_revoked", "effect": "not_applied"})),
        )
            .into_response(),
        _ => StatusCode::NOT_FOUND.into_response(),
    }
}

pub(super) async fn stub_with(live: &'static AtomicU64) -> (String, Seen) {
    let seen: Seen = Arc::default();
    let log = seen.clone();
    let app = Router::new().fallback(move |uri: Uri, headers: HeaderMap, _body: Bytes| {
        let log = log.clone();
        async move {
            let request = record_request(&uri, &headers);
            let auth = request.auth.clone();
            log.lock().unwrap().push(request);
            stub_response(&uri, &headers, auth, live).await
        }
    });
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let origin = format!("http://{}", listener.local_addr().unwrap());
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    (origin, seen)
}
