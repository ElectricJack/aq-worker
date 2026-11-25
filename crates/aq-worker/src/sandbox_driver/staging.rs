//! Node-owned evidence that all source inputs reached this sandbox and its
//! runner started. Keep it outside the worktree and the ephemeral driver dir.
use super::*;
use serde::{Deserialize, Serialize};
use std::{io::Write as _, os::unix::fs::OpenOptionsExt as _};

#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Eq, Serialize)]
pub(crate) enum Phase {
    PrepareFailed,
    Started,
}

#[derive(Deserialize, Serialize)]
struct Receipt {
    sandbox: SandboxIdentity,
    container_id: String,
    phase: Phase,
}

fn receipt_path(workspace: &Path, attempt: uuid::Uuid) -> PathBuf {
    workspace.with_extension(format!("sandbox-{attempt}.started.json"))
}

pub(crate) fn record(
    workspace: &Path,
    sandbox: &SandboxIdentity,
    container_id: &str,
    phase: Phase,
) -> Result<()> {
    let path = receipt_path(workspace, sandbox.owner.attempt_id);
    let bytes = serde_json::to_vec(&Receipt {
        sandbox: sandbox.clone(),
        container_id: container_id.into(),
        phase,
    })?;
    let pending = path.with_extension(format!("{}.tmp", uuid::Uuid::new_v4()));
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&pending)?;
    let installed = file
        .write_all(&bytes)
        .and_then(|()| file.sync_all())
        .and_then(|()| std::fs::hard_link(&pending, &path));
    let removed = std::fs::remove_file(&pending);
    installed?;
    removed?;
    std::fs::File::open(path.parent().context("sandbox receipt parent")?)?.sync_all()?;
    Ok(())
}

pub(crate) fn phase(
    workspace: &Path,
    sandbox: &SandboxIdentity,
    container_id: Option<&str>,
) -> Result<Phase> {
    let path = receipt_path(workspace, sandbox.owner.attempt_id);
    let bytes = read_private(&path, 4096)?;
    let receipt: Receipt = serde_json::from_slice(&bytes)?;
    ensure!(
        &receipt.sandbox == sandbox
            && (container_id == Some(receipt.container_id.as_str())
                || (container_id.is_none() && receipt.phase == Phase::PrepareFailed)),
        "sandbox staging receipt identity changed"
    );
    Ok(receipt.phase)
}

pub(crate) fn require_started(
    workspace: &Path,
    sandbox: &SandboxIdentity,
    container_id: &str,
) -> Result<()> {
    ensure!(
        phase(workspace, sandbox, Some(container_id))? == Phase::Started,
        "sandbox runner did not start"
    );
    Ok(())
}
