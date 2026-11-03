//! Minimal Docker Engine API transport. The backend owns timeouts; transports
//! only move bytes and never put request bodies in errors.
use bytes::Bytes;
use http_body_util::{BodyExt, Full, Limited};
use hyper_util::rt::TokioIo;
use std::{fmt, future::Future, path::PathBuf};
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
