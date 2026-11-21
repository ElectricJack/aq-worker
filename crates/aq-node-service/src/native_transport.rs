//! Bounded retries for native HTTP requests; callers supply their boot clock.
use anyhow::{Result, ensure};
use reqwest::{Response, StatusCode, header};
use std::{future::Future, time::Duration};
/// A refused native request: its status and the `Retry-After` seconds it carried.
#[derive(Debug)]
pub struct HttpStatus(pub StatusCode, pub Option<Duration>);
impl std::fmt::Display for HttpStatus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "native endpoint returned {}", self.0)
    }
}
impl std::error::Error for HttpStatus {}
/// A send that failed in transport (connect, timeout, reset): its disposition is unknown.
#[derive(Debug)]
pub struct TransportFailed(pub &'static str);
impl std::fmt::Display for TransportFailed {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.0)
    }
}
impl std::error::Error for TransportFailed {}
/// Logs a refused response (its body cut to 400 characters) and returns its `HttpStatus`.
pub async fn refused(response: Response) -> anyhow::Error {
    let status = response.status();
    let path = response.url().path().to_owned();
    let retry_after = (response.headers().get(header::RETRY_AFTER))
        .and_then(|v| v.to_str().ok()?.trim().parse().ok())
        .map(Duration::from_secs);
    let body = response.text().await.unwrap_or_default();
    let body: String = body.chars().take(400).collect();
    tracing::warn!(%path, %status, %body, "native endpoint refused");
    let gone = status == StatusCode::CONFLICT && body.contains("execution cancelled"); // released: 410, never retried
    let error = anyhow::Error::from(HttpStatus(
        [status, StatusCode::GONE][gone as usize],
        retry_after,
    ));
    // The owner's login age limit is a refusal the node shows, not only logs.
    match serde_json::from_str::<serde_json::Value>(&body) {
        Ok(v)
            if v["message"]
                .as_str()
                .is_some_and(|m| m.starts_with("codex_login_stale")) =>
        {
            error.context(v["message"].as_str().unwrap_or_default().to_owned())
        }
        _ => error,
    }
}
/// How often one request is resent after a `503`, and the first and longest wait between sends.
pub const SAME_TRIES: u32 = 8;
pub const SAME_BACKOFF: (Duration, Duration) =
    (Duration::from_millis(60), Duration::from_millis(480));
/// A `503` (`row lock contended; retry the same operation`, `unavailable`) says nothing was
/// applied and asks for the same operation again. True after a short jittered wait when the
/// caller should resend the identical request; false when the response is final, `tries` are
/// used up, or the wait would pass `deadline_ns`. A transport failure is never resent here.
pub async fn retry_same(response: &Response, tries: &mut u32, deadline_ns: u64, now: u64) -> bool {
    if response.status() != StatusCode::SERVICE_UNAVAILABLE || *tries + 1 >= SAME_TRIES {
        return false;
    }
    let (base, cap) = SAME_BACKOFF;
    let wait = crate::backoff::equal_jittered(*tries, base, cap, &mut rand::thread_rng());
    if now.saturating_add(wait.as_nanos() as u64) >= deadline_ns {
        return false;
    }
    *tries += 1;
    let path = response.url().path();
    tracing::warn!(%path, attempt = *tries, "native endpoint answered 503; same operation resent in {wait:?}");
    tokio::time::sleep(wait).await;
    true
}
/// Start and stop reports carry durable operation IDs. Resend their exact request on
/// explicit contention while the caller still owns its deadline and shutdown handling.
pub async fn phase_terminal_response(
    request: reqwest::RequestBuilder,
    endpoint: &str,
) -> Result<Response> {
    let replayable = matches!(endpoint, "started" | "stopped");
    let mut tries = 0;
    let response = loop {
        let next = request
            .try_clone()
            .ok_or_else(|| anyhow::anyhow!("terminal request body is not replayable"))?;
        let response = next
            .send()
            .await
            .map_err(|_| anyhow::anyhow!("phase terminal transport failed; disposition unknown"))?;
        if !replayable || !retry_same(&response, &mut tries, u64::MAX, boot_ns()?).await {
            break response;
        }
    };
    if !response.status().is_success() {
        return Err(refused(response).await);
    }
    let mut versions = response
        .headers()
        .get_all("aq-phase-terminal-api-version")
        .iter();
    ensure!(
        versions.next().is_some_and(|v| v == "1")
            && versions.next().is_none()
            && response
                .headers()
                .get(header::CONTENT_TYPE)
                .is_some_and(|v| v == "application/json"),
        "phase terminal response version/type"
    );
    Ok(response)
}
/// A read-only preparation fetch gives up only after `FETCH_TRIES` tries and `FETCH_SPAN`
/// (deadline permitting); its backoff (equal jitter) starts at 500 ms and is capped at 8 s.
pub const FETCH_TRIES: u32 = 5;
pub const FETCH_SPAN: Duration = Duration::from_secs(10);
pub const FETCH_BACKOFF: (Duration, Duration) =
    (Duration::from_millis(500), Duration::from_secs(8));
