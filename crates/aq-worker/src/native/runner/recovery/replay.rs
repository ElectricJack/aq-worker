use super::*;
use crate::native::journal::launch::NativeHostHistory;
pub(super) enum Receipt {
    Started(aq_phase_terminal_protocol::StartedReceipt),
    Stopped(aq_phase_terminal_protocol::StoppedReceipt),
    RefreshStarted(aq_phase_terminal_protocol::refresh_v1::StartedReceipt),
    RefreshStopped(aq_phase_terminal_protocol::refresh_v1::StoppedReceipt),
    Result(aq_session_protocol::Digest, result_record::Receipt),
}
impl Receipt {
    pub(super) fn retain(self, journal: &mut Journal, id: Uuid) -> Result<()> {
        match self {
            Self::Started(r) => journal.phase_started_receipt(id, r),
            Self::Stopped(r) => journal.phase_stopped_receipt(id, r),
            Self::RefreshStarted(r) => journal.refresh_started_receipt(id, r),
            Self::RefreshStopped(r) => journal.refresh_stopped_receipt(id, r),
            Self::Result(digest, r) => journal.retain_result_receipt(id, &digest, r),
        }
    }
}

pub(super) async fn replay(
    client: &NativeClient,
    attempt: &Attempt,
    mut retain: impl FnMut(Receipt) -> Result<()>,
) -> Result<ReconcileDisposition> {
    // A missing or mixed history cannot change on retry: the service holds the slot (R1).
    let profile = TerminalProfile::for_attempt(attempt);
    let profile = profile.map_err(|e| RecoveryHold("journal_inconsistent", e))?;
    let host = attempt.native_host.as_ref().unwrap();
    ensure!(
        host.stopped.is_some(),
        "native recovery requires retained physical stop"
    );
    replay_terminal(client, host, profile, &mut retain).await?;
    replay_result(client, attempt, retain).await
}
async fn replay_terminal(
    client: &NativeClient,
    host: &NativeHostHistory,
    profile: TerminalProfile,
    retain: &mut impl FnMut(Receipt) -> Result<()>,
) -> Result<()> {
    match profile {
        TerminalProfile::Frozen => {
            let terminal = host.terminal.as_ref().unwrap();
            let stopped = terminal
                .stopped
                .as_ref()
                .context("native Stopped outbox absent")?;
            if terminal.started.receipt.is_none() {
                retain(Receipt::Started(
                    client
                        .phase_terminal_started(&terminal.started.request)
                        .await?,
                ))?;
            }
            if stopped.receipt.is_none() {
                retain(Receipt::Stopped(
                    client.phase_terminal_stopped(&stopped.request).await?,
                ))?;
            }
        }
        TerminalProfile::Refresh => {
            let terminal = host.refresh_terminal.as_ref().unwrap();
            let stopped = terminal
                .stopped
                .as_ref()
                .context("native Stopped outbox absent")?;
            if terminal.started.receipt.is_none() {
                retain(Receipt::RefreshStarted(
                    client
                        .refresh_terminal_started(&terminal.started.request)
                        .await?,
                ))?;
            }
            if stopped.receipt.is_none() {
                retain(Receipt::RefreshStopped(
                    client.refresh_terminal_stopped(&stopped.request).await?,
                ))?;
            }
        }
    }
    Ok(())
}
async fn replay_result(
    client: &NativeClient,
    attempt: &Attempt,
    mut retain: impl FnMut(Receipt) -> Result<()>,
) -> Result<ReconcileDisposition> {
    let Some(history) = &attempt.result else {
        return Ok(ReconcileDisposition::StopReported);
    };
    history.validate()?;
    if history.receipt.is_some() {
        return Ok(ReconcileDisposition::ResultRecorded);
    }
    let Some(intent) = &history.report else {
        // A missing claim/source receipt cannot be invented from historical data.
        return Ok(ReconcileDisposition::StopReported);
    };
    ensure!(
        intent.bytes == serde_json::to_vec(&intent.request)?,
        "result request bytes changed"
    );
    let receipt = client
        .report(
            &intent.request,
            &history.scope.binding.expected_outcome,
            &history
                .stop
                .as_ref()
                .context("result actual stop absent")?
                .stopped_receipt,
            history.source.pushed_headers(),
        )
        .await?;
    retain(Receipt::Result(intent.digest.clone(), receipt))?;
    Ok(ReconcileDisposition::ResultRecorded)
}
