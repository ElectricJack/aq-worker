//! Fixed-upstream, bounded AQ forwarding. The agent supplies AQ authorization;
//! only this runner-owned listener supplies Cloudflare Access authorization.
use super::private::RunnerFiles;
use anyhow::{Result, ensure};
use reqwest::{
    Url,
    header::{HeaderMap, HeaderName, HeaderValue},
};
use serde::Deserialize;
use std::{path::Path, sync::Arc, time::Duration};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{TcpListener, TcpStream},
    sync::{Semaphore, watch},
    task::{JoinHandle, JoinSet},
};
use zeroize::Zeroize;
const MAX_HEAD: usize = 16384;
const MAX_BODY: usize = 4 * 1024 * 1024;
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct AccessToken {
    client_id: String,
    client_secret: String,
}
impl Drop for AccessToken {
    fn drop(&mut self) {
        self.client_id.zeroize();
        self.client_secret.zeroize();
    }
}
struct State {
    origin: Url,
    token: AccessToken,
    client: reqwest::Client,
    authority: String,
}
pub struct AqApiProxy {
    origin: String,
    stop: watch::Sender<bool>,
    task: Option<JoinHandle<()>>,
}
impl AqApiProxy {
    pub async fn start(server: &str, access_file: &Path) -> Result<Self> {
        let url = Url::parse(server)?;
        ensure!(
            url.scheme() == "https"
                && url.host_str().is_some_and(
                    |host| !host.starts_with('[') && host.parse::<std::net::IpAddr>().is_err()
                ),
            "AQ proxy requires HTTPS machine hostname"
        );
        Self::start_inner(url, access_file).await
    }
    #[cfg(any(test, feature = "test-support", feature = "backend-conformance"))]
    pub async fn development_loopback(server: &str, access_file: &Path) -> Result<Self> {
        let url = Url::parse(server)?;
        ensure!(
            url.scheme() == "http" && url.host_str() == Some("127.0.0.1") && url.port().is_some(),
            "fixture requires numeric loopback"
        );
        Self::start_inner(url, access_file).await
    }
    async fn start_inner(origin: Url, access_file: &Path) -> Result<Self> {
        ensure!(
            origin.username().is_empty()
                && origin.password().is_none()
                && origin.path() == "/"
                && origin.query().is_none()
                && origin.fragment().is_none(),
            "AQ proxy requires fixed origin"
        );
        let bytes = RunnerFiles::read_secret(access_file)?;
        let token: AccessToken = serde_json::from_slice(&bytes)
            .map_err(|_| anyhow::anyhow!("invalid runner Access file"))?;
        for value in [&token.client_id, &token.client_secret] {
            ensure!(
                !value.is_empty()
                    && value.len() <= 4096
                    && value.bytes().all(|b| b.is_ascii_graphic()),
                "invalid runner Access value"
            );
        }
        let client = reqwest::Client::builder()
            .no_proxy()
            .redirect(reqwest::redirect::Policy::none())
            .connect_timeout(Duration::from_secs(5))
            .timeout(Duration::from_secs(25))
            .build()?;
        let listener = TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0)).await?;
        let local = listener.local_addr()?;
        let state = Arc::new(State {
            origin,
            token,
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
                            let result = tokio::time::timeout(Duration::from_secs(30), forward(&state, &mut stream)).await;
                            match result {
                                Ok(Ok(bytes)) => { let _ = tokio::time::timeout(Duration::from_secs(5), stream.write_all(&bytes)).await; }
                                _ => { let _ = tokio::time::timeout(Duration::from_secs(1), stream.write_all(b"HTTP/1.1 502 Bad Gateway\r\nContent-Length: 0\r\nConnection: close\r\n\r\n")).await; }
                            }
                        });
                    }
                }
            }
            // Dropping the JoinSet cancels every connection before the owner exits.
        });
        Ok(Self {
            origin: format!("http://{local}"),
            stop,
            task: Some(task),
        })
    }
    pub fn origin(&self) -> &str {
        &self.origin
    }
    pub async fn stop(mut self) -> Result<()> {
        let _ = self.stop.send(true);
        if let Some(task) = self.task.take() {
            task.await?;
        }
        Ok(())
    }
}
impl Drop for AqApiProxy {
    fn drop(&mut self) {
        let _ = self.stop.send(true);
        if let Some(task) = self.task.take() {
            task.abort();
        }
    }
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
fn contains_token(state: &State, bytes: &[u8]) -> bool {
    [&state.token.client_id, &state.token.client_secret]
        .iter()
        .any(|value| {
            bytes
                .windows(value.len())
                .any(|part| part == value.as_bytes())
        })
}
async fn forward(state: &State, socket: &mut TcpStream) -> Result<Vec<u8>> {
    let mut head = Vec::new();
    while !head.ends_with(b"\r\n\r\n") {
        head.push(socket.read_u8().await?);
        ensure!(head.len() <= MAX_HEAD, "AQ request header bound");
    }
    let text = std::str::from_utf8(&head)?;
    let mut lines = text.split("\r\n");
    let mut first = lines.next().unwrap_or_default().split(' ');
    let method = reqwest::Method::from_bytes(first.next().unwrap_or_default().as_bytes())?;
    let target = first.next().unwrap_or_default();
    ensure!(
        target.starts_with('/')
            && !target.starts_with("//")
            && !target.contains('#')
            && !target.contains('\\')
            && first.next() == Some("HTTP/1.1")
            && first.next().is_none(),
        "AQ request target"
    );
    let mut headers = HeaderMap::new();
    let mut names = std::collections::BTreeSet::new();
    let mut length = 0usize;
    let mut host = false;
    for line in lines.filter(|line| !line.is_empty()) {
        let (name, value) = line
            .split_once(':')
            .ok_or_else(|| anyhow::anyhow!("AQ request header"))?;
        let name = HeaderName::from_bytes(name.as_bytes())?;
        let value = HeaderValue::from_str(value.trim())?;
        ensure!(
            names.insert(name.as_str().to_owned()),
            "duplicate AQ request header"
        );
        ensure!(
            !matches!(name.as_str(), "transfer-encoding" | "upgrade" | "expect"),
            "unsupported AQ framing"
        );
        if name == "host" {
            host = value.as_bytes() == state.authority.as_bytes();
        }
        if name == "content-length" {
            let text = value.to_str()?;
            ensure!(
                !text.is_empty() && text.bytes().all(|b| b.is_ascii_digit()),
                "AQ request length"
            );
            length = text.parse()?;
        }
        headers.append(name, value);
    }
    ensure!(host && length <= MAX_BODY, "AQ authority/body bound");
    let connection: Vec<String> = headers
        .get("connection")
        .and_then(|v| v.to_str().ok())
        .unwrap_or_default()
        .split(',')
        .map(|v| v.trim().to_ascii_lowercase())
        .collect();
    // Connection-nominated Authorization cannot silently remove the agent's credential.
    ensure!(
        !connection.iter().any(|v| v == "authorization"),
        "AQ authorization nominated as hop header"
    );
    let removed: Vec<_> = headers
        .keys()
        .filter(|name| {
            access_header(name.as_str())
                || hop_header(name.as_str())
                || connection.iter().any(|v| v == name.as_str())
        })
        .cloned()
        .collect();
    for name in removed {
        headers.remove(name);
    }
    for (name, value) in [
        ("cf-access-client-id", &state.token.client_id),
        ("cf-access-client-secret", &state.token.client_secret),
    ] {
        let mut value = HeaderValue::from_str(value)?;
        value.set_sensitive(true);
        headers.insert(HeaderName::from_static(name), value);
    }
    headers.insert(
        reqwest::header::ACCEPT_ENCODING,
        HeaderValue::from_static("identity"),
    );
    let mut body = vec![0; length];
    socket.read_exact(&mut body).await?;
    // Concatenation preserves the fixed authority even for unusual paths.
    let url = Url::parse(&format!(
        "{}{}",
        state.origin.as_str().trim_end_matches('/'),
        target
    ))?;
    ensure!(
        url.origin() == state.origin.origin(),
        "AQ upstream authority changed"
    );
    let mut response = state
        .client
        .request(method, url)
        .headers(headers)
        .body(body)
        .send()
        .await?;
    ensure!(
        !response.status().is_redirection(),
        "AQ upstream redirect refused"
    );
    ensure!(
        response
            .headers()
            .get_all("content-encoding")
            .iter()
            .all(|value| value
                .to_str()
                .is_ok_and(|value| value.trim().eq_ignore_ascii_case("identity"))),
        "encoded AQ upstream response refused"
    );
    let status = response.status().as_u16();
    let mut result = format!("HTTP/1.1 {status} Upstream\r\nConnection: close\r\n").into_bytes();
    let response_connection: Vec<String> = response
        .headers()
        .get("connection")
        .and_then(|v| v.to_str().ok())
        .unwrap_or_default()
        .split(',')
        .map(|v| v.trim().to_ascii_lowercase())
        .collect();
    for (name, value) in response.headers() {
        ensure!(
            !contains_token(state, value.as_bytes()),
            "AQ upstream returned protected material"
        );
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
    while let Some(chunk) = response.chunk().await? {
        ensure!(
            body.len().saturating_add(chunk.len()) <= MAX_BODY,
            "AQ upstream body bound"
        );
        body.extend_from_slice(&chunk);
    }
    ensure!(
        !contains_token(state, &body),
        "AQ upstream returned protected material"
    );
    result.extend_from_slice(format!("Content-Length: {}\r\n\r\n", body.len()).as_bytes());
    result.extend_from_slice(&body);
    Ok(result)
}
