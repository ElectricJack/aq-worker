//! One bounded HTTP/1.1 exchange from a local caller to a fixed AQ origin.
//!
//! Extracted from the B6b `AqApiProxy` (aq-worker
//! `native/runner/sandbox/proxy.rs`) and shared by it and the agent channel.
//! The framing rules are B6b's: fixed upstream, one request per connection,
//! no `Transfer-Encoding`, `Upgrade` or `Expect`, bounded head and body, no
//! redirects, identity encoding only, and a response that echoes injected
//! material is refused.
use reqwest::{
    Url,
    header::{HeaderMap, HeaderName, HeaderValue},
};
use std::time::Duration;
use tokio::io::{AsyncRead, AsyncReadExt};
use zeroize::Zeroizing;

pub const MAX_HEAD: usize = 16384;
pub const MAX_BODY: usize = 4 * 1024 * 1024;

/// Agent-facing prefixes the channel forwards (0006 §5.1). New commands live
/// under `/api/v1/commands`, so they never need a change here.
pub const AGENT_PREFIXES: &[&str] = &["/api/v1/commands/", "/api/v1/operations/", "/v1/agent/"];
/// Node and worker namespaces, refused by name.
pub const NODE_PREFIXES: &[&str] = &["/v1/worker/", "/v2/worker/", "/v1/execution/"];

/// Which origins an upstream may name.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum Origins {
    /// `https://<hostname>` only (no IP literal).
    MachineHostname,
    /// `http://127.0.0.1:<port>` only (fixtures and the local MS1 server).
    NumericLoopback,
    /// The node's own enrollment (`server_url`), under the rule its native
    /// transport applies (aq-worker `NativeClient::from_config`): `https://`
    /// with any host, or `http://` to a numeric loopback address. Never
    /// stricter than the connection the node already makes with its own
    /// credential, and never looser: no plaintext to a named or remote host.
    Node,
}

/// Whether `origin` is a node enrollment the channel can serve ([`Origins::Node`]).
pub fn node_origin(origin: &str) -> anyhow::Result<()> {
    Upstream::check(origin, Origins::Node).map(|_| ())
}

impl Origins {
    fn admits(self, url: &Url) -> bool {
        let host = url.host_str().unwrap_or_default();
        let ip = host
            .trim_start_matches('[')
            .trim_end_matches(']')
            .parse::<std::net::IpAddr>()
            .ok();
        let https = url.scheme() == "https" && !host.is_empty();
        let hostname = https && ip.is_none();
        let loopback =
            url.scheme() == "http" && url.host_str() == Some("127.0.0.1") && url.port().is_some();
        let node_loopback = url.scheme() == "http" && ip.is_some_and(|ip| ip.is_loopback());
        match self {
            Self::MachineHostname => hostname,
            Self::NumericLoopback => loopback,
            Self::Node => https || node_loopback,
        }
    }
}

pub(crate) struct Upstream {
    pub origin: Url,
    pub client: reqwest::Client,
}
impl Upstream {
    fn check(origin: &str, allowed: Origins) -> anyhow::Result<Url> {
        let url = Url::parse(origin)?;
        anyhow::ensure!(
            allowed.admits(&url),
            "AQ upstream must be HTTPS, or plain HTTP only to a numeric loopback address"
        );
        anyhow::ensure!(
            url.username().is_empty()
                && url.password().is_none()
                && url.path() == "/"
                && url.query().is_none()
                && url.fragment().is_none(),
            "AQ proxy requires fixed origin"
        );
        Ok(url)
    }
    pub fn new(origin: &str, allowed: Origins, timeout: Duration) -> anyhow::Result<Self> {
        let url = Self::check(origin, allowed)?;
        let client = reqwest::Client::builder()
            .no_proxy()
            .redirect(reqwest::redirect::Policy::none())
            .connect_timeout(Duration::from_secs(5))
            .timeout(timeout)
            .build()?;
        Ok(Self {
            origin: url,
            client,
        })
    }
}

/// Headers a listener injects into every forwarded request.
pub(crate) type Inject = Vec<(HeaderName, HeaderValue)>;

/// What a listener injects into one upstream send, taken just before it.
pub(crate) struct Taken {
    pub headers: Inject,
    /// Values a response must never echo.
    pub protected: Vec<Zeroizing<Vec<u8>>>,
    /// The credential version the headers carry; `None` never retries.
    pub version: Option<u64>,
}

/// The injected material, read as late as possible: a rotation that commits
/// while a caller is still writing its request is picked up by that request.
pub(crate) trait Credentials: Send + Sync {
    fn take(&self) -> Option<Taken>;
    /// The version held now; a change after [`Credentials::take`] means a
    /// rotation raced the send.
    fn version(&self) -> Option<u64> {
        None
    }
}

