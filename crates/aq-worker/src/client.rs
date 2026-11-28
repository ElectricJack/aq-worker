//! HTTP client for the worker listener, with failure classification.

use std::fmt;
use std::time::Duration;

use aq_protocol::{
    ApiError, CREDENTIAL_REVOKED, CompatibilityError, EnrollRequest, EnrollResponse,
    HeartbeatRequest, HeartbeatResponse, INVALID_CREDENTIAL, PROTOCOL_VERSION, ProbeRequest,
    ProbeResponse,
};
use rand::Rng;
use reqwest::header::{CONTENT_TYPE, HeaderMap, HeaderValue};
use reqwest::{Response, StatusCode, redirect};
use serde::Serialize;
use serde::de::DeserializeOwned;

use crate::backoff;
use crate::config::WorkerConfig;

pub const ACCESS_REJECTED_MSG: &str =
    "Cloudflare Access rejected the request (check service token)";
const DEFAULT_INTERVAL_SECS: u64 = 15;

#[derive(Debug)]
pub enum CallError {
    /// Retry with backoff.
    Transient(String),
    Revoked,
    InvalidCredential,
    Incompatible(CompatibilityError),
    AccessRejected,
    Other(String),
}

impl fmt::Display for CallError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            CallError::Transient(m) => write!(f, "temporary failure: {m}"),
            CallError::Revoked | CallError::InvalidCredential => {
                write!(f, "credential rejected; re-enroll with aq-worker enroll")
            }
            CallError::Incompatible(c) => write!(f, "{}", c.message),
            CallError::AccessRejected => write!(f, "{ACCESS_REJECTED_MSG}"),
            CallError::Other(m) => write!(f, "{m}"),
        }
    }
}

impl std::error::Error for CallError {}

#[derive(Clone)]
pub struct Client {
    http: reqwest::Client,
    base: String,
    credential: Option<String>,
}

pub(crate) fn secret_header(value: &str) -> anyhow::Result<HeaderValue> {
    let mut v = HeaderValue::from_str(value)?;
    v.set_sensitive(true);
    Ok(v)
}

impl Client {
    pub fn new(
        server_url: &str,
        credential: Option<String>,
        cf_client_id: Option<String>,
        cf_client_secret: Option<String>,
    ) -> anyhow::Result<Client> {
        let url = reqwest::Url::parse(server_url)
            .map_err(|e| anyhow::anyhow!("invalid server URL {server_url:?}: {e}"))?;
        if !matches!(url.scheme(), "http" | "https") || url.host_str().is_none() {
            anyhow::bail!("invalid server URL {server_url:?}: must be http:// or https://");
        }
        let mut headers = HeaderMap::new();
        if let (Some(id), Some(secret)) = (cf_client_id, cf_client_secret) {
            headers.insert("cf-access-client-id", secret_header(&id)?);
            headers.insert("cf-access-client-secret", secret_header(&secret)?);
        }
        let http = aq_node_service::transport_health::tune(reqwest::Client::builder())
            .redirect(redirect::Policy::none())
            .timeout(Duration::from_secs(10))
            .default_headers(headers)
            .build()?;
        Ok(Client {
            http,
            base: server_url.trim_end_matches('/').to_string(),
            credential,
        })
    }

    pub fn from_config(cfg: &WorkerConfig) -> anyhow::Result<Client> {
        Client::new(
            &cfg.server_url,
            Some(cfg.credential.clone()),
            cfg.cf_client_id.clone(),
            cfg.cf_client_secret.clone(),
        )
    }

    pub async fn enroll(&self, req: &EnrollRequest) -> Result<EnrollResponse, CallError> {
        self.post("/v1/worker/enroll", req, false).await
    }

    pub async fn heartbeat(&self, req: &HeartbeatRequest) -> Result<HeartbeatResponse, CallError> {
        self.post("/v1/worker/heartbeat", req, true).await
    }

    pub async fn probe(&self, req: &ProbeRequest) -> Result<ProbeResponse, CallError> {
        self.post("/v1/worker/probe", req, true).await
    }

    /// H2 controller facts use the enrolled node credential, never a runner permit.
    pub async fn report_vm_time(
        &self,
        fact: &crate::backend::boat::VmTimeFact,
    ) -> Result<(), CallError> {
        #[derive(serde::Deserialize)]
        struct Receipt {
            recorded: bool,
        }
        let receipt: Receipt = self
            .post("/v1/worker/observations/vm-time", fact, true)
            .await?;
        if !receipt.recorded {
            return Err(CallError::Other("VM-time report was not recorded".into()));
        }
        Ok(())
    }
    /// Authenticated JSON POST for routes that have no dedicated method.
    pub async fn post<B: Serialize, T: DeserializeOwned>(
        &self,
        path: &str,
        body: &B,
        auth: bool,
    ) -> Result<T, CallError> {
        let mut rb = self.http.post(format!("{}{path}", self.base)).json(body);
        if auth && let Some(cred) = &self.credential {
            let v = secret_header(&format!("Bearer {cred}"))
                .map_err(|_| CallError::Other("credential is not a valid header value".into()))?;
            rb = rb.header(reqwest::header::AUTHORIZATION, v);
        }
        let resp = aq_node_service::transport_health::observe(rb.send().await)
            // Connect failures, timeouts and mid-flight resets are all retryable.
            .map_err(|e| {
                if e.is_builder() {
                    CallError::Other(error_chain(e))
                } else {
                    CallError::Transient(error_chain(e))
                }
            })?;
        classify(resp).await
    }
}

