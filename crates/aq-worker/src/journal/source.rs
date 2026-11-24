//! Verify the frozen source assignment, retained artifacts and transfer receipts.
use super::*;
use crate::source::SourceState;
pub(super) fn validate(r: &AttemptRecord, source: &SourceState, node_id: Uuid) -> Result<()> {
    if source.report_sealed {
        validate_sealed(r, source)?;
    }
    if let Some(download) = &source.download {
        download.validate(&r.offer, &source.binding)?;
    }
    if let Some(upload) = &source.upload {
        upload.validate(
            source
                .transfer
                .as_ref()
                .context("upload manifest missing")?,
            node_id,
        )?;
    }
    ensure!(
        r.offer
            .spec
            .bindings
            .as_ref()
            .and_then(|b| b.source.as_ref())
            == Some(&source.binding),
        "corrupt frozen source binding"
    );
    ensure!(
        source.operations.len() <= 64
            && source.managed_root.as_ref().is_none_or(|p| p.is_absolute()),
        "corrupt source runtime state"
    );
    for op in source.operations.values() {
        ensure!(
            !op.operation_id.is_nil()
                && op.request["operation_id"] == op.operation_id.to_string()
                && op.request["repository_id"] == source.binding.repository_id.to_string(),
            "corrupt source operation binding"
        );
    }
    validate_receipt(source)
}
fn validate_sealed(r: &AttemptRecord, source: &SourceState) -> Result<()> {
    let report = r.report.as_ref().context("sealed report missing")?;
    report.validate()?;
    let transfer = source
        .transfer
        .as_ref()
        .context("sealed transfer missing")?;
    let receipt = source.receipt.as_ref().context("sealed receipt missing")?;
    ensure!(
        source.upload.is_some()
            && report.identity == r.offer.identity
            && report.attempt == r.offer.attempt
            && report.resolved_spec_digest == r.offer.spec.resolved_spec_digest
            && report.supersedes_report_id.is_none()
            && sealed_artifact_matches(report, transfer, receipt),
        "corrupt sealed source report"
    );
    Ok(())
}
fn validate_receipt(source: &SourceState) -> Result<()> {
    if let Some(receipt) = &source.receipt {
        receipt.validate()?;
        ensure!(
            source
                .transfer
                .as_ref()
                .is_some_and(|t| t.transfer_id == receipt.transfer_id)
                && source
                    .checkpoint
                    .as_ref()
                    .is_some_and(|c| c.source == receipt.source)
                && receipt.repository_id == source.binding.repository_id
                && receipt.binding_generation == source.binding.binding_generation,
            "corrupt source retention association"
        );
    }
    Ok(())
}

fn sealed_artifact_matches(
    report: &ResultReport,
    transfer: &TransferManifest,
    receipt: &CentralRetentionReceipt,
) -> bool {
    report.artifacts.len() == 1
        && report.artifacts[0].artifact_id == transfer.artifact_id
        && report.artifacts[0].digest == transfer.manifest_digest
        && Some(report.artifacts[0].bytes)
            == transfer.manifest_bytes.checked_add(transfer.bundle_bytes)
        && report.artifacts[0].central_receipt.as_deref()
            == Some(receipt.receipt_id.to_string().as_str())
}
