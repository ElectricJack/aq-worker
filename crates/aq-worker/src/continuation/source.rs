//! Closed history proposals only. The native stopped-workspace owner must recheck
//! actual roots/proof and fsync each proposal before its corresponding I/O.
use super::history::*;
use crate::source::{Checkpoint, GitAction, PendingGitOperation};
use anyhow::{Context, Result, ensure};
use aq_worker_protocol::{ObjectId, Validate as _};
use serde_json::Value;

pub fn plan_checkpoint(
    old: &ParkingHistory,
    head: ObjectId,
    observed_workspace_bytes: u64,
) -> Result<ParkingHistory> {
    old.validate()?;
    ensure!(old.stop.is_some(), "parking source needs stopped anchor");
    if old.source.operations.contains_key("checkpoint") {
        return Ok(old.clone());
    }
    head.validate()?;
    ensure!(
        old.source.checkpoint.is_none() && old.source.exported.is_none(),
        "checkpoint history already present"
    );
    let mut next = old.clone();
    let id = old.intent.operations.checkpoint;
    next.source.checkpoint_head = Some(head.clone());
    next.source.observed_workspace_bytes = Some(observed_workspace_bytes);
    next.source.operations.insert(
        "checkpoint".into(),
        PendingGitOperation {
            operation_id: id,
            request: GitAction::Checkpoint {
                workspace_id: old.intent.workspace.workspace_id,
                generation: old.intent.workspace.generation,
                head,
            }
            .request(&old.intent.source, id),
            result: None,
        },
    );
    next.validate()?;
    Ok(next)
}
pub fn record_checkpoint(old: &ParkingHistory, result: Value) -> Result<ParkingHistory> {
    old.validate()?;
    let pending = old
        .source
        .operations
        .get("checkpoint")
        .context("checkpoint request not durable")?;
    if let Some(previous) = &pending.result {
        ensure!(previous == &result, "checkpoint replay result changed");
        return Ok(old.clone());
    }
    let checkpoint: Checkpoint = serde_json::from_value(result.clone())?;
    if let Some(unchanged) = result.get("unchanged") {
        ensure!(
            unchanged.as_bool()
                == Some(
                    checkpoint.source
                        == *old
                            .source
                            .checkpoint_head
                            .as_ref()
                            .context("checkpoint head absent")?
                ),
            "checkpoint unchanged relationship"
        );
    }
    let mut next = old.clone();
    next.source.checkpoint = Some(checkpoint);
    next.source.operations.get_mut("checkpoint").unwrap().result = Some(result);
    next.validate()?;
    Ok(next)
}
pub fn plan_export(old: &ParkingHistory) -> Result<ParkingHistory> {
    old.validate()?;
    if old.source.operations.contains_key("export") {
        return Ok(old.clone());
    }
    ensure!(
        old.source
            .operations
            .get("checkpoint")
            .and_then(|op| op.result.as_ref())
            .is_some(),
        "checkpoint result is not durable"
    );
    let checkpoint = old
        .source
        .checkpoint
        .as_ref()
        .context("checkpoint absent")?;
    let mut next = old.clone();
    let id = old.intent.operations.export;
    next.source.operations.insert(
        "export".into(),
        PendingGitOperation {
            operation_id: id,
            request: GitAction::Export {
                source: checkpoint.source.clone(),
                retention_handle: checkpoint.retention_handle,
            }
            .request(&old.intent.source, id),
            result: None,
        },
    );
    next.validate()?;
    Ok(next)
}
/// `exported` must come from actual descriptor-relative retained artifact reads,
/// not from treating the aq-git reply as proof of its own file bytes.
pub fn record_export(
    old: &ParkingHistory,
    result: Value,
    exported: ParkingExport,
) -> Result<ParkingHistory> {
    old.validate()?;
    let pending = old
        .source
        .operations
        .get("export")
        .context("export request not durable")?;
    if let Some(previous) = &pending.result {
        ensure!(
            previous == &result
                && serde_json::to_value(
                    old.source
                        .exported
                        .as_ref()
                        .context("export metadata absent")?
                )? == serde_json::to_value(&exported)?,
            "export replay result/bytes changed"
        );
        return Ok(old.clone());
    }
    let mut next = old.clone();
    next.source.exported = Some(exported);
    next.source.operations.get_mut("export").unwrap().result = Some(result);
    next.validate()?;
    ensure!(next.settled_source_claim()?, "both source results required");
    Ok(next)
}
