//! Explicitly ignored actual pinned tmux/public-Git composition. The private
//! factory is test-only; these fake receipts qualify no production owner/wake.
use crate::{
    continuation::{
        history::ParkingHistory,
        orchestration::{checkpoint_stopped_source, prepare_stopped_upload},
        synthetic_retention::SyntheticRetentionOwner,
        transfer::record_upload_progress,
    },
    native::{parking::DurableStoppedWorkspace, preparation::InputPipeline},
    source_transfer::UploadAction,
};
use anyhow::{Result, ensure};
use std::{
    sync::Arc,
    time::{SystemTime, UNIX_EPOCH},
};

fn now_ms() -> Result<u64> {
    Ok(SystemTime::now()
        .duration_since(UNIX_EPOCH)?
        .as_millis()
        .try_into()?)
}

/// The coordinator is a concrete test-only byte-retaining owner. No arbitrary
/// HTTP client/trait or deserialized receipt supplies production authority.
async fn upload_to_fixture_owner(
    pipeline: &Arc<InputPipeline>,
    stopped: &DurableStoppedWorkspace,
) -> Result<(ParkingHistory, SyntheticRetentionOwner)> {
    let initial = prepare_stopped_upload(pipeline, stopped).await?;
    let start = now_ms()?;
    let mut owner = SyntheticRetentionOwner::new(
        &initial,
        start,
        start
            .checked_add(60_000)
            .ok_or_else(|| anyhow::anyhow!("fixture interval overflow"))?,
    )?;
    let transfer = initial.source.transfer.as_ref().unwrap();
    let chunk = aq_worker_protocol::MAX_CHUNK_BYTES as u64;
    let bound = transfer.manifest_bytes.div_ceil(chunk) + transfer.bundle_bytes.div_ceil(chunk) + 3;
    let mut lost_reply_checked = false;
    for _ in 0..bound {
        let history = prepare_stopped_upload(pipeline, stopped).await?;
        if history.source.receipt.is_some() {
            ensure!(
                lost_reply_checked,
                "fixture did not exercise a durable pending chunk replay"
            );
            return Ok((history, owner));
        }
        let mut step = pipeline.parking_upload_step(stopped).await?;
        ensure!(
            serde_json::to_value(&step.state)? == serde_json::to_value(&history.source.upload)?,
            "action state was not already durable"
        );
        let mut status = owner.accept(&step.action, now_ms()?)?;
        if !lost_reply_checked && matches!(&step.action, UploadAction::Chunk { .. }) {
            // Deliberately omit the local acknowledgment after the fake owner
            // accepted bytes. Reobserve the fsync-bound original pending recipe.
            let after_loss = pipeline.parking_history(stopped).await?;
            ensure!(
                after_loss.digest()? == history.digest()?,
                "lost response changed local history"
            );
            let replay = pipeline.parking_upload_step(stopped).await?;
            let (
                UploadAction::Chunk {
                    request: a,
                    bytes: ab,
                },
                UploadAction::Chunk {
                    request: b,
                    bytes: bb,
                },
            ) = (&step.action, &replay.action)
            else {
                anyhow::bail!("expected exact chunk replay");
            };
            ensure!(
                a == b && ab == bb,
                "lost response replaced chunk identity/bytes"
            );
            let replay_status = owner.accept(&replay.action, now_ms()?)?;
            ensure!(
                serde_json::to_value(&status)? == serde_json::to_value(&replay_status)?,
                "owner replay changed result/retention interval"
            );
            status = replay_status;
            step = replay;
            lost_reply_checked = true;
        }
        let proposed = record_upload_progress(&history, &step.action, status)?;
        pipeline
            .parking_commit(stopped, history.digest()?, proposed)
            .await?;
    }
    anyhow::bail!("bounded fixture upload did not obtain exact source receipt")
}

fn fixture_reference(label: &str) -> aq_session_protocol::ArtifactRef {
    aq_session_protocol::ArtifactRef {
        id: aq_session_protocol::Text::new(label).unwrap(),
        version: aq_session_protocol::Text::new("1").unwrap(),
        digest: aq_session_protocol::Digest::of(label.as_bytes()),
    }
}
fn checkpoint_claim(supplied: bool) -> aq_lifecycle_protocol::CheckpointClaim {
    use aq_lifecycle_protocol::{CheckpointClaim, SummaryClaim, SummaryText};
    if !supplied {
        return CheckpointClaim::Missing {
            reason: aq_session_protocol::Text::new(
                "Actual local fixture intentionally supplied no summary",
            )
            .unwrap(),
        };
    }
    let summary = SummaryClaim {
        text: SummaryText::new("Disposable actual local-parking fixture. Source is captured; this is not a real dependency wait or production suspension.").unwrap(),
        context_references:vec![],
        pending_operation_ids:vec![uuid::Uuid::new_v4()],
    };
    let summary_digest = aq_session_protocol::canonical_digest(&summary).unwrap();
    CheckpointClaim::Supplied {
        summary,
        summary_digest,
    }
}