/// Renders a reqwest error with its whole `source()` chain, so the OS-level
/// cause (connection refused, DNS, TLS) is visible. The URL is stripped, since
/// it could carry userinfo.
pub fn error_chain(e: reqwest::Error) -> String {
    let e = e.without_url();
    let mut msg = e.to_string();
    let mut src = std::error::Error::source(&e);
    while let Some(s) = src {
        msg.push_str(": ");
        msg.push_str(&s.to_string());
        src = s.source();
    }
    msg
}

fn is_html(resp: &Response) -> bool {
    resp.headers()
        .get(CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|v| v.trim_start().to_ascii_lowercase().starts_with("text/html"))
}

async fn classify<T: DeserializeOwned>(resp: Response) -> Result<T, CallError> {
    let status = resp.status();
    // Timeouts and rate limits (including Cloudflare's 429 page) are retryable.
    if status.is_server_error()
        || matches!(
            status,
            StatusCode::REQUEST_TIMEOUT | StatusCode::TOO_MANY_REQUESTS
        )
    {
        return Err(CallError::Transient(format!("server returned {status}")));
    }
    // Cloudflare Access answers a rejected service token with a redirect to
    // its login page or an HTML page; never follow or parse those.
    if status.is_redirection() || is_html(&resp) {
        return Err(CallError::AccessRejected);
    }
    let bytes = resp
        .bytes()
        .await
        .map_err(|e| CallError::Transient(error_chain(e)))?;
    if status.is_success() {
        return serde_json::from_slice(&bytes)
            .map_err(|_| CallError::Other("server sent an unreadable response".into()));
    }
    Err(classify_error(status, &bytes))
}

fn classify_error(status: StatusCode, bytes: &[u8]) -> CallError {
    let api: Option<ApiError> = serde_json::from_slice(bytes).ok();
    match (status, api) {
        (StatusCode::UPGRADE_REQUIRED, _) => serde_json::from_slice(bytes)
            .map(CallError::Incompatible)
            .unwrap_or_else(|_| {
                CallError::Other("server requires a different protocol version".into())
            }),
        (StatusCode::UNAUTHORIZED | StatusCode::FORBIDDEN, None) => CallError::AccessRejected,
        (StatusCode::UNAUTHORIZED, Some(e)) if e.error == CREDENTIAL_REVOKED => CallError::Revoked,
        (StatusCode::UNAUTHORIZED, Some(e)) if e.error == INVALID_CREDENTIAL => {
            CallError::InvalidCredential
        }
        (StatusCode::NOT_FOUND, None) => CallError::Other(
            "server returned 404 Not Found; is --server pointing at the worker port (8081)?".into(),
        ),
        (StatusCode::NOT_FOUND, Some(e)) => CallError::Other(format!(
            "{} ({}); is --server pointing at the worker port (8081)?",
            e.message, e.error
        )),
        (_, Some(e)) => CallError::Other(format!("{} ({})", e.message, e.error)),
        (s, None) => CallError::Other(format!("unexpected response {s}")),
    }
}

fn nonce() -> String {
    format!("{:032x}", rand::thread_rng().r#gen::<u128>())
}

async fn sleep_or_shutdown<F: Future + Unpin>(d: Duration, shutdown: &mut F) -> bool {
    tokio::select! {
        _ = tokio::time::sleep(d) => false,
        _ = shutdown => true,
    }
}

async fn next_interval(client: &Client, probed: bool) -> Result<u64, CallError> {
    if probed {
        let req = HeartbeatRequest {
            worker_version: env!("AQ_WORKER_VERSION").to_string(),
            protocol_version: PROTOCOL_VERSION,
        };
        return client
            .heartbeat(&req)
            .await
            .map(|r| r.heartbeat_interval_secs);
    }
    let request = ProbeRequest { nonce: nonce() };
    let response = client.probe(&request).await?;
    if response.nonce != request.nonce {
        return Err(CallError::Other("probe nonce mismatch".into()));
    }
    Ok(0)
}

/// One probe at startup, then a heartbeat every server-provided interval.
/// Transient failures back off with jitter; any other error is returned.
/// `Ok(())` means `shutdown` resolved.
pub async fn run_loop(client: &Client, shutdown: impl Future) -> Result<(), CallError> {
    tokio::pin!(shutdown);
    let mut attempt: u32 = 0;
    let mut probed = false;
    loop {
        let result = tokio::select! {
            _ = &mut shutdown => return Ok(()),
            interval = next_interval(client, probed) => interval,
        };
        let delay = match result {
            Ok(secs) => {
                attempt = 0;
                if !probed {
                    probed = true;
                    tracing::info!("probe succeeded");
                    continue;
                }
                tracing::debug!("heartbeat ok");
                Duration::from_secs(match secs {
                    0 => DEFAULT_INTERVAL_SECS,
                    seconds => seconds,
                })
            }
            Err(CallError::Transient(msg)) => {
                let d = backoff::delay(attempt, &mut rand::thread_rng());
                tracing::warn!(
                    attempt = attempt + 1,
                    delay_ms = d.as_millis() as u64,
                    "{msg}; retrying"
                );
                attempt = attempt.saturating_add(1);
                d
            }
            Err(e) => return Err(e),
        };
        if sleep_or_shutdown(delay, &mut shutdown).await {
            return Ok(());
        }
    }
}
