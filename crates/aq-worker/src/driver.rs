//! Minimal outbound synthetic-execution driver. All acceptance/report retries
//! reuse journaled bytes. It intentionally refuses uncertain restart relaunch.
use crate::{
    clock::{BootClock, Clock},
    config::WorkerConfig,
    execution_client::{ExecutionClient, RemoteError},
    journal::{Journal, Limits},
    runtime::Runtime,
};
use anyhow::{Context, Result, bail, ensure};
use aq_worker_protocol::*;
use std::{path::Path, time::Duration};
use uuid::Uuid;

pub struct SourceOptions {
    pub binary: std::path::PathBuf,
    pub binary_sha256: String,
    pub state_dir: std::path::PathBuf,
    pub git_executable: std::path::PathBuf,
    pub max_bundle_bytes: u64,
    pub max_expanded_bytes: u64,
}
pub struct DriverOptions {
    pub source: Option<SourceOptions>,
    pub max_agents: u32,
    pub idle_timeout_secs: u64,
    pub exit_after_result: bool,
    pub drop_acceptance_response_once: bool,
    pub drop_report_response_once: bool,
}
fn retryable(error: &anyhow::Error) -> bool {
    error
        .downcast_ref::<RemoteError>()
        .is_none_or(|e| e.error.retryable)
}
async fn retry_delay() {
    tokio::time::sleep(Duration::from_millis(100)).await;
}