async fn actual_parking_case(supplied: bool) -> Result<()> {
    use crate::native::journal::parking_fixture::ActualParkingFixture;
    use aq_lifecycle_protocol::{CheckpointClaim, ContinuationRef, SuspensionIntent, WaitRef};
    use aq_session_protocol::Digest;
    let mut fixture = ActualParkingFixture::prepare(5_000).await;
    let pipeline = fixture.pipeline();
    let id = fixture.execution().history().attempt_id();
    let original_deadline = fixture.execution().deadline_ns();
    let original_hard_deadline = fixture.execution().hard_deadline_ns();
    let running = fixture.launch().await?;
    let before = pipeline.available().await?;
    let before_state = pipeline.snapshot().await?;
    let resources = before_state.attempts[&id]
        .reservation
        .offer
        .offered
        .offer
        .manifest
        .resources
        .clone();
    let disk = before_state.attempts[&id].reservation.disk_bytes;
    ensure!(
        before_state.attempts[&id]
            .native_host
            .as_ref()
            .is_some_and(|host| host.running.is_some()),
        "actual Running history absent"
    );
    let host_history = before_state.attempts[&id].native_host.as_ref().unwrap();
    let cwd_marker = host_history
        .intent
        .host_root
        .join(host_history.intent.launch_id.to_string())
        .join("fixture-marker.cwd");
    for _ in 0..50 {
        if cwd_marker.is_file() {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    use std::os::unix::ffi::OsStrExt;
    ensure!(
        std::fs::read(&cwd_marker)? == fixture.workspace().as_os_str().as_bytes(),
        "actual pinned program did not record its expected workspace cwd"
    );

    let observed_before_edits = running.observe()?;
    ensure!(
        observed_before_edits.phase == aq_session_host::HostPhase::Running,
        "actual child must still be Running before fixture edits"
    );
    // Fixture-owned edits, not a claim that a model/provider generated changes.
    // These remain below the source owner's original workspace reservation.
    let tracked: &[u8] = if supplied {
        b"WRK16 supplied tracked fixture edit\n"
    } else {
        b"WRK16 missing-summary tracked fixture edit\n"
    };
    let untracked = b"WRK16 disposable untracked fixture bytes\n";
    std::fs::write(fixture.workspace().join("native-input.txt"), tracked)?;
    std::fs::write(fixture.workspace().join("wrk16-untracked.txt"), untracked)?;
    let observed_after_edits = running.observe()?;
    ensure!(
        observed_after_edits.phase == aq_session_host::HostPhase::Running
            && observed_after_edits.guardian == observed_before_edits.guardian
            && observed_after_edits.pane == observed_before_edits.pane,
        "actual child/guardian/pane incarnation changed during fixture edits"
    );
    let issued = now_ms()?;
    let suspension = SuspensionIntent {
        binding: fixture.binding().clone(),
        continuation: ContinuationRef {
            continuation_id: uuid::Uuid::new_v4(),
            generation: 1,
            wait: WaitRef {
                wait_id: uuid::Uuid::new_v4(),
                generation: 1,
                predicate_digest: Digest::of(
                    b"synthetic wait marker, not a verified Graph outcome",
                ),
            },
        },
        issued_at_ms: issued,
        deadline_ms: issued
            .checked_add(30_000)
            .ok_or_else(|| anyhow::anyhow!("fixture deadline overflow"))?,
        max_summary_bytes: 16_384,
        retention_policy: fixture_reference("synthetic-retention-profile-v1"),
    };
    let claim = checkpoint_claim(supplied);
    let fence = pipeline
        .begin_synthetic_parking(
            fixture.execution(),
            suspension,
            claim,
            fixture_reference("explicit-fake-hold-not-production-authority"),
        )
        .await?;
    let stopped = pipeline.stop_parking(fence).await?;
    let stopped_history = pipeline.parking_history(&stopped).await?;
    ensure!(
        stopped_history.stop.is_some() && !stopped_history.settled_source_claim()?,
        "stop must precede source completion"
    );
    let stopped_state = pipeline.snapshot().await?;
    let physical = stopped_state.attempts[&id]
        .native_host
        .as_ref()
        .and_then(|host| host.stopped.as_ref())
        .ok_or_else(|| anyhow::anyhow!("actual fsynced native stopped record absent"))?;
    ensure!(
        physical.observation.kernel_wait_echild
            && physical.observation.reaped_count > 0
            && physical.observation.phase == aq_session_host::HostPhase::Stopped,
        "actual kernel-reaped stopped observation required"
    );
    let held = pipeline.available().await?;
    ensure!(
        held.slots == before.slots
            && held.memory_bytes == before.memory_bytes
            && held.cpu_millis == before.cpu_millis
            && held.disk_bytes == before.disk_bytes,
        "stop alone must not free unsettled Git capacity"
    );

    let captured = checkpoint_stopped_source(&pipeline, &stopped).await?;
    ensure!(
        captured.settled_source_claim()? && captured.completed_source_claim()? == supplied,
        "settled source and summary/migration prerequisites must remain distinct"
    );
    ensure!(
        captured.source.operations.len() == 2
            && captured
                .source
                .operations
                .values()
                .all(|op| op.result.is_some()),
        "both exact Git outbox results required"
    );
    let checkpoint = captured.source.checkpoint.as_ref().unwrap();
    ensure!(
        checkpoint.source != captured.intent.source.base,
        "tracked/untracked fixture changes were not checkpointed"
    );
    let exported = captured.source.exported.as_ref().unwrap();
    ensure!(
        !exported.manifest.reference_only && exported.manifest.bundle_size > 0,
        "actual changed Git bundle absent"
    );
    // Public aq-git creates a distinct local verification checkout at the
    // captured object and retires it afterward. This is content verification,
    // not a successor/Base import or a different-node bootstrap.
    fixture
        .verify_captured_files(&stopped, tracked, untracked)
        .await?;
    let idle = pipeline.available().await?;
    ensure!(
        idle.slots == before.slots.checked_add(1).unwrap()
            && idle.memory_bytes
                == before
                    .memory_bytes
                    .checked_add(resources.memory_bytes)
                    .unwrap()
            && idle.cpu_millis == before.cpu_millis.checked_add(resources.cpu_millis).unwrap(),
        "proven idle compute did not become available"
    );
    ensure!(
        idle.disk_bytes == before.disk_bytes && captured.intent.reserved_disk_bytes == disk,
        "protected disk must remain fully charged after source settlement"
    );

    let (mut retained_source, mut owner) = upload_to_fixture_owner(&pipeline, &stopped).await?;
    let identity = retained_source
        .source
        .upload
        .as_ref()
        .unwrap()
        .identity
        .clone();
    if supplied {
        owner.retain_summary(&retained_source.intent.checkpoint)?;
        let CheckpointClaim::Supplied { summary, .. } = &retained_source.intent.checkpoint.claim
        else {
            anyhow::bail!("supplied fixture changed claim");
        };
        for operation in &summary.pending_operation_ids {
            owner.resolve_fixture_effect(*operation);
        }
        owner.reconcile_fixture_effects(fixture_reference(
            "explicit-fake-owner-current-effects-decision",
        ))?;
        let receipt = owner.finish(&retained_source, &identity, now_ms()?)?;
        ensure!(
            receipt.source.bytes
                == retained_source
                    .source
                    .exported
                    .as_ref()
                    .unwrap()
                    .total_bytes,
            "receipt must count retained wire bytes"
        );
        retained_source.retention = Some(receipt);
        retained_source = pipeline
            .parking_commit(
                &stopped,
                pipeline.parking_history(&stopped).await?.digest()?,
                retained_source,
            )
            .await?;
        ensure!(
            retained_source.retention.is_some(),
            "synthetic profile receipts were not durably recorded"
        );
    } else {
        ensure!(
            owner
                .retain_summary(&retained_source.intent.checkpoint)
                .is_err(),
            "Missing cannot create checkpoint retention"
        );
        ensure!(
            owner
                .reconcile_fixture_effects(fixture_reference(
                    "explicit-fake-owner-current-effects-decision"
                ))
                .is_err(),
            "Missing summary cannot produce migratable reconciliation"
        );
        ensure!(
            owner
                .finish(&retained_source, &identity, now_ms()?)
                .is_err()
                && retained_source.retention.is_none()
                && !retained_source.completed_source_claim()?,
            "Missing summary must remain held for recovery"
        );
    }
    let final_capacity = pipeline.available().await?;
    ensure!(
        final_capacity.slots == idle.slots
            && final_capacity.memory_bytes == idle.memory_bytes
            && final_capacity.cpu_millis == idle.cpu_millis
            && final_capacity.disk_bytes == before.disk_bytes,
        "receipt/summary state changed independent compute/disk projection"
    );
    ensure!(
        fixture.execution().deadline_ns() == original_deadline
            && fixture.execution().hard_deadline_ns() == original_hard_deadline,
        "parking must not extend original execution deadlines"
    );
    let final_state = pipeline.snapshot().await?;
    ensure!(
        serde_json::to_value(&final_state.attempts[&id].phase_execution)?
            == serde_json::to_value(&before_state.attempts[&id].phase_execution)?,
        "parking must not create replacement execution authority"
    );
    fixture.confirm_stopped(&stopped).await?;
    eprintln!(
        "WRK16A actual public-Git/tmux case supplied={supplied}, source_wire_bytes={}, protected_disk_bytes={disk}, compute_released=true, production_retention=false",
        retained_source
            .source
            .exported
            .as_ref()
            .unwrap()
            .total_bytes
    );
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires Root-reviewed retained Git/host/tmux/program pins and explicit physical lease; never ambient replacements"]
async fn actual_pinned_tmux_git_parking_retention_and_missing_summary_keep_disk_protected() {
    tokio::time::timeout(std::time::Duration::from_secs(90), async {
        actual_parking_case(true).await?;
        actual_parking_case(false).await
    })
    .await
    .expect("bounded actual composition timed out; uncertain fixture roots must remain retained")
    .unwrap();
}
