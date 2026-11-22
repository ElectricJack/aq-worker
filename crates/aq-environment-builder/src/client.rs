//! Node-credential HTTP client for `/v1/worker/environments` and its
//! `claim`, `complete` and `ready` routes.
use reqwest::StatusCode;
use reqwest::header::{AUTHORIZATION, CONTENT_TYPE, HeaderMap, HeaderName, HeaderValue};
use serde_json::Value;
use std::time::Duration;

/// Claim and complete replies: a Job or an acknowledgement, bounded by the server's limits.
const MAX_RESPONSE_BYTES: u64 = 1024 * 1024;

/// The worker API origin and the node's credentials (from its worker config).
pub struct Server {
    pub url: String,
    pub credential: String,
    /// The optional Cloudflare Access client id and secret.
    pub cf_access: Option<(String, String)>,
}

impl Server {
    pub(crate) fn remote(&self) -> aq_node_git::Remote {
        let cf = (self.cf_access.as_ref()).map(|(id, secret)| (id.as_str(), secret.as_str()));
        aq_node_git::Remote::new(&self.url, &self.credential, cf)
    }
}

#[derive(Debug)]
pub enum Refused {
    /// Network failure, timeout, 408/429 or 5xx: the same request may be sent again.
    Transient(String),
    /// 403 on claim: this node is not in the server's `template-build` worker group.
    Forbidden,
    /// 409 on complete: another node holds the job's claim now.
    Conflict,
    /// 413 on complete: the result exceeds the server's bound from `environment_limits`.
    TooLarge,
    /// A reply over `cap` bytes (`bytes` when the server sent its length).
    Oversized {
        bytes: Option<u64>,
        cap: u64,
    },
    Other(String),
}

impl std::fmt::Display for Refused {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Transient(m) => write!(f, "temporary failure: {m}"),
            Self::Forbidden => f.write_str("this node is not a template-build node"),
            Self::Conflict => f.write_str("another node holds the claim"),
            Self::TooLarge => f.write_str("the result exceeds the server's bound"),
            Self::Oversized {
                bytes: Some(n),
                cap,
            } => {
                write!(f, "the reply is {n} bytes, over the cap of {cap} bytes")
            }
            Self::Oversized { bytes: None, cap } => {
                write!(f, "the reply is over the cap of {cap} bytes")
            }
            Self::Other(m) => f.write_str(m),
        }
    }
}

pub(crate) struct Client {
    http: reqwest::Client,
    base: String,
}

fn secret(value: &str) -> anyhow::Result<HeaderValue> {
    let mut value = HeaderValue::from_str(value)
        .map_err(|_| anyhow::anyhow!("credential is not a valid header value"))?;
    value.set_sensitive(true);
    Ok(value)
}

impl Client {
    pub(crate) fn new(server: &Server) -> anyhow::Result<Self> {
        let url =
            reqwest::Url::parse(&server.url).map_err(|_| anyhow::anyhow!("invalid server URL"))?;
        anyhow::ensure!(
            matches!(url.scheme(), "http" | "https") && url.host_str().is_some(),
            "invalid server URL: must be http:// or https://"
        );
        let mut headers = HeaderMap::new();
        headers.insert(
            AUTHORIZATION,
            secret(&format!("Bearer {}", server.credential))?,
        );
        headers.insert(CONTENT_TYPE, HeaderValue::from_static("application/json"));
        if let Some((id, sec)) = &server.cf_access {
            headers.insert(HeaderName::from_static("cf-access-client-id"), secret(id)?);
            headers.insert(
                HeaderName::from_static("cf-access-client-secret"),
                secret(sec)?,
            );
        }
        let http = reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .timeout(Duration::from_secs(10))
            .default_headers(headers)
            .build()?;
        let base = server.url.trim_end_matches('/').to_owned();
        Ok(Self { http, base })
    }

