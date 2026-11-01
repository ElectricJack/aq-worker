//! Finite history advancement, separate from native CAS/fsync and live ownership.
use super::{history::*, source, transfer};
use anyhow::{Context, Result, ensure};
use aq_worker_protocol::{MAX_CHUNK_BYTES, TransferFile};
use serde::Serialize;

fn same(a: &impl Serialize, b: &impl Serialize) -> Result<bool> {
    Ok(serde_json::to_value(a)? == serde_json::to_value(b)?)
}
fn result(history: &ParkingHistory, key: &str) -> Option<serde_json::Value> {
    history
        .source
        .operations
        .get(key)
        .and_then(|op| op.result.clone())
}
/// Accepts a single finite proposal (or exact idempotent replay). No caller may
/// combine arbitrary monotonic-looking offsets, rewrite a frozen recipe, or
/// manufacture a stopped anchor through this helper. Authenticated response and
/// actual artifact/proof ownership remain the native pipeline's responsibility.
pub(crate) fn validate_advance(old: &ParkingHistory, next: &ParkingHistory) -> Result<()> {
    old.validate()?;
    next.validate()?;
    if same(old, next)? {
        return Ok(());
    }
    ensure!(
        old.stop.is_some(),
        "parking advance requires actual stopped history"
    );
    let mut expected;
    if !old.source.operations.contains_key("checkpoint") {
        expected = source::plan_checkpoint(
            old,
            next.source
                .checkpoint_head
                .clone()
                .context("checkpoint head absent")?,
            next.source
                .observed_workspace_bytes
                .context("workspace bytes absent")?,
        )?;
    } else if result(old, "checkpoint").is_none() {
        expected = source::record_checkpoint(
            old,
            result(next, "checkpoint").context("checkpoint result absent")?,
        )?;
    } else if !old.source.operations.contains_key("export") {
        expected = source::plan_export(old)?;
    } else if result(old, "export").is_none() {
        expected = source::record_export(
            old,
            result(next, "export").context("export result absent")?,
            next.source
                .exported
                .clone()
                .context("actual export metadata absent")?,
        )?;
    } else if old.source.transfer.is_none() {
        expected = transfer::plan_transfer(old)?;
    } else if !same(&old.source.upload, &next.source.upload)? {
        expected = old.clone();
        let before = old.source.upload.as_ref().context("upload owner absent")?;
        let after = next
            .source
            .upload
            .as_ref()
            .context("upload owner removed")?;
        let upload = expected.source.upload.as_mut().unwrap();
        if !before.begun {
            upload.begun = true;
        } else if let Some(pending) = &before.pending {
            ensure!(
                after.pending.is_none(),
                "pending upload recipe cannot change"
            );
            match pending.file {
                TransferFile::Manifest => {
                    upload.manifest_offset = upload
                        .manifest_offset
                        .checked_add(u64::from(pending.length))
                        .context("manifest offset overflow")?
                }
                TransferFile::Bundle => {
                    upload.bundle_offset = upload
                        .bundle_offset
                        .checked_add(u64::from(pending.length))
                        .context("bundle offset overflow")?
                }
            }
            upload.pending = None;
        } else {
            let pending = after
                .pending
                .as_ref()
                .context("upload progress lacks a durable pending request")?;
            let manifest = old
                .source
                .transfer
                .as_ref()
                .context("upload transfer absent")?;
            let (file, offset, size) = if before.manifest_offset < manifest.manifest_bytes {
                (
                    TransferFile::Manifest,
                    before.manifest_offset,
                    manifest.manifest_bytes,
                )
            } else {
                (
                    TransferFile::Bundle,
                    before.bundle_offset,
                    manifest.bundle_bytes,
                )
            };
            ensure!(
                size > offset
                    && pending.file == file
                    && pending.offset == offset
                    && u64::from(pending.length) == (size - offset).min(MAX_CHUNK_BYTES as u64),
                "pending chunk does not follow finite bounded recipe"
            );
            upload.pending = Some(pending.clone());
        }
    } else if old.source.receipt.is_none() && next.source.receipt.is_some() {
        expected = old.clone();
        let upload = old.source.upload.as_ref().context("upload owner absent")?;
        let manifest = old
            .source
            .transfer
            .as_ref()
            .context("upload transfer absent")?;
        ensure!(
            upload.begun
                && upload.pending.is_none()
                && upload.manifest_offset == manifest.manifest_bytes
                && upload.bundle_offset == manifest.bundle_bytes,
            "retention claim precedes completed upload"
        );
        expected.source.receipt = next.source.receipt.clone();
    } else if old.retention.is_none() && next.retention.is_some() {
        expected = old.clone();
        expected.retention = next.retention.clone();
    } else {
        anyhow::bail!("unsupported parking history advance");
    }
    ensure!(
        same(&expected, next)?,
        "parking advance changed unrelated or immutable facts"
    );
    Ok(())
}
