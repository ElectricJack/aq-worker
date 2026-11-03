//! Runner hydration from central retention (0005 section 4 step 4).
//!
//! The sandbox always fetches the retained revision through the permit-
//! authenticated artifact export and imports it with the local aq-git. It is
//! given no Git remote URL or credential, so it cannot contact the project's
//! remote (SBX17). Bytes and digests are checked against the retention
//! descriptor before aq-git sees them, and the result is checked again
//! against aq-git's own import, retention and workspace inspection.
use super::auth::RunnerHttp;
use crate::native::preparation::hydration::{files::Staging, validate_retained};
use crate::source::{GitAction, GitClient, GitManifest, Workspace};
use anyhow::{Context as _, Result, ensure};
use aq_worker_protocol::{SourceBinding, TransferFile, TransferManifest, Validate as _};
use serde_json::json;
use std::sync::Arc;
use uuid::Uuid;

const CHUNK: u32 = 262_144;

/// A clean aq-git workspace at exactly the retained revision.
pub struct HydratedSandbox {
    workspace: Workspace,
    manifest: TransferManifest,
    native: GitManifest,
    _stage: Arc<Staging>,
}
impl HydratedSandbox {
    pub fn workspace(&self) -> &Workspace {
        &self.workspace
    }
    pub fn manifest(&self) -> &TransferManifest {
        &self.manifest
    }
    pub fn native_manifest(&self) -> &GitManifest {
        &self.native
    }
}

async fn blocking<T: Send + 'static>(f: impl FnOnce() -> Result<T> + Send + 'static) -> Result<T> {
    tokio::task::spawn_blocking(f)
        .await
        .context("runner hydration worker failed")?
}

/// `expected` is the source pinned by the frozen launch; `git` must come from
/// the bundle-pinned aq-git (`GitService::start_with_git`).
pub async fn hydrate(
    http: &RunnerHttp,
    expected: &SourceBinding,
    git: &GitClient,
) -> Result<HydratedSandbox> {
    expected.validate()?;
    let binding = http.binding();
    let m = http.source_status(&binding).await?;
    ensure!(
        m.source == *expected && m.checkpoint == expected.base,
        "retained source is not the pinned revision"
    );
    let marker = serde_json::to_vec(&json!({
        "schema_version": 1,
        "runner": {"sandbox": binding.sandbox_id, "channel": binding.channel_id},
        "attempt": binding.attempt_id,
        "manifest": m,
    }))?;
    let stage = {
        let (root, m) = (git.root().to_owned(), m.clone());
        Arc::new(blocking(move || Staging::open(&root, &m, &marker)).await?)
    };
    for (file, size) in [
        (TransferFile::Manifest, m.manifest_bytes),
        (TransferFile::Bundle, m.bundle_bytes),
    ] {
        let mut offset = 0;
        while offset < size {
            let bytes = http
                .source_chunk(&binding, &m, file.clone(), offset, CHUNK)
                .await?;
            let next = offset + bytes.len() as u64;
            let (stage, file) = (stage.clone(), file.clone());
            blocking(move || stage.append(&file, offset, &bytes)).await?;
            offset = next;
        }
    }
    let (git, b, manifest, staged) = (git.clone(), expected.clone(), m.clone(), stage.clone());
    let (workspace, native) = blocking(move || {
        let native = staged.verify(&manifest)?;
        staged.publish(&manifest)?;
        if git.validate_binding(&b).is_err() {
            git.exchange(&GitAction::Register.request(&b, Uuid::new_v4()))?;
            git.validate_binding(&b)?;
        }
        let imported = git.exchange(
            &GitAction::Import {
                artifact_handle: manifest.artifact_id,
            }
            .request(&b, Uuid::new_v4()),
        )?;
        let create = Uuid::new_v4();
        let created = git.exchange(&GitAction::Create.request(&b, create))?;
        let workspace: Workspace = serde_json::from_value(created.clone())?;
        let retention = git.query(
            &b,
            "retention.inspect",
            json!({"handle_id": imported["retention_handle"], "limit": 1}),
        )?;
        let inspect = git.query(
            &b,
            "workspace.inspect",
            json!({"workspace_id": workspace.workspace_id}),
        )?;
        let native_again = staged.verify(&manifest)?;
        ensure!(
            native_again.canonical_digest()? == native.canonical_digest()?,
            "retained manifest changed during import"
        );
        validate_retained(
            &git, &b, &manifest, &native, create, &imported, &created, &retention, &inspect,
        )?;
        Ok((workspace, native))
    })
    .await?;
    Ok(HydratedSandbox {
        workspace,
        manifest: m,
        native,
        _stage: stage,
    })
}
