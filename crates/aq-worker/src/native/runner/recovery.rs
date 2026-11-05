//! Historical reporting only. No executor, Git custody or admission is recovered.
use super::*;
use crate::native::journal::Attempt;
use aq_admission::result_record;
use profile::TerminalProfile;

#[derive(Debug, PartialEq, Eq)]
pub enum ReconcileDisposition {
    /// A granted launch that never reached Started is durably closed and the
    /// server acknowledged `LaunchClosedAwaitingSettlement` (B5b/AQ-08).
    LaunchClosed,
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
    let mut attempt = journal
        .state()
        .attempts
        .get(&attempt_id)
        .context("native recovery attempt absent")?
        .clone();
    // B5b/AQ-08: a granted Frozen-phase launch with no Running history never
    // started. Stop any host it configured, durably close it, and report the
    // closure; never relaunch it.
    if unstarted(&attempt) {
        return close_unstarted(worker, config, &held, &mut journal, attempt_id).await;
    }
    // MS1 restart drill: a node that died while its host ran left no stop in
    // the journal. Recover the host owner from its sealed intent, request a
    // stop, and retain the actual kernel stopped proof. Never relaunch.
    if let Some(h) = attempt.native_host.as_ref()
        && h.stopped.is_none()
        && let Some(intent) = h.host_intent.as_ref()
    {
        let mut handle = aq_session_host::SessionHost::recover(&h.intent.host_root, intent)?;
        handle.stop()?;
        handle.wait_stopped(std::time::Duration::from_secs(10))?;
        let proof = handle.stopped_proof()?;
        journal.native_stopped(attempt_id, &proof)?;
        tracing::warn!(%attempt_id, "recovered physical host stop after node restart");

        attempt = journal
            .state()
            .attempts
            .get(&attempt_id)
            .context("native recovery attempt absent")?
            .clone();
    }
    // MS1 review: a host stop kills the node-side driver, not its docker-local
    // sandbox, where Claude would keep running with the attempt's token. Stop
    // (never destroy) any still-running sandbox of this attempt.
    if attempt.native_host.is_some() {
        stop_claude_sandbox(config, worker.node_id, attempt_id).await?;
    }
    let client = NativeClient::from_config(worker)?;
    replay(&client, &attempt, |receipt| {
        check_root(config, &held)?;
        receipt.retain(&mut journal, attempt_id)
    })
    .await
}

/// Stops any still-running docker-local sandbox of a Claude docker attempt
/// (its disk is preserved). Other executors have no sandbox.
async fn stop_claude_sandbox(config: &NativeConfig, node: Uuid, attempt: Uuid) -> Result<()> {
    if let Some(super::codex::ExecutorConfig::ClaudeDockerLocalResultLocal { settings, .. }) =
        &config.executor
    {
        super::claude_docker::stop_attempt_sandbox(&settings.host.docker_local, node, attempt)
            .await?;
    }
    Ok(())
}

/// A Frozen-phase launch whose host never recorded Running or a terminal.
fn unstarted(a: &Attempt) -> bool {
    a.phase_acceptance.is_some()
        && a.refresh_acceptance.is_none()
        && a.native_host.as_ref().is_some_and(|h| {
            h.running.is_none()
                && h.started_receipt.is_none()
                && h.terminal.is_none()
                && h.refresh_terminal.is_none()
        })
}

