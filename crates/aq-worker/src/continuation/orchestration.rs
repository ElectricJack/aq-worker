//! Synthetic local-parking source composition through the retained physical
//! owner. No history/DTO can construct the token accepted here.
use super::{history::ParkingHistory, source};
use crate::native::{parking::DurableStoppedWorkspace, preparation::InputPipeline};
use anyhow::{Context, Result, bail};
use std::sync::Arc;

/// Drives only the two source outbox operations. A failure preserves the actual
/// opaque stopped owner at the caller and its original durable operation IDs.
/// The return value is metadata, not central retention or resumed authority.
#[cfg(any(test, feature = "test-support"))]
pub async fn checkpoint_stopped_source(
    pipeline: &Arc<InputPipeline>,
    stopped: &DurableStoppedWorkspace,
) -> Result<ParkingHistory> {
    // Two intent/result pairs plus a terminal observation. This cannot become
    // an unbounded recovery/retry engine or create replacement operation IDs.
    for _ in 0..5 {
        let history = pipeline.parking_history(stopped).await?;
        if history.settled_source_claim()? {
            return Ok(history);
        }
        let expected = history.digest()?;
        let proposed = if !history.source.operations.contains_key("checkpoint") {
            let observed = pipeline.parking_observe_source(stopped).await?;
            source::plan_checkpoint(&history, observed.head, observed.workspace_bytes)?
        } else if history.source.operations["checkpoint"].result.is_none() {
            let result = pipeline.parking_git(stopped, "checkpoint").await?;
            source::record_checkpoint(&history, result)?
        } else if !history.source.operations.contains_key("export") {
            source::plan_export(&history)?
        } else {
            let pending = history
                .source
                .operations
                .get("export")
                .context("export intent absent")?;
            if pending.result.is_some() {
                bail!("terminal source results lack a validated complete projection");
            }
            let result = pipeline.parking_git(stopped, "export").await?;
            let exported = pipeline
                .parking_inspect_export(stopped, result.clone())
                .await?;
            source::record_export(&history, result, exported)?
        };
        pipeline.parking_commit(stopped, expected, proposed).await?;
    }
    bail!("bounded source sequence did not reach settled results")
}

/// Prepares at most one upload outbox step through the actual retained source
/// owner. Returns only durable metadata, never a client, send permission or
/// retention claim. The synthetic fixture's explicit coordinator exchange is
/// separate; an unchanged Begin/Finalize step performs no journal rewrite.
#[cfg(any(test, feature = "test-support"))]
pub async fn prepare_stopped_upload(
    pipeline: &Arc<InputPipeline>,
    stopped: &DurableStoppedWorkspace,
) -> Result<ParkingHistory> {
    let mut history = pipeline.parking_history(stopped).await?;
    if history.source.receipt.is_some() {
        return Ok(history);
    }
    if history.source.transfer.is_none() {
        let proposed = super::transfer::plan_transfer(&history)?;
        history = pipeline
            .parking_commit(stopped, history.digest()?, proposed)
            .await?;
    }
    let step = pipeline.parking_upload_step(stopped).await?;
    let mut proposed = history.clone();
    proposed.source.upload = Some(step.state);
    if proposed.digest()? != history.digest()? {
        return pipeline
            .parking_commit(stopped, history.digest()?, proposed)
            .await;
    }
    Ok(history)
}
