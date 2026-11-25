//! Stopped-only, finite source effects through the original Git owner.
use super::*;
mod export;
use crate::{
    source::{GitAction, GitClient, GitManifest, PendingGitOperation},
    source_transfer::{
        UploadState,
        steps::{self, UploadAcknowledgment},
    },
    transfer_client::TransferClient,
};
use aq_worker_protocol::{AttemptRef, NodeIdentity, SourceRole, TransferManifest, Validate as _};
use export::verify_export;
use sha2::{Digest as _, Sha256};
use std::io::Read;
impl InputPipeline {
    /// Only an actual stopped owner can checkpoint/export. Fresh current grant
    /// is intentionally unnecessary for preserving already-produced source.
    pub async fn capture_result_source(&self, stopped: &DurableStoppedResult) -> Result<()> {
        let _lane = self
            .bulk_slots
            .clone()
            .try_acquire_owned()
            .context("native source capture overloaded")?;
        capture(&Live(self, stopped)).await
    }
    /// One bounded authenticated transfer step; intent precedes I/O and the
    /// original pending chunk/finalize is reused after response loss.
    pub async fn transfer_result_source_step(
        &self,
        stopped: &DurableStoppedResult,
        client: &TransferClient,
    ) -> Result<bool> {
        let _lane = self
            .bulk_slots
            .clone()
            .try_acquire_owned()
            .context("native result transfer overloaded")?;
        transfer_step(&Live(self, stopped), client).await
    }
}
impl InputPipeline {
    /// Spec 0016 §5.3: a worktree attempt commits and pushes its task ref
    /// instead of checkpoint, export and upload. False: not a worktree attempt.
    pub async fn push_result_source(&self, stopped: &DurableStoppedResult) -> Result<bool> {
        push(&Live(self, stopped)).await
    }
    /// Removes a worktree attempt's worktree once its report is recorded.
    pub async fn end_result_source(&self, stopped: &DurableStoppedResult) {
        cleanup(&Live(self, stopped)).await;
    }
}
pub(super) async fn cleanup<O: StoppedOwner>(o: &O) {
    let (Some(node), Ok(h)) = (o.git().node.clone(), snapshot(o).await) else {
        return;
    };
    let id = o.attempt_id();
    if let Err(error) = node.end_siblings(&h.scope.workspace.path).await {
        tracing::warn!(attempt_id=%id, %error, "sibling worktrees not removed");
    }
    if h.source.pushed.is_some()
        && let Err(error) = node.end(h.scope.workspace.path).await
    {
        tracing::warn!(attempt_id=%id, %error, "attempt worktree not removed");
    }
}
/// After the stopped-writer proof: commit and push under the grant's lease.
pub(super) async fn push<O: StoppedOwner>(o: &O) -> Result<bool> {
    o.verify()?;
    let id = o.attempt_id();
    let (at, required) = o
        .journal(move |j| {
            let attempt = &j.state().attempts[&id];
            Ok((
                attempt.source.clone(),
                attempt.reservation.workspace_required,
            ))
        })
        .await?;
    let expected = snapshot(o).await?;
    ensure!(
        required == aq_node_git::workspaces::load(&expected.scope.workspace.path)?.has_resources(),
        "workspace_required_mismatch: frozen supplemental result plan absent or changed",
    );
    ensure!(
        !required || o.git().node.is_some(),
        "supplemental result needs node Git"
    );
    if let Some(node) = o.git().node.clone() {
        o.roots().verify()?;
        node.finish_siblings(
            id,
            &expected.scope.workspace.path,
            expected.scope.git_author.clone(),
        )
        .await?;
        o.verify()?;
    }
    let Some(path) = at.and_then(|s| s.worktree) else {
        return Ok(false);
    };
    if expected.source.pushed.is_some() {
        return Ok(true);
    }
    let node = o.git().node.clone().context("node git is not configured")?;
    // A moved task ref is recorded as push_conflict and reported; never forced.
    let (_, pushed) = node
        .finish_as(
            path,
            format!("AQ attempt {id}"),
            expected.scope.git_author.clone(),
        )
        .await?;
    o.verify()?;
    o.journal(move |journal| {
        let digest = expected.digest()?;
        journal.update_result(id, &digest, |h, _| {
            h.source.pushed = Some(pushed);
            Ok(())
        })?;
        Ok(true)
    })
    .await
}
pub(super) async fn snapshot<O: StoppedOwner>(o: &O) -> Result<ResultHistory> {
    o.verify()?;
    let id = o.attempt_id();
    o.journal(move |journal| {
        Ok(journal.state().attempts[&id]
            .result
            .as_ref()
            .context("result history absent")?
            .clone())
    })
    .await
}
pub(super) async fn capture<O: StoppedOwner>(o: &O) -> Result<()> {
    git_effect(o, "checkpoint").await?;
    git_effect(o, "export").await
}
/// Finite transfer bound: every manifest and bundle chunk, plus begin and finalize.
pub(crate) fn transfer_bound(h: &ResultHistory) -> Result<u64> {
    let export = h
        .source
        .export
        .as_ref()
        .context("actual result export absent")?;
    let chunk = aq_worker_protocol::MAX_CHUNK_BYTES as u64;
    export
        .manifest_bytes
        .div_ceil(chunk)
        .checked_add(export.manifest.bundle_size.div_ceil(chunk))
        .and_then(|n| n.checked_add(2))
        .context("result transfer finite bound overflow")
}
async fn git_effect<O: StoppedOwner>(o: &O, key: &'static str) -> Result<()> {
    o.verify()?;
    let id = o.attempt_id();
    let expected = snapshot(o).await?;
    if expected
        .source
        .operations
        .get(key)
        .is_some_and(|op| op.result.is_some())
    {
        return Ok(());
    }
    let git = o.git();
    let prepared = prepare_git_intent(o, key, &git, expected).await?;
    let operation = prepared.source.operations[key].clone();
    let expected_digest = prepared.digest()?;
    let roots = o.roots();
    let (value, export) = tokio::task::spawn_blocking(move || -> Result<_> {
        roots.verify()?;
        let value = git.exchange(&operation.request)?;
        ensure!(
            serde_json::to_vec(&value)?.len() <= MAX_GIT_REPLY_BYTES,
            "oversized result source reply; original operation retained"
        );
        let export = if key == "export" {
            Some(verify_export(git.root(), &value, &prepared)?)
        } else {
            None
        };
        roots.verify()?;
        Ok((value, export))
    })
    .await??;
    o.verify()?;
    o.journal(move |journal| {
        journal.update_result(id, &expected_digest, |h, _| {
            let op = h
                .source
                .operations
                .get_mut(key)
                .context("result source intent absent")?;
            ensure!(op.result.is_none(), "result source effect raced");
            op.result = Some(value.clone());
            if key == "checkpoint" {
                h.source.checkpoint = Some(serde_json::from_value(value)?)
            } else {
                h.source.export = export
            }
            Ok(())
        })?;
        Ok(())
    })
    .await
}
async fn prepare_git_intent<O: StoppedOwner>(
    o: &O,
    key: &'static str,
    git: &GitClient,
    expected: ResultHistory,
) -> Result<ResultHistory> {
    if expected.source.operations.contains_key(key) {
        return Ok(expected);
    }
    let id = o.attempt_id();
    // Query/census before freezing ONLY the first checkpoint intent.
    // A retry always uses the original expected head/generation/UUID.
    let (head, size) = if key == "checkpoint" {
        let (head, bytes) = checkpoint_observation(o, git.clone(), expected.scope.clone()).await?;
        (Some(head), Some(bytes))
    } else {
        (None, None)
    };
    o.verify()?;
    ensure!(
        key != "checkpoint"
            || expected.scope.git_author.is_none()
            || git.supports_checkpoint_author(),
        "local aq-git no longer supports the reserved checkpoint author"
    );
    o.journal(move |journal| {
        let digest = expected.digest()?;
        journal.update_result(id, &digest, |h, _| freeze_operation(h, key, head, size))
    })
    .await
}

