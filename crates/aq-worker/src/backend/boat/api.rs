//! Bounded HTTP control channel. Provider bodies/URLs and keys never enter errors.
use super::*;
use reqwest::{Client, Method};
use serde_json::Value;
use std::time::Duration;
use zeroize::Zeroizing;

pub(super) struct Api {
    client: Client,
    url: String,
    org: String,
    key: BoatKey,
    timeout_ms: u64,
}
/// Server-side bound sent with every sandbox command.
pub(super) const COMMAND_TIMEOUT_SECS: u64 = 60;
/// A command call legitimately blocks for up to its server-side bound, so its
/// HTTP deadline must outlast that or a healthy long command reads as Transport.
fn request_timeout(path: &str, api_timeout_ms: u64) -> Duration {
    let extra = if path.ends_with("/commands") {
        COMMAND_TIMEOUT_SECS * 1000
    } else {
        0
    };
    Duration::from_millis(api_timeout_ms.saturating_add(extra))
}
impl Api {
    pub fn new(config: &BoatConfig, key: BoatKey) -> Result<Self, BackendError> {
        let client = Client::builder()
            .timeout(Duration::from_millis(config.api_timeout_ms))
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .map_err(|_| unavailable(BackendUnavailable::Transport))?;
        Ok(Self {
            client,
            url: config.api_url.trim_end_matches('/').into(),
            org: config.attempt_org.clone(),
            key,
            timeout_ms: config.api_timeout_ms,
        })
    }
    pub async fn raw(
        &self,
        method: Method,
        path: &str,
        body: Option<Value>,
        idempotency: Option<&str>,
    ) -> Result<(u16, Vec<u8>), BackendError> {
        let mut request = self
            .client
            .request(method.clone(), format!("{}{path}", self.url))
            .timeout(request_timeout(path, self.timeout_ms))
            .bearer_auth(self.key.expose())
            .header("X-Boat-Org", &self.org);
        if let Some(key) = idempotency {
            request = request.header("Idempotency-Key", key);
        }
        if method == Method::DELETE {
            let id = path
                .trim_start_matches("/sandboxes/")
                .split('/')
                .next()
                .unwrap_or("");
            request = request.header("X-Ascii-Confirm-Delete", id);
        }
        if let Some(value) = body {
            let bytes = Zeroizing::new(serde_json::to_vec(&value).map_err(|_| unknown())?);
            request = request
                .header("Content-Type", "application/json")
                .body(bytes.to_vec());
        }
        let mut response = request
            .send()
            .await
            .map_err(|_| unavailable(BackendUnavailable::Transport))?;
        let status = response.status().as_u16();
        if status >= 500 {
            return Err(unavailable(BackendUnavailable::Api));
        }
        if status == 429 {
            return Err(unavailable(BackendUnavailable::RateLimited));
        }
        let mut bytes = Vec::new();
        while let Some(chunk) = response
            .chunk()
            .await
            .map_err(|_| unavailable(BackendUnavailable::Transport))?
        {
            if bytes.len() + chunk.len() > 4 * 1024 * 1024 {
                return Err(unknown());
            }
            bytes.extend_from_slice(&chunk);
        }
        if matches!(status, 400 | 402 | 403 | 409) {
            let v: Value = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
            let code = v["code"]
                .as_str()
                .or_else(|| v["error"]["code"].as_str())
                .or_else(|| v["error"].as_str())
                .unwrap_or("");
            if status == 402
                || matches!(
                    code,
                    "sandbox_limit_reached"
                        | "sandbox_limit"
                        | "active_sandbox_limit"
                        | "active_sandbox_limit_reached"
                        | "max_sandboxes_reached"
                        | "limit_reached"
                        | "concurrent_sandbox_limit"
                        | "no_ready_machine"
                        | "start_limit_reached"
                )
            {
                return Err(unavailable(BackendUnavailable::Capacity));
            }
        }
        Ok((status, bytes))
    }
    pub async fn json(
        &self,
        method: Method,
        path: &str,
        body: Option<Value>,
        idempotency: Option<&str>,
    ) -> Result<Value, BackendError> {
        let (status, bytes) = self.raw(method, path, body, idempotency).await?;
        if status == 404 {
            return Err(BackendError::NotFound);
        }
        if !(200..300).contains(&status) {
            return Err(BackendError::Uncertain(UnconfirmedReason::BackendFailure));
        }
        let value: Value = serde_json::from_slice(&bytes).map_err(|_| unknown())?;
        if value["ok"] != true {
            return Err(unknown());
        }
        Ok(value)
    }
}
pub(super) fn unavailable(reason: BackendUnavailable) -> BackendError {
    BackendError::Unavailable(reason)
}
pub(super) fn unknown() -> BackendError {
    BackendError::Uncertain(UnconfirmedReason::UnknownState)
}
pub(super) fn encode(s: &str) -> String {
    reqwest::Url::parse("http://localhost/")
        .unwrap()
        .query_pairs_mut()
        .append_pair("", s)
        .finish()
        .query()
        .unwrap()
        .trim_start_matches('=')
        .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn command_requests_outlast_their_server_side_bound() {
        assert_eq!(
            request_timeout("/sandboxes/bx_1", 30_000),
            Duration::from_secs(30)
        );
        let command = request_timeout("/sandboxes/bx_1/commands", 30_000);
        assert!(command > Duration::from_secs(COMMAND_TIMEOUT_SECS));
        assert_eq!(command, Duration::from_secs(90));
    }
}