    async fn post(&self, path: &str, body: Vec<u8>) -> Result<(StatusCode, Value), Refused> {
        let request = self.http.post(format!("{}{path}", self.base)).body(body);
        self.send(request, MAX_RESPONSE_BYTES).await
    }

    async fn send(
        &self,
        request: reqwest::RequestBuilder,
        cap: u64,
    ) -> Result<(StatusCode, Value), Refused> {
        let transient = |e: reqwest::Error| Refused::Transient(e.without_url().to_string());
        let mut response = request.send().await.map_err(transient)?;
        let status = response.status();
        if status.is_server_error()
            || matches!(
                status,
                StatusCode::REQUEST_TIMEOUT | StatusCode::TOO_MANY_REQUESTS
            )
        {
            return Err(Refused::Transient(format!("server returned {status}")));
        }
        if status != StatusCode::OK {
            return Ok((status, Value::Null));
        }
        let length = response.content_length();
        if let Some(bytes) = length.filter(|n| *n > cap) {
            return Err(Refused::Oversized {
                bytes: Some(bytes),
                cap,
            });
        }
        let mut bytes = Vec::new();
        while let Some(chunk) = response.chunk().await.map_err(transient)? {
            if (bytes.len() + chunk.len()) as u64 > cap {
                return Err(Refused::Oversized { bytes: length, cap });
            }
            bytes.extend_from_slice(&chunk);
        }
        let reply = serde_json::from_slice(&bytes);
        Ok((
            status,
            reply.map_err(|_| Refused::Other("unreadable response".into()))?,
        ))
    }

    /// One Job, or `None` (204) when the queue is empty.
    pub(crate) async fn claim(&self) -> Result<Option<Value>, Refused> {
        match self
            .post("/v1/worker/environments/claim", b"{}".to_vec())
            .await?
        {
            (StatusCode::OK, job) => Ok(Some(job)),
            (StatusCode::NO_CONTENT, _) => Ok(None),
            (StatusCode::FORBIDDEN, _) => Err(Refused::Forbidden),
            (status, _) => Err(Refused::Other(format!("claim: server returned {status}"))),
        }
    }

    /// Posts a Job result. A replay of the same result gets the recorded answer.
    pub(crate) async fn complete(&self, body: &[u8]) -> Result<(), Refused> {
        match self
            .post("/v1/worker/environments/complete", body.to_vec())
            .await?
        {
            (StatusCode::OK, reply) if reply["recorded"] == Value::Bool(true) => Ok(()),
            (StatusCode::CONFLICT, _) => Err(Refused::Conflict),
            (StatusCode::PAYLOAD_TOO_LARGE, _) => Err(Refused::TooLarge),
            (status, _) => Err(Refused::Other(format!(
                "complete: server returned {status}"
            ))),
        }
    }

    /// `GET /v1/worker/environments`: what the server has admitted, at most `cap` bytes.
    pub(crate) async fn admitted(
        &self,
        cap: u64,
    ) -> Result<Vec<crate::node::AdmittedEnvironment>, Refused> {
        let url = format!("{}/v1/worker/environments", self.base);
        match self.send(self.http.get(url), cap).await? {
            (StatusCode::OK, mut reply) => serde_json::from_value(reply["environments"].take())
                .map_err(|_| Refused::Other("unreadable admitted environments".into())),
            (status, _) => Err(Refused::Other(format!(
                "environments: server returned {status}"
            ))),
        }
    }

    /// `POST /v1/worker/environments/ready`: replaces the node's ready set.
    pub(crate) async fn ready(&self, digests: &[String]) -> Result<(), Refused> {
        let body = serde_json::to_vec(&serde_json::json!({ "ready": digests }))
            .map_err(|e| Refused::Other(e.to_string()))?;
        match self.post("/v1/worker/environments/ready", body).await? {
            (StatusCode::OK, _) => Ok(()),
            (status, _) => Err(Refused::Other(format!("ready: server returned {status}"))),
        }
    }
}