async fn checkpoint_observation<O: StoppedOwner>(
    o: &O,
    git: GitClient,
    scope: ScopeRecord,
) -> Result<(aq_worker_protocol::ObjectId, u64)> {
    let id = o.attempt_id();
    let binding = scope.source.clone();
    let workspace = scope.workspace.clone();
    let roots = o.roots();
    // The original complete allocation receipt is retained by this
    // same journal owner. It is a comparison pin, not current proof.
    let original = o
        .journal(move |journal| {
            journal.state().attempts[&id]
                .source
                .as_ref()
                .and_then(|s| s.operations.get("create"))
                .and_then(|o| o.result.clone())
                .context("result original workspace allocation absent")
        })
        .await?;
    tokio::task::spawn_blocking(move || -> Result<_> {
        roots.verify()?;
        let bytes = crate::source::workspace_bytes(&workspace.path)?;
        let value = git.query(
            &binding,
            "workspace.inspect",
            serde_json::json!({"workspace_id":workspace.workspace_id}),
        )?;
        let head = inspect_checkpoint_workspace(&value, &original, &scope)?;
        roots.verify()?;
        Ok((head, bytes))
    })
    .await?
}
fn freeze_operation(
    h: &mut ResultHistory,
    key: &str,
    head: Option<aq_worker_protocol::ObjectId>,
    size: Option<u64>,
) -> Result<()> {
    let (operation, action) = match key {
        "checkpoint" => {
            h.source.head = head;
            h.source.workspace_bytes = size;
            ensure!(
                size.is_some_and(|n| n <= h.scope.workspace_limit_bytes),
                "stopped workspace exceeds original reservation"
            );
            (
                h.scope.operations.checkpoint,
                GitAction::Checkpoint {
                    workspace_id: h.scope.workspace.workspace_id,
                    generation: h.scope.workspace.generation,
                    head: h.source.head.clone().unwrap(),
                },
            )
        }
        "export" => {
            let checkpoint = h
                .source
                .checkpoint
                .as_ref()
                .context("result checkpoint absent")?;
            (
                h.scope.operations.export,
                GitAction::Export {
                    source: checkpoint.source.clone(),
                    retention_handle: checkpoint.retention_handle,
                },
            )
        }
        _ => anyhow::bail!("unsupported result source action"),
    };
    h.source.operations.insert(
        key.into(),
        PendingGitOperation {
            operation_id: operation,
            request: action.request_as(&h.scope.source, operation, h.scope.git_author.as_ref()),
            result: None,
        },
    );
    Ok(())
}
pub(super) async fn transfer_step<O: StoppedOwner>(o: &O, client: &TransferClient) -> Result<bool> {
    let id = o.attempt_id();
    let mut history = snapshot(o).await?;
    if history.source.receipt.is_some() {
        return Ok(true);
    }
    if history.source.transfer.is_none() {
        let old = history.clone();
        history = o
            .journal(move |journal| {
                let digest = old.digest()?;
                journal.update_result(id, &digest, |h, _| start_upload(h))
            })
            .await?;
    }
    let manifest = history.source.transfer.as_ref().unwrap().clone();
    let upload = history.source.upload.as_ref().unwrap();
    let node = upload.identity.node_id;
    let plan = steps::propose_upload(upload, &manifest, Some(&history.scope.managed_root), node)?;
    // Persist even the original pending request before any network exchange.
    let observed = history.digest()?;
    let state = plan.state;
    let action = plan.action;
    history = o
        .journal(move |journal| {
            journal.update_result(id, &observed, |h, _| {
                h.source.upload = Some(state);
                Ok(())
            })
        })
        .await?;
    o.verify()?;
    let status = action.execute(client).await?;
    o.verify()?;
    let next = steps::acknowledge_upload(
        history.source.upload.as_ref().unwrap(),
        &manifest,
        (&action, status),
        node,
    )?;
    let digest = history.digest()?;
    let done = o
        .journal(move |journal| {
            let updated = journal.update_result(id, &digest, |h, _| {
                match next {
                    UploadAcknowledgment::Progress(state) => h.source.upload = Some(state),
                    UploadAcknowledgment::RetentionClaim(receipt) => {
                        let e = h.source.export.as_ref().unwrap();
                        steps::validate_retention_claim(
                            &receipt,
                            &manifest,
                            &e.manifest,
                            (&h.scope.source, &e.manifest.source),
                        )?;
                        h.source.receipt = Some(receipt);
                    }
                }
                Ok(())
            })?;
            Ok(updated.source.receipt.is_some())
        })
        .await?;
    Ok(done)
}
fn start_upload(h: &mut ResultHistory) -> Result<()> {
    let e = h.source.export.as_ref().context("result export absent")?;
    let b = &h.scope.binding.launch;
    let aq_session_protocol::SessionIdentity::Remote {
        node_id,
        boot_id,
        session_id,
        ..
    } = b.session
    else {
        anyhow::bail!("result remote session absent")
    };
    let manifest = TransferManifest {
        transfer_id: h.scope.operations.transfer,
        role: SourceRole::Checkpoint,
        checkpoint: e.manifest.source.clone(),
        attempt: AttemptRef {
            attempt_id: b.attempt_id,
            session_id,
            generation: b.assignment_generation,
        },
        resolved_spec_digest: b.execution_spec_digest.as_str().into(),
        source: h.scope.source.clone(),
        artifact_id: e.artifact,
        manifest_digest: e.manifest_digest.clone(),
        bundle_digest: e.manifest.bundle_sha256.clone(),
        manifest_bytes: e.manifest_bytes,
        bundle_bytes: e.manifest.bundle_size,
    };
    manifest.validate()?;
    h.source.transfer = Some(manifest);
    h.source.upload = Some(UploadState {
        identity: NodeIdentity { node_id, boot_id },
        begin_operation: h.scope.operations.upload_begin,
        finalize_operation: h.scope.operations.upload_finalize,
        begun: false,
        manifest_offset: 0,
        bundle_offset: 0,
        pending: None,
    });
    Ok(())
}
/// aq-git's active inspection is an envelope containing the complete workspace
/// record. Compare it with the original allocation, then use the current HEAD;
/// dirty contents are precisely what this stopped checkpoint must preserve.
fn inspect_checkpoint_workspace(
    value: &serde_json::Value,
    original: &serde_json::Value,
    scope: &ScopeRecord,
) -> Result<aq_worker_protocol::ObjectId> {
    ensure!(
        original.is_object()
            && value.get("workspace") == Some(original)
            && original["workspace_id"] == scope.workspace.workspace_id.to_string()
            && original["generation"] == scope.workspace.generation
            && original["path"] == serde_json::to_value(&scope.workspace.path)?
            && same_workspace_source(original, &scope.source)
            && original["lifecycle"] == "active",
        "result workspace changed before checkpoint"
    );
    let head: aq_worker_protocol::ObjectId = serde_json::from_value(value["head"].clone())?;
    head.validate()?;
    Ok(head)
}

