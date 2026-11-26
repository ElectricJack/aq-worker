//! Shared JSON request policy for the Docker-compatible sandbox daemon.
use super::*;
use serde_json::{Value, json};

pub const MAX_JSON_BYTES: usize = 8 * 1024 * 1024;

#[derive(Debug)]
pub enum ApiError {
    Transport(CallError),
    UnexpectedStatus,
    InvalidBody,
}
impl From<CallError> for ApiError {
    fn from(error: CallError) -> Self {
        Self::Transport(error)
    }
}

pub struct Api<T> {
    transport: T,
    prefix: &'static str,
    timeout: Duration,
}

impl<T: DockerTransport> Api<T> {
    pub fn new(transport: T, prefix: &'static str, timeout: Duration) -> Self {
        Self {
            transport,
            prefix,
            timeout,
        }
    }

    pub async fn call(
        &self,
        method: DockerMethod,
        path: &str,
        content_type: Option<&'static str>,
        body: Zeroizing<Vec<u8>>,
        max_response_bytes: usize,
        timeout: Duration,
    ) -> Result<DockerResponse, CallError> {
        let request = DockerRequest {
            method,
            path: format!("{}{path}", self.prefix),
            content_type,
            body,
            max_response_bytes,
        };
        let result = call(&self.transport, request, timeout).await;
        if matches!(result, Err(CallError::Timeout)) {
            let operation = path
                .split_once('?')
                .map_or(path, |(operation, _)| operation);
            tracing::warn!(?method, operation, "sandbox daemon operation timed out");
        }
        result
    }

    pub async fn send(
        &self,
        method: DockerMethod,
        path: &str,
        body: Option<&Value>,
    ) -> Result<DockerResponse, CallError> {
        self.send_with(method, path, body, self.timeout).await
    }

    pub async fn send_with(
        &self,
        method: DockerMethod,
        path: &str,
        body: Option<&Value>,
        timeout: Duration,
    ) -> Result<DockerResponse, CallError> {
        let (content_type, bytes) = match body {
            Some(value) => (
                Some("application/json"),
                Zeroizing::new(serde_json::to_vec(value).expect("JSON value serializes")),
            ),
            None => (None, Zeroizing::default()),
        };
        self.call(method, path, content_type, bytes, MAX_JSON_BYTES, timeout)
            .await
    }

    pub async fn get_json(&self, path: &str) -> Result<Value, ApiError> {
        self.get_optional(path)
            .await?
            .ok_or(ApiError::UnexpectedStatus)
    }

    /// Only a definitive 404 is absence.
    pub async fn get_optional(&self, path: &str) -> Result<Option<Value>, ApiError> {
        let response = self.send(DockerMethod::Get, path, None).await?;
        match response.status {
            200 => serde_json::from_slice(&response.body)
                .map(Some)
                .map_err(|_| ApiError::InvalidBody),
            404 => Ok(None),
            _ => Err(ApiError::UnexpectedStatus),
        }
    }

    pub async fn archive(&self, path: String, max: usize) -> Result<Option<Vec<u8>>, CallError> {
        archive(&self.transport, path, max, self.timeout).await
    }

    /// The worker verifies the identity and state of the running sandbox before
    /// letting the daemon start this fixed, unprivileged initializer.
    pub async fn start_task_postgres(&self, sandbox: &str) -> Result<(), ApiError> {
        let request = json!({
            "User": "agent",
            "Cmd": ["/bin/bash", "-c", crate::task_postgres::BOOTSTRAP],
            "AttachStdin": false,
            "AttachStdout": false,
            "AttachStderr": false,
            "Privileged": false,
            "Tty": false,
        });
        let path = format!("/containers/{sandbox}/exec");
        let created = self.send(DockerMethod::Post, &path, Some(&request)).await?;
        if created.status != 201 {
            return Err(ApiError::UnexpectedStatus);
        }
        let value: Value =
            serde_json::from_slice(&created.body).map_err(|_| ApiError::InvalidBody)?;
        let id = value["Id"]
            .as_str()
            .filter(|id| id.len() == 64 && id.bytes().all(|value| value.is_ascii_hexdigit()))
            .ok_or(ApiError::InvalidBody)?;
        let start = json!({ "Detach": true, "Tty": false });
        let reply = self
            .send(
                DockerMethod::Post,
                &format!("/exec/{id}/start"),
                Some(&start),
            )
            .await?;
        if reply.status != 200 {
            return Err(ApiError::UnexpectedStatus);
        }
        Ok(())
    }
}

/// Preserves slashes, colons and digest separators in an Engine image route.
pub fn image_path(image: &str) -> String {
    let keep = |c: u8| c.is_ascii_alphanumeric() || b"-._~/:@".contains(&c);
    let encoded = image.bytes().map(|c| match keep(c) {
        true => (c as char).to_string(),
        false => format!("%{c:02X}"),
    });
    format!("/images/{}", encoded.collect::<String>())
}