/// Fixed headers (the B6b loopback's Access service token).
pub(crate) struct Fixed(pub Inject, pub Vec<Zeroizing<Vec<u8>>>);
impl Credentials for Fixed {
    fn take(&self) -> Option<Taken> {
        Some(Taken {
            headers: self.0.clone(),
            protected: self.1.clone(),
            version: None,
        })
    }
}

pub(crate) struct Rules {
    /// Required `Host` value (a TCP listener's authority); `None` accepts any
    /// (a Unix socket has no authority).
    pub host: Option<String>,
    /// Drop the caller's `Authorization`: the channel injects the attempt's.
    pub replace_authorization: bool,
    /// Forward only the agent-facing prefixes (0006 §5.1).
    pub agent_paths: bool,
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Failure {
    /// The path is outside the channel (`channel_refused`).
    Refused(&'static str),
    /// The request framing was refused or the caller went away.
    Framing,
    /// The upstream failed, redirected, encoded or echoed protected material.
    Upstream,
}

pub(crate) struct Forwarded {
    pub response: Vec<u8>,
    pub status: u16,
    /// The `code` of a 401 answer (`session_revoked`, `unauthorized`,
    /// `session_expired`): the attempt credential was not accepted.
    pub refused: Option<String>,
    /// The answer says `effect: not_applied`.
    pub not_applied: bool,
    /// The credential version the answered request carried.
    pub version: Option<u64>,
    /// The caller asked for an agent API version other than 1. aq-server's
    /// command surface answers that `unauthorized`, which says nothing about
    /// the credential.
    pub foreign_version: bool,
}

/// Method and path (no query) of a request, for logs. Never headers or bodies.
#[derive(Clone, Default)]
pub(crate) struct Seen {
    pub method: String,
    pub path: String,
}

/// Request headers a caller may send upstream. Everything else is dropped:
/// cookies, `Forwarded`/`X-Forwarded-*`/`X-Real-IP` and other proxy identity,
/// every `cf-*` header, `X-AQ-Dev-Email`, `Origin`. The aq CLI's channel mode
/// (aq-server `aq-human-http` `Client::channel`) sends `Host`, `Accept`,
/// `X-AQ-Request`, `Content-Type` and `Content-Length`; the B7 agent client
/// adds `X-AQ-Agent-API-Version` (and, on B6b, `Authorization`).
const FORWARDED: &[&str] = &[
    "accept",
    "content-type",
    "user-agent",
    "x-aq-request",
    "x-aq-agent-api-version",
    "x-aq-graph-api-version",
    "x-aq-refusal-version",
];

fn check(condition: bool, failure: Failure) -> Result<(), Failure> {
    if condition { Ok(()) } else { Err(failure) }
}

fn access_header(name: &str) -> bool {
    name.starts_with("cf-access-")
}

fn hop_header(name: &str) -> bool {
    matches!(
        name,
        "host"
            | "connection"
            | "keep-alive"
            | "proxy-authenticate"
            | "proxy-authorization"
            | "te"
            | "trailer"
            | "transfer-encoding"
            | "upgrade"
            | "content-length"
    )
}

fn holds(bytes: &[u8], protected: &[&[u8]]) -> bool {
    protected.iter().any(|value| {
        !value.is_empty()
            && bytes.len() >= value.len()
            && bytes.windows(value.len()).any(|part| part == *value)
    })
}

/// The coarse channel path rule (0006 §5.1): agent prefixes are forwarded,
/// node and worker namespaces and everything else are refused. Dot segments
/// and encoded separators are refused, so URL normalization can never move a
/// forwarded path into a refused namespace.
pub fn agent_path(path: &str) -> Result<(), &'static str> {
    if !normal_path(path) {
        return Err("the path is not in normal form");
    }
    if NODE_PREFIXES.iter().any(|p| path.starts_with(p)) {
        return Err("node and worker routes are not reachable from a sandbox");
    }
    if path == "/api/v1/commands" || AGENT_PREFIXES.iter().any(|p| path.starts_with(p)) {
        return Ok(());
    }
    Err("the path is outside the agent channel")
}

fn normal_path(path: &str) -> bool {
    let lower = path.to_ascii_lowercase();
    let dot_segments = path.split('/').any(|s| matches!(s, "." | ".."));
    let encoded = ["%2e", "%2f", "%5c", "%00"]
        .iter()
        .any(|e| lower.contains(e));
    !dot_segments && !encoded
}

fn connection_tokens(headers: &HeaderMap) -> Vec<String> {
    headers
        .get("connection")
        .and_then(|v| v.to_str().ok())
        .unwrap_or_default()
        .split(',')
        .map(|v| v.trim().to_ascii_lowercase())
        .collect()
}

pub(crate) struct Forwarder<'a> {
    pub upstream: &'a Upstream,
    pub rules: &'a Rules,
    pub credentials: &'a dyn Credentials,
}

