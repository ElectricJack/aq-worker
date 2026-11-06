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
    /// Either of the above.
    Either,
}

pub(crate) struct Upstream {
    pub origin: Url,
    pub client: reqwest::Client,
}
impl Upstream {
    pub fn new(origin: &str, allowed: Origins, timeout: Duration) -> anyhow::Result<Self> {
        let url = Url::parse(origin)?;
        let hostname = url.scheme() == "https"
            && url.host_str().is_some_and(|host| {
                !host.starts_with('[') && host.parse::<std::net::IpAddr>().is_err()
            });
        let loopback =
            url.scheme() == "http" && url.host_str() == Some("127.0.0.1") && url.port().is_some();
        anyhow::ensure!(
            match allowed {
                Origins::MachineHostname => hostname,
                Origins::NumericLoopback => loopback,
                Origins::Either => hostname || loopback,
            },
            "AQ upstream must be an HTTPS machine hostname or a numeric loopback fixture"
        );
        anyhow::ensure!(
            url.username().is_empty()
                && url.password().is_none()
                && url.path() == "/"
                && url.query().is_none()
                && url.fragment().is_none(),
            "AQ proxy requires fixed origin"
        );
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
    /// The upstream answered `session_revoked`: the attempt's credential is gone.
    pub revoked: bool,
}

/// Method and path (no query) of a request, for logs. Never headers or bodies.
#[derive(Clone, Default)]
pub(crate) struct Seen {
    pub method: String,
    pub path: String,
}

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
    let lower = path.to_ascii_lowercase();
    if path.split('/').any(|s| s == "." || s == "..")
        || ["%2e", "%2f", "%5c", "%00"]
            .iter()
            .any(|e| lower.contains(e))
    {
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

fn connection_tokens(headers: &HeaderMap) -> Vec<String> {
    headers
        .get("connection")
        .and_then(|v| v.to_str().ok())
        .unwrap_or_default()
        .split(',')
        .map(|v| v.trim().to_ascii_lowercase())
        .collect()
}

pub(crate) async fn forward<S: AsyncRead + Unpin>(
    upstream: &Upstream,
    rules: &Rules,
    inject: &[(HeaderName, HeaderValue)],
    protected: &[&[u8]],
    socket: &mut S,
    seen: &mut Seen,
) -> Result<Forwarded, Failure> {
    let mut head = Vec::new();
    while !head.ends_with(b"\r\n\r\n") {
        head.push(socket.read_u8().await.map_err(|_| Failure::Framing)?);
        check(head.len() <= MAX_HEAD, Failure::Framing)?;
    }
    let text = std::str::from_utf8(&head).map_err(|_| Failure::Framing)?;
    let mut lines = text.split("\r\n");
    let mut first = lines.next().unwrap_or_default().split(' ');
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
    let connection = connection_tokens(&headers);
    // Connection-nominated Authorization cannot silently remove the credential.
    check(
        !connection.iter().any(|v| v == "authorization"),
        Failure::Framing,
    )?;
    let removed: Vec<_> = headers
        .keys()
        .filter(|name| {
            access_header(name.as_str())
                || hop_header(name.as_str())
                || (rules.replace_authorization && name.as_str() == "authorization")
                || connection.iter().any(|v| v == name.as_str())
        })
        .cloned()
        .collect();
    for name in removed {
        headers.remove(name);
    }
    for (name, value) in inject {
        headers.insert(name.clone(), value.clone());
    }
    headers.insert(
        reqwest::header::ACCEPT_ENCODING,
        HeaderValue::from_static("identity"),
    );
    let mut body = vec![0; length];
    socket
        .read_exact(&mut body)
        .await
        .map_err(|_| Failure::Framing)?;
    // Concatenation preserves the fixed authority even for unusual paths.
    let url = Url::parse(&format!(
        "{}{}",
        upstream.origin.as_str().trim_end_matches('/'),
        target
    ))
    .map_err(|_| Failure::Framing)?;
    check(url.origin() == upstream.origin.origin(), Failure::Framing)?;
    if rules.agent_paths {
        // Defence in depth: parsing must not have moved the path.
        check(
            url.path() == path,
            Failure::Refused("the path is not in normal form"),
        )?;
    }
    let mut response = upstream
        .client
        .request(method, url)
        .headers(headers)
        .body(body)
        .send()
        .await
        .map_err(|_| Failure::Upstream)?;
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
        if !access_header(name.as_str())
            && !hop_header(name.as_str())
            && !response_connection.iter().any(|v| v == name.as_str())
        {
            result.extend_from_slice(name.as_str().as_bytes());
            result.extend_from_slice(b": ");
            result.extend_from_slice(value.as_bytes());
            result.extend_from_slice(b"\r\n");
        }
    }
    let mut body = zeroize::Zeroizing::new(Vec::new());
    while let Some(chunk) = response.chunk().await.map_err(|_| Failure::Upstream)? {
        check(
            body.len().saturating_add(chunk.len()) <= MAX_BODY,
            Failure::Upstream,
        )?;
        body.extend_from_slice(&chunk);
    }
    check(!holds(&body, protected), Failure::Upstream)?;
    let revoked = status == 401
        && serde_json::from_slice::<serde_json::Value>(&body)
            .is_ok_and(|v| v["code"] == "session_revoked");
    result.extend_from_slice(format!("Content-Length: {}\r\n\r\n", body.len()).as_bytes());
    result.extend_from_slice(&body);
    Ok(Forwarded {
        response: result,
        status,
        revoked,
    })
}
