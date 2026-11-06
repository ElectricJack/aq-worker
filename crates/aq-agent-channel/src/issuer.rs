//! Agent credential issuance behind a small interface.
//!
//! The channel asks a [`CredentialIssuer`] for the attempt's credential at
//! open and again before expiry. [`NodeIssuer`] is the production issuer: the
//! existing `/v2/worker/agent-session` route under the node credential, with
//! the `expected_credential_version` fence. Spec 0006 phase 2 seals the
//! attempt's grants into the same `agent_launch_bindings` row this route
//! issues from, so the call itself does not change when grants land.
use crate::AccessHeaders;
use reqwest::header::{AUTHORIZATION, CONTENT_TYPE, HeaderValue};
use serde_json::{Value, json};
use std::{future::Future, pin::Pin, time::Duration};
use uuid::Uuid;
use zeroize::Zeroizing;

/// One issued credential. Never printed.
pub struct Issued {
    pub token: Zeroizing<String>,
    /// Server credential version (the rotation fence).
    pub version: u64,
    /// Unix milliseconds.
    pub expires_at_ms: u64,
}
impl std::fmt::Debug for Issued {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "Issued {{ token: <redacted>, version: {}, expires_at_ms: {} }}",
            self.version, self.expires_at_ms
        )
    }
}
impl Issued {
    /// Shape check the channel applies to every issuance.
    pub fn valid_shape(&self) -> bool {
        self.token.starts_with(crate::CREDENTIAL_PREFIX)
            && (36..=4096).contains(&self.token.len())
            && self.token.bytes().all(|b| b.is_ascii_graphic())
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum IssueError {
    /// The server refused for good (revoked, expired, stale or fenced); the
    /// code is the server's. The channel closes.
    Refused(String),
    /// Transport or server unavailable; worth retrying before expiry.
    Unavailable,
}
impl std::fmt::Display for IssueError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Refused(code) => write!(f, "credential issuance refused ({code})"),
            Self::Unavailable => f.write_str("credential issuance unavailable"),
        }
    }
}
impl std::error::Error for IssueError {}

pub type IssueFuture<'a> = Pin<Box<dyn Future<Output = Result<Issued, IssueError>> + Send + 'a>>;

/// Issues (and rotates) one attempt's agent credential.
pub trait CredentialIssuer: Send + Sync + 'static {
    /// `expected_version` is the version the caller holds (0 before the first).
    fn issue(&self, expected_version: u64) -> IssueFuture<'_>;
}

/// The attempt the credential is bound to (`SessionIdentity::Remote`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct AttemptIdentity {
    pub node_id: Uuid,
    pub boot_id: Uuid,
    pub attempt_id: Uuid,
    pub session_id: Uuid,
    pub generation: u64,
}

/// `/v2/worker/agent-session` under the node credential.
pub struct NodeIssuer {
    client: reqwest::Client,
    url: reqwest::Url,
    node_bearer: Zeroizing<String>,
    access: Option<AccessHeaders>,
    attempt: AttemptIdentity,
}
impl NodeIssuer {
    pub fn new(
        origin: &str,
        node_credential: &str,
        access: Option<AccessHeaders>,
        attempt: AttemptIdentity,
    ) -> anyhow::Result<Self> {
        let upstream = crate::forward::Upstream::new(
            origin,
            crate::forward::Origins::Either,
            Duration::from_secs(10),
        )?;
        anyhow::ensure!(
            !node_credential.starts_with(crate::CREDENTIAL_PREFIX)
                && (16..=4096).contains(&node_credential.len())
                && node_credential.bytes().all(|b| b.is_ascii_graphic()),
            "node credential class/bound refused"
        );
        Ok(Self {
            url: upstream.origin.join("/v2/worker/agent-session")?,
            client: upstream.client,
            node_bearer: Zeroizing::new(format!("Bearer {node_credential}")),
            access,
            attempt,
        })
    }
    async fn exchange(&self, expected: u64) -> Result<Issued, IssueError> {
        let a = &self.attempt;
        let body = json!({
            "identity": {"node_id": a.node_id, "boot_id": a.boot_id},
            "attempt": {"attempt_id": a.attempt_id, "session_id": a.session_id, "generation": a.generation},
            "expected_credential_version": expected,
        });
        let mut bearer =
            HeaderValue::from_str(&self.node_bearer).map_err(|_| IssueError::Unavailable)?;
        bearer.set_sensitive(true);
        let mut request = self
            .client
            .post(self.url.clone())
            .header(AUTHORIZATION, bearer)
            .header(CONTENT_TYPE, "application/json")
            .header("X-AQ-Agent-API-Version", "1")
            .header("X-AQ-Request", "1")
            .body(body.to_string());
        if let Some(access) = &self.access {
            for (name, value) in access.headers().map_err(|_| IssueError::Unavailable)? {
                request = request.header(name, value);
            }
        }
        let response = request.send().await.map_err(|_| IssueError::Unavailable)?;
        let status = response.status().as_u16();
        let bytes = Zeroizing::new(
            response
                .bytes()
                .await
                .map_err(|_| IssueError::Unavailable)?
                .to_vec(),
        );
        let value: Value = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
        if status != 200 {
            return Err(match status {
                401 | 403 | 404 | 409 => IssueError::Refused(
                    value["code"]
                        .as_str()
                        .filter(|c| c.len() <= 64 && c.bytes().all(|b| b.is_ascii_graphic()))
                        .unwrap_or("refused")
                        .to_owned(),
                ),
                _ => IssueError::Unavailable,
            });
        }
        parse_issued(&value, expected).ok_or(IssueError::Refused("invalid_response".into()))
    }
}
impl CredentialIssuer for NodeIssuer {
    fn issue(&self, expected_version: u64) -> IssueFuture<'_> {
        Box::pin(self.exchange(expected_version))
    }
}

/// The issuance response (`{"api_version":1,"token","context":{"credential":
/// {"version","expires_at"}}}`); the version must be `expected + 1`.
pub(crate) fn parse_issued(value: &Value, expected: u64) -> Option<Issued> {
    let credential = &value["context"]["credential"];
    let version: u64 = credential["version"].as_str()?.parse().ok()?;
    let expires = chrono::DateTime::parse_from_rfc3339(credential["expires_at"].as_str()?).ok()?;
    let issued = Issued {
        token: Zeroizing::new(value["token"].as_str()?.to_owned()),
        version,
        expires_at_ms: u64::try_from(expires.timestamp_millis()).ok()?,
    };
    (value["api_version"] == 1 && Some(version) == expected.checked_add(1) && issued.valid_shape())
        .then_some(issued)
}
