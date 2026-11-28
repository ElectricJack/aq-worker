//! Historical reporting only. No executor, Git custody or admission is recovered.
use super::codex::ExecutorConfig;
use super::*;
use crate::backend::{
    SandboxIdentity, SandboxState,
    docker_local::{DockerLocalBackend, tar::TreeEntry},
};
use crate::native::journal::{Attempt, RecoveryHold};
use crate::sandbox_driver as driver;
use aq_admission::{
    control::{CurrentPhase, FailureCode, ReconcileEntry},
    result_record,
    transport::{JournalEvidence, Reconcile, ReconcileAttempt},
};
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
    /// R1: it failed in preparation before its Prepare was journaled. Nothing ran and nothing
    /// is owed: the server closes its pre-grant claim itself.
    NeverExecuted,
}

mod closure;
use closure::closure_request;
mod replay;
#[cfg(test)]
use replay::Receipt;
use replay::replay;

/// The state-dir lock, so a running node never shares the scope with recovery.
pub fn lock_recovery_scope(config: &NativeConfig) -> Result<std::fs::File> {
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
    let _held = lock_recovery_scope(config)?;
    let journal = Journal::open(&root, node, config.capacity.clone())?;
    let attempts = &journal.state().attempts;
    Ok(attempts
        .iter()
        .filter(|(_, a)| keep(a))
        .map(|(id, _)| *id)
        .collect())
}

