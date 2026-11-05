//! Runner-held Gemini API forwarding (SBX7). The agent sees only a derived
//! per-attempt capability; this listener swaps it for the provider key and
//! forwards only model-generation paths to the fixed upstream origin.
use super::private::RunnerFiles;
use crate::native::adapter::gemini::{CAPABILITY_PREFIX, is_capability};
use anyhow::{Result, ensure};
use reqwest::{
    Url,
    header::{HeaderMap, HeaderName, HeaderValue},
};
use std::{fmt::Write as _, path::Path, sync::Arc, time::Duration};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{TcpListener, TcpStream},
    sync::{Semaphore, watch},
    task::{JoinHandle, JoinSet},
};
use zeroize::Zeroizing;

pub const GEMINI_UPSTREAM: &str = "https://generativelanguage.googleapis.com";
const MAX_HEAD: usize = 16384;
const MAX_BODY: usize = 4 * 1024 * 1024;
const MAX_RESPONSE: usize = 16 * 1024 * 1024;
/// One model turn streams for minutes; the whole exchange stays bounded.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(300);
const KEY_HEADER: &str = "x-goog-api-key";
const METHODS: [&str; 3] = ["streamGenerateContent", "generateContent", "countTokens"];

struct State {
    origin: Url,
    key: Zeroizing<String>,
    capability: Zeroizing<String>,
    client: reqwest::Client,
    authority: String,
}

pub struct GeminiApiProxy {
    origin: String,
    capability: Zeroizing<String>,
    stop: watch::Sender<bool>,
    task: Option<JoinHandle<()>>,
}

impl GeminiApiProxy {
    pub async fn start(key_file: &Path) -> Result<Self> {
        Self::start_inner(Url::parse(GEMINI_UPSTREAM)?, key_file).await
    }
    #[cfg(any(test, feature = "test-support", feature = "backend-conformance"))]
    pub async fn development_loopback(upstream: &str, key_file: &Path) -> Result<Self> {
        let url = Url::parse(upstream)?;
        ensure!(
            url.scheme() == "http" && url.host_str() == Some("127.0.0.1") && url.port().is_some(),
            "fixture requires numeric loopback"
        );
        Self::start_inner(url, key_file).await
    }
    async fn start_inner(origin: Url, key_file: &Path) -> Result<Self> {
        ensure!(
            origin.username().is_empty()
                && origin.password().is_none()
                && origin.path() == "/"
                && origin.query().is_none()
                && origin.fragment().is_none(),
            "Gemini proxy requires fixed origin"
        );
        let bytes = RunnerFiles::read_secret(key_file)?;
        let key = std::str::from_utf8(&bytes)
            .map_err(|_| anyhow::anyhow!("invalid Gemini key file"))?
            .trim();
        ensure!(
            !key.is_empty()
                && key.len() <= 512
                && key
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_'),
            "invalid Gemini key file"
        );
        let key = Zeroizing::new(key.to_owned());
        let mut capability = Zeroizing::new(String::from(CAPABILITY_PREFIX));
        for byte in rand::random::<[u8; 32]>() {
            write!(capability, "{byte:02x}")?;
        }
        debug_assert!(is_capability(&capability));
        let client = reqwest::Client::builder()
            .no_proxy()
            .redirect(reqwest::redirect::Policy::none())
            .connect_timeout(Duration::from_secs(10))
            .timeout(REQUEST_TIMEOUT)
            .build()?;
        let listener = TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0)).await?;
        let local = listener.local_addr()?;
        let state = Arc::new(State {
            origin,
            key,
            capability: capability.clone(),
            client,
            authority: local.to_string(),
        });
        let (stop, mut stopped) = watch::channel(false);
        let task = tokio::spawn(async move {
            let slots = Arc::new(Semaphore::new(16));
            let mut requests = JoinSet::new();
            loop {
                tokio::select! {
                    biased;
                    _ = stopped.changed() => break,
                    Some(_) = requests.join_next(), if !requests.is_empty() => {},
                    incoming = listener.accept() => {
                        let Ok((mut stream, peer)) = incoming else { break };
                        if !peer.ip().is_loopback() { continue; }
                        let Ok(slot) = slots.clone().try_acquire_owned() else { continue };
                        let state = state.clone();
                        requests.spawn(async move {
                            let _slot = slot;
                            let result = tokio::time::timeout(REQUEST_TIMEOUT + Duration::from_secs(5), forward(&state, &mut stream)).await;
                            match result {
                                Ok(Ok(bytes)) => { let _ = tokio::time::timeout(Duration::from_secs(5), stream.write_all(&bytes)).await; }
                                _ => { let _ = tokio::time::timeout(Duration::from_secs(1), stream.write_all(b"HTTP/1.1 502 Bad Gateway\r\nContent-Length: 0\r\nConnection: close\r\n\r\n")).await; }
                            }
                        });
                    }
                }
            }
        });
        Ok(Self {
            origin: format!("http://{local}"),
            capability,
            stop,
            task: Some(task),
        })
    }
    pub fn origin(&self) -> &str {
        &self.origin
    }
    /// The per-attempt value the agent presents as `GEMINI_API_KEY`.
    pub fn local_capability(&self) -> &str {
        &self.capability
    }
    pub async fn stop(mut self) -> Result<()> {
        let _ = self.stop.send(true);
        if let Some(task) = self.task.take() {
            task.await?;
        }
        Ok(())
    }
}

