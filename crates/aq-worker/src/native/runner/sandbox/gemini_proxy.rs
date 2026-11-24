//! Runner-held Gemini API forwarding (SBX7). The agent sees only a derived
//! per-attempt capability; this listener swaps it for the provider key and
//! forwards only model-generation paths to the fixed upstream origin.
mod policy;
mod request;
mod response;
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
        let key = read_key(key_file)?;
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
        let (stop, stopped) = watch::channel(false);
        let task = tokio::spawn(serve(listener, state, stopped));
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

fn read_key(key_file: &Path) -> Result<Zeroizing<String>> {
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
    Ok(Zeroizing::new(key.to_owned()))
}
async fn serve(listener: TcpListener, state: Arc<State>, mut stopped: watch::Receiver<bool>) {
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
}
async fn forward(state: &State, socket: &mut TcpStream) -> Result<Vec<u8>> {
    let (target, headers, length) = request::read(state, socket).await?;
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
    let response = state
        .client
        .post(url)
        .headers(headers)
        .body(body)
        .send()
        .await?;
    response::encode(state, response).await
}

#[cfg(test)]
mod tests;
