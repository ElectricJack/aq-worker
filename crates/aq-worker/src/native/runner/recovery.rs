//! Historical reporting only. No executor, Git custody or admission is recovered.
use super::codex::ExecutorConfig;
use super::*;
use crate::backend::{
    SandboxIdentity, SandboxState,
    docker_local::{
        DockerLocalBackend,
        tar::{self, TreeEntry},
    },
};
use crate::native::journal::{Attempt, InconsistentJournal};
use crate::sandbox_driver as driver;
use aq_admission::result_record;
use aq_agent_result_local as local;
use aq_handoff_note::{preserved_claim_bytes as preserved, salvage_note};
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
    /// R1: it failed before any execution authority (in preparation). Nothing ran and nothing
    /// is owed: the server closes its pre-grant claim itself.
    NeverExecuted,
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

/// The state-dir lock, so a running node never shares the scope with recovery.
fn lock_scope(config: &NativeConfig) -> Result<std::fs::File> {
    let held = std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .open(&config.state_dir)?;
    fs2::FileExt::try_lock_exclusive(&held).context("native state scope already owned")?;
    check_root(config, &held)?;
    Ok(held)
}
/// Attempt ids in an existing state dir's journal (empty when no journal exists). Takes the
/// state-dir lock, so it fails while a running node owns the scope.
pub fn retained_attempts(config: &NativeConfig, node: Uuid) -> Result<Vec<Uuid>> {
    attempts_where(config, node, |_| true)
}
/// The retained attempts whose result claim is recorded: their work is in the worktree.
pub fn claimed_attempts(config: &NativeConfig, node: Uuid) -> Result<Vec<Uuid>> {
    // A preserved sandbox's claim says so, durably (its driver-dir marker is runtime): kept.
    attempts_where(config, node, |a| {
        (a.result.as_ref().and_then(|h| h.claim.as_ref())).is_some_and(|c| !preserved(&c.bytes))
    })
}
fn attempts_where(
    config: &NativeConfig,
    node: Uuid,
    keep: fn(&Attempt) -> bool,
) -> Result<Vec<Uuid>> {
    let root = config.state_dir.join("journal");
    if std::fs::symlink_metadata(&root).is_err_and(|e| e.kind() == std::io::ErrorKind::NotFound) {
        return Ok(vec![]);
    }
    let _held = lock_scope(config)?;
    let journal = Journal::open(&root, node, config.capacity.clone())?;
    let attempts = &journal.state().attempts;
    Ok(attempts
        .iter()
        .filter(|(_, a)| keep(a))
        .map(|(id, _)| *id)
        .collect())
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
    let held = lock_scope(config)?;
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
    if never_executed(&attempt) {
        return Ok(ReconcileDisposition::NeverExecuted);
    }
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
        let last = h.running.as_ref().map(|r| &r.observation);
        let proof = aq_session_host::recover_stopped(&h.intent.host_root, intent, last)?;
        journal.native_stopped(attempt_id, &proof)?;
        let how = proof.observation().stop_reason.clone();
        tracing::warn!(%attempt_id, ?how, "recovered physical host stop after node restart");

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
    if let Some(h) = attempt.native_host.as_ref() {
        stop_claude_sandbox(config, worker.node_id, attempt_id).await?;
        crate::native::launch::wipe_claude_driver_secrets(
            &crate::native::launch::claude_driver_dir(&h.intent.host_root, h.intent.launch_id),
        );
    }
    let client = NativeClient::from_config(worker)?;
    replay(&client, &attempt, |receipt| {
        check_root(config, &held)?;
        receipt.retain(&mut journal, attempt_id)
    })
    .await
}