/// Registers the repository an attempt's recorded offer names with this slot's aq-git, from
/// the offer's own binding. Recovery restarts aq-git on the attempt's state, which may hold
/// the registration (accepted when it is this binding) or predate it (an attempt that was
/// interrupted after its offer was recorded). The worker has no configured repositories, so
/// this is how recovery gets one. A binding that differs (another generation or identity) is
/// refused. `false` when the attempt recorded no source.
pub async fn register_recorded_source(
    worker: &WorkerConfig,
    config: &NativeConfig,
    attempt: Uuid,
) -> Result<bool> {
    let binding = {
        let _held = lock_recovery_scope(config)?;
        let root = config.state_dir.join("journal");
        let journal = Journal::open(&root, worker.node_id, config.capacity.clone())?;
        let recorded = journal.state().attempts.get(&attempt);
        let recorded = recorded.context("native recovery attempt absent")?;
        recorded
            .source
            .as_ref()
            .and_then(|s| s.manifest.as_ref())
            .map(|m| m.source.clone())
    };
    let Some(binding) = binding else {
        return Ok(false);
    };
    let git = config.git_client(worker)?;
    tokio::task::spawn_blocking(move || git.ensure_registered(&binding)).await??;
    Ok(true)
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
    let held = lock_recovery_scope(config)?;
    // Unlike run_once, never create a journal or a new boot/attempt identity.
    let mut journal = Journal::open(
        &config.state_dir.join("journal"),
        worker.node_id,
        config.capacity.clone(),
    )?;
    // R4: boxed, as are the inner futures below: an Attempt is about 35 KiB.
    let mut attempt = attempt_of(&journal, attempt_id)?;
    match pre_launch(&attempt) {
        Some(PreLaunch::NeverExecuted) => return Ok(ReconcileDisposition::NeverExecuted),
        Some(PreLaunch::PrepareUnconfirmed) => {
            let client = NativeClient::from_config(worker)?;
            return Box::pin(reconcile_prepared(&client, journal.state(), &attempt)).await;
        }
        Some(PreLaunch::Granted) => {
            return Err(RecoveryHold(
                "launch_closure_pending",
                anyhow::anyhow!("execution granted, no launch recorded"),
            )
            .into());
        }
        None => {}
    }
    // B5b/AQ-08: a granted Frozen-phase launch with no Running history never
    // started. Stop any host it configured, durably close it, and report the
    // closure; never relaunch it.
    if unstarted(&attempt) {
        return Box::pin(close_unstarted(
            worker,
            config,
            (&held, &mut journal),
            attempt_id,
        ))
        .await;
    }
    // MS1 restart drill: a node that died while its host ran left no stop in
    // the journal. Recover the host owner from its sealed intent, request a
    // stop, and retain the actual kernel stopped proof. Never relaunch.
    if let Some(h) = attempt.native_host.as_ref().filter(|h| h.stopped.is_none())
        && let Some(intent) = &h.host_intent
    {
        let last = h.running.as_ref().map(|r| &r.observation);
        let proof = aq_session_host::recover_stopped(&h.intent.host_root, intent, last)?;
        journal.native_stopped(attempt_id, &proof)?;
        let how = proof.observation().stop_reason.clone();
        tracing::warn!(%attempt_id, ?how, "recovered physical host stop after node restart");

        attempt = attempt_of(&journal, attempt_id)?;
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
    if let Some(done) = settled(Box::pin(reconcile_native(worker, config, attempt)).await?) {
        return Ok(done);
    }
    let Some(ExecutorConfig::ClaudeDockerLocalResultLocal { settings, .. }) = &config.executor
    else {
        return Ok(SalvageDisposition::Unsalvageable("no_sandbox_executor"));
    };
    let held = lock_recovery_scope(config)?;
    let root = config.state_dir.join("journal");
    let journal = Journal::open(&root, worker.node_id, config.capacity.clone())?;
    let a = journal
        .state()
        .attempts
        .get(&attempt)
        .context("native recovery attempt absent")?;
    let (Some(h), Some(host)) = (a.result.clone().map(Box::new), a.native_host.as_ref()) else {
        return Ok(SalvageDisposition::Unsalvageable("result_scope_absent"));
    };
    let roots = crate::native::result::salvage_roots(&h)?;
    // A handoff the service requested before the cut keeps its reason.
    let dir =
        crate::native::launch::claude_driver_dir(&host.intent.host_root, host.intent.launch_id);
    let reason = driver::read_handoff_marker(&dir.join(driver::HANDOFF_MARKER));
    let reason = reason.unwrap_or(aq_protocol::HandoffReason::Restart);
    // A normal driver claim already follows copy-back. Older preserved or
    // salvaged claims may have been journaled after an unverified import.
    let needs_staging_evidence = h.salvaged.is_some()
        || h.claim
            .as_ref()
            .is_some_and(|claim| preserved(&claim.bytes));
    let claim = if h.claim.is_some() && !needs_staging_evidence {
        None
    } else {
        let mut backend =
            DockerLocalBackend::connect_unix(settings.host.docker_local.clone()).await?;
        let sandboxes = stopped_sandboxes(&mut backend, worker.node_id, attempt).await?;
        let container_id = match sandboxes.as_slice() {
            [sandbox] => backend.container_id(sandbox).await?,
            _ => None,
        };
        if h.claim.is_some()
            && let Err(reason) = driver::staging::recoverable(
                &h.scope.workspace.path,
                &sandboxes,
                container_id.as_deref(),
            )
        {
            return Ok(SalvageDisposition::Unsalvageable(reason));
        }
        let max = settings.host.max_workspace_bytes;
        let backend = &backend;
        let archive = async |s: &SandboxIdentity| {
            let entries = backend.stopped_work_tree(s, max).await?;
            let siblings = driver::projects::returned_trees(
                &h.scope.workspace.path,
                driver::projects::after_tree(max, &entries)?,
                |project, limit| async move {
                    Ok(backend.stopped_project_tree(s, project, limit).await?)
                },
            )
            .await?;
            driver::projects::refuse_credentials(&entries, &siblings)?;
            ensure!(
                container_id.is_some() && backend.container_id(s).await? == container_id,
                "sandbox container changed during salvage"
            );
            driver::projects::apply(&siblings)?;
            Ok(entries)
        };
        let proof = host.stopped.as_ref().map(|s| &s.observation.stop_reason);
        if h.claim.is_some() {
            None
        } else {
            match Box::pin(salvage_claim(
                &h,
                (reason, proof),
                (&sandboxes, container_id.as_deref()),
                archive,
            ))
            .await?
            {
                Ok(claim) => Some(claim),
                Err(why) => return Ok(SalvageDisposition::Unsalvageable(why)),
            }
        }
    };
    let git = config.git_client(worker)?;
    let transfer = crate::transfer_client::TransferClient::from_config(worker, 1)?;
    let client = NativeClient::from_config(worker)?;
    check_root(config, &held)?;
    let finish = crate::native::result::finish_salvage(
        journal, attempt, claim, roots, git, &transfer, &client,
    );
    Box::pin(finish).await?;
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
    (sandboxes, container_id): (&[SandboxIdentity], Option<&str>),
    archive: impl AsyncFnOnce(&SandboxIdentity) -> Result<Vec<TreeEntry>>,
) -> Result<Result<local::Submission, &'static str>> {
    let (sandbox, phase) =
        match driver::staging::recoverable(&h.scope.workspace.path, sandboxes, container_id) {
            Ok(proven) => proven,
            Err(reason) => return Ok(Err(reason)),
        };
    let changed = match phase {
        driver::staging::Phase::PrepareFailed => Vec::new(),
        driver::staging::Phase::Started => {
            let entries = archive(sandbox).await?;
            if aq_sandbox_archive::workspace::credential_shaped(&entries) {
                return Ok(Err("credential_in_workspace"));
            }
            driver::sync_tree(&h.scope.workspace.path, &entries)?
        }
    };
    if let Ok(Some(original)) = crate::native::result::retained_submission(&h.scope) {
        return Ok(Ok(original));
    }
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

/// A boxed copy of the attempt's history, made off the async frames (about 35 KiB).
fn attempt_of(journal: &Journal, id: Uuid) -> Result<Box<Attempt>> {
    let a = journal.state().attempts.get(&id);
    Ok(Box::new(
        a.context("native recovery attempt absent")?.clone(),
    ))
}
#[derive(Debug, PartialEq, Eq)]
enum PreLaunch {
    NeverExecuted,
    PrepareUnconfirmed,
    Granted,
}

/// Disposition of an attempt with no host or result history.
fn pre_launch(a: &Attempt) -> Option<PreLaunch> {
    if a.native_host.is_some() || a.result.is_some() {
        return None;
    }
    let granted = a.initial_grant.is_some() || a.phase_execution.is_some();
    let prepared = a.prepared.is_some() || a.phase_prepared.is_some();
    Some(
        match (
            granted || a.refresh_execution.is_some(),
            prepared || a.refresh_prepared.is_some(),
        ) {
            (false, false) => PreLaunch::NeverExecuted,
            (false, true) => PreLaunch::PrepareUnconfirmed,
            (true, _) => PreLaunch::Granted,
        },
    )
}

async fn reconcile_prepared(
    client: &NativeClient,
    state: &crate::native::journal::State,
    attempt: &Attempt,
) -> Result<ReconcileDisposition> {
    let reservation = &attempt.reservation;
    let request = Reconcile {
        schema_version: 1,
        operation_id: Uuid::new_v4(),
        identity: state.identity.clone(),
        after_attempt: None,
        limit: 1,
        attempts: vec![ReconcileAttempt {
            binding: reservation.offer.offered.offer.binding.clone(),
            guardian_id: reservation.guardian_id,
            launch_id: reservation.launch_id,
            last_grant_id: None,
            journal: JournalEvidence::Recorded {
                revision: state.revision,
                digest: crate::native::journal::canonical_digest(state)?,
            },
        }],
    };
    let page = client.reconcile(&request).await?;
    match page.attempts.as_slice() {
        [ReconcileEntry::Current { authority }] => match &authority.phase {
            CurrentPhase::NeverGranted { closure } => {
                ensure!(closure.version.as_str() == "1"
                    && closure.id.as_str().strip_prefix("aq.never-granted/")
                        .is_some_and(|id| Uuid::parse_str(id).is_ok()),
                    "invalid never-granted closure identity");
                tracing::info!(attempt = %authority.binding.attempt_id, closure = %closure.id.as_str(),
                    "closed preparation confirmed by server; ready to archive the slot");
                Ok(ReconcileDisposition::NeverExecuted)
            }
            CurrentPhase::Released { release }
                if authority.versions.grant_revision > 0
                    && release.version.as_str() == "1"
                    && release.id.as_str() == format!("aq.granted-never-started/{}", reservation.offer.offered.offer.binding.attempt_id) =>
            {
                tracing::info!(attempt = %authority.binding.attempt_id, receipt = %release.id.as_str(),
                    "server settled the undelivered launch grant; ready to archive the slot");
                Ok(ReconcileDisposition::NeverExecuted)
            }
            CurrentPhase::Released { .. } | CurrentPhase::Stopped { .. }
            | CurrentPhase::Granted { started_operation_id: Some(_), .. } => Err(RecoveryHold(
                "prepare_history_conflict",
                anyhow::anyhow!("server history cannot establish that the launch missing from this journal never started"),
            ).into()),
            _ => anyhow::bail!("waiting for AQ to settle this preparation or its undelivered grant"),
        },
        [ReconcileEntry::Refused { code: FailureCode::UnknownOrNotOwned, .. }] => Err(RecoveryHold(
            "prepare_reconciliation_scope_refused",
            anyhow::anyhow!("the server could not identify this preparation in the worker's scope"),
        ).into()),
        _ => anyhow::bail!("waiting for AQ to settle this preparation or its undelivered grant"),
    }
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

async fn close_unstarted(
    worker: &WorkerConfig,
    config: &NativeConfig,
    (held, journal): (&std::fs::File, &mut Journal),
    attempt_id: Uuid,
) -> Result<ReconcileDisposition> {
    let attempt = attempt_of(journal, attempt_id)?;
    let h = attempt.native_host.as_ref().unwrap();
    if let (None, None, Some(_), Some(intent)) =
        (&h.closure, &h.stopped, &h.guardian, &h.host_intent)
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

#[cfg(test)]
mod tests;
