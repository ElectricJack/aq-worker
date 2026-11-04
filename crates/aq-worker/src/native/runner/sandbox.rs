//! Fresh local sandbox execution. This path never registers, polls or accepts.
pub mod agent_access;
pub mod auth;
pub mod confinement;
pub mod harness_config;
mod journal;
mod manifest;
pub mod private;
pub mod proxy;
pub mod responses;
pub use manifest::FrozenLaunch;
pub(crate) fn boot_ns() -> anyhow::Result<u64> {
    super::super::http::boot_ns()
}
pub mod channels;
pub mod disk_watch;
pub mod hydrate;
pub mod observations;
pub mod output;
mod output_runtime;
mod startup;
pub use startup::SandboxStartup;
pub mod permit_transport;
mod transport;
use crate::config::WorkerConfig;
use anyhow::{Result, ensure};
use aq_admission_sandbox::sandbox::DelegatedPermit;
use aq_session_host::LocalEnvironment;
use aq_session_protocol::Digest;
use serde::{Deserialize, Serialize};
use std::{
    path::{Path, PathBuf},
    time::Duration,
};
use tokio::sync::watch;
pub use transport::{RenewalDelivery, RenewalOutcome, RunnerTransport, TransportFuture};
use uuid::Uuid;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct AttemptKey {
    pub launch_id: Uuid,
    pub attempt_id: Uuid,
    pub manifest_digest: Digest,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct CentralReceipt {
    pub receipt_id: Uuid,
    pub attempt: AttemptKey,
    pub artifact_digest: Digest,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct RetentionReceipts {
    pub source: Option<CentralReceipt>,
    pub result: Option<CentralReceipt>,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct FinishedReceipt {
    pub receipt_id: Uuid,
    pub attempt: AttemptKey,
}
/// Constructed only from an exact guardian stopped proof, never deserialized.
pub struct StoppedAttempt {
    key: AttemptKey,
    workspace: PathBuf,
    evidence: Vec<u8>,
}
impl StoppedAttempt {
    pub fn key(&self) -> &AttemptKey {
        &self.key
    }
    pub fn workspace(&self) -> &Path {
        &self.workspace
    }
    pub fn evidence(&self) -> &[u8] {
        &self.evidence
    }
}
/// Constructed only after both checked receipts are durable locally.
pub struct RetainedAttempt {
    stopped: StoppedAttempt,
    receipts: RetentionReceipts,
}
impl RetainedAttempt {
    pub fn stopped(&self) -> &StoppedAttempt {
        &self.stopped
    }
    pub fn receipts(&self) -> &RetentionReceipts {
        &self.receipts
    }
}
#[derive(Debug, PartialEq, Eq)]
pub enum RunOutcome {
    Finished,
    Preserved {
        reason: &'static str,
        stop_proven: bool,
    },
}
pub struct RunOptions {
    /// Default 15 seconds. Smaller intervals are useful for deterministic fixtures.
    pub renewal_interval: Duration,
}
impl Default for RunOptions {
    fn default() -> Self {
        Self {
            renewal_interval: Duration::from_secs(15),
        }
    }
}
#[allow(clippy::too_many_arguments)]
pub async fn run<'a>(
    worker: &WorkerConfig,
    frozen: &FrozenLaunch,
    permit: DelegatedPermit,
    state_dir: &Path,
    local: &LocalEnvironment,
    startup: impl Into<SandboxStartup<'a>>,
    transport: &mut impl RunnerTransport,
    shutdown: watch::Receiver<bool>,
    options: RunOptions,
) -> Result<RunOutcome> {
    run_inner(
        worker,
        frozen,
        permit,
        state_dir,
        local,
        startup.into(),
        transport,
        shutdown,
        options,
        None,
    )
    .await
}
/// Separated-user entrypoint. Bootstrap the runner before starting Tokio, keep
/// state private, and hydrate an agent-accessible workspace outside that state.
#[allow(clippy::too_many_arguments)]
pub async fn run_as_agent<'a>(
    worker: &WorkerConfig,
    frozen: &FrozenLaunch,
    permit: DelegatedPermit,
    state_dir: &Path,
    local: &LocalEnvironment,
    startup: impl Into<SandboxStartup<'a>>,
    transport: &mut impl RunnerTransport,
    shutdown: watch::Receiver<bool>,
    options: RunOptions,
    agent: aq_session_host::users::AgentIdentity,
) -> Result<RunOutcome> {
    private::check_directory(state_dir)?;
    ensure!(
        !local.workspace.starts_with(state_dir),
        "agent workspace must be outside runner state"
    );
    run_inner(
        worker,
        frozen,
        permit,
        state_dir,
        local,
        startup.into(),
        transport,
        shutdown,
        options,
        Some(agent),
    )
    .await
}
#[allow(clippy::too_many_arguments)]
async fn run_inner(
    worker: &WorkerConfig,
    frozen: &FrozenLaunch,
    permit: DelegatedPermit,
    state_dir: &Path,
    local: &LocalEnvironment,
    startup: SandboxStartup<'_>,
    transport: &mut impl RunnerTransport,
    mut shutdown: watch::Receiver<bool>,
    options: RunOptions,
    agent: Option<aq_session_host::users::AgentIdentity>,
) -> Result<RunOutcome> {
    use crate::native::launch::sandbox as host;
    use aq_admission_sandbox::sandbox::{RenewalReply, RunnerRenewalRequest};
    use aq_session_protocol::canonical_digest;
    let (initial_deadline, hard) = frozen.validate_local(state_dir)?;
    frozen
        .authority
        .validate_handoff(&permit, frozen.delivery.clock.server_time_ms)?;
    ensure!(
        worker.node_id == permit.binding.node.node_id,
        "worker identity required"
    );
    ensure!(
        !options.renewal_interval.is_zero() && options.renewal_interval <= Duration::from_secs(15),
        "renewal interval bounds"
    );
    ensure!(
        local.workspace.canonicalize()? == local.workspace
            && (agent.is_some()
                || (local.workspace.starts_with(&frozen.state_dir)
                    && local.workspace != frozen.state_dir)),
        "workspace must be prepared inside bound state directory"
    );
    let authority = transport.authority_client();
    if let Some(http) = &authority {
        http.bind_execution(frozen, &permit)?;
    }
    startup.validate(frozen, authority.as_ref())?;
    transport.preflight(worker)?;
    let authority_lost = || authority.as_ref().is_some_and(|http| http.authority_lost());
    ensure!(!authority_lost(), "runner authority lost before launch");
    ensure!(!*shutdown.borrow(), "cancelled before launch");
    let now = boot_ns()?;
    let hard = hard.min(
        frozen
            .sent_boot_ns
            .checked_add(
                frozen
                    .manifest
                    .resources
                    .wall_time_ms
                    .checked_mul(1_000_000)
                    .ok_or_else(|| anyhow::anyhow!("wall overflow"))?,
            )
            .ok_or_else(|| anyhow::anyhow!("wall overflow"))?,
    );
    let permit_deadline = permit
        .expires_at_ms
        .checked_sub(frozen.delivery.clock.server_time_ms)
        .and_then(|v| v.checked_mul(1_000_000))
        .and_then(|v| v.checked_add(frozen.sent_boot_ns))
        .ok_or_else(|| anyhow::anyhow!("delegated deadline overflow"))?;
    let mut deadline = initial_deadline
        .min(permit_deadline)
        .min(hard)
        .min(now.saturating_add(aq_session_host::MAX_PERMIT_MS * 1_000_000));
    let mut request = RunnerRenewalRequest {
        schema_version: 1,
        operation_id: Uuid::new_v4(),
        authority: frozen.authority.clone(),
        permit,
        revision: frozen.revision,
    };
    request.validate()?;
    let mut journal = journal::Journal::create(state_dir, frozen)?;
    let key = AttemptKey {
        launch_id: frozen.launch_id,
        attempt_id: frozen.manifest.bindings.attempt_id,
        manifest_digest: canonical_digest(&frozen.manifest)?,
    };
    let initial = local_permit(frozen, request.revision, now, deadline)?;
    journal.record("initial-permit", &initial)?;
    let root = journal.directory.join("hosts");
    let adapter = startup.adapter();
    let (mut handle, output_runtime) = match startup
        .launch(
            &root,
            frozen,
            initial,
            local,
            &mut journal,
            agent,
            shutdown.clone(),
        )
        .await
    {
        Ok(launched) => launched,
        Err(_) => {
            let proof = journal
                .intent
                .as_ref()
                .and_then(|intent| host::recover_stopped(&root, intent).ok());
            if let Some(proof) = &proof {
                journal.record("stopped", proof)?;
            }
            return Ok(RunOutcome::Preserved {
                reason: "launch failed",
                stop_proven: proof.is_some(),
            });
        }
    };
    // Collection/storage/delivery have their own thread and connection pool.
    // Missing source support is recorded explicitly; it grants no execution time.
    let observations = (|| -> Result<Option<observations::ObservationTask>> {
        let Some(descriptor) = adapter.transcript_source(&handle.identity().intent)? else {
            journal.record(
                "observation-incomplete",
                &"launched transcript metadata unavailable",
            )?;
            return Ok(None);
        };
        let http = authority
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("observation permit channel unavailable"))?;
        let source = observations::source_for_launch(
            frozen,
            &descriptor,
            // The launch does not deliver the server's observation catalog
            // entry yet, so the legacy launch-ID identity remains in use.
            None,
        )?;
        let channels = channels::SandboxChannels::new(http)?;
        Ok(Some(observations::ObservationTask::start(
            journal.directory.join("observations"),
            source,
            descriptor,
            &handle.identity().intent,
            vec![local.workspace.clone(), handle.directory().to_owned()],
            channels,
            agent.map_or_else(|| unsafe { libc::geteuid() }, |a| a.uid),
        )?))
    })();
    let observations = match observations {
        Ok(task) => task,
        Err(_) => {
            let _ = journal.record("observation-incomplete", &"transcript admission refused");
            None
        }
    };
    let interval = options.renewal_interval.as_nanos() as u64;
    let mut next_renew = boot_ns().unwrap_or(now).saturating_add(interval);
    let reason = 'running: loop {
        if *shutdown.borrow() {
            break "cancelled";
        }
        if authority_lost() {
            break "runner authority lost";
        }
        let now = match boot_ns() {
            Ok(now) => now,
            Err(_) => break "clock unavailable",
        };
        if let Some(reason) = stop_reason(&handle, now, deadline) {
            break reason;
        }
        if now < next_renew {
            tokio::select! {
                changed = shutdown.changed() => { if changed.is_err() || *shutdown.borrow() { break "cancelled"; } },
                _ = tokio::time::sleep(Duration::from_millis(25)) => {},
            }
            continue;
        }
        let nonce = Uuid::new_v4();
        let sent = now;
        let response = {
            let pending = transport.renew(worker, &request, nonce);
            tokio::pin!(pending);
            loop {
                tokio::select! {
                    biased;
                    changed = shutdown.changed() => { if changed.is_err() || *shutdown.borrow() { break 'running "cancelled"; } },
                    _ = tokio::time::sleep(Duration::from_millis(25)) => {
                        if authority_lost() { break 'running "runner authority lost"; }
                        let now=match boot_ns() {Ok(now)=>now,Err(_)=>break 'running "clock unavailable"};
                        if let Some(reason)=stop_reason(&handle,now,deadline) {break 'running reason;}
                    },
                    result = &mut pending => break result,
                }
            }
        };
        let received = match boot_ns() {
            Ok(now) => now,
            Err(_) => break "clock unavailable",
        };
        // CLOCK_BOOTTIME is checked even if the network future wins after suspend.
        if authority_lost() {
            break "runner authority lost";
        }
        if received >= deadline {
            break "renewal deadline expired";
        }
        next_renew = received.saturating_add(interval);
        let delivery = match response {
            Ok(RenewalOutcome::Delivered(delivery)) => delivery,
            Ok(RenewalOutcome::Unavailable) | Err(_) => continue,
            Ok(RenewalOutcome::Refused) => break "renewal refused",
        };
        let renewed = match transport::validate_renewal(
            &delivery, &request, nonce, sent, received, deadline, hard,
        ) {
            Ok(value) => {
                value.min(received.saturating_add(aq_session_host::MAX_PERMIT_MS * 1_000_000))
            }
            Err(_) => break "invalid renewal",
        };
        match &delivery.response.reply {
            RenewalReply::AwaitingNodeReadoption { .. } => continue,
            RenewalReply::Renewed { permit, .. } => {
                let local_permit =
                    match local_permit(frozen, delivery.response.revision, received, renewed) {
                        Ok(value) => value,
                        Err(_) => break "invalid renewal deadline",
                    };
                // Do not serialize DelegatedPermit or its credential into evidence.
                if journal
                    .record(
                        &format!("renewal-{}", delivery.response.revision),
                        &local_permit,
                    )
                    .is_err()
                {
                    break "renewal persistence failed";
                }
                if handle.renew(&local_permit).is_err() {
                    // Natural completion can race the local renewal install.
                    // It still must pass the exact stopped-proof/receipt gates.
                    if stop_reason(&handle, boot_ns().unwrap_or(deadline), deadline)
                        == Some("harness exited")
                    {
                        break "harness exited";
                    }
                    break "guardian renewal refused";
                }
                deadline = renewed;
                request.permit = permit.clone();
                request.revision = delivery.response.revision;
                request.operation_id = Uuid::new_v4();
            }
        }
    };
    // Signal only; never await telemetry before stopping writers.
    if let Some(task) = &observations {
        task.stop();
    }
    let proof = match host::stop(&mut handle) {
        Ok(proof) => proof,
        Err(_) => {
            return Ok(RunOutcome::Preserved {
                reason: "stop uncertain",
                stop_proven: false,
            });
        }
    };
    if let Some(runtime) = output_runtime {
        runtime.stop().await;
    }
    journal.record("stopped", &proof)?;
    if reason != "harness exited" {
        journal.record("preserved", &reason)?;
        return Ok(RunOutcome::Preserved {
            reason,
            stop_proven: true,
        });
    }
    let stopped = StoppedAttempt {
        key,
        workspace: local.workspace.clone(),
        evidence: serde_json::to_vec(&proof)?,
    };
    let receipts =
        match tokio::time::timeout(Duration::from_secs(30), transport.retain(worker, &stopped))
            .await
        {
            Ok(Ok(receipts)) => receipts,
            _ => {
                return Ok(RunOutcome::Preserved {
                    reason: "retention unavailable",
                    stop_proven: true,
                });
            }
        };
    if validate_receipts(&stopped, &receipts).is_err() {
        return Ok(RunOutcome::Preserved {
            reason: "retention receipts incomplete",
            stop_proven: true,
        });
    }
    journal.record("retention", &receipts)?;
    let retained = RetainedAttempt { stopped, receipts };
    let receipt = match tokio::time::timeout(
        Duration::from_secs(30),
        transport.finished(worker, &retained),
    )
    .await
    {
        Ok(Ok(receipt))
            if !receipt.receipt_id.is_nil() && receipt.attempt == retained.stopped.key =>
        {
            receipt
        }
        _ => {
            return Ok(RunOutcome::Preserved {
                reason: "finished outcome unknown",
                stop_proven: true,
            });
        }
    };
    journal.record("finished", &receipt)?;
    Ok(RunOutcome::Finished)
}
fn local_permit(
    frozen: &FrozenLaunch,
    sequence: u64,
    now: u64,
    deadline: u64,
) -> Result<aq_session_host::ExecutionPermit> {
    let permit = aq_session_host::ExecutionPermit {
        session: frozen.manifest.bindings.session.clone(),
        manifest_digest: aq_session_protocol::canonical_digest(&frozen.manifest)?,
        sequence,
        issued_boot_ms: now / 1_000_000,
        expires_boot_ms: deadline / 1_000_000,
    };
    permit.validate_at(&permit.session, &permit.manifest_digest, now / 1_000_000)?;
    Ok(permit)
}
fn stop_reason(
    handle: &aq_session_host::SessionHandle,
    now: u64,
    deadline: u64,
) -> Option<&'static str> {
    if now >= deadline {
        return Some("renewal deadline expired");
    }
    match handle.observe() {
        Ok(observation) => match observation.phase {
            aq_session_host::HostPhase::Stopping | aq_session_host::HostPhase::Stopped => Some(
                if observation.stop_reason.as_deref() == Some("harness pane exited") {
                    "harness exited"
                } else {
                    "guardian stopped"
                },
            ),
            aq_session_host::HostPhase::StopUncertain => Some("stop uncertain"),
            _ => None,
        },
        Err(_) => Some("host observation unavailable"),
    }
}
fn validate_receipts(stopped: &StoppedAttempt, receipts: &RetentionReceipts) -> Result<()> {
    let source = receipts
        .source
        .as_ref()
        .ok_or_else(|| anyhow::anyhow!("source receipt missing"))?;
    let result = receipts
        .result
        .as_ref()
        .ok_or_else(|| anyhow::anyhow!("result receipt missing"))?;
    ensure!(
        !source.receipt_id.is_nil()
            && !result.receipt_id.is_nil()
            && source.receipt_id != result.receipt_id
            && source.attempt == stopped.key
            && result.attempt == stopped.key
            && result.artifact_digest == Digest::of(&stopped.evidence),
        "retention receipt scope/evidence mismatch"
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn central_retention_receipts_require_exact_attempt_and_result_evidence() {
        let stopped = StoppedAttempt {
            key: AttemptKey {
                launch_id: Uuid::new_v4(),
                attempt_id: Uuid::new_v4(),
                manifest_digest: Digest::of(b"manifest"),
            },
            workspace: "/unused".into(),
            evidence: b"kernel stopped proof".to_vec(),
        };
        let source = CentralReceipt {
            receipt_id: Uuid::new_v4(),
            attempt: stopped.key.clone(),
            artifact_digest: Digest::of(b"retained source"),
        };
        let result = CentralReceipt {
            receipt_id: Uuid::new_v4(),
            attempt: stopped.key.clone(),
            artifact_digest: Digest::of(stopped.evidence()),
        };
        let valid = RetentionReceipts {
            source: Some(source),
            result: Some(result),
        };
        validate_receipts(&stopped, &valid).unwrap();
        let mut wrong = valid.clone();
        wrong.result.as_mut().unwrap().artifact_digest = Digest::of(b"other result");
        assert!(validate_receipts(&stopped, &wrong).is_err());
        let mut wrong = valid.clone();
        wrong.source.as_mut().unwrap().attempt.attempt_id = Uuid::new_v4();
        assert!(validate_receipts(&stopped, &wrong).is_err());
        let mut wrong = valid;
        wrong.result.as_mut().unwrap().receipt_id = wrong.source.as_ref().unwrap().receipt_id;
        assert!(validate_receipts(&stopped, &wrong).is_err());
    }
}

#[cfg(test)]
mod auth_tests;
#[cfg(test)]
mod channels_tests;
#[cfg(test)]
mod confinement_tests;
#[cfg(test)]
mod hydrate_tests;

#[cfg(test)]
mod observations_tests;
#[cfg(test)]
mod output_tests;
