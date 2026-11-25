//! Bounded Docker-compatible Engine requests for a node-owned Unix socket.
//! Minimal Docker Engine API transport. The backend owns timeouts; transports
//! only move bytes and never put request bodies in errors.

mod info;
mod socket;
mod workspace_path;
pub use workspace_path::WorkspaceBindPaths;
mod storage;
pub use info::*;
pub use socket::*;
pub use storage::*;

use bytes::Bytes;
use http_body_util::{BodyExt, Full, Limited};
use hyper_util::rt::TokioIo;
use std::{fmt, future::Future, path::PathBuf, time::Duration};
use zeroize::Zeroizing;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DockerMethod {
    Get,
    Post,
    Put,
    Delete,
}

pub struct DockerRequest {
    pub method: DockerMethod,
    /// Absolute path with API version prefix and query, e.g. `/v1.44/info`.
    pub path: String,
    pub content_type: Option<&'static str>,
    /// May carry private material (archive upload), so it is zeroized on drop.
    pub body: Zeroizing<Vec<u8>>,
    /// Responses larger than this fail with `TransportErrorKind::TooLarge`.
    pub max_response_bytes: usize,
}
impl fmt::Debug for DockerRequest {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("DockerRequest")
            .field("method", &self.method)
            .field("path", &self.path)
            .field("body_len", &self.body.len())
            .finish()
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DockerResponse {
    pub status: u16,
    pub body: Vec<u8>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TransportErrorKind {
    Io,
    TooLarge,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TransportError {
    pub kind: TransportErrorKind,
    pub message: String,
}
impl TransportError {
    pub fn io(message: impl fmt::Display) -> Self {
        Self {
            kind: TransportErrorKind::Io,
            message: message.to_string(),
        }
    }
}

pub trait DockerTransport: Send + Sync + 'static {
    fn send(
        &self,
        request: DockerRequest,
    ) -> impl Future<Output = Result<DockerResponse, TransportError>> + Send;
}

#[derive(Debug)]
pub enum CallError {
    Timeout,
    Transport,
    TooLarge,
}

/// Apply a deadline independently of the response byte cap on the request.
pub async fn call(
    transport: &impl DockerTransport,
    request: DockerRequest,
    timeout: Duration,
) -> Result<DockerResponse, CallError> {
    match tokio::time::timeout(timeout, transport.send(request)).await {
        Err(_) => Err(CallError::Timeout),
        Ok(Err(error)) if error.kind == TransportErrorKind::TooLarge => Err(CallError::TooLarge),
        Ok(Err(_)) => Err(CallError::Transport),
        Ok(Ok(response)) => Ok(response),
    }
}

/// Uses the daemon API for the stopped container or the volume it left behind.
pub fn workspace_archive_path(api: &str, id: &str, work_volume: &str, container: bool) -> String {
    if container {
        format!("{api}/containers/{id}/archive?path=%2Faq%2Fwork")
    } else {
        format!("/v4.0.0/libpod/volumes/{work_volume}/export")
    }
}

/// The caller verifies sandbox identity before choosing a container or volume path.
pub async fn archive(
    transport: &impl DockerTransport,
    path: String,
    max_response_bytes: usize,
    timeout: Duration,
) -> Result<Option<Vec<u8>>, CallError> {
    let request = DockerRequest {
        method: DockerMethod::Get,
        path,
        content_type: None,
        body: Zeroizing::default(),
        max_response_bytes,
    };
    let response = call(transport, request, timeout.max(Duration::from_secs(60))).await?;
    match response.status {
        200 => Ok(Some(response.body)),
        404 => Ok(None),
        _ => Err(CallError::Transport),
    }
}

/// One HTTP/1.1 connection per request over the dedicated daemon's Unix socket.
#[derive(Debug, Clone)]
pub struct UnixSocketTransport {
    socket: PathBuf,
}
impl UnixSocketTransport {
    pub fn new(socket: PathBuf) -> Self {
        Self { socket }
    }
}

impl DockerTransport for UnixSocketTransport {
    async fn send(&self, request: DockerRequest) -> Result<DockerResponse, TransportError> {
        let stream = tokio::net::UnixStream::connect(&self.socket)
            .await
            .map_err(TransportError::io)?;
        let (mut sender, connection) = hyper::client::conn::http1::handshake(TokioIo::new(stream))
            .await
            .map_err(TransportError::io)?;
        tokio::spawn(async move {
            let _ = connection.await;
        });
        let method = match request.method {
            DockerMethod::Get => hyper::Method::GET,
            DockerMethod::Post => hyper::Method::POST,
            DockerMethod::Put => hyper::Method::PUT,
            DockerMethod::Delete => hyper::Method::DELETE,
        };
        let mut builder = hyper::Request::builder()
            .method(method)
            .uri(request.path.as_str())
            .header(hyper::header::HOST, "docker");
        if let Some(content_type) = request.content_type {
            builder = builder.header(hyper::header::CONTENT_TYPE, content_type);
        }
        // hyper needs owned bytes; this copy is not zeroized (recorded in C2.md).
        let http_request = builder
            .body(Full::new(Bytes::copy_from_slice(&request.body)))
            .map_err(TransportError::io)?;
        let response = sender
            .send_request(http_request)
            .await
            .map_err(TransportError::io)?;
        let status = response.status().as_u16();
        let body = Limited::new(response.into_body(), request.max_response_bytes)
            .collect()
            .await
            .map_err(|error| {
                if error.is::<http_body_util::LengthLimitError>() {
                    TransportError {
                        kind: TransportErrorKind::TooLarge,
                        message: "response exceeds limit".into(),
                    }
                } else {
                    TransportError::io(error)
                }
            })?
            .to_bytes()
            .to_vec();
        Ok(DockerResponse { status, body })
    }
}

/// Percent-encodes everything except RFC 3986 unreserved characters.
pub fn encode(value: &str) -> String {
    value
        .bytes()
        .map(|byte| match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' => {
                (byte as char).to_string()
            }
            _ => format!("%{byte:02X}"),
        })
        .collect()
}
