//! Read the provider conversation from a Docker sandbox.
use crate::backend::{
    BackendError, SandboxIdentity,
    docker_local::{DockerLocalBackend, DockerTransport},
};
use aq_sandbox_archive::conversation::{MAX_LOG_BYTES, from_archive};
pub use aq_sandbox_archive::conversation::{MAX_SNAPSHOT_BYTES, Provider, Snapshot};
use uuid::Uuid;

/// Load one provider session from the fresh sandbox's private log directory.
pub async fn read_snapshot<T: DockerTransport>(
    backend: &mut DockerLocalBackend<T>,
    sandbox: &SandboxIdentity,
    provider: Provider,
    directory: &str,
    expected_session: Option<Uuid>,
    token: Option<&[u8]>,
) -> anyhow::Result<Option<Snapshot>> {
    let path = match (provider, expected_session) {
        (Provider::Claude, Some(id)) => format!("{directory}/{id}.jsonl"),
        _ => directory.to_owned(),
    };
    let raw = match backend.get_archive(sandbox, &path, MAX_LOG_BYTES).await {
        Ok(raw) => zeroize::Zeroizing::new(raw),
        Err(BackendError::NotFound | BackendError::InvalidRequest(_)) => return Ok(None),
        Err(error) => return Err(error.into()),
    };
    Ok(from_archive(&raw, provider, expected_session, token)?)
}
