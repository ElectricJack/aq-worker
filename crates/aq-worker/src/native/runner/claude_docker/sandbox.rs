use super::*;

pub(super) fn before_host_stop(
    docker_local: crate::backend::docker_local::DockerLocalConfig,
    (node, id): (uuid::Uuid, uuid::Uuid),
    leased: Option<PathBuf>,
    link: aq_agent_channel::NodeLink,
) -> crate::native::runner::codex::PreStop {
    Box::new(move || {
        Box::pin(async move {
            stop_attempt_sandbox(&docker_local, node, id).await?;
            // Still before Stopped is reported: the lease is admitted until then.
            let Some(dir) = &leased else { return Ok(()) };
            let written = async {
                let mut backend = crate::backend::docker_local::DockerLocalBackend::connect_unix(
                    docker_local.clone(),
                )
                .await?;
                write_back_login(&mut backend, (node, id), dir, &link).await
            };
            match written.await {
                Ok(None) => {}
                outcome => {
                    tracing::warn!(attempt_id = %id, ?outcome, "Codex login write-back")
                }
            }
            Ok(())
        })
    })
}

pub(super) async fn release_reported_sandbox(
    docker: &crate::backend::docker_local::DockerLocalConfig,
    root: &std::path::Path,
    node: uuid::Uuid,
    id: uuid::Uuid,
) {
    for round in 1..=2 {
        let released = async {
            let mut backend =
                crate::backend::docker_local::DockerLocalBackend::connect_unix(docker.clone())
                    .await?;
            destroy_claimed_sandboxes(&mut backend, root, node, &[id]).await
        };
        match released.await {
            Ok(_) => break,
            Err(e) => {
                tracing::warn!(attempt_id = %id, round, "claimed sandbox kept: {e:#}")
            }
        }
        tokio::time::sleep(std::time::Duration::from_secs(round)).await;
    }
}

/// Stops (never destroys) every sandbox of this attempt that may still run,
/// and fails unless each stop is backend-confirmed. A sandbox already stopped,
/// archived or destroyed is left as is.
pub(crate) async fn stop_attempt_sandbox(
    docker_local: &crate::backend::docker_local::DockerLocalConfig,
    node: uuid::Uuid,
    attempt: uuid::Uuid,
) -> Result<()> {
    let mut backend =
        crate::backend::docker_local::DockerLocalBackend::connect_unix(docker_local.clone())
            .await?;
    stop_owned_sandboxes(&mut backend, node, attempt).await
}

/// C5b: a Codex login that the attempt's stopped sandbox refreshed goes back to the
/// server's login store as the next version of this attempt's lease. Nothing to do
/// without a lease file (Claude, or a login delivered without a version).
pub async fn write_back_login<T: crate::backend::docker_local::DockerTransport>(
    backend: &mut crate::backend::docker_local::DockerLocalBackend<T>,
    (node, attempt): (uuid::Uuid, uuid::Uuid),
    driver_dir: &std::path::Path,
    link: &aq_agent_channel::NodeLink,
) -> Result<Option<aq_agent_channel::login::WriteBack>> {
    use crate::backend::PlacementBackend;
    use aq_agent_channel::login::{LEASE_FILE, LoginLease};
    let Ok(lease) = aq_handoff_note::read_private(&driver_dir.join(LEASE_FILE), 65536) else {
        return Ok(None);
    };
    let lease: LoginLease = serde_json::from_slice(&lease)?;
    let auth = [
        crate::native::launch::claude_docker::CODEX_HOME,
        "/auth.json",
    ]
    .concat();
    for entry in backend.list_owned(node).await? {
        if entry.sandbox.owner.attempt_id != attempt {
            continue;
        }
        let after = zeroize::Zeroizing::new(backend.read_file(&entry.sandbox, &auth).await?);
        if aq_session_host::PrivateLaunchInput::validate_chatgpt_auth_json(&after).is_ok()
            && lease.refreshed(&after)
        {
            let digest = serde_json::to_value(Digest::of(&after))?;
            return Ok(Some(link.write_back_login(&lease, &after, digest).await?));
        }
    }
    Ok(None)
}

pub(crate) async fn stop_owned_sandboxes<B: crate::backend::PlacementBackend>(
    backend: &mut B,
    node: uuid::Uuid,
    attempt: uuid::Uuid,
) -> Result<()> {
    use crate::backend::SandboxState;
    for entry in backend.list_owned(node).await? {
        if entry.sandbox.owner.attempt_id != attempt
            || matches!(
                entry.state,
                SandboxState::Stopped | SandboxState::Archived | SandboxState::Destroyed
            )
        {
            continue;
        }
        let stopped = backend.stop(&entry.sandbox).await;
        tracing::warn!(attempt_id = %attempt, sandbox = %entry.sandbox.backend_id,
            confirmed = stopped.is_confirmed(), "attempt sandbox stop");
        ensure!(
            stopped.is_confirmed(),
            "sandbox {} stop unconfirmed; the harness may still run (stop uncertain)",
            entry.sandbox.backend_id
        );
    }
    Ok(())
}

