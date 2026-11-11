//! The node's authenticated link to aq-server (bearer, optional Cloudflare Access).
use crate::enrollment_file::EnrollmentFile;
use anyhow::{Context, Result, bail, ensure};
use aq_protocol::{
    ControlResponse, HandoffNotice, MeasurementSubmission, QualificationPin, SlotsRequest,
    SlotsResponse, valid_machine_key,
};
use reqwest::header;
use serde::{Serialize, de::DeserializeOwned};
use std::time::Duration;
use zeroize::Zeroizing;

pub struct ServerLink {
    origin: url::Url,
    credential: Zeroizing<String>,
    access: Option<(String, Zeroizing<String>)>,
    http: reqwest::Client,
}
/// The error of a non-2xx response; callers find it with `e.downcast_ref::<LinkError>()`.
#[derive(Debug, thiserror::Error)]
#[error("server answered {status}{}", code.as_deref().map(|c| format!(" ({c})")).unwrap_or_default())]
pub struct LinkError {
    pub status: u16,
    pub code: Option<String>,
    /// Server-provided transcript cursor, when present in a conflict response.
    pub next_sequence: Option<u64>,
}
impl LinkError {
    /// AQ itself refused the credential: 401 or 403 in AQ's own error schema.
    pub fn revoked(&self) -> bool {
        matches!(self.status, 401 | 403) && self.code.is_some()
    }
    /// A redirect, or a 401/403 without AQ's schema: an edge in front of AQ (Cloudflare
    /// Access) refused the request. That is a misconfigured Access token, never a revoke.
    pub fn edge_refused(&self) -> bool {
        (300..400).contains(&self.status)
            || (matches!(self.status, 401 | 403) && self.code.is_none())
    }
}
/// The node's server-origin rule: https to any host, plain http only to 127.0.0.1 or ::1;
/// no userinfo, query, fragment or path.
pub fn checked_origin(origin: &str) -> Result<url::Url> {
    let origin = url::Url::parse(origin).map_err(|_| anyhow::anyhow!("invalid server origin"))?;
    let loopback = matches!(origin.host(), Some(url::Host::Ipv4(ip)) if ip.is_loopback() && ip.octets() == [127, 0, 0, 1])
        || matches!(origin.host(), Some(url::Host::Ipv6(ip)) if ip.is_loopback());
    ensure!(
        origin.scheme() == "https" || (origin.scheme() == "http" && loopback),
        "the server origin must be https (plain http only to 127.0.0.1 or ::1)"
    );
    ensure!(
        origin.host_str().is_some()
            && origin.username().is_empty()
            && origin.password().is_none()
            && origin.path() == "/"
            && origin.query().is_none()
            && origin.fragment().is_none(),
        "the server origin must be a bare scheme://host[:port]"
    );
    Ok(origin)
}
impl ServerLink {
    /// A bounded, original-node WebSocket. Never follows a redirect or exposes credentials to a sandbox.
    pub async fn terminal_stream(
        &self,
        id: uuid::Uuid,
    ) -> Result<
        tokio_tungstenite::WebSocketStream<
            tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>,
        >,
    > {
        self.terminal_stream_path(&format!("/v1/worker/manual-agents/{id}/terminal-stream"))
            .await
    }
    pub async fn host_terminal_stream(
        &self,
        id: uuid::Uuid,
    ) -> Result<
        tokio_tungstenite::WebSocketStream<
            tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>,
        >,
    > {
        self.terminal_stream_path(&format!("/v1/worker/host-terminals/{id}/terminal-stream"))
            .await
    }
    pub async fn host_terminal_poll(&self, body: &serde_json::Value) -> Result<serde_json::Value> {
        self.post("/v1/worker/host-terminals", body).await
    }
    async fn terminal_stream_path(
        &self,
        path: &str,
    ) -> Result<
        tokio_tungstenite::WebSocketStream<
            tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>,
        >,
    > {
        use tokio_tungstenite::tungstenite::client::IntoClientRequest;
        let mut url = self.origin.join(path)?;
        let scheme = if url.scheme() == "https" { "wss" } else { "ws" };
        url.set_scheme(scheme)
            .map_err(|_| anyhow::anyhow!("invalid stream origin"))?;
        let mut request = url
            .as_str()
            .into_client_request()
            .map_err(|_| anyhow::anyhow!("invalid terminal stream request"))?;
        let mut bearer = header::HeaderValue::from_str(&format!("Bearer {}", &*self.credential))
            .map_err(|_| anyhow::anyhow!("invalid node credential"))?;
        bearer.set_sensitive(true);
        request.headers_mut().insert(header::AUTHORIZATION, bearer);
        if let Some((id, secret)) = &self.access {
            let mut value = header::HeaderValue::from_str(secret)
                .map_err(|_| anyhow::anyhow!("invalid Access secret"))?;
            value.set_sensitive(true);
            request
                .headers_mut()
                .insert("cf-access-client-id", header::HeaderValue::from_str(id)?);
            request
                .headers_mut()
                .insert("cf-access-client-secret", value);
        }
        let config = tokio_tungstenite::tungstenite::protocol::WebSocketConfig::default()
            .max_message_size(Some(131072))
            .max_frame_size(Some(131072));
        tokio::time::timeout(
            Duration::from_secs(10),
            tokio_tungstenite::connect_async_with_config(request, Some(config), false),
        )
        .await
        .map_err(|_| anyhow::anyhow!("terminal stream handshake timed out"))?
        .map(|(socket, _)| socket)
        .map_err(|_| anyhow::anyhow!("terminal stream handshake refused"))
    }
    pub async fn manual_poll(&self, body: &serde_json::Value) -> Result<serde_json::Value> {
        self.post("/v1/worker/manual-agents", body).await
    }
    /// Append an immutable transcript chunk or finalize an archive.
    pub async fn manual_transcript(
        &self,
        id: uuid::Uuid,
        body: &serde_json::Value,
    ) -> Result<serde_json::Value> {
        self.post(&format!("/v1/worker/manual-agents/{id}/transcript"), body)
            .await
    }
    /// Refuses an origin outside `checked_origin`'s rule and an empty or non-ASCII credential.
    pub fn new(origin: &str, credential: &str, access: Option<(&str, &str)>) -> Result<Self> {
        let origin = checked_origin(origin)?;
        ensure!(
            !credential.is_empty() && credential.bytes().all(|b| b.is_ascii_graphic()),
            "invalid node credential"
        );
        let http = reqwest::Client::builder()
            .timeout(Duration::from_secs(10))
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .context("build HTTP client")?;
        Ok(Self {
            origin,
            credential: Zeroizing::new(credential.to_owned()),
            access: access.map(|(id, secret)| (id.to_owned(), Zeroizing::new(secret.to_owned()))),
            http,
        })
    }
    pub fn from_enrollment(file: &EnrollmentFile) -> Result<Self> {
        let access = match (&file.cf_client_id, &file.cf_client_secret) {
            (Some(id), Some(secret)) => Some((id.as_str(), secret.as_str())),
            (None, None) => None,
            _ => bail!("Access client id and secret must both be set"),
        };
        Self::new(&file.server_url, &file.credential, access)
    }
    pub async fn slots(&self, req: &SlotsRequest) -> Result<SlotsResponse> {
        self.post("/v1/worker/slots", req).await
    }
    pub async fn control(&self) -> Result<ControlResponse> {
        self.post(
            "/v1/worker/control",
            &serde_json::json!({"schema_version": 1}),
        )
        .await
    }
    pub async fn handoff(&self, notice: &HandoffNotice) -> Result<()> {
        self.post::<_, serde_json::Value>("/v1/worker/handoffs", notice)
            .await
            .map(drop)
    }
    /// D2 ruling C: releases an attempt whose host was lost, with an outside stop proof.
    pub async fn lost_attempt(
        &self,
        r: &aq_protocol::LostAttemptRelease,
    ) -> Result<aq_protocol::LostAttemptReleased> {
        self.post("/v1/worker/lost-attempts", r).await
    }
    pub async fn submit_measurement(&self, s: &MeasurementSubmission) -> Result<QualificationPin> {
        self.post("/v1/worker/qualifications", s).await
    }
    pub async fn machine_report<T: Serialize + Sync>(&self, report: &T) -> Result<()> {
        self.post::<_, serde_json::Value>("/v1/worker/machine", report)
            .await
            .map(drop)
    }
    /// Every request: bearer, Access headers when set, JSON. A non-2xx answer becomes a
    /// `LinkError` carrying the status and the server's `error` code, never the request.
    async fn post<B: Serialize + Sync, T: DeserializeOwned>(
        &self,
        path: &str,
        body: &B,
    ) -> Result<T> {
        let mut bearer = header::HeaderValue::from_str(&format!("Bearer {}", &*self.credential))
            .map_err(|_| anyhow::anyhow!("invalid node credential"))?;
        bearer.set_sensitive(true);
        let mut request = self
            .http
            .post(self.origin.join(path)?)
            .header(header::AUTHORIZATION, bearer)
            .header(header::CONTENT_TYPE, "application/json")
            .body(serde_json::to_vec(body)?);
        if let Some((id, secret)) = &self.access {
            let mut secret = header::HeaderValue::from_str(secret)
                .map_err(|_| anyhow::anyhow!("invalid Access secret"))?;
            secret.set_sensitive(true);
            request = request
                .header("cf-access-client-id", id)
                .header("cf-access-client-secret", secret);
        }
        let response = request
            .send()
            .await
            .map_err(|e| anyhow::anyhow!("request to {path} failed: {}", e.without_url()))?;
        let status = response.status();
        let bytes = response
            .bytes()
            .await
            .map_err(|e| anyhow::anyhow!("read {path} reply: {}", e.without_url()))?;
        if !status.is_success() {
            let code = serde_json::from_slice::<aq_protocol::ApiError>(&bytes)
                .ok()
                .map(|e| e.error);
            return Err(LinkError {
                status: status.as_u16(),
                code,
                next_sequence: if status.as_u16() == 409 {
                    serde_json::from_slice::<serde_json::Value>(&bytes)
                        .ok()
                        .and_then(|v| v.get("next_sequence").and_then(|n| n.as_u64()))
                } else {
                    None
                },
            }
            .into());
        }
        let bytes = if bytes.is_empty() {
            &b"null"[..]
        } else {
            &bytes[..]
        };
        serde_json::from_slice(bytes).with_context(|| format!("decode {path} reply"))
    }
}
/// The aq-worker-ctl machine-config rule: lower-case, every byte outside [a-z0-9-] becomes '-',
/// cut to 63. None when the result is not a valid key (for example it starts with '-').
pub fn default_machine_key(hostname: &str) -> Option<String> {
    let key: String = hostname
        .to_lowercase()
        .bytes()
        .map(|b| {
            if b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-' {
                b as char
            } else {
                '-'
            }
        })
        .take(63)
        .collect();
    valid_machine_key(&key).then_some(key)
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::{Json, Router, http::HeaderMap, routing::post};
    use std::sync::{Arc, Mutex};

    #[test]
    fn refuses_plain_http_off_loopback() {
        assert!(ServerLink::new("http://10.0.0.5:8081", "c", None).is_err());
        assert!(ServerLink::new("http://127.0.0.1:28081", "c", None).is_ok());
        assert!(ServerLink::new("http://[::1]:28081", "c", None).is_ok());
        assert!(ServerLink::new("https://aq.example.com", "c", None).is_ok());
        assert!(ServerLink::new("https://u:p@aq.example.com", "c", None).is_err());
        assert!(ServerLink::new("https://aq.example.com/x", "c", None).is_err());
    }
    #[tokio::test]
    async fn sends_bearer_and_access_headers() {
        let seen = Arc::new(Mutex::new(Vec::<(String, String, String, String)>::new()));
        let log = seen.clone();
        let handler = move |name: &'static str| {
            let log = log.clone();
            post(move |h: HeaderMap, Json(_): Json<serde_json::Value>| {
                let log = log.clone();
                async move {
                    let get = |k: &str| {
                        h.get(k)
                            .map(|v| v.to_str().unwrap().to_owned())
                            .unwrap_or_default()
                    };
                    log.lock().unwrap().push((
                        name.into(),
                        get("authorization"),
                        get("cf-access-client-id"),
                        get("cf-access-client-secret"),
                    ));
                    Json(serde_json::json!(null))
                }
            })
        };
        let app = Router::new()
            .route("/v1/worker/handoffs", handler("handoffs"))
            .route("/v1/worker/machine", handler("machine"));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let origin = format!("http://{}", listener.local_addr().unwrap());
        let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let link = ServerLink::new(&origin, "node-cred", Some(("cid", "csecret"))).unwrap();
        link.handoff(&HandoffNotice {
            schema_version: 1,
            attempt_id: uuid::Uuid::nil(),
            reason: aq_protocol::HandoffReason::Restart,
        })
        .await
        .unwrap();
        link.machine_report(&serde_json::json!({"a": 1}))
            .await
            .unwrap();
        server.abort();
        let seen = seen.lock().unwrap();
        assert_eq!(
            seen.iter().map(|s| s.0.as_str()).collect::<Vec<_>>(),
            ["handoffs", "machine"]
        );
        for s in seen.iter() {
            assert_eq!(
                (s.1.as_str(), s.2.as_str(), s.3.as_str()),
                ("Bearer node-cred", "cid", "csecret")
            );
        }
    }
    #[tokio::test]
    async fn error_carries_status_not_secrets() {
        let app = Router::new().route(
            "/v1/worker/control",
            post(|| async {
                (
                    axum::http::StatusCode::UNAUTHORIZED,
                    Json(serde_json::json!({"error": "unauthorized", "message": "no"})),
                )
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let origin = format!("http://{}", listener.local_addr().unwrap());
        let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let link = ServerLink::new(&origin, "very-secret-credential", None).unwrap();
        let error = link.control().await.unwrap_err();
        server.abort();
        let e = error.downcast_ref::<LinkError>().unwrap();
        assert_eq!((e.status, e.code.as_deref()), (401, Some("unauthorized")));
        assert!(!format!("{error:#}").contains("very-secret"));
    }
    /// Review M5: only AQ's own 401/403 is a revoke; an edge's redirect or bare 403 is not.
    #[tokio::test]
    async fn edge_refusals_are_not_revocations() {
        use axum::http::{StatusCode, header::LOCATION};
        let app = Router::new()
            .route(
                "/v1/worker/control",
                post(|| async {
                    (
                        StatusCode::FOUND,
                        [(LOCATION, "https://team.cloudflareaccess.com/login")],
                        "",
                    )
                }),
            )
            .route(
                "/v1/worker/handoffs",
                post(|| async {
                    (
                        StatusCode::FORBIDDEN,
                        "<html>Forbidden. You don't have permission</html>",
                    )
                }),
            )
            .route(
                "/v1/worker/machine",
                post(|| async {
                    (
                        StatusCode::FORBIDDEN,
                        Json(serde_json::json!({"error": "revoked", "message": "no"})),
                    )
                }),
            );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let origin = format!("http://{}", listener.local_addr().unwrap());
        let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let link = ServerLink::new(&origin, "c", None).unwrap();
        let notice = HandoffNotice {
            schema_version: 1,
            attempt_id: uuid::Uuid::nil(),
            reason: aq_protocol::HandoffReason::Restart,
        };
        let errors = [
            link.control().await.map(drop).unwrap_err(),
            link.handoff(&notice).await.unwrap_err(),
            link.machine_report(&serde_json::json!({}))
                .await
                .unwrap_err(),
        ];
        server.abort();
        let kinds: Vec<_> = (errors.iter())
            .map(|e| e.downcast_ref::<LinkError>().unwrap())
            .map(|l| (l.status, l.revoked(), l.edge_refused()))
            .collect();
        assert_eq!(
            kinds,
            [(302, false, true), (403, false, true), (403, true, false)]
        );
    }
    #[test]
    fn default_machine_key_follows_the_ctl_rule() {
        assert_eq!(
            default_machine_key("DESKTOP-7Q").as_deref(),
            Some("desktop-7q")
        );
        assert_eq!(
            default_machine_key("my_box.local").as_deref(),
            Some("my-box-local")
        );
        assert_eq!(default_machine_key("-x"), None);
        assert_eq!(default_machine_key(&"a".repeat(80)).unwrap().len(), 63);
    }
}