/// The wait before retry `attempt` (0-based) of a read-only fetch that failed with `e`, or None
/// when `e` is final: only 502/503/504, 408, 429 and transport failures are transient. A
/// `Retry-After` longer than the backoff is waited instead; one beyond the cap ends the retries.
pub fn fetch_retry_wait(e: &anyhow::Error, attempt: u32) -> Option<Duration> {
    let (base, cap) = FETCH_BACKOFF;
    let after = match e.downcast_ref::<HttpStatus>() {
        Some(HttpStatus(s, after)) if matches!(s.as_u16(), 408 | 429 | 502 | 503 | 504) => *after,
        Some(_) => return None,
        None => {
            e.downcast_ref::<TransportFailed>()?;
            None
        }
    };
    let backoff = crate::backoff::equal_jittered(attempt, base, cap, &mut rand::thread_rng());
    after.map_or(Some(backoff), |d| (d <= cap).then_some(d.max(backoff)))
}
/// Retries an idempotent read while it fails transiently, until it has had `FETCH_TRIES` tries
/// and `FETCH_SPAN` has passed, and only while the next wait ends before `deadline_ns` (R1: one
/// `503 overloaded` failed a whole attempt; R5: five fast tries did not outlast a restart).
pub async fn fetch_retried<T, F>(
    deadline_ns: u64,
    mut call: impl FnMut() -> F,
    now: impl Fn() -> Result<u64>,
) -> Result<T>
where
    F: Future<Output = Result<T>>,
{
    let (mut attempt, started) = (0, tokio::time::Instant::now());
    loop {
        let e = match call().await {
            Ok(value) => return Ok(value),
            Err(e) => e,
        };
        let ends = |w: &Duration| now().is_ok_and(|n| n + (w.as_nanos() as u64) < deadline_ns);
        let more = attempt + 1 < FETCH_TRIES || started.elapsed() < FETCH_SPAN;
        let wait = fetch_retry_wait(&e, attempt).filter(|w| more && ends(w));
        let Some(wait) = wait else {
            (attempt > 0).then(|| tracing::warn!(tries = attempt + 1, "{e:#}; not retried again"));
            return Err(e);
        };
        attempt += 1;
        tracing::warn!(attempt, "{e:#}; retrying in {wait:?}");
        tokio::time::sleep(wait).await;
    }
}

/// Linux monotonic clock including suspended time.
pub fn boot_ns() -> Result<u64> {
    let mut time = libc::timespec {
        tv_sec: 0,
        tv_nsec: 0,
    };
    ensure!(
        unsafe { libc::clock_gettime(libc::CLOCK_BOOTTIME, &mut time) } == 0,
        "boot clock unavailable"
    );
    let seconds = u64::try_from(time.tv_sec)?;
    let nanos = u64::try_from(time.tv_nsec)?;
    seconds
        .checked_mul(1_000_000_000)
        .and_then(|n| n.checked_add(nanos))
        .ok_or_else(|| anyhow::anyhow!("boot clock overflow"))
}

