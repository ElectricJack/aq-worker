//! Closed source effects through an actual stopped workspace owner.
use super::*;
use crate::{
    continuation::history::ParkingExport, source::GitManifest,
    source_transfer::steps::ProposedUploadStep,
};
use aq_worker_protocol::{ObjectId, Validate as _};
use serde_json::{Value, json};
use sha2::{Digest as _, Sha256};
use std::io::Read;

impl InputPipeline {
    /// Only first-checkpoint planning may observe a new head/generation. Replay
    /// must use the already durable exact request, even after its own lost reply.
    pub(crate) async fn parking_observe_source(
        self: &Arc<Self>,
        stopped: &DurableStoppedWorkspace,
    ) -> Result<StoppedSourceObservation> {
        let custody = stopped.custody.clone();
        let pipeline = self.clone();
        self.storage(move |owner| {
            let history = custody.check(&pipeline, owner)?;
            ensure!(
                !history.source.operations.contains_key("checkpoint"),
                "parking checkpoint already planned; replay exact original operation"
            );
            let git = custody.fence.scope.inputs.workspace().retained_git();
            git.validate_binding(&history.intent.source)?;
            let inspect = git.query(
                &history.intent.source,
                "workspace.inspect",
                json!({"workspace_id":history.intent.workspace.workspace_id}),
            )?;
            let original = owner.journal.state().attempts[&custody.fence.scope.id]
                .source
                .as_ref()
                .unwrap()
                .operations["create"]
                .result
                .as_ref()
                .unwrap();
            ensure!(
                inspect["workspace"] == *original,
                "parking current workspace identity/generation changed before checkpoint"
            );
            let head: ObjectId = serde_json::from_value(inspect["head"].clone())?;
            head.validate()?;
            let workspace_bytes = crate::source::workspace_bytes(&history.intent.workspace.path)?;
            ensure!(
                workspace_bytes <= history.intent.workspace_limit_bytes
                    && workspace_bytes <= history.intent.reserved_disk_bytes,
                "parking dirty workspace exceeds retained reservation"
            );
            custody.check(&pipeline, owner)?;
            Ok(StoppedSourceObservation {
                head,
                workspace_bytes,
            })
        })
        .await
    }
    /// Never accepts an operation/request/socket from its caller. Cancellation
    /// retains the admitted storage owner; a lost reply leaves the same outbox.
    pub(crate) async fn parking_git(
        self: &Arc<Self>,
        stopped: &DurableStoppedWorkspace,
        key: &str,
    ) -> Result<Value> {
        ensure!(
            matches!(key, "checkpoint" | "export"),
            "unsupported parking Git effect"
        );
        let key = key.to_string();
        let custody = stopped.custody.clone();
        let pipeline = self.clone();
        self.storage(move |owner| {
            let history = custody.check(&pipeline, owner)?;
            let operation = history
                .source
                .operations
                .get(&key)
                .context("parking Git request not durable")?;
            if let Some(result) = &operation.result {
                return Ok(result.clone());
            }
            let git = custody.fence.scope.inputs.workspace().retained_git();
            // No original-head/generation inspection here: a successful lost
            // checkpoint reply may already have advanced this exact workspace.
            let result = git.exchange(&operation.request)?;
            custody.check(&pipeline, owner)?;
            Ok(result)
        })
        .await
    }
    pub(crate) async fn parking_inspect_export(
        self: &Arc<Self>,
        stopped: &DurableStoppedWorkspace,
        result: Value,
    ) -> Result<ParkingExport> {
        let custody = stopped.custody.clone();
        let pipeline = self.clone();
        self.storage(move |owner| {
            let history = custody.check(&pipeline, owner)?;
            ensure!(
                history.source.operations.contains_key("export"),
                "parking export request not durable"
            );
            let checkpoint = history
                .source
                .checkpoint
                .as_ref()
                .context("parking checkpoint absent")?;
            let artifact: Uuid = serde_json::from_value(result["artifact_handle"].clone())?;
            let manifest: GitManifest = serde_json::from_value(result["manifest"].clone())?;
            ensure!(
                !artifact.is_nil()
                    && artifact == manifest.transfer_id
                    && manifest.source == checkpoint.source
                    && manifest.base == history.intent.source.base
                    && manifest.repository_id == history.intent.source.repository_id
                    && manifest.binding_generation == history.intent.source.binding_generation
                    && manifest.repository_identity == history.intent.source.logical_identity,
                "parking export exact original source"
            );
            let mut file = crate::source_transfer::artifact_file(
                &history.intent.managed_root,
                artifact,
                false,
                "manifest.json",
                false,
            )?;
            let before = file.metadata()?;
            ensure!(
                before.len() > 0 && before.len() <= 1_048_576,
                "parking manifest member bound"
            );
            let mut bytes = Vec::new();
            Read::by_ref(&mut file)
                .take(1_048_577)
                .read_to_end(&mut bytes)?;
            ensure!(
                bytes.len() as u64 == before.len(),
                "parking manifest length changed"
            );
            let actual: GitManifest = serde_json::from_slice(&bytes)?;
            ensure!(
                crate::native::journal::canonical_digest(&actual)?
                    == crate::native::journal::canonical_digest(&manifest)?,
                "parking actual exported manifest differs"
            );
            same_member(&before, &file.metadata()?)?;
            let total = (bytes.len() as u64)
                .checked_add(manifest.bundle_size)
                .context("parking artifact length overflow")?;
            ensure!(
                total <= history.intent.artifact_limit_bytes
                    && total <= history.intent.reserved_disk_bytes,
                "parking exported artifact exceeds original reservation"
            );
            if manifest.bundle_size > 0 {
                let mut bundle = crate::source_transfer::artifact_file(
                    &history.intent.managed_root,
                    artifact,
                    false,
                    "objects.bundle",
                    false,
                )?;
                let before = bundle.metadata()?;
                ensure!(
                    before.len() == manifest.bundle_size,
                    "parking bundle actual length"
                );
                let mut hash = Sha256::new();
                let mut seen = 0u64;
                let mut chunk = [0u8; 65536];
                loop {
                    let n = bundle.read(&mut chunk)?;
                    if n == 0 {
                        break;
                    }
                    seen = seen
                        .checked_add(n as u64)
                        .context("parking bundle count overflow")?;
                    ensure!(seen <= manifest.bundle_size, "parking bundle grew");
                    hash.update(&chunk[..n]);
                }
                ensure!(
                    seen == manifest.bundle_size
                        && manifest.bundle_sha256.as_deref()
                            == Some(format!("{:x}", hash.finalize()).as_str()),
                    "parking bundle actual digest"
                );
                same_member(&before, &bundle.metadata()?)?;
            } else {
                ensure!(
                    manifest.reference_only && manifest.bundle_sha256.is_none(),
                    "parking empty export profile"
                );
            }
            custody.check(&pipeline, owner)?;
            Ok(ParkingExport {
                artifact,
                manifest,
                raw_manifest_digest: format!("{:x}", Sha256::digest(&bytes)),
                manifest_bytes: bytes.len() as u64,
                total_bytes: total,
            })
        })
        .await
    }
    pub(crate) async fn parking_upload_step(
        self: &Arc<Self>,
        stopped: &DurableStoppedWorkspace,
    ) -> Result<ProposedUploadStep> {
        let custody = stopped.custody.clone();
        let pipeline = self.clone();
        self.storage(move |owner| {
            let history = custody.check(&pipeline, owner)?;
            let transfer = history
                .source
                .transfer
                .as_ref()
                .context("parking transfer not durable")?;
            let upload = history
                .source
                .upload
                .as_ref()
                .context("parking upload not durable")?;
            let step = crate::source_transfer::steps::propose_upload(
                upload,
                transfer,
                Some(&history.intent.managed_root),
                upload.identity.node_id,
            )?;
            custody.check(&pipeline, owner)?;
            Ok(step)
        })
        .await
    }
}
fn same_member(before: &std::fs::Metadata, after: &std::fs::Metadata) -> Result<()> {
    use std::os::unix::fs::MetadataExt;
    ensure!(
        before.dev() == after.dev()
            && before.ino() == after.ino()
            && before.len() == after.len()
            && before.mtime() == after.mtime()
            && before.mtime_nsec() == after.mtime_nsec(),
        "parking retained artifact member changed"
    );
    Ok(())
}