impl Drop for GeminiApiProxy {
    fn drop(&mut self) {
        let _ = self.stop.send(true);
        if let Some(task) = self.task.take() {
            task.abort();
        }
    }
}

/// `/v1beta/models/<model>:<method>` with an optional `?alt=sse`. A `key=`
/// query (the other Gemini credential channel) is never forwarded.
fn allowed_target(target: &str) -> bool {
    let (path, query) = target.split_once('?').unwrap_or((target, ""));
    let Some(rest) = path.strip_prefix("/v1beta/models/") else {
        return false;
    };
    let Some((model, method)) = rest.split_once(':') else {
        return false;
    };
    !model.is_empty()
        && model.len() <= 128
        && model
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'.' | b'_'))
        && !model.starts_with('.')
        && METHODS.contains(&method)
        && (query.is_empty() || query == "alt=sse")
}

fn dropped_header(name: &str) -> bool {
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
            | "authorization"
            | "accept-encoding"
            | KEY_HEADER
            | "x-goog-user-project"
            | "x-gemini-api-privileged-user-id"
    )
}

fn contains(haystack: &[u8], needle: &str) -> bool {
    haystack
        .windows(needle.len())
        .any(|part| part == needle.as_bytes())
}

async fn forward(state: &State, socket: &mut TcpStream) -> Result<Vec<u8>> {
    let mut head = Zeroizing::new(Vec::new());
    while !head.ends_with(b"\r\n\r\n") {
        head.push(socket.read_u8().await?);
        ensure!(head.len() <= MAX_HEAD, "Gemini request header bound");
    }
    let text = std::str::from_utf8(&head)?;
    let mut lines = text.split("\r\n");
    let mut first = lines.next().unwrap_or_default().split(' ');
    ensure!(first.next() == Some("POST"), "Gemini request method");
    let target = first.next().unwrap_or_default().to_owned();
    ensure!(
        allowed_target(&target) && first.next() == Some("HTTP/1.1") && first.next().is_none(),
        "Gemini request target"
    );
    let mut headers = HeaderMap::new();
    let mut names = std::collections::BTreeSet::new();
    let mut length = None;
    let mut host = false;
    let mut authorized = false;
    for line in lines.filter(|line| !line.is_empty()) {
        let (name, value) = line
            .split_once(':')
            .ok_or_else(|| anyhow::anyhow!("Gemini request header"))?;
        let name = HeaderName::from_bytes(name.as_bytes())?;
        let value = HeaderValue::from_str(value.trim())?;
        ensure!(
            names.insert(name.as_str().to_owned()),
            "duplicate Gemini request header"
        );
        ensure!(
            !matches!(name.as_str(), "transfer-encoding" | "upgrade" | "expect"),
            "unsupported Gemini framing"
        );
        match name.as_str() {
            "host" => host = value.as_bytes() == state.authority.as_bytes(),
            KEY_HEADER => authorized = value.as_bytes() == state.capability.as_bytes(),
            "content-length" => {
                let text = value.to_str()?;
                ensure!(
                    !text.is_empty() && text.bytes().all(|b| b.is_ascii_digit()),
                    "Gemini request length"
                );
                length = Some(text.parse::<usize>()?);
            }
            _ => {}
        }
        if !dropped_header(name.as_str()) {
            headers.append(name, value);
        }
    }
    // Refused before any upstream connection: no capability, no forwarding.
    ensure!(authorized, "Gemini capability refused");
    let length = length.ok_or_else(|| anyhow::anyhow!("Gemini request length"))?;
    ensure!(host && length <= MAX_BODY, "Gemini authority/body bound");
    let connection: Vec<String> = headers
        .get("connection")
        .and_then(|v| v.to_str().ok())
        .unwrap_or_default()
        .split(',')
        .map(|v| v.trim().to_ascii_lowercase())
        .collect();
    for name in connection.iter().filter(|v| !v.is_empty()) {
        headers.remove(name.as_str());
    }
    let mut key = HeaderValue::from_str(&state.key)?;
    key.set_sensitive(true);
    headers.insert(HeaderName::from_static(KEY_HEADER), key);
    headers.insert(
        reqwest::header::ACCEPT_ENCODING,
        HeaderValue::from_static("identity"),
    );
    let mut body = vec![0; length];
    socket.read_exact(&mut body).await?;
    let url = Url::parse(&format!(
        "{}{}",
        state.origin.as_str().trim_end_matches('/'),
        target
    ))?;
    ensure!(
        url.origin() == state.origin.origin(),
        "Gemini upstream authority changed"
    );
    let mut response = state
        .client
        .post(url)
        .headers(headers)
        .body(body)
        .send()
        .await?;
    ensure!(
        !response.status().is_redirection(),
        "Gemini upstream redirect refused"
    );
    ensure!(
        response
            .headers()
            .get_all("content-encoding")
            .iter()
            .all(|value| value
                .to_str()
                .is_ok_and(|value| value.trim().eq_ignore_ascii_case("identity"))),
        "encoded Gemini upstream response refused"
    );
    let status = response.status().as_u16();
    let mut result = format!("HTTP/1.1 {status} Upstream\r\nConnection: close\r\n").into_bytes();
    for (name, value) in response.headers() {
        ensure!(
            !contains(value.as_bytes(), &state.key),
            "Gemini upstream returned protected material"
        );
        if !dropped_header(name.as_str()) && !name.as_str().starts_with("set-cookie") {
            result.extend_from_slice(name.as_str().as_bytes());
            result.extend_from_slice(b": ");
            result.extend_from_slice(value.as_bytes());
            result.extend_from_slice(b"\r\n");
        }
    }
    let mut body = Zeroizing::new(Vec::new());
    while let Some(chunk) = response.chunk().await? {
        ensure!(
            body.len().saturating_add(chunk.len()) <= MAX_RESPONSE,
            "Gemini upstream body bound"
        );
        body.extend_from_slice(&chunk);
    }
    ensure!(
        !contains(&body, &state.key),
        "Gemini upstream returned protected material"
    );
    result.extend_from_slice(format!("Content-Length: {}\r\n\r\n", body.len()).as_bytes());
    result.extend_from_slice(&body);
    Ok(result)
}

#[cfg(test)]
mod tests;
