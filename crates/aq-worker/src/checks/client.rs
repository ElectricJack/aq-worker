//! Node-credential HTTP client for `/v1/worker/checks/{claim,stop}`.
use std::fmt;
use std::future::Future;
use std::time::Duration;

use reqwest::header::{AUTHORIZATION, CONTENT_TYPE, HeaderMap, HeaderName, HeaderValue};
use reqwest::{StatusCode, redirect};
use serde_json::Value;
use zeroize::Zeroizing;

use crate::client::error_chain;
use crate::config::WorkerConfig;

const MAX_RESPONSE_BYTES: usize = 1024 * 1024;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CheckCallError {
    /// Network failure, timeout, 408/429 or 5xx: retry the identical request.
    Transient(String),
    /// 409: the server already holds a different stop for this operation.
    Conflict,
    /// 404: no such attempt for this node.
    NotFound,
    /// 400: the server will never accept this body.
    Invalid(String),
    /// Any other refusal (auth, Access redirect, unreadable reply): fatal.
    Rejected(String),
}
impl fmt::Display for CheckCallError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Transient(m) => write!(f, "temporary failure: {m}"),
            Self::Conflict => write!(f, "server holds a different stop"),
            Self::NotFound => write!(f, "check attempt not found for this node"),
            Self::Invalid(m) => write!(f, "invalid: {m}"),
            Self::Rejected(m) => write!(f, "rejected: {m}"),
        }
    }
}
impl std::error::Error for CheckCallError {}

/// The two calls the check runner makes, so it can be tested without HTTP.
pub trait CheckTransport: Send + Sync {
    /// Raw claim entries; each is validated individually by the runner.
    fn claim(&self, limit: u32) -> impl Future<Output = Result<Vec<Value>, CheckCallError>> + Send;
    /// Sends a pre-serialized stop body unchanged.
    fn stop(&self, body: &[u8]) -> impl Future<Output = Result<(), CheckCallError>> + Send;
}

pub struct CheckClient {
    http: reqwest::Client,
    base: String,
    bearer: HeaderValue,
}

fn secret(value: &str) -> anyhow::Result<HeaderValue> {
    let mut v = HeaderValue::from_str(value)
        .map_err(|_| anyhow::anyhow!("credential is not a valid header value"))?;
    v.set_sensitive(true);
    Ok(v)
}

impl CheckClient {
    pub fn from_config(cfg: &WorkerConfig) -> anyhow::Result<Self> {
        let url = reqwest::Url::parse(&cfg.server_url)
            .map_err(|_| anyhow::anyhow!("invalid server URL"))?;
        anyhow::ensure!(
            matches!(url.scheme(), "http" | "https") && url.host_str().is_some(),
            "invalid server URL: must be http:// or https://"
        );
        let mut headers = HeaderMap::new();
        if let (Some(id), Some(sec)) = (&cfg.cf_client_id, &cfg.cf_client_secret) {
            headers.insert(HeaderName::from_static("cf-access-client-id"), secret(id)?);
            headers.insert(
                HeaderName::from_static("cf-access-client-secret"),
                secret(sec)?,
            );
        }
        let http = reqwest::Client::builder()
            .redirect(redirect::Policy::none())
            .timeout(Duration::from_secs(10))
            .default_headers(headers)
            .build()?;
        let bearer = Zeroizing::new(format!("Bearer {}", cfg.credential));
        Ok(Self {
            http,
            base: cfg.server_url.trim_end_matches('/').to_string(),
            bearer: secret(&bearer)?,
        })
    }

    async fn post(&self, path: &str, body: Vec<u8>) -> Result<Value, CheckCallError> {
        let response = self
            .http
            .post(format!("{}{path}", self.base))
            .header(CONTENT_TYPE, "application/json")
            .header(AUTHORIZATION, self.bearer.clone())
            .body(body)
            .send()
            .await
            .map_err(|e| CheckCallError::Transient(error_chain(e)))?;
        let status = response.status();
        if status.is_server_error()
            || matches!(
                status,
                StatusCode::REQUEST_TIMEOUT | StatusCode::TOO_MANY_REQUESTS
            )
        {
            return Err(CheckCallError::Transient(format!(
                "server returned {status}"
            )));
        }
        if status == StatusCode::CONFLICT {
            return Err(CheckCallError::Conflict);
        }
        if status == StatusCode::NOT_FOUND {
            return Err(CheckCallError::NotFound);
        }
        if status == StatusCode::BAD_REQUEST {
            return Err(CheckCallError::Invalid(format!("server returned {status}")));
        }
        let html = response
            .headers()
            .get(CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .is_some_and(|v| v.trim_start().to_ascii_lowercase().starts_with("text/html"));
        if !status.is_success() || html {
            return Err(CheckCallError::Rejected(format!(
                "server returned {status}"
            )));
        }
        let mut response = response;
        let mut bytes = Vec::new();
        while let Some(chunk) = response
            .chunk()
            .await
            .map_err(|e| CheckCallError::Transient(error_chain(e)))?
        {
            if bytes.len() + chunk.len() > MAX_RESPONSE_BYTES {
                return Err(CheckCallError::Rejected("response too large".into()));
            }
            bytes.extend_from_slice(&chunk);
        }
        serde_json::from_slice(&bytes)
            .map_err(|_| CheckCallError::Rejected("unreadable response".into()))
    }
}

impl CheckTransport for CheckClient {
    async fn claim(&self, limit: u32) -> Result<Vec<Value>, CheckCallError> {
        let limit = limit.clamp(1, super::MAX_CLAIM_LIMIT);
        let body = serde_json::to_vec(&serde_json::json!({ "limit": limit }))
            .map_err(|e| CheckCallError::Rejected(e.to_string()))?;
        let reply = self.post("/v1/worker/checks/claim", body).await?;
        match reply.get("checks") {
            Some(Value::Array(checks)) if checks.len() <= super::MAX_CLAIM_LIMIT as usize => {
                Ok(checks.clone())
            }
            _ => Err(CheckCallError::Rejected("malformed claim response".into())),
        }
    }

    async fn stop(&self, body: &[u8]) -> Result<(), CheckCallError> {
        let reply = self.post("/v1/worker/checks/stop", body.to_vec()).await?;
        if reply.get("recorded") == Some(&Value::Bool(true)) {
            Ok(())
        } else {
            Err(CheckCallError::Rejected("stop was not recorded".into()))
        }
    }
}