/// Closure evidence for a launch that never started: a durable suppression when
/// no host process was ever started, otherwise the host's own stopped proof.
fn closure_request(
    a: &Attempt,
    revision: u64,
    state: &crate::native::journal::State,
    operation_id: Uuid,
) -> Result<aq_phase_terminal_protocol::ClosureRequest> {
    use aq_admission::granted_closure::{
        DriverIntent, Evidence, HostStoppedProof, JournalCheckpoint, Request, SuppressionRecord,
    };
    let h = a.native_host.as_ref().context("closure host absent")?;
    let i = &h.intent;
    let execution = a.phase_scope()?;
    let previous = JournalCheckpoint {
        revision: state.revision,
        digest: crate::native::journal::canonical_digest(state)?,
    };
    let evidence = match (&h.guardian, &h.stopped) {
        (None, _) if !h.start_intended => {
            let record = SuppressionRecord {
                schema_version: 1,
                operation_id,
                suppression_id: Uuid::new_v4(),
                binding: i.binding.clone(),
                grant_id: i.grant_id,
                guardian_id: i.logical_guardian_id,
                launch_id: i.launch_id,
                reservation_id: i.reservation_id,
                original_kernel_boot_id: execution.kernel_boot_id,
                recording_kernel_boot_id: state.kernel_boot_id,
                previous: previous.clone(),
                terminal_revision: revision,
            };
            let record_digest = record.digest()?;
            Evidence::Suppressed {
                record,
                record_digest,
            }
        }
        (Some(_), Some(stopped)) => {
            let proof = HostStoppedProof {
                observation: serde_json::from_value(serde_json::to_value(&stopped.observation)?)?,
            };
            let host = h
                .host_intent
                .as_ref()
                .context("closure host intent absent")?;
            Evidence::HostStopped {
                driver_intent: Box::new(DriverIntent {
                    binding: i.binding.clone(),
                    grant_id: i.grant_id,
                    guardian_id: i.logical_guardian_id,
                    reservation_id: i.reservation_id,
                    host: serde_json::from_value(serde_json::to_value(host)?)?,
                    journal: previous.clone(),
                }),
                proof_digest: proof.digest()?,
                proof,
            }
        }
        _ => anyhow::bail!("unstarted launch has no closure evidence (host not stopped)"),
    };
    let original = Request {
        schema_version: 1,
        operation_id,
        binding: i.binding.clone(),
        grant_id: i.grant_id,
        guardian_id: i.logical_guardian_id,
        launch_id: i.launch_id,
        reservation_id: i.reservation_id,
        runtime: i.runtime.runtime.clone(),
        qualification: i.runtime.qualification.clone(),
        journal: JournalCheckpoint {
            revision,
            digest: previous.digest,
        },
        evidence,
    };
    crate::native::journal::phase_terminal::with_context(a, |c| {
        let request = aq_phase_terminal_protocol::ClosureRequest {
            schema_version: 1,
            negotiation: aq_phase_terminal_protocol::Negotiation::current()?,
            origin: aq_phase_terminal_protocol::Origin::for_context(c)?,
            original,
        };
        request.validate_for(c)?;
        Ok(request)
    })
}

async fn close_unstarted(
    worker: &WorkerConfig,
    config: &NativeConfig,
    held: &std::fs::File,
    journal: &mut Journal,
    attempt_id: Uuid,
) -> Result<ReconcileDisposition> {
    let attempt = journal
        .state()
        .attempts
        .get(&attempt_id)
        .context("native recovery attempt absent")?
        .clone();
    let h = attempt.native_host.as_ref().unwrap();
    if h.closure.is_none()
        && h.stopped.is_none()
        && let (Some(_), Some(intent)) = (&h.guardian, &h.host_intent)
    {
        // A guardian was started: stop it and retain its actual kernel proof.
        let mut handle = aq_session_host::SessionHost::recover(&h.intent.host_root, intent)?;
        handle.stop()?;
        handle.wait_stopped(std::time::Duration::from_secs(10))?;
        let proof = handle.stopped_proof()?;
        check_root(config, held)?;
        journal.native_stopped(attempt_id, &proof)?;
        tracing::warn!(%attempt_id, "stopped the unstarted launch's host before closure");
    }
    stop_claude_sandbox(config, worker.node_id, attempt_id).await?;
    let operation = Uuid::new_v4();
    check_root(config, held)?;
    let request = journal.native_closure(attempt_id, |a, revision, state| {
        closure_request(a, revision, state, operation)
    })?;
    let recorded = journal
        .state()
        .attempts
        .get(&attempt_id)
        .and_then(|a| a.native_host.as_ref())
        .and_then(|h| h.closure.as_ref())
        .is_some_and(|c| c.receipt.is_some());
    if recorded {
        return Ok(ReconcileDisposition::LaunchClosed);
    }
    let client = NativeClient::from_config(worker)?;
    let receipt = client.phase_terminal_closure(&request).await?;
    check_root(config, held)?;
    journal.native_closure_receipt(attempt_id, receipt)?;
    tracing::warn!(%attempt_id, "granted launch closed before Started; awaiting server settlement");
    Ok(ReconcileDisposition::LaunchClosed)
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