fn same_workspace_source(
    original: &serde_json::Value,
    source: &aq_worker_protocol::SourceBinding,
) -> bool {
    original["repository_id"] == source.repository_id.to_string()
        && original["binding_generation"] == source.binding_generation
        && original["base"] == source.base.hex
}
#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;
    // Exact active envelope emitted by aq-git WorkspaceManager::workspace.inspect,
    // including its full allocation record (not the worker's three-field view).
    fn inspection() -> (ResultHistory, serde_json::Value, serde_json::Value) {
        let h = super::super::tests::history();
        let original = serde_json::json!({
            "format_version":1,"workspace_id":h.scope.workspace.workspace_id,
            "repository_id":h.scope.source.repository_id,
            "binding_generation":h.scope.source.binding_generation,
            "generation":h.scope.workspace.generation,"base":h.scope.source.base.hex,
            "path":h.scope.workspace.path,"lifecycle":"active",
            "allocating_operation":Uuid::new_v4(),"last_checkpoint":null,
            "retiring_operation":null,"config_digest":"a".repeat(64)
        });
        let value = serde_json::json!({"workspace":original,
            "head":{"algorithm":"sha1","hex":"b".repeat(40)},
            "clean":false,"status_encoding":"hex","status":"31204d20736565642e74787400"});
        (h, original, value)
    }
    #[test]
    fn checkpoint_inspection_accepts_actual_nested_dirty_workspace_and_current_head() {
        let (h, original, value) = inspection();
        let head = inspect_checkpoint_workspace(&value, &original, &h.scope).unwrap();
        assert_eq!(head.hex, "b".repeat(40));
        assert_ne!(
            head, h.scope.source.base,
            "preserve current committed HEAD too"
        );
        assert_eq!(value["clean"], false, "stopped edits need not be clean");
    }
    #[test]
    fn checkpoint_inspection_refuses_flat_inactive_and_changed_full_allocation() {
        let (h, original, value) = inspection();
        let mut flat = original.clone();
        flat["head"] = value["head"].clone();
        assert!(inspect_checkpoint_workspace(&flat, &original, &h.scope).is_err());
        for field in [
            "workspace_id",
            "repository_id",
            "binding_generation",
            "generation",
            "base",
            "path",
            "lifecycle",
            "allocating_operation",
            "last_checkpoint",
            "retiring_operation",
            "config_digest",
        ] {
            let mut changed = value.clone();
            changed["workspace"][field] = serde_json::json!("changed");
            assert!(
                inspect_checkpoint_workspace(&changed, &original, &h.scope).is_err(),
                "full original allocation field {field} must remain exact"
            );
        }
        let mut inactive = original;
        inactive["lifecycle"] = serde_json::json!("retired");
        let mut same = value;
        same["workspace"] = inactive.clone();
        assert!(inspect_checkpoint_workspace(&same, &inactive, &h.scope).is_err());
    }
    #[test]
    fn checkpoint_inspection_refuses_wrong_scope_or_malformed_current_head() {
        let (mut h, original, mut value) = inspection();
        h.scope.workspace.workspace_id = Uuid::new_v4();
        assert!(inspect_checkpoint_workspace(&value, &original, &h.scope).is_err());
        let (h, original, _) = inspection();
        // Use matching full allocation but an invalid object ID.
        value["workspace"] = original.clone();
        value["head"] = serde_json::json!({"algorithm":"sha1","hex":"not-an-oid"});
        assert!(inspect_checkpoint_workspace(&value, &original, &h.scope).is_err());
        value.as_object_mut().unwrap().remove("head");
        assert!(inspect_checkpoint_workspace(&value, &original, &h.scope).is_err());
    }
    fn export() -> (tempfile::TempDir, ResultHistory, serde_json::Value, Vec<u8>) {
        let root = tempfile::tempdir().unwrap();
        let mut h = super::super::tests::history();
        let checkpoint = aq_worker_protocol::ObjectId {
            algorithm: aq_worker_protocol::ObjectAlgorithm::Sha1,
            hex: "b".repeat(40),
        };
        h.source.checkpoint = Some(crate::source::Checkpoint {
            workspace_id: h.scope.workspace.workspace_id,
            workspace_generation: 2,
            source: checkpoint.clone(),
            retention_handle: Uuid::new_v4(),
        });
        let bundle = b"actual finite bundle bytes for file verifier only".to_vec();
        let id = Uuid::new_v4();
        let native = GitManifest {
            format_version: 1,
            repository_id: h.scope.source.repository_id,
            repository_identity: h.scope.source.logical_identity.clone(),
            binding_generation: h.scope.source.binding_generation,
            transfer_id: id,
            object_format: "sha1".into(),
            base: h.scope.source.base.clone(),
            source: checkpoint,
            prerequisites: vec![h.scope.source.base.clone()],
            bundle_size: bundle.len() as u64,
            bundle_sha256: Some(format!("{:x}", Sha256::digest(&bundle))),
            producer_installation_id: Uuid::new_v4(),
            reference_only: false,
            caller_correlation: serde_json::json!({}),
        };
        let dir = root.path().join("artifacts").join(id.to_string());
        std::fs::create_dir_all(&dir).unwrap();
        let bytes = serde_json::to_vec_pretty(&native).unwrap();
        for (name, data) in [
            ("manifest.json", bytes.as_slice()),
            ("objects.bundle", bundle.as_slice()),
        ] {
            let p = dir.join(name);
            std::fs::write(&p, data).unwrap();
            std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o600)).unwrap();
        }
        (
            root,
            h,
            serde_json::json!({"artifact_handle":id,"manifest":native}),
            bytes,
        )
    }
    #[test]
    fn result_export_distinguishes_raw_upload_hash_from_native_receipt_hash() {
        let (root, h, value, bytes) = export();
        let actual = verify_export(root.path(), &value, &h).unwrap();
        assert_eq!(
            actual.manifest_digest,
            format!("{:x}", Sha256::digest(&bytes))
        );
        assert_ne!(
            actual.manifest_digest,
            actual.manifest.canonical_digest().unwrap(),
            "pretty raw upload bytes must not be silently relabeled canonical"
        );
        assert_eq!(actual.manifest_bytes, bytes.len() as u64);
    }
    #[test]
    fn result_export_rejects_changed_bundle_before_retention() {
        let (root, h, value, _) = export();
        let first = verify_export(root.path(), &value, &h).unwrap();
        std::fs::write(
            root.path()
                .join("artifacts")
                .join(first.artifact.to_string())
                .join("objects.bundle"),
            vec![b'x'; first.manifest.bundle_size as usize],
        )
        .unwrap();
        assert!(verify_export(root.path(), &value, &h).is_err());
    }
    #[test]
    fn result_export_refuses_symlink_manifest_and_out_of_scope_reply() {
        let (root, h, mut value, _) = export();
        let actual = verify_export(root.path(), &value, &h).unwrap();
        let dir = root
            .path()
            .join("artifacts")
            .join(actual.artifact.to_string());
        let manifest = dir.join("manifest.json");
        std::fs::rename(&manifest, dir.join("retained.json")).unwrap();
        std::os::unix::fs::symlink(dir.join("retained.json"), &manifest).unwrap();
        assert!(verify_export(root.path(), &value, &h).is_err());
        value["manifest"]["repository_id"] = Uuid::new_v4().to_string().into();
        assert!(verify_export(root.path(), &value, &h).is_err());
    }
}