/// Destroys the stopped sandboxes of `attempts`, whose result claims are recorded (the work
/// is in the worktree). Running or unknown sandboxes, and those a driver under `host_root`
/// preserved (`aq_handoff_note::preserved`), are left alone. Returns how many went.
pub async fn destroy_claimed_sandboxes<B: crate::backend::PlacementBackend>(
    backend: &mut B,
    host_root: &std::path::Path,
    node: uuid::Uuid,
    attempts: &[uuid::Uuid],
) -> Result<usize> {
    use crate::backend::{Confirmation, DestroyAuthorization};
    let mut destroyed = 0;
    for entry in backend.list_owned(node).await? {
        let id = entry.sandbox.owner.attempt_id;
        if !releasable(&entry, attempts, host_root) {
            continue;
        }
        let evidence = format!("node-worktree:{id}");
        let authorization = DestroyAuthorization::retained(entry.sandbox.clone(), evidence)?;
        match backend.destroy(&entry.sandbox, &authorization).await {
            Confirmation::Confirmed(_) => destroyed += 1,
            other => anyhow::bail!(
                "destroy of {} not confirmed: {other:?}",
                entry.sandbox.backend_id
            ),
        }
    }
    Ok(destroyed)
}
fn releasable(
    entry: &crate::backend::InventoryEntry,
    claimed: &[uuid::Uuid],
    root: &std::path::Path,
) -> bool {
    let attempt = entry.sandbox.owner.attempt_id;
    claimed.contains(&attempt)
        && entry.state == crate::backend::SandboxState::Stopped
        && !aq_handoff_note::preserved(root, &attempt.to_string())
}

/// The service's safety net: destroys the leftover stopped sandboxes of this state dir's
/// claimed attempts. Takes the state-dir lock (`claimed_attempts`).
pub async fn release_claimed_sandboxes(config: &NativeConfig, node: uuid::Uuid) -> Result<usize> {
    let claimed = crate::native::runner::claimed_attempts(config, node)?;
    let Some(crate::native::runner::codex::ExecutorConfig::ClaudeDockerLocalResultLocal {
        settings,
        ..
    }) = &config.executor
    else {
        return Ok(0);
    };
    if claimed.is_empty() {
        return Ok(0);
    }
    let docker = settings.host.docker_local.clone();
    let mut backend =
        crate::backend::docker_local::DockerLocalBackend::connect_unix(docker).await?;
    destroy_claimed_sandboxes(&mut backend, &settings.host.root, node, &claimed).await
}

/// Operator release of a preserved (stopped, never running) docker-local
/// sandbox of one attempt (SBX5 "an operator explicitly releases it"). The
/// sandbox is destroyed only with the operator's non-empty release reason as
/// destroy evidence; a running or unknown sandbox is refused. Returns the
/// released backend identifiers.
pub async fn release_preserved_sandbox(
    config: &NativeConfig,
    node: uuid::Uuid,
    attempt: uuid::Uuid,
    reason: &str,
) -> Result<Vec<String>> {
    use crate::backend::{
        Confirmation, DestroyAuthorization, PlacementBackend, SandboxState,
        docker_local::DockerLocalBackend,
    };
    let Some(crate::native::runner::codex::ExecutorConfig::ClaudeDockerLocalResultLocal {
        settings,
        ..
    }) = &config.executor
    else {
        anyhow::bail!("not a Claude docker-local node configuration");
    };
    ensure!(
        !reason.trim().is_empty() && reason.len() <= 512,
        "an operator release needs a reason (at most 512 bytes)"
    );
    let mut backend = DockerLocalBackend::connect_unix(settings.host.docker_local.clone()).await?;
    let mut released = Vec::new();
    for entry in backend.list_owned(node).await? {
        if entry.sandbox.owner.attempt_id != attempt {
            continue;
        }
        ensure!(
            entry.state == SandboxState::Stopped,
            "sandbox {} is {:?}, not stopped; it is never released while it may run",
            entry.sandbox.backend_id,
            entry.state
        );
        let authorization = DestroyAuthorization::operator_released(
            entry.sandbox.clone(),
            format!("operator-release:{attempt}:{reason}"),
        )?;
        match backend.destroy(&entry.sandbox, &authorization).await {
            Confirmation::Confirmed(_) => released.push(entry.sandbox.backend_id.clone()),
            other => anyhow::bail!(
                "destroy of {} not confirmed: {other:?}",
                entry.sandbox.backend_id
            ),
        }
    }
    ensure!(
        !released.is_empty(),
        "no preserved sandbox of attempt {attempt} on this node"
    );
    Ok(released)
}