struct Request {
    method: reqwest::Method,
    url: Url,
    headers: HeaderMap,
    body: Vec<u8>,
}

impl Forwarder<'_> {
    pub async fn forward<S: AsyncRead + Unpin>(
        &self,
        socket: &mut S,
        seen: &mut Seen,
    ) -> Result<Forwarded, Failure> {
        let request = self.read_request(socket, seen).await?;
        // Take credentials after reading the caller's body. If rotation raced an
        // unapplied request, retry once using the new credential.
        let taken = self.credentials.take().ok_or(Failure::Upstream)?;
        let done = request.send(self.upstream, &taken).await?;
        if done.rotation_raced(self.credentials) {
            let current = self.credentials.take().ok_or(Failure::Upstream)?;
            return request.send(self.upstream, &current).await;
        }
        Ok(done)
    }

    async fn read_request<S: AsyncRead + Unpin>(
        &self,
        socket: &mut S,
        seen: &mut Seen,
    ) -> Result<Request, Failure> {
        let mut head = Vec::new();
        while !head.ends_with(b"\r\n\r\n") {
            head.push(socket.read_u8().await.map_err(|_| Failure::Framing)?);
            check(head.len() <= MAX_HEAD, Failure::Framing)?;
        }
        let text = std::str::from_utf8(&head).map_err(|_| Failure::Framing)?;
        let mut lines = text.split("\r\n");
        let (method, target) = request_line(lines.next().unwrap_or_default(), self.rules, seen)?;
        let (headers, length) = request_headers(lines, self.rules)?;
        let mut body = vec![0; length];
        socket
            .read_exact(&mut body)
            .await
            .map_err(|_| Failure::Framing)?;
        let url = self.upstream.request_url(target, self.rules.agent_paths)?;
        Ok(Request {
            method,
            url,
            headers,
            body,
        })
    }
}

fn request_line<'a>(
    line: &'a str,
    rules: &Rules,
    seen: &mut Seen,
) -> Result<(reqwest::Method, &'a str), Failure> {
    let mut first = line.split(' ');
    let method = reqwest::Method::from_bytes(first.next().unwrap_or_default().as_bytes())
        .map_err(|_| Failure::Framing)?;
    let target = first.next().unwrap_or_default();
    check(
        target.starts_with('/')
            && !target.starts_with("//")
            && !target.contains('#')
            && !target.contains('\\')
            && first.next() == Some("HTTP/1.1")
            && first.next().is_none(),
        Failure::Framing,
    )?;
    let path = target.split('?').next().unwrap_or_default();
    *seen = Seen {
        method: method.to_string(),
        path: path.chars().take(256).collect(),
    };
    if rules.agent_paths {
        agent_path(path).map_err(Failure::Refused)?;
    }
    Ok((method, target))
}

fn request_headers<'a>(
    lines: impl Iterator<Item = &'a str>,
    rules: &Rules,
) -> Result<(HeaderMap, usize), Failure> {
    let mut headers = HeaderMap::new();
    let mut names = std::collections::BTreeSet::new();
    let mut length = 0usize;
    let mut host = rules.host.is_none();
    for line in lines.filter(|line| !line.is_empty()) {
        let (name, value) = line.split_once(':').ok_or(Failure::Framing)?;
        let name = HeaderName::from_bytes(name.as_bytes()).map_err(|_| Failure::Framing)?;
        let value = HeaderValue::from_str(value.trim()).map_err(|_| Failure::Framing)?;
        check(names.insert(name.as_str().to_owned()), Failure::Framing)?;
        check(
            !matches!(name.as_str(), "transfer-encoding" | "upgrade" | "expect"),
            Failure::Framing,
        )?;
        if name == "host"
            && let Some(expected) = &rules.host
        {
            host = value.as_bytes() == expected.as_bytes();
        }
        if name == "content-length" {
            let text = value.to_str().map_err(|_| Failure::Framing)?;
            check(
                !text.is_empty() && text.bytes().all(|b| b.is_ascii_digit()),
                Failure::Framing,
            )?;
            length = text.parse().map_err(|_| Failure::Framing)?;
        }
        headers.append(name, value);
    }
    check(host && length <= MAX_BODY, Failure::Framing)?;
    filter_request_headers(&mut headers, rules)?;
    Ok((headers, length))
}