pub async fn body(mut response: Response, limit: usize) -> Result<zeroize::Zeroizing<Vec<u8>>> {
    ensure!(
        response.content_length().is_none_or(|n| n <= limit as u64),
        "native response size"
    );
    let mut bytes = zeroize::Zeroizing::new(Vec::new());
    while let Some(chunk) = response
        .chunk()
        .await
        .map_err(|_| anyhow::anyhow!("native response read failed"))?
    {
        ensure!(
            chunk.len() <= limit.saturating_sub(bytes.len()),
            "native response size"
        );
        bytes.extend_from_slice(&chunk);
    }
    Ok(bytes)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{
        Arc,
        atomic::{AtomicU32, Ordering},
    };
    /// Sends as the control callers do: the identical request again while `retry_same` says so.
    async fn send(url: &str) -> (StatusCode, u32) {
        let (mut tries, mut sends) = (0, 0);
        loop {
            let response = reqwest::Client::new()
                .post(url)
                .body("same")
                .send()
                .await
                .unwrap();
            sends += 1;
            if !retry_same(&response, &mut tries, u64::MAX, 0).await {
                return (response.status(), sends);
            }
        }
    }
    #[tokio::test]
    async fn a_503_is_resent_in_place_and_other_refusals_are_final() {
        use axum::{Router, extract::State, routing::post};
        let seen = Arc::new(AtomicU32::new(0));
        let contended = |State(seen): State<Arc<AtomicU32>>, body: String| async move {
            assert_eq!(body, "same");
            match seen.fetch_add(1, Ordering::SeqCst) {
                0 => (
                    StatusCode::SERVICE_UNAVAILABLE,
                    r#"{"error":"overloaded","message":"row lock contended; retry the same operation"}"#,
                ),
                1 => (
                    StatusCode::SERVICE_UNAVAILABLE,
                    r#"{"kind":"failure","body":{"schema_version":1,"code":"unavailable","operation_id":null}}"#,
                ),
                _ => (StatusCode::OK, "{}"),
            }
        };
        let app = Router::new()
            .route("/proposal", post(contended))
            .route("/conflict", post(|| async { StatusCode::CONFLICT }))
            .route("/down", post(|| async { StatusCode::SERVICE_UNAVAILABLE }))
            .with_state(seen.clone());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let started = std::time::Instant::now();
        assert_eq!(send(&format!("{base}/proposal")).await, (StatusCode::OK, 3));
        assert!(started.elapsed() < Duration::from_secs(1), "short waits");
        assert_eq!(
            send(&format!("{base}/conflict")).await,
            (StatusCode::CONFLICT, 1)
        );
        // Bounded: a server that stays unavailable ends the operation after SAME_TRIES sends.
        assert_eq!(
            send(&format!("{base}/down")).await,
            (StatusCode::SERVICE_UNAVAILABLE, SAME_TRIES)
        );
        // And never past the caller's send deadline.
        let response = reqwest::Client::new()
            .post(format!("{base}/down"))
            .send()
            .await
            .unwrap();
        assert!(!retry_same(&response, &mut 0, 1_000, 999).await);
    }
    #[tokio::test]
    async fn contended_start_and_stop_resend_identical_bytes_and_preserve_the_receipt() {
        for endpoint in ["started", "stopped"] {
            use axum::{Json, Router, extract::State, response::IntoResponse, routing::post};
            let calls = Arc::new(AtomicU32::new(0));
            let stop = |State(calls): State<Arc<AtomicU32>>,
                        headers: header::HeaderMap,
                        body: String| async move {
                assert_eq!(headers["aq-phase-terminal-api-version"], "1");
                assert_eq!(
                    body,
                    r#"{"operation_id":"synthetic-terminal","outcome":"success"}"#
                );
                if calls.fetch_add(1, Ordering::SeqCst) == 0 {
                    return StatusCode::SERVICE_UNAVAILABLE.into_response();
                }
                ([("aq-phase-terminal-api-version", "1")], Json("receipt")).into_response()
            };
            let app = Router::new()
                .route(&format!("/{endpoint}"), post(stop))
                .with_state(calls.clone());
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let url = format!("http://{}/{endpoint}", listener.local_addr().unwrap());
            let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
            let request = reqwest::Client::new()
                .post(url)
                .header("aq-phase-terminal-api-version", "1")
                .body(r#"{"operation_id":"synthetic-terminal","outcome":"success"}"#);
            let receipt = phase_terminal_response(request, endpoint).await.unwrap();
            assert_eq!(receipt.text().await.unwrap(), r#""receipt""#);
            assert_eq!(calls.load(Ordering::SeqCst), 2);
            server.abort();
        }
    }
    /// The owner's login age limit is refused by the server with a reason the node shows,
    /// on the attempt's material fetch (`refused`) and on the session lease alike.
    #[tokio::test]
    async fn a_stale_login_refusal_keeps_the_servers_reason() {
        use axum::{Json, Router, http::StatusCode as Status, routing::post};
        const REASON: &str = "codex_login_stale: stored Codex login is 10 days old (limit 7): import your current login";
        let stale = || async {
            let body = serde_json::json!({"error": "conflict", "message": REASON});
            (Status::CONFLICT, Json(body))
        };
        let other = || async {
            let body = serde_json::json!({"error": "conflict", "message": "private material delivery is not currently authorized"});
            (Status::CONFLICT, Json(body))
        };
        let id = uuid::Uuid::from_u128(7);
        let app = Router::new()
            .route("/stale", post(stale))
            .route("/other", post(other))
            .route(&format!("/v1/worker/manual-agents/{id}/login"), post(stale));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let origin = format!("http://{}", listener.local_addr().unwrap());
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let post = |path: &'static str| {
            reqwest::Client::new()
                .post(format!("{origin}{path}"))
                .send()
        };
        let error = refused(post("/stale").await.unwrap()).await;
        assert_eq!(
            format!("{error:#}"),
            format!("{REASON}: native endpoint returned 409 Conflict")
        );
        assert_eq!(
            error.downcast_ref::<HttpStatus>().unwrap().0,
            StatusCode::CONFLICT
        );
        let error = refused(post("/other").await.unwrap()).await;
        assert_eq!(error.to_string(), "native endpoint returned 409 Conflict");
        let link =
            crate::link::ServerLink::new(&origin, "node-credential-0123456789", None).unwrap();
        assert_eq!(
            link.session_login(id).await.unwrap_err().to_string(),
            REASON
        );
    }
}
