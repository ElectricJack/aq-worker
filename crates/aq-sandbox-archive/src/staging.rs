//! Node-owned evidence that all source inputs reached this sandbox and its
//! runner started. Keep it outside the worktree and the ephemeral driver dir.
use crate::placement_api::SandboxIdentity;
use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::{io::Write as _, os::unix::fs::OpenOptionsExt as _};

#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Eq, Serialize)]
pub enum Phase {
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

pub fn record(
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

pub fn phase(
    workspace: &Path,
    sandbox: &SandboxIdentity,
    container_id: Option<&str>,
) -> Result<Phase> {
    let path = receipt_path(workspace, sandbox.owner.attempt_id);
    let bytes = aq_handoff_note::read_private(&path, 4096)?;
    let receipt: Receipt = serde_json::from_slice(&bytes)?;
    ensure!(
        &receipt.sandbox == sandbox
            && (container_id == Some(receipt.container_id.as_str())
                || (container_id.is_none() && receipt.phase == Phase::PrepareFailed)),
        "sandbox staging receipt identity changed"
    );
    Ok(receipt.phase)
}

pub fn require_started(
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

/// Resolve an interrupted attempt's single preserved sandbox and its provenance.
pub fn recoverable<'a>(
    workspace: &Path,
    sandboxes: &'a [SandboxIdentity],
    container_id: Option<&str>,
) -> Result<(&'a SandboxIdentity, Phase), &'static str> {
    let [sandbox] = sandboxes else {
        return Err(if sandboxes.is_empty() {
            "sandbox_absent"
        } else {
            "sandbox_ambiguous"
        });
    };
    phase(workspace, sandbox, container_id).map(|phase| (sandbox, phase)).map_err(|error| {
        tracing::warn!(attempt_id = %sandbox.owner.attempt_id, %error, "sandbox copy-back withheld without staging evidence");
        "sandbox_staging_unproven"
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::placement_api::{BackendKind, Ownership};
    use uuid::Uuid;

    #[test]
    fn only_prepare_failed_evidence_can_recover_a_stopped_volume_without_a_container() {
        let dir = tempfile::tempdir().unwrap();
        let workspace = dir.path().join("work");
        for phase in [Phase::PrepareFailed, Phase::Started] {
            let sandbox = SandboxIdentity {
                kind: BackendKind::DockerLocal,
                backend_id: "sandbox".into(),
                owner: Ownership {
                    node_id: Uuid::new_v4(),
                    boot_id: Uuid::new_v4(),
                    attempt_id: Uuid::new_v4(),
                },
            };
            let candidates = std::slice::from_ref(&sandbox);
            assert!(recoverable(&workspace, candidates, None).is_err());
            record(&workspace, &sandbox, "original", phase).unwrap();
            assert!(recoverable(&workspace, candidates, Some("replacement")).is_err());
            assert_eq!(
                recoverable(&workspace, candidates, Some("original")),
                Ok((&sandbox, phase))
            );
            assert_eq!(
                recoverable(&workspace, candidates, None).is_ok(),
                phase == Phase::PrepareFailed
            );
            assert!(record(&workspace, &sandbox, "replacement", phase).is_err());
        }
    }
}