pub async fn run_execution(
    config: &WorkerConfig,
    state_dir: &Path,
    executable: &Path,
    mut options: DriverOptions,
) -> Result<()> {
    crate::guardian::probe_profile()?;
    let git_service = options
        .source
        .as_ref()
        .map(|s| {
            crate::source::GitService::start_with_git(
                &s.binary,
                &s.binary_sha256,
                &s.state_dir,
                &s.git_executable,
            )
        })
        .transpose()?;
    let git = git_service.as_ref().map(|s| &s.client);
    if let (Some(git), Some(source)) = (git, options.source.as_ref()) {
        git.validate_storage_profile(source.max_bundle_bytes, source.max_expanded_bytes)?;
    }
    let existing_journal = state_dir.join("journal.json").exists();
    let identity = NodeIdentity {
        node_id: config.node_id,
        boot_id: Uuid::new_v4(),
    };
    let capacity = Resources {
        slots: options.max_agents,
        cpu_millis: 1_000_000,
        memory_bytes: 64 * 1024 * 1024 * 1024,
        disk_bytes: if git.is_some() {
            8 * 1024 * 1024 * 1024
        } else {
            1024 * 1024 * 1024
        },
    };
    let limits = Limits {
        capacity,
        max_pending_results: 64,
        max_pending_bytes: if git.is_some() {
            256 * 1024 * 1024
        } else {
            1024 * 1024
        },
        max_preparing: options.max_agents as usize,
        minimum_free_disk_bytes: 16 * 1024 * 1024,
    };
    let mut runtime = Runtime::new(
        Journal::open(state_dir, identity.clone(), limits)?,
        BootClock,
    );
    let client = ExecutionClient::from_config(config)?;
    let mut registration = RegisterRequest {
        identity: identity.clone(),
        operation_id: Uuid::new_v4(),
        protocol_version: EXECUTION_PROTOCOL_VERSION,
        worker_version: env!("CARGO_PKG_VERSION").into(),
        capabilities: vec![
            FAKE_HARNESS_CAPABILITY.into(),
            crate::guardian::PROFILE.into(),
        ],
    };
    if git.is_some() {
        registration.capabilities.push(SOURCE_CAPABILITY.into());
    }
    // Node recovery does not acquire new execution authority. It must be usable
    // before the old boot's registration has been explicitly replaced.
    if existing_journal {
        let _uncertain = runtime.recover_owned_launches()?;
        if let Some(git) = git {
            let collect: Vec<_> = runtime
                .journal
                .state()
                .attempts
                .iter()
                .filter(|(_, r)| {
                    r.source
                        .as_ref()
                        .is_some_and(|s| s.workspace.is_some() && s.exported.is_none())
                        && r.stop_certainty == StopCertainty::ConfirmedStopped
                        && r.stopped_proof.is_some()
                        && r.result.is_none()
                })
                .map(|(id, _)| *id)
                .collect();
            for id in collect {
                runtime.checkpoint_source(id, git)?;
                runtime.export_source(id, git)?;
                println!(
                    "{}",
                    serde_json::json!({"event":"source_pending","attempt_id":id,"recovered":true})
                );
            }
        }
        let response = match client.reconcile(&runtime.reconcile_request()).await {
            Ok(response) => response,
            Err(error)
                if error
                    .downcast_ref::<RemoteError>()
                    .is_some_and(|e| e.error.code == ErrorCode::StaleBoot)
                    && runtime.journal.state().attempts.is_empty() =>
            {
                ReconcileResponse {
                    attempts: vec![],
                    next_cursor: None,
                }
            }
            Err(error) => return Err(error),
        };
        println!(
            "{}",
            serde_json::json!({"event":"reconciling","identity":identity,"dispositions":response})
        );
        for disposition in &response.attempts {
            if let Some(result) = &disposition.result {
                let exists = runtime.journal.state().attempts.values().any(|r| {
                    r.report
                        .as_ref()
                        .is_some_and(|p| p.report_id == result.report_id)
                });
                if exists {
                    runtime.acknowledge_result(result.clone())?;
                }
            }
        }
        let reports: Vec<_> = runtime
            .journal
            .state()
            .attempts
            .values()
            .filter_map(|r| {
                if r.phase == Phase::Reporting
                    && r.source.as_ref().is_none_or(|s| {
                        s.receipt.is_some()
                            || (!s.operations.contains_key("create")
                                && s.workspace.is_none()
                                && r.launch_id.is_none())
                    })
                    && matches!(
                        r.stop_certainty,
                        StopCertainty::ConfirmedStopped | StopCertainty::NotStarted
                    )
                {
                    r.report.clone()
                } else {
                    None
                }
            })
            .collect();
        let mut recovered = 0;
        for report in reports {
            if let Some(response) = submit_report(&client, &mut runtime, &report).await? {
                runtime.acknowledge_result(response.clone())?;
                if response.retention_release {
                    retire_local(&mut runtime, report.attempt.attempt_id, git)?;
                }
            }
            recovered += 1;
        }
        let released: Vec<_> = runtime
            .journal
            .state()
            .attempts
            .iter()
            .filter_map(|(id, r)| {
                (r.phase == Phase::Acknowledged
                    && r.result.as_ref().is_some_and(|x| x.retention_release))
                .then_some(*id)
            })
            .collect();
        for id in released {
            retire_local(&mut runtime, id, git)?;
        }
        let uncertain = runtime.journal.state().attempts.values().any(|r| {
            r.phase != Phase::Retired
                && !matches!(
                    r.stop_certainty,
                    StopCertainty::ConfirmedStopped | StopCertainty::NotStarted
                )
        });
        if uncertain {
            println!(
                "{}",
                serde_json::json!({"event":"recovery_hold","registration":registration,"protected_usage":runtime.journal.protected_usage()?,"available":runtime.journal.available()?})
            );
            bail!(
                "prior writers/reservations require process reconciliation; preserved without relaunch"
            );
        }
        runtime.journal.transaction(|s| {
            s.reconciliation_required = false;
            Ok(())
        })?;
        if recovered > 0 {
            if options.exit_after_result {
                return Ok(());
            }
            // Receipt recovery may have supplied the daemon's first durable
            // stopped proof, so acquire a fresh explicit recovery disposition.
            client.reconcile(&runtime.reconcile_request()).await?;
        }
    }
    if runtime.journal.state().local_drain {
        println!(
            "{}",
            serde_json::json!({"event":"drained","protected_usage":runtime.journal.protected_usage()?,"available":runtime.journal.available()?})
        );
        return Ok(());
    }
    let registered = match client.register(&registration).await {
        Ok(r) => r,
        Err(error) => {
            println!(
                "{}",
                serde_json::json!({"event":"incarnation_replacement_required","registration":registration})
            );
            return Err(error);
        }
    };
    ensure!(
        registered.identity == identity
            && registered
                .capabilities
                .contains(&FAKE_HARNESS_CAPABILITY.into()),
        "coordinator negotiated different identity/capabilities"
    );
    println!(
        "{}",
        serde_json::json!({"event":"registered","identity":identity})
    );
    let mut idle_deadline = runtime
        .clock
        .now_ms()
        .saturating_add(options.idle_timeout_secs.saturating_mul(1000));
    loop {
        ensure!(
            runtime.clock.now_ms() < idle_deadline,
            "execution idle timeout"
        );
        let inventory_sequence = runtime.journal.transaction(|s| {
            s.inventory_sequence = s
                .inventory_sequence
                .checked_add(1)
                .context("inventory overflow")?;
            Ok(s.inventory_sequence)
        })?;
        let poll = PollRequest {
            identity: identity.clone(),
            operation_id: Uuid::new_v4(),
            inventory_sequence,
            available: runtime.journal.available()?,
            max_items: MAX_BATCH as u32,
        };
        let response = match client.poll(&poll).await {
            Ok(r) => r,
            Err(e) if retryable(&e) => {
                retry_delay().await;
                continue;
            }
            Err(e) => return Err(e),
        };
        for directive in &response.directives {
            let ack = runtime.receive_directive(directive)?;
            let result = client.directive_ack(&ack).await?;
            ensure!(
                result.operation_id == ack.operation_id,
                "directive ack mismatch"
            );
        }
        for offer in response.offers {
            if runtime
                .journal
                .state()
                .attempts
                .contains_key(&offer.attempt.attempt_id)
            {
                continue;
            }
            // Server epoch is used only relative to the supplied server epoch.
            if response.server_time_ms >= offer.offer_expires_at_ms {
                continue;
            }
            let attempt_id = offer.attempt.attempt_id;
            let source = offer.spec.bindings.as_ref().and_then(|b| b.source.as_ref());
            let admitted = if let Some(source) = source {
                match git {
                    Some(git) => git
                        .base_available(source)
                        .and_then(|()| runtime.reserve_source(offer.clone(), git)),
                    None => Err(anyhow::anyhow!("local source adapter is not configured")),
                }
            } else {
                runtime.reserve(offer.clone())
            };
            let acceptance = match admitted {
                Ok(r) => r,
                Err(e) => {
                    let decline = DeclineRequest {
                        identity: identity.clone(),
                        operation_id: Uuid::new_v4(),
                        offer_id: offer.offer_id,
                        reason: e.to_string(),
                    };
                    client.decline(&decline).await?;
                    continue;
                }
            };
            let authorized = loop {
                let sent = runtime.clock.now_ms();
                if sent >= acceptance.reservation_deadline_ms {
                    break false;
                }
                match client.accept(&acceptance).await {
                    Ok(_) if options.drop_acceptance_response_once => {
                        options.drop_acceptance_response_once = false;
                        continue;
                    }
                    Ok(response) => match runtime.authorize(response.grant, sent) {
                        Ok(()) => break true,
                        Err(error) => {
                            if runtime.clock.now_ms() >= acceptance.reservation_deadline_ms {
                                break false;
                            }
                            return Err(error);
                        }
                    },
                    Err(error) if retryable(&error) => retry_delay().await,
                    Err(error) => return Err(error),
                }
            };
            if !authorized {
                let report = runtime.report_not_started(attempt_id, ProcessOutcome::Expired)?;
                if let Some(response) = submit_report(&client, &mut runtime, &report).await? {
                    runtime.acknowledge_result(response.clone())?;
                    if response.retention_release {
                        retire_local(&mut runtime, attempt_id, git)?;
                    }
                }
                println!(
                    "{}",
                    serde_json::json!({"event":"reservation_expired_not_started","attempt_id":attempt_id})
                );
                if options.exit_after_result {
                    return Ok(());
                }
                continue;
            }
            crate::fault::checkpoint(runtime.journal.root(), "grant-durable", None)?;
            let observation =
                runtime.observation(attempt_id, "authorized synthetic fixture".into())?;
            let ack = client.observe(&observation).await?;
            runtime.acknowledge_observation(attempt_id, &ack)?;
            if source.is_some() {
                runtime.prepare_source(attempt_id, git.context("source service absent")?)?;
            }
            let mut handle = runtime.launch_fixture(attempt_id, executable)?;
            let control = control_fixture(&mut runtime, &client, attempt_id, &mut handle).await;
            if let Err(error) = control {
                // Even failed persistence/transport cannot justify leaving the
                // owned synthetic writer alive. The durable state stays protected.
                let _ = handle.kill();
                while handle.try_wait()?.is_none() {
                    retry_delay().await;
                }
                let _ = runtime.finish_fixture(attempt_id, handle);
                return Err(error);
            }
            let report = runtime.finish_fixture(attempt_id, handle)?;
            if source.is_some() {
                let git = git.context("source service absent")?;
                runtime.checkpoint_source(attempt_id, git)?;
                let exported = runtime.export_source(attempt_id, git)?;
                println!(
                    "{}",
                    serde_json::json!({"event":"source_pending","attempt_id":attempt_id,"artifact_id":exported["artifact_handle"],"protected_usage":runtime.journal.protected_usage()?,"pending_bytes":runtime.journal.state().attempts[&attempt_id].pending_bytes})
                );
                // No source-transfer capability/client is claimed: preserve locally;
                // never submit a success/empty-artifact report as completion.
                if options.exit_after_result {
                    return Ok(());
                }
                continue;
            }
            let observation = runtime.observation(
                attempt_id,
                "synthetic writers confirmed stopped; immutable report pending".into(),
            )?;
            deliver_stopped_observation(&client, &mut runtime, &observation).await?;
            let report_retry_deadline = runtime.clock.now_ms().saturating_add(30_000);
            loop {
                ensure!(
                    runtime.clock.now_ms() < report_retry_deadline,
                    "report remains durable and unacknowledged after retry timeout"
                );
                match submit_report(&client, &mut runtime, &report).await {
                    Ok(_) if options.drop_report_response_once => {
                        options.drop_report_response_once = false;
                        continue;
                    }
                    Ok(Some(result)) => {
                        runtime.acknowledge_result(result.clone())?;
                        println!(
                            "{}",
                            serde_json::json!({"event":"result_acknowledged","attempt":report.attempt,"report_id":report.report_id,"disposition":result.disposition})
                        );
                        if result.retention_release {
                            retire_local(&mut runtime, attempt_id, git)?;
                        }
                        break;
                    }
                    Ok(None) => break,
                    Err(e) if retryable(&e) => retry_delay().await,
                    Err(e) => return Err(e),
                }
            }
            idle_deadline = runtime
                .clock
                .now_ms()
                .saturating_add(options.idle_timeout_secs.saturating_mul(1000));
            if options.exit_after_result || runtime.journal.state().local_drain {
                return Ok(());
            }
        }
        retry_delay().await;
    }
}

