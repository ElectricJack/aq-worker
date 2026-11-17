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
    HttpStatus([status, StatusCode::GONE][gone as usize], retry_after).into()
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