#[derive(Debug, PartialEq, Eq)]
pub enum SalvageDisposition {
    AlreadyReported,
    NotStarted,
    Salvaged,
    Unsalvageable(&'static str),
}
/// W4: after a crash or reboot, reports an interrupted docker-local attempt as a
/// Failed handoff result carrying the workspace its stopped sandbox saved, so its
/// successor starts from that work. Never invents a success; repeated calls are
/// safe (a recorded report gives AlreadyReported and posts nothing). The sandbox
/// is left stopped: releasing it stays an operator action.
pub async fn salvage_interrupted(
    worker: &WorkerConfig,
    config: &NativeConfig,
    attempt: Uuid,
) -> Result<SalvageDisposition> {
    if let Some(done) = settled(reconcile_native(worker, config, attempt).await?) {
        return Ok(done);
    }
    let Some(ExecutorConfig::ClaudeDockerLocalResultLocal { settings, .. }) = &config.executor
    else {
        return Ok(SalvageDisposition::Unsalvageable("no_sandbox_executor"));
    };
    let held = lock_scope(config)?;
    let root = config.state_dir.join("journal");
    let journal = Journal::open(&root, worker.node_id, config.capacity.clone())?;
    let a = journal
        .state()
        .attempts
        .get(&attempt)
        .context("native recovery attempt absent")?;
    let (Some(h), Some(host)) = (a.result.clone(), a.native_host.as_ref()) else {
        return Ok(SalvageDisposition::Unsalvageable("result_scope_absent"));
    };
    let roots = crate::native::result::salvage_roots(&h)?;
    // A handoff the service requested before the cut keeps its reason.
    let dir =
        crate::native::launch::claude_driver_dir(&host.intent.host_root, host.intent.launch_id);
    let reason = driver::read_handoff_marker(&dir.join(driver::HANDOFF_MARKER));
    let reason = reason.unwrap_or(aq_protocol::HandoffReason::Restart);
    // A retained claim was submitted by the driver after its copy-back: keep it.
    let claim = if h.claim.is_some() {
        None
    } else {
        let mut backend =
            DockerLocalBackend::connect_unix(settings.host.docker_local.clone()).await?;
        let sandboxes = stopped_sandboxes(&mut backend, worker.node_id, attempt).await?;
        let max = settings.host.max_workspace_bytes;
        let bound = usize::try_from(max)?.saturating_mul(2);
        let archive =
            async |s: &SandboxIdentity| Ok(backend.get_archive(s, "/aq/work", bound).await?);
        let proof = host.stopped.as_ref().map(|s| &s.observation.stop_reason);
        match salvage_claim(&h, (reason, proof), &sandboxes, max, archive).await? {
            Ok(claim) => Some(claim),
            Err(why) => return Ok(SalvageDisposition::Unsalvageable(why)),
        }
    };
    let git = GitClient::new(&config.git_socket, &config.git_root)?;
    let transfer = crate::transfer_client::TransferClient::from_config(worker, 1)?;
    let client = NativeClient::from_config(worker)?;
    check_root(config, &held)?;
    crate::native::result::finish_salvage(journal, attempt, claim, roots, git, &transfer, &client)
        .await?;
    tracing::warn!(%attempt, ?reason, "interrupted attempt salvaged as a failed handoff result");
    Ok(SalvageDisposition::Salvaged)
}
/// A reconciled attempt that needs no salvage: its report is recorded, or it never started.
fn settled(reconciled: ReconcileDisposition) -> Option<SalvageDisposition> {
    match reconciled {
        ReconcileDisposition::ResultRecorded => Some(SalvageDisposition::AlreadyReported),
        ReconcileDisposition::LaunchClosed | ReconcileDisposition::NeverExecuted => {
            Some(SalvageDisposition::NotStarted)
        }
        ReconcileDisposition::StopReported => None,
    }
}
/// The attempt's stopped sandboxes (reconcile_native stopped any still running).
async fn stopped_sandboxes(
    backend: &mut impl crate::backend::PlacementBackend,
    node: Uuid,
    attempt: Uuid,
) -> Result<Vec<SandboxIdentity>> {
    let owned = backend.list_owned(node).await?.into_iter();
    let mine =
        owned.filter(|e| e.sandbox.owner.attempt_id == attempt && e.state == SandboxState::Stopped);
    Ok(mine.map(|e| e.sandbox).collect())
}
/// Copies the stopped sandbox's `/aq/work` into the attempt's workspace and returns the
/// handoff claim, or why the work cannot be salvaged.
async fn salvage_claim(
    h: &crate::native::result::history::ResultHistory,
    (reason, proof): (aq_protocol::HandoffReason, Option<&Option<String>>),
    sandboxes: &[SandboxIdentity],
    max_workspace_bytes: u64,
    archive: impl AsyncFnOnce(&SandboxIdentity) -> Result<Vec<u8>>,
) -> Result<Result<local::Submission, &'static str>> {
    let [sandbox] = sandboxes else {
        return Ok(Err(if sandboxes.is_empty() {
            "sandbox_absent"
        } else {
            "sandbox_ambiguous"
        }));
    };
    let entries = tar::read_tree(&archive(sandbox).await?, max_workspace_bytes)?;
    let secret = |b: &[u8]| {
        aq_agent_channel::holds_agent_credential(b) || aq_agent_channel::holds_claude_token_shape(b)
    };
    if entries.iter().any(|e| {
        secret(e.path().as_bytes())
            || matches!(e, TreeEntry::File { data, .. } if secret(data))
            || matches!(e, TreeEntry::Symlink { target, .. } if secret(target.as_bytes()))
    }) {
        return Ok(Err("credential_in_workspace"));
    }
    let changed = driver::sync_tree(&h.scope.workspace.path, &entries)?;
    let conclusion = local::Conclusion::new(salvage_note(reason, &changed, proof))?;
    let input = local::SubmissionInput {
        schema_version: 1,
        conclusion,
    };
    Ok(Ok(local::Submission::new(h.scope.binding.clone(), input)?))
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

/// No execution authority, host or result was ever recorded (it failed in preparation).
fn never_executed(a: &Attempt) -> bool {
    a.initial_grant.is_none()
        && a.phase_execution.is_none()
        && a.refresh_execution.is_none()
        && a.native_host.is_none()
        && a.result.is_none()
}
/// A Frozen-phase launch whose host never recorded Running or a terminal.
fn unstarted(a: &Attempt) -> bool {
    (a.phase_acceptance.is_some() != a.refresh_acceptance.is_some())
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
) -> Result<crate::native::journal::launch::ClosureRequest> {
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
    use crate::native::journal::launch::ClosureRequest;
    if a.refresh_acceptance.is_some() {
        use aq_phase_terminal_protocol::refresh_v1 as wire;
        return crate::native::journal::refresh_terminal::with_context(a, |c| {
            let request = wire::ClosureRequest {
                schema_version: 1,
                negotiation: wire::Negotiation::current()?,
                origin: wire::Origin::for_context(c)?,
                original,
            };
            request.validate_for(c)?;
            Ok(ClosureRequest::Refresh(request))
        });
    }
    crate::native::journal::phase_terminal::with_context(a, |c| {
        let request = aq_phase_terminal_protocol::ClosureRequest {
            schema_version: 1,
            negotiation: aq_phase_terminal_protocol::Negotiation::current()?,
            origin: aq_phase_terminal_protocol::Origin::for_context(c)?,
            original,
        };
        request.validate_for(c)?;
        Ok(ClosureRequest::Frozen(request))
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
        let proof = aq_session_host::recover_stopped(&h.intent.host_root, intent, None)?;
        check_root(config, held)?;
        journal.native_stopped(attempt_id, &proof)?;
        tracing::warn!(%attempt_id, "stopped the unstarted launch's host before closure");
    }
    stop_claude_sandbox(config, worker.node_id, attempt_id).await?;
    crate::native::launch::wipe_claude_driver_secrets(&crate::native::launch::claude_driver_dir(
        &h.intent.host_root,
        h.intent.launch_id,
    ));
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
    let receipt = client.launch_closure(&request).await?;
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
    // A missing or mixed history cannot change on retry: the service holds the slot (R1).
    let profile = TerminalProfile::for_attempt(attempt).map_err(InconsistentJournal)?;
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