/// Stopped writers need no renewed execution authority to deliver their durable
/// observations. Retrying this exact node exchange must precede report delivery;
/// an ordinary disconnect must not strand an already journaled final report.
async fn deliver_stopped_observation<C: Clock>(
    client: &ExecutionClient,
    runtime: &mut Runtime<C>,
    observation: &Observation,
) -> Result<()> {
    let deadline = runtime.clock.now_ms().saturating_add(30_000);
    loop {
        let remaining = deadline.saturating_sub(runtime.clock.now_ms());
        ensure!(
            remaining > 0,
            "stopped observation and report remain durable after retry timeout"
        );
        match tokio::time::timeout(
            Duration::from_millis(remaining),
            client.observe(observation),
        )
        .await
        {
            Ok(Ok(ack)) => {
                return runtime.acknowledge_observation(observation.attempt.attempt_id, &ack);
            }
            Ok(Err(error)) if retryable(&error) => retry_delay().await,
            Ok(Err(error)) => return Err(error),
            Err(_) => bail!("stopped observation and report remain durable after retry timeout"),
        }
    }
}

async fn control_fixture(
    runtime: &mut Runtime<BootClock>,
    client: &ExecutionClient,
    attempt_id: Uuid,
    handle: &mut crate::fixture::FixtureHandle,
) -> Result<()> {
    let mut next_poll = runtime.clock.now_ms();
    let now = runtime.clock.now_ms();
    let mut next_renew = now.saturating_add(
        runtime.journal.state().attempts[&attempt_id]
            .grant_deadline_ms
            .unwrap()
            .saturating_sub(now)
            / 3,
    );
    let mut renewal: Option<RenewRequest> = None;
    let mut kill_sent = false;
    loop {
        if handle.try_wait()?.is_some() {
            return Ok(());
        }
        let now = runtime.clock.now_ms();
        runtime.expire()?;
        let record = &runtime.journal.state().attempts[&attempt_id];
        if record.phase == Phase::Stopping {
            if !kill_sent {
                handle.stop(record.stop_reason.unwrap_or(ProcessOutcome::Cancelled))?;
                kill_sent = true;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
            continue;
        }
        let deadline = record.grant_deadline_ms.context("missing grant deadline")?;
        let timeout = Duration::from_millis(deadline.saturating_sub(now).clamp(1, 500));
        if now >= next_poll {
            let sequence = runtime.journal.transaction(|s| {
                s.inventory_sequence = s
                    .inventory_sequence
                    .checked_add(1)
                    .context("inventory overflow")?;
                Ok(s.inventory_sequence)
            })?;
            let request = PollRequest {
                identity: runtime.journal.state().identity.clone(),
                operation_id: Uuid::new_v4(),
                inventory_sequence: sequence,
                available: runtime.journal.available()?,
                max_items: MAX_BATCH as u32,
            };
            match tokio::time::timeout(timeout, client.poll(&request)).await {
                Ok(Ok(response)) => {
                    for directive in response.directives {
                        let ack = runtime.receive_directive(&directive)?;
                        if directive.attempt.attempt_id == attempt_id && !kill_sent {
                            handle.kill()?;
                            kill_sent = true;
                        }
                        // Receipt is durable; killing the owned writer precedes
                        // waiting for any network acknowledgement.
                        let remaining = runtime.journal.state().attempts[&attempt_id]
                            .grant_deadline_ms
                            .unwrap()
                            .saturating_sub(runtime.clock.now_ms());
                        if remaining > 0 {
                            let _ = tokio::time::timeout(
                                Duration::from_millis(remaining.min(100)),
                                client.directive_ack(&ack),
                            )
                            .await;
                        }
                    }
                }
                Ok(Err(error)) if !retryable(&error) => {
                    runtime.request_stop(attempt_id, ProcessOutcome::Expired)?;
                }
                _ => {}
            }
            next_poll = now.saturating_add(100);
        }
        if runtime.journal.state().attempts[&attempt_id].phase == Phase::Stopping {
            continue;
        }
        let now = runtime.clock.now_ms();
        if now >= next_renew {
            let record = &runtime.journal.state().attempts[&attempt_id];
            let request = renewal.get_or_insert_with(|| RenewRequest {
                identity: runtime.journal.state().identity.clone(),
                operation_id: Uuid::new_v4(),
                attempt: record.offer.attempt.clone(),
                grant_sequence: record.grant.as_ref().unwrap().grant_sequence,
            });
            let sent = runtime.clock.now_ms();
            let remaining = record.grant_deadline_ms.unwrap().saturating_sub(sent);
            if remaining == 0 {
                continue;
            }
            match tokio::time::timeout(
                Duration::from_millis(remaining.min(500)),
                client.renew(request),
            )
            .await
            {
                Ok(Ok(response)) => {
                    runtime.renew(response.grant, sent)?;
                    let record = &runtime.journal.state().attempts[&attempt_id];
                    handle.renew(
                        record.grant.as_ref().unwrap().grant_sequence,
                        record.grant_deadline_ms.unwrap(),
                    )?;
                    renewal = None;
                    let record = &runtime.journal.state().attempts[&attempt_id];
                    let now = runtime.clock.now_ms();
                    next_renew = now
                        .saturating_add(record.grant_deadline_ms.unwrap().saturating_sub(now) / 3);
                }
                Ok(Err(error)) if !retryable(&error) => {
                    runtime.request_stop(attempt_id, ProcessOutcome::Expired)?;
                }
                _ => {
                    next_renew = now.saturating_add(100);
                }
            }
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

async fn submit_report(
    client: &ExecutionClient,
    runtime: &mut Runtime<BootClock>,
    report: &ResultReport,
) -> Result<Option<ResultResponse>> {
    let record = &runtime.journal.state().attempts[&report.attempt.attempt_id];
    if report.stop_certainty == StopCertainty::NotStarted && record.grant.is_none() {
        let request = runtime.unstarted_decline(report.attempt.attempt_id)?;
        match client.decline(&request).await {
            Ok(ack) => {
                runtime.acknowledge_unstarted_decline(report.attempt.attempt_id, ack)?;
                return Ok(None);
            }
            Err(error)
                if error
                    .downcast_ref::<RemoteError>()
                    .is_some_and(|e| e.error.code == ErrorCode::IdConflict) => {}
            Err(error) => return Err(error),
        }
    }
    Ok(Some(client.report(report).await?))
}

fn retire_local<C: Clock>(
    runtime: &mut Runtime<C>,
    id: Uuid,
    git: Option<&crate::source::GitClient>,
) -> Result<()> {
    if runtime.journal.state().attempts[&id].source.is_some() {
        runtime.retire_source(
            id,
            git.context("source retirement requires configured aq-git")?,
        )?;
    }
    runtime.retire(id)
}
