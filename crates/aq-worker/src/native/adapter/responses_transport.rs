//! Bounded per-attempt Responses boundary. This owner grants no launch authority.
//! A request cap is per response; this is not cumulative token/cost accounting.
use super::*;
use anyhow::Context;
use aq_session_protocol::ModelSettings;
use std::{net::Ipv4Addr, time::Duration};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{TcpListener, TcpStream},
    sync::watch,
    task::JoinHandle,
};
mod items;
mod policy;
pub use policy::{MAX_REQUEST_BYTES, ResponsesScope};
const MAX_HEAD: usize = 16384;
const MAX_STREAM: usize = 16 * 1024 * 1024;
const MAX_REQUESTS: usize = 64;

/// Closed upstream route. Redirects, ambient proxies and caller-provided paths
/// cannot turn a scoped request into arbitrary credential-bearing traffic.
pub struct UpstreamEndpoint(reqwest::Url);
impl UpstreamEndpoint {
    pub fn openai() -> Self {
        Self(reqwest::Url::parse("https://api.openai.com/v1/responses").expect("fixed URL"))
    }
    #[cfg(any(test, feature = "test-support"))]
    pub fn development_loopback(origin: &str) -> Result<Self> {
        let origin = super::codex::development_origin(origin)?;
        Ok(Self(reqwest::Url::parse(&format!(
            "{origin}/v1/responses"
        ))?))
    }
}
struct State {
    scope: ResponsesScope,
    endpoint: UpstreamEndpoint,
    local_key: Zeroizing<Vec<u8>>,
    upstream_key: Zeroizing<Vec<u8>>,
    client: reqwest::Client,
    authority: String,
}
/// Opaque process-local owner. Dropping or stopping it closes active forwarding;
/// it is not a physical process stopped proof or a durable grant receipt.
pub struct ScopedResponsesTransport {
    origin: String,
    binding: LaunchBinding,
    stop: watch::Sender<bool>,
    task: Option<JoinHandle<Result<()>>>,
}
impl ScopedResponsesTransport {
    pub async fn start(
        scope: ResponsesScope,
        endpoint: UpstreamEndpoint,
        local_key: Zeroizing<Vec<u8>>,
        upstream_key: Zeroizing<Vec<u8>>,
    ) -> Result<Self> {
        scope.remaining()?;
        for key in [&local_key, &upstream_key] {
            ensure!(
                !key.is_empty() && key.len() <= 4096 && key.iter().all(|b| b.is_ascii_graphic()),
                "scoped transport key format"
            );
        }
        ensure!(
            !key_matches(&local_key, &upstream_key),
            "local scoped capability must differ from upstream credential"
        );
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).await?;
        let origin = format!("http://127.0.0.1:{}", listener.local_addr()?.port());
        let client = reqwest::Client::builder()
            .no_proxy()
            .redirect(reqwest::redirect::Policy::none())
            .connect_timeout(Duration::from_secs(5))
            .pool_max_idle_per_host(0)
            .build()?;
        let binding = scope.binding().clone();
        let state = State {
            scope,
            endpoint,
            local_key,
            upstream_key,
            client,
            authority: origin
                .strip_prefix("http://")
                .expect("fixed local origin")
                .into(),
        };
        let (stop, mut stopped) = watch::channel(false);
        let task = tokio::spawn(async move {
            let serving = async {
                for _ in 0..MAX_REQUESTS {
                    let remaining = state.scope.remaining()?;
                    let (mut stream, peer) =
                        tokio::time::timeout(remaining, listener.accept()).await??;
                    ensure!(peer.ip().is_loopback(), "nonlocal scoped peer");
                    let work = serve(&state, &mut stream);
                    let _result = tokio::time::timeout(
                        state.scope.remaining()?.min(Duration::from_secs(120)),
                        work,
                    )
                    .await;
                    // Closing refuses malformed input and aborts an incomplete stream.
                    // Never append a second HTTP status after a streamed response.
                }
                anyhow::bail!("scoped request count exhausted")
            };
            tokio::select! { result=serving=>result, _=stopped.changed()=>Ok(()) }
        });
        Ok(Self {
            origin,
            binding,
            stop,
            task: Some(task),
        })
    }
    pub fn origin(&self) -> &str {
        &self.origin
    }
    pub fn binding(&self) -> &LaunchBinding {
        &self.binding
    }
    pub async fn stop(mut self) -> Result<()> {
        let _ = self.stop.send(true);
        if let Some(task) = self.task.take() {
            task.await??;
        }
        Ok(())
    }
}
impl Drop for ScopedResponsesTransport {
    fn drop(&mut self) {
        let _ = self.stop.send(true);
        if let Some(task) = self.task.take() {
            task.abort();
        }
    }
}
fn key_matches(expected: &[u8], actual: &[u8]) -> bool {
    if expected.len() != actual.len() {
        return false;
    }
    expected
        .iter()
        .zip(actual)
        .fold(0u8, |different, (a, b)| different | (a ^ b))
        == 0
}
async fn serve(state: &State, stream: &mut TcpStream) -> Result<()> {
    let mut head = Zeroizing::new(Vec::new());
    loop {
        let byte = tokio::time::timeout(Duration::from_secs(5), stream.read_u8()).await??;
        head.push(byte);
        ensure!(head.len() <= MAX_HEAD, "scoped HTTP header bound");
        if head.ends_with(b"\r\n\r\n") {
            break;
        }
    }
    let text = std::str::from_utf8(&head)?;
    let mut lines = text.split("\r\n");
    ensure!(
        lines.next() == Some("POST /v1/responses HTTP/1.1"),
        "unsupported scoped route/method"
    );
    let mut names = std::collections::BTreeSet::new();
    let mut length = None;
    let mut authorized = false;
    let mut content_type = false;
    let mut host = false;
    for line in lines.filter(|line| !line.is_empty()) {
        let (name, value) = line.split_once(':').context("invalid scoped HTTP header")?;
        ensure!(
            !name.is_empty() && name.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-'),
            "scoped header name"
        );
        ensure!(
            value
                .bytes()
                .all(|b| b.is_ascii_graphic() || b == b' ' || b == b'\t'),
            "scoped header value"
        );
        let name = name.to_ascii_lowercase();
        let value = value.trim();
        ensure!(names.insert(name.clone()), "duplicate scoped HTTP header");
        ensure!(
            !matches!(
                name.as_str(),
                "transfer-encoding" | "content-encoding" | "upgrade" | "expect"
            ),
            "unsupported scoped HTTP framing"
        );
        match name.as_str() {
            "content-length" => {
                ensure!(
                    !value.is_empty() && value.bytes().all(|b| b.is_ascii_digit()),
                    "scoped content length"
                );
                length = Some(value.parse::<usize>()?);
            }
            "authorization" => {
                authorized = value
                    .strip_prefix("Bearer ")
                    .is_some_and(|key| key_matches(&state.local_key, key.as_bytes()))
            }
            "content-type" => content_type = value == "application/json",
            "host" => host = value == state.authority,
            _ => {}
        }
    }
    ensure!(
        authorized && content_type && host,
        "scoped HTTP authorization/type"
    );
    let length = length.context("scoped content length absent")?;
    ensure!(
        length > 0 && length <= MAX_REQUEST_BYTES,
        "scoped HTTP body bound"
    );
    let mut incoming = Zeroizing::new(vec![0; length]);
    tokio::time::timeout(Duration::from_secs(10), stream.read_exact(&mut incoming)).await??;
    let body = state.scope.prepare(&incoming)?;
    state.scope.remaining()?;
    let mut upstream = state
        .client
        .post(state.endpoint.0.clone())
        .bearer_auth(std::str::from_utf8(&state.upstream_key)?)
        .header("content-type", "application/json")
        .header("accept", "text/event-stream")
        .body(body.to_vec())
        .send()
        .await?;
    ensure!(
        upstream.status() == reqwest::StatusCode::OK
            && upstream
                .headers()
                .get("content-type")
                .and_then(|v| v.to_str().ok())
                .is_some_and(|v| v.split(';').next() == Some("text/event-stream")),
        "unsupported upstream Responses status/type"
    );
    stream.write_all(b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nTransfer-Encoding: chunked\r\nConnection: close\r\nCache-Control: no-store\r\n\r\n").await?;
    let mut bytes = 0usize;
    while let Some(chunk) = upstream.chunk().await? {
        state.scope.remaining()?;
        bytes = bytes
            .checked_add(chunk.len())
            .context("scoped response bytes overflow")?;
        ensure!(bytes <= MAX_STREAM, "scoped Responses stream bound");
        for part in chunk.chunks(16384) {
            if part.is_empty() {
                continue;
            }
            stream
                .write_all(format!("{:x}\r\n", part.len()).as_bytes())
                .await?;
            stream.write_all(part).await?;
            stream.write_all(b"\r\n").await?;
        }
    }
    stream.write_all(b"0\r\n\r\n").await?;
    stream.shutdown().await?;
    Ok(())
}
#[cfg(test)]
mod tests;
