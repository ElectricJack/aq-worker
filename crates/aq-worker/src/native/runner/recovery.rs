//! Historical reporting only. No executor, Git custody or admission is recovered.
use super::*;
use crate::native::journal::Attempt;
use aq_admission::result_record;
use profile::TerminalProfile;

#[derive(Debug, PartialEq, Eq)]
pub enum ReconcileDisposition {
    /// The stopped lifecycle is acknowledged; no frozen result report exists.
    StopReported,
    /// The exact frozen result report is retained by the server.
    ResultRecorded,
}

enum Receipt {
    Started(aq_phase_terminal_protocol::StartedReceipt),
    Stopped(aq_phase_terminal_protocol::StoppedReceipt),
    RefreshStarted(aq_phase_terminal_protocol::refresh_v1::StartedReceipt),
    RefreshStopped(aq_phase_terminal_protocol::refresh_v1::StoppedReceipt),
    Result(aq_session_protocol::Digest, result_record::Receipt),
}
impl Receipt {
    fn retain(self, journal: &mut Journal, id: Uuid) -> Result<()> {
        match self {
            Self::Started(r) => journal.phase_started_receipt(id, r),
            Self::Stopped(r) => journal.phase_stopped_receipt(id, r),
            Self::RefreshStarted(r) => journal.refresh_started_receipt(id, r),
            Self::RefreshStopped(r) => journal.refresh_stopped_receipt(id, r),
            Self::Result(digest, r) => journal.retain_result_receipt(id, &digest, r),
        }
    }
}

/// Reconcile one preserved, physically stopped attempt using its original IDs
/// and outboxes. Repeated calls are safe after partial delivery/response loss.
/// A successful call does not authorize new execution or release local holds.
pub async fn reconcile_native(
    worker: &WorkerConfig,
    config: &NativeConfig,
    attempt_id: Uuid,
) -> Result<ReconcileDisposition> {
    config.validate()?;
    let held = std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .open(&config.state_dir)?;
    fs2::FileExt::try_lock_exclusive(&held).context("native state scope already owned")?;
    check_root(config, &held)?;
    // Unlike run_once, never create a journal or a new boot/attempt identity.
    let mut journal = Journal::open(
        &config.state_dir.join("journal"),
        worker.node_id,
        config.capacity.clone(),
    )?;
    let attempt = journal
        .state()
        .attempts
        .get(&attempt_id)
        .context("native recovery attempt absent")?
        .clone();
    let client = NativeClient::from_config(worker)?;
    replay(&client, &attempt, |receipt| {
        check_root(config, &held)?;
        receipt.retain(&mut journal, attempt_id)
    })
    .await
}

async fn replay(
    client: &NativeClient,
    attempt: &Attempt,
    mut retain: impl FnMut(Receipt) -> Result<()>,
) -> Result<ReconcileDisposition> {
    let profile = TerminalProfile::for_attempt(attempt)?;
    let host = attempt.native_host.as_ref().unwrap();
    ensure!(
        host.stopped.is_some(),
        "native recovery requires retained physical stop"
    );
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
        )
        .await?;
    retain(Receipt::Result(intent.digest.clone(), receipt))?;
    Ok(ReconcileDisposition::ResultRecorded)
}

#[cfg(test)]
mod tests;
