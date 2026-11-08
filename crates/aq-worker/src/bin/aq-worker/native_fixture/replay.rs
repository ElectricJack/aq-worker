//! In-process fixture reconciliation only. Snapshot data is a refusal guard;
//! only the original live pipeline performs the existing immutable report replay.
use super::*;
use aq_admission::{result_record, transport::WorkerStopEvidence};
use aq_session_protocol::{ArtifactRef, Digest};
use aq_worker::native::{journal::State, result::history::ReportIntent};
use uuid::Uuid;

fn report_matches_stop(report: &ReportIntent, evidence: &WorkerStopEvidence) -> Result<()> {
    result_record::validate_report(&report.request)?;
    ensure!(
        report.bytes == serde_json::to_vec(&report.request)?
            && report.digest == result_record::report_digest(&report.request)?
            && serde_json::to_value(&report.request.stop)? == serde_json::to_value(evidence)?,
        "fixture replay requires the exact retained report and typed stop"
    );
    Ok(())
}
fn candidate(state: &State, id: Uuid) -> Result<(Vec<u8>, Digest, ArtifactRef)> {
    let a = state
        .attempts
        .get(&id)
        .context("fixture replay attempt absent")?;
    let h = a.result.as_ref().context("fixture replay result absent")?;
    h.validate()?;
    let host = a
        .native_host
        .as_ref()
        .context("fixture replay host absent")?;
    let physical = host
        .stopped
        .as_ref()
        .context("fixture replay physical stop absent")?;
    let stop = h
        .stop
        .as_ref()
        .context("fixture replay result stop absent")?;
    ensure!(
        h.receipt.is_none()
            && matches!(
                h.failure,
                Some(aq_agent_result_local::RecoveryCode::ReportUnknown)
            )
            && host.running.is_some()
            && host.uncertain.is_none()
            && physical.observation.phase == aq_session_host::HostPhase::Stopped
            && physical.observation.kernel_wait_echild
            && stop.proof_digest == physical.proof_digest
            && stop.native_stopped_revision == physical.recorded_revision
            && h.scope.binding.launch.attempt_id == id,
        "fixture replay requires stopped report-response uncertainty only"
    );
    macro_rules! paired {
        ($terminal:expr) => {{
            let started = &$terminal.started;
            started
                .receipt
                .as_ref()
                .context("fixture replay Started ACK absent")?
                .validate_for(&started.request)?;
            let stopped = $terminal
                .stopped
                .as_ref()
                .context("fixture replay Stopped absent")?;
            let receipt = stopped
                .receipt
                .as_ref()
                .context("fixture replay Stopped ACK absent")?;
            receipt.validate_for(&stopped.request)?;
            (
                stopped.request.original.evidence.clone(),
                receipt.original.artifact()?,
            )
        }};
    }
    let (evidence, receipt) = match (
        &a.phase_execution,
        &a.refresh_execution,
        &host.terminal,
        &host.refresh_terminal,
    ) {
        (Some(_), None, Some(t), None) => paired!(t),
        (None, Some(_), None, Some(t)) => paired!(t),
        _ => anyhow::bail!("fixture replay missing or mixed terminal profile"),
    };
    ensure!(
        receipt == stop.stopped_receipt,
        "fixture replay stop receipt differs"
    );
    let report = h
        .report
        .as_ref()
        .context("fixture replay exact report absent")?;
    report_matches_stop(report, &evidence)?;
    Ok((report.bytes.clone(), report.digest.clone(), receipt))
}
pub(super) async fn once(pipeline: &Arc<InputPipeline>, id: Uuid) -> Result<()> {
    let before = pipeline.snapshot().await?;
    let (bytes, digest, stopped_receipt) = candidate(&before, id)?;
    // Exactly one existing owner call. No recapture, execution, new operation,
    // admission, deadline/liveness refresh or restart reconstruction occurs.
    let returned = pipeline.replay_result_report(id).await?;
    let after = pipeline.snapshot().await?;
    let h = after
        .attempts
        .get(&id)
        .and_then(|a| a.result.as_ref())
        .context("fixture replay retained result absent")?;
    h.validate()?;
    let report = h
        .report
        .as_ref()
        .context("fixture replay retained report absent")?;
    let retained = h
        .receipt
        .as_ref()
        .context("fixture replay receipt not retained")?;
    ensure!(
        report.bytes == bytes
            && report.digest == digest
            && returned.report_digest == digest
            && returned.stopped_receipt == stopped_receipt
            && serde_json::to_value(retained)? == serde_json::to_value(&returned)?,
        "fixture replay receipt or original request changed"
    );
    let marker = serde_json::json!({"schema_version":1,"phase":"fixture_b1_replay_recorded",
        "attempt_id":id,"operation_id":returned.operation_id,"report_id":returned.report_id,
        "report_digest":returned.report_digest});
    let mut output = std::io::stdout().lock();
    serde_json::to_writer(&mut output, &marker)?;
    output.write_all(b"\n")?;
    output.flush()?;
    Ok(())
}
/// Never converts an execution error to success, even if reconciliation records
/// a receipt. FnOnce also bounds the opt-in recovery to one owner invocation.
pub(super) async fn preserve_execution_error<F, Fut>(
    result: Result<()>,
    enabled: bool,
    replay: F,
) -> Result<()>
where
    F: FnOnce() -> Fut,
    Fut: std::future::Future<Output = Result<()>>,
{
    match result {
        Ok(()) => Ok(()),
        Err(original) => {
            if enabled {
                let _ = replay().await;
            }
            Err(original)
        }
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    #[tokio::test]
    async fn replay_is_opt_in_once_and_never_hides_original_execution_error() {
        let calls = AtomicUsize::new(0);
        for (enabled, replay_ok) in [(false, true), (true, true), (true, false)] {
            let before = calls.load(Ordering::SeqCst);
            let original = anyhow::anyhow!("original execution failure");
            let returned = preserve_execution_error(Err(original), enabled, || async {
                calls.fetch_add(1, Ordering::SeqCst);
                if replay_ok {
                    Ok(())
                } else {
                    anyhow::bail!("replay refusal")
                }
            })
            .await
            .unwrap_err();
            assert_eq!(returned.to_string(), "original execution failure");
            assert_eq!(calls.load(Ordering::SeqCst) - before, usize::from(enabled));
        }
        preserve_execution_error(Ok(()), true, || async {
            calls.fetch_add(1, Ordering::SeqCst);
            Ok(())
        })
        .await
        .unwrap();
        assert_eq!(
            calls.load(Ordering::SeqCst),
            2,
            "successful execution never replays"
        );
    }
    #[test]
    fn frozen_report_guard_rejects_changed_bytes_digest_and_typed_stop() {
        let fixture: serde_json::Value = serde_json::from_slice(include_bytes!(
            "../../../../../../vendor/aq-admission-0.1.10/artifacts/result-record.fixture.json"
        ))
        .unwrap();
        let request: aq_admission::transport::ResultReport =
            serde_json::from_value(fixture["report"].clone()).unwrap();
        let mut report = ReportIntent {
            bytes: serde_json::to_vec(&request).unwrap(),
            digest: result_record::report_digest(&request).unwrap(),
            request,
            recorded_revision: 3,
        };
        let mut evidence = report.request.stop.clone();
        report_matches_stop(&report, &evidence).unwrap();
        report.bytes.push(b' ');
        assert!(report_matches_stop(&report, &evidence).is_err());
        report.bytes.pop();
        let digest = report.digest.clone();
        report.digest = Digest::of(b"changed");
        assert!(report_matches_stop(&report, &evidence).is_err());
        report.digest = digest;
        evidence.stop_operation_id = Uuid::new_v4();
        assert!(report_matches_stop(&report, &evidence).is_err());
    }
    #[test]
    fn empty_history_cannot_authorize_a_replay_candidate() {
        let state = State {
            format: 12,
            identity: aq_admission::transport::WorkerIdentity {
                node_id: Uuid::new_v4(),
                boot_id: Uuid::new_v4(),
            },
            kernel_boot_id: Uuid::new_v4(),
            revision: 1,
            attempts: Default::default(),
        };
        assert!(candidate(&state, Uuid::new_v4()).is_err());
    }
}
