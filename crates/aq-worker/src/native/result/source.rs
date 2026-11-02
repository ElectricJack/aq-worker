//! Stopped-only, finite source effects through the original Git owner.
use super::*;
use crate::{
    source::{GitAction, GitManifest, PendingGitOperation},
    source_transfer::{
        UploadAction, UploadState,
        steps::{self, UploadAcknowledgment},
    },
    transfer_client::TransferClient,
};
use aq_worker_protocol::{AttemptRef, NodeIdentity, SourceRole, TransferManifest, Validate as _};
use sha2::{Digest as _, Sha256};
use std::io::Read;
impl InputPipeline {
    pub(super) async fn result_snapshot(
        &self,
        stopped: &DurableStoppedResult,
    ) -> Result<ResultHistory> {
        stopped.verify(self)?;
        let id = stopped.attempt_id();
        self.storage(move |owner| {
            Ok(owner.journal.state().attempts[&id]
                .result
                .as_ref()
                .context("result history absent")?
                .clone())
        })
        .await
    }
    async fn result_git_effect(
        &self,
        stopped: &DurableStoppedResult,
        key: &'static str,
    ) -> Result<()> {
        stopped.verify(self)?;
        let id = stopped.attempt_id();
        let c = stopped.custody.clone();
        let expected = self.result_snapshot(stopped).await?;
        if expected
            .source
            .operations
            .get(key)
            .is_some_and(|op| op.result.is_some())
        {
            return Ok(());
        }
        let git = c.scope.inputs.workspace().git().clone();
        let prepared = if expected.source.operations.contains_key(key) {
            expected
        } else {
            let observed = expected.clone();
            let c = c.clone();
            // Query/census before freezing ONLY the first checkpoint intent.
            // A retry always uses the original expected head/generation/UUID.
            let (head, size) = if key == "checkpoint" {
                let git = git.clone();
                let binding = expected.scope.source.clone();
                let workspace = expected.scope.workspace.clone();
                let roots = c.clone();
                tokio::task::spawn_blocking(move || -> Result<_> {
                    roots.roots.verify()?;
                    let bytes = crate::source::workspace_bytes(&workspace.path)?;
                    let value = git.query(
                        &binding,
                        "workspace.inspect",
                        serde_json::json!({"workspace_id":workspace.workspace_id}),
                    )?;
                    let head = serde_json::from_value::<aq_worker_protocol::ObjectId>(
                        value["head"].clone(),
                    )?;
                    ensure!(
                        value["workspace_id"] == workspace.workspace_id.to_string()
                            && value["generation"] == workspace.generation,
                        "result workspace changed before checkpoint"
                    );
                    roots.roots.verify()?;
                    Ok((Some(head), Some(bytes)))
                })
                .await??
            } else {
                (None, None)
            };
            stopped.verify(self)?;
            self.storage(move |owner| {
                let digest = observed.digest()?;
                owner.journal.update_result(id, &digest, |h, _| {
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
                            request: action.request(&h.scope.source, operation),
                            result: None,
                        },
                    );
                    Ok(())
                })
            })
            .await?
        };
        let operation = prepared.source.operations[key].clone();
        let expected_digest = prepared.digest()?;
        let c = stopped.custody.clone();
        let (value, export) = tokio::task::spawn_blocking(move || -> Result<_> {
            c.roots.verify()?;
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
            c.roots.verify()?;
            Ok((value, export))
        })
        .await??;
        stopped.verify(self)?;
        self.storage(move |owner| {
            owner.journal.update_result(id, &expected_digest, |h, _| {
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
    /// Only an actual stopped owner can checkpoint/export. Fresh current grant
    /// is intentionally unnecessary for preserving already-produced source.
    pub async fn capture_result_source(&self, stopped: &DurableStoppedResult) -> Result<()> {
        let _lane = self
            .bulk_slots
            .clone()
            .try_acquire_owned()
            .context("native source capture overloaded")?;
        self.result_git_effect(stopped, "checkpoint").await?;
        self.result_git_effect(stopped, "export").await
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
        let id = stopped.attempt_id();
        let mut history = self.result_snapshot(stopped).await?;
        if history.source.receipt.is_some() {
            return Ok(true);
        }
        if history.source.transfer.is_none() {
            let old = history.clone();
            history = self
                .storage(move |owner| {
                    let digest = old.digest()?;
                    owner.journal.update_result(id, &digest, |h, _| {
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
                    })
                })
                .await?;
        }
        let manifest = history.source.transfer.as_ref().unwrap().clone();
        let upload = history.source.upload.as_ref().unwrap();
        let node = upload.identity.node_id;
        let plan =
            steps::propose_upload(upload, &manifest, Some(&history.scope.managed_root), node)?;
        // Persist even the original pending request before any network exchange.
        let observed = history.digest()?;
        let state = plan.state;
        let action = plan.action;
        history = self
            .storage(move |owner| {
                owner.journal.update_result(id, &observed, |h, _| {
                    h.source.upload = Some(state);
                    Ok(())
                })
            })
            .await?;
        stopped.verify(self)?;
        let status = match &action {
            UploadAction::Begin {
                identity,
                operation_id,
                manifest,
            } => client.begin(identity, *operation_id, manifest).await?,
            UploadAction::Chunk { request, bytes } => client.upload(request, bytes.clone()).await?,
            UploadAction::Finalize(request) => client.finalize(request).await?,
        };
        stopped.verify(self)?;
        let next = steps::acknowledge_upload(
            history.source.upload.as_ref().unwrap(),
            &manifest,
            &action,
            status,
            node,
        )?;
        let digest = history.digest()?;
        let done = self
            .storage(move |owner| {
                let updated = owner.journal.update_result(id, &digest, |h, _| {
                    match next {
                        UploadAcknowledgment::Progress(state) => h.source.upload = Some(state),
                        UploadAcknowledgment::RetentionClaim(receipt) => {
                            let e = h.source.export.as_ref().unwrap();
                            steps::validate_retention_claim(
                                &receipt,
                                &manifest,
                                &e.manifest,
                                &h.scope.source,
                                &e.manifest.source,
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
}
fn verify_export(
    root: &Path,
    value: &serde_json::Value,
    h: &ResultHistory,
) -> Result<ExportRecord> {
    let artifact: Uuid = serde_json::from_value(value["artifact_handle"].clone())?;
    let manifest: GitManifest = serde_json::from_value(value["manifest"].clone())?;
    ensure!(
        !artifact.is_nil()
            && manifest.transfer_id == artifact
            && manifest.format_version == 1
            && manifest.object_format == "sha1"
            && manifest.repository_id == h.scope.source.repository_id
            && manifest.repository_identity == h.scope.source.logical_identity
            && manifest.binding_generation == h.scope.source.binding_generation
            && manifest.base == h.scope.source.base
            && manifest.source
                == h.source
                    .checkpoint
                    .as_ref()
                    .context("result checkpoint absent")?
                    .source
            && !manifest.producer_installation_id.is_nil()
            && manifest.reference_only == (manifest.bundle_size == 0)
            && manifest.prerequisites.iter().all(|p| p == &manifest.base),
        "result actual Git export manifest differs"
    );
    let mut file =
        crate::source_transfer::artifact_file(root, artifact, false, "manifest.json", false)?;
    let size = file.metadata()?.len();
    ensure!(
        (1..=1_048_576).contains(&size),
        "result export manifest bound"
    );
    let mut bytes = Vec::new();
    Read::by_ref(&mut file)
        .take(1_048_577)
        .read_to_end(&mut bytes)?;
    ensure!(
        bytes.len() as u64 == size
            && serde_json::to_value(serde_json::from_slice::<GitManifest>(&bytes)?)?
                == serde_json::to_value(&manifest)?,
        "result exact exported manifest bytes differ"
    );
    let total = size
        .checked_add(manifest.bundle_size)
        .context("result export bytes overflow")?;
    ensure!(
        total <= h.scope.artifact_limit_bytes,
        "result export exceeds original disk reservation"
    );
    if manifest.bundle_size > 0 {
        let mut bundle =
            crate::source_transfer::artifact_file(root, artifact, false, "objects.bundle", false)?;
        ensure!(
            bundle.metadata()?.len() == manifest.bundle_size,
            "result bundle length changed"
        );
        let mut hash = Sha256::new();
        let mut buffer = [0u8; 65536];
        let mut read = 0u64;
        loop {
            let n = bundle.read(&mut buffer)?;
            if n == 0 {
                break;
            }
            read = read
                .checked_add(n as u64)
                .context("result bundle overflow")?;
            ensure!(read <= manifest.bundle_size, "result bundle grew");
            hash.update(&buffer[..n]);
        }
        ensure!(
            read == manifest.bundle_size
                && manifest.bundle_sha256.as_deref()
                    == Some(format!("{:x}", hash.finalize()).as_str()),
            "result actual bundle hash differs"
        );
    } else {
        ensure!(
            manifest.bundle_sha256.is_none(),
            "reference-only bundle has digest"
        );
    }
    Ok(ExportRecord {
        artifact,
        manifest,
        manifest_digest: format!("{:x}", Sha256::digest(&bytes)),
        manifest_bytes: size,
        total_bytes: total,
    })
}
#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;
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