fn filter_request_headers(headers: &mut HeaderMap, rules: &Rules) -> Result<(), Failure> {
    let connection = connection_tokens(headers);
    // Connection-nominated Authorization cannot silently remove the credential.
    check(
        !connection.iter().any(|v| v == "authorization"),
        Failure::Framing,
    )?;
    let removed: Vec<_> = headers
        .keys()
        .filter(|name| {
            let name = name.as_str();
            let allowed = FORWARDED.contains(&name)
                || (!rules.replace_authorization && name == "authorization");
            !allowed || connection.iter().any(|v| v == name)
        })
        .cloned()
        .collect();
    for name in removed {
        headers.remove(name);
    }
    headers.insert(
        reqwest::header::ACCEPT_ENCODING,
        HeaderValue::from_static("identity"),
    );
    Ok(())
}

impl Upstream {
    fn request_url(&self, target: &str, agent_paths: bool) -> Result<Url, Failure> {
        let path = target.split('?').next().unwrap_or_default();
        // Concatenation preserves the fixed authority even for unusual paths.
        let url = Url::parse(&format!(
            "{}{}",
            self.origin.as_str().trim_end_matches('/'),
            target
        ))
        .map_err(|_| Failure::Framing)?;
        check(url.origin() == self.origin.origin(), Failure::Framing)?;
        if agent_paths {
            // Defence in depth: parsing must not have moved the path.
            check(
                url.path() == path,
                Failure::Refused("the path is not in normal form"),
            )?;
        }
        Ok(url)
    }
}

impl Request {
    async fn send(&self, upstream: &Upstream, taken: &Taken) -> Result<Forwarded, Failure> {
        let mut sent = self.headers.clone();
        for (name, value) in &taken.headers {
            sent.insert(name.clone(), value.clone());
        }
        let response = upstream
            .client
            .request(self.method.clone(), self.url.clone())
            .headers(sent)
            .body(self.body.clone())
            .send()
            .await
            .map_err(|_| Failure::Upstream)?;
        let protected: Vec<&[u8]> = taken.protected.iter().map(|v| v.as_slice()).collect();
        let mut done = respond(response, &protected).await?;
        done.version = taken.version;
        done.foreign_version = self
            .headers
            .get("x-aq-agent-api-version")
            .is_some_and(|value| value != "1");
        Ok(done)
    }
}

impl Forwarded {
    fn rotation_raced(&self, credentials: &dyn Credentials) -> bool {
        let unapplied = self.not_applied && self.refused.as_deref() == Some("unauthorized");
        unapplied && self.version.is_some() && credentials.version() != self.version
    }
}

fn forward_response_header(name: &str, connection: &[String]) -> bool {
    let cookie = matches!(name, "set-cookie" | "set-cookie2");
    let connection_header = hop_header(name) || connection.iter().any(|v| v == name);
    !(cookie || connection_header || access_header(name))
}

/// The caller-facing answer: no redirects, identity encoding, no hop-by-hop,
/// Access or cookie headers, nothing that echoes `protected`.
async fn respond(
    mut response: reqwest::Response,
    protected: &[&[u8]],
) -> Result<Forwarded, Failure> {
    check(!response.status().is_redirection(), Failure::Upstream)?;
    check(
        response
            .headers()
            .get_all("content-encoding")
            .iter()
            .all(|value| {
                value
                    .to_str()
                    .is_ok_and(|value| value.trim().eq_ignore_ascii_case("identity"))
            }),
        Failure::Upstream,
    )?;
    let status = response.status().as_u16();
    let mut result = format!("HTTP/1.1 {status} Upstream\r\nConnection: close\r\n").into_bytes();
    let response_connection = connection_tokens(response.headers());
    for (name, value) in response.headers() {
        check(!holds(value.as_bytes(), protected), Failure::Upstream)?;
        // `Set-Cookie` never reaches a sandbox (an Access edge sets `CF_Authorization`).
        if forward_response_header(name.as_str(), &response_connection) {
            result.extend_from_slice(name.as_str().as_bytes());
            result.extend_from_slice(b": ");
            result.extend_from_slice(value.as_bytes());
            result.extend_from_slice(b"\r\n");
        }
    }
    let mut body = Zeroizing::new(Vec::new());
    while let Some(chunk) = response.chunk().await.map_err(|_| Failure::Upstream)? {
        check(
            body.len().saturating_add(chunk.len()) <= MAX_BODY,
            Failure::Upstream,
        )?;
        body.extend_from_slice(&chunk);
    }
    check(!holds(&body, protected), Failure::Upstream)?;
    let parsed = serde_json::from_slice::<serde_json::Value>(&body).unwrap_or_default();
    let refused = (status == 401)
        .then(|| parsed["code"].as_str())
        .flatten()
        .filter(|code| code.len() <= 64)
        .map(str::to_owned);
    result.extend_from_slice(format!("Content-Length: {}\r\n\r\n", body.len()).as_bytes());
    result.extend_from_slice(&body);
    Ok(Forwarded {
        response: result,
        status,
        refused,
        not_applied: parsed["effect"] == "not_applied",
        version: None,
        foreign_version: false,
    })
}
