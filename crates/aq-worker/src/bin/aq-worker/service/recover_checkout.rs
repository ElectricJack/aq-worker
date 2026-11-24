//! Owner recovery of a stopped attempt from a pinned, clean node worktree.
use super::*;
use aq_admission::transport::ProcessOutcome;
use aq_agent_result_local::{Conclusion, Submission, SubmissionInput};
use aq_worker::{
    backend::{PlacementBackend, SandboxState, docker_local::DockerLocalBackend},
    native::{http::NativeClient, journal::Journal, result, runner::lock_recovery_scope},
    transfer_client::TransferClient,
};

#[derive(Args)]
pub struct Checkout {
    #[arg(long)]
    profile: String,
    #[arg(long)]
    node: u32,
    #[arg(long)]
    slot: u32,
    #[arg(long)]
    attempt_id: Uuid,
    #[arg(long)]
    expected_head: String,
    /// Use the original clean checkout and keep the credential-quarantined sandbox stopped.
    #[arg(long)]
    original_checkpoint: bool,
}
impl Checkout {
    pub(super) async fn run(&self) -> Result<()> {
        run(
            &self.profile,
            self.node,
            self.slot,
            self.attempt_id,
            &self.expected_head,
            self.original_checkpoint,
        )
        .await
    }
}

async fn run(
    profile: &str,
    node: u32,
    slot: u32,
    attempt: Uuid,
    head: &str,
    original_checkpoint: bool,
) -> Result<()> {
    ensure!(
        head.len() == 40
            && head
                .bytes()
                .all(|c| c.is_ascii_digit() || (b'a'..=b'f').contains(&c)),
        "expected a full lowercase Git commit"
    );
    let (machine, service) = loaded(profile)?;
    ensure!(
        machine.enrolled_nodes().contains(&node)
            && (1..=aq_protocol::MAX_NODE_SLOTS).contains(&slot),
        "unknown node or slot"
    );
    let layout = machine.for_node(node);
    let enrollment = layout.slot_enrollment(slot);
    let worker = WorkerConfig::load(&enrollment)?;
    let ctx = SlotContext {
        slot,
        node_id: worker.node_id,
        enrollment,
        state_dir: std::fs::canonicalize(layout.current(slot))?,
        git_socket: layout.git_socket(slot),
        host_root: layout.host_root(slot),
        hold: std::time::Duration::ZERO,
    };
    let cfg = slot_native_config(&service.native_template, &ctx)?;
    let held = lock_recovery_scope(&cfg)?;
    let journal = Journal::open(
        &cfg.state_dir.join("journal"),
        worker.node_id,
        cfg.capacity.clone(),
    )?;
    let a = journal
        .state()
        .attempts
        .get(&attempt)
        .context("attempt absent from this slot")?;
    let history = a.result.as_ref().context("result scope absent")?;
    let note = if original_checkpoint {
        Conclusion::new(format!(
            "Outcome: failed. The owner verified the retained checkout at the original input checkpoint {head}. The stopped sandbox remains quarantined after a credential check. Its contents were excluded from the recovered source. Continue from the original checkout and perform the required checks."
        ))?
    } else {
        Conclusion::new(format!(
            "Outcome: failed. Owner recovered the retained Git checkout at {head}. The attempt stopped without a result and its sandbox is absent. Sandbox-only edits are unknown. Verify the checkout and required checks in the replacement attempt."
        ))?
    };
    let claim = Submission::new(
        history.scope.binding.clone(),
        SubmissionInput {
            schema_version: 1,
            conclusion: note,
        },
    )?;
    if let Some(existing) = &history.claim {
        ensure!(
            history.salvaged.is_some()
                && existing.submission()? == claim
                && history
                    .stop
                    .as_ref()
                    .is_some_and(|s| matches!(s.outcome, ProcessOutcome::Failed)),
            "attempt already has a different result"
        );
        if let Some(receipt) = &history.receipt {
            aq_node_service::write_private(&layout.recovery_recorded(slot), attempt.as_bytes())?;
            println!(
                "Failed handoff already recorded for attempt {attempt}, report {}",
                receipt.report_id
            );
            return Ok(());
        }
    } else {
        ensure!(
            history.stop.is_none()
                && history.report.is_none()
                && history.source.checkpoint.is_none(),
            "attempt already has a result in progress"
        );
    }
    ensure!(
        a.native_host
            .as_ref()
            .and_then(|h| h.stopped.as_ref())
            .is_some(),
        "physical stop proof missing"
    );
    let roots = result::salvage_roots(history)?;
    let path = history.scope.workspace.path.clone();
    let source = a.source.as_ref().context("source scope absent")?;
    ensure!(
        source.worktree.as_ref() == Some(&path) && history.scope.managed_root == cfg.git_root,
        "workspace differs from the recorded source"
    );
    let Some(ExecutorConfig::ClaudeDockerLocalResultLocal { settings, .. }) = &cfg.executor else {
        bail!("docker-local executor required")
    };
    let mut backend = DockerLocalBackend::connect_unix(settings.host.docker_local.clone()).await?;
    let sandbox_states: Vec<_> = backend
        .list_owned(worker.node_id)
        .await?
        .into_iter()
        .filter(|entry| entry.sandbox.owner.attempt_id == attempt)
        .map(|entry| entry.state)
        .collect();
    validate_recovery_source(
        original_checkpoint,
        layout.held_reason(slot).as_deref().map(str::trim),
        source.manifest.as_ref().map(|m| m.checkpoint.hex.as_str()),
        head,
        &sandbox_states,
    )?;
    layout.slot_runtime_for_recovery(slot)?;
    let helper =
        aq_node_service::AqGit::start(&service.aq_git, &cfg.state_dir, &cfg.git_socket).await?;
    let resume = history.claim.is_some();
    let outcome = async {
        let git = cfg.git_client(&worker)?;
        git.node
            .as_ref()
            .context("node Git required")?
            .verify_checkout(attempt, path.clone(), head.to_owned())
            .await?;
        aq_node_service::session::check_state_root(&cfg.state_dir, &held)?;
        if original_checkpoint {
            let marker = preserved_sandbox_record(&cfg.state_dir, attempt);
            let proof = serde_json::json!({"attempt_id":attempt,"checkpoint":head,"reason":"credential_in_workspace"});
            aq_node_service::write_private(&marker, &serde_json::to_vec(&proof)?)?;
        }
        let transfer = TransferClient::from_config(&worker, 1)?;
        let client = NativeClient::from_config(&worker)?;
        result::finish_salvage(
            journal,
            attempt,
            (!resume).then_some(claim),
            roots,
            git,
            &transfer,
            &client,
        )
        .await
    }
    .await;
    let stopped = helper.stop().await;
    let receipt = outcome?;
    stopped?;
    aq_node_service::write_private(&layout.recovery_recorded(slot), attempt.as_bytes())?;
    println!(
        "Recorded failed handoff from retained commit {head} for attempt {attempt}, report {}",
        receipt.report_id
    );
    Ok(())
}

fn validate_recovery_source(
    original_checkpoint: bool,
    held_reason: Option<&str>,
    admitted_head: Option<&str>,
    expected_head: &str,
    sandboxes: &[SandboxState],
) -> Result<()> {
    if original_checkpoint {
        ensure!(
            held_reason == Some("credential_in_workspace"),
            "slot is not held for credential inspection"
        );
        ensure!(
            admitted_head == Some(expected_head),
            "requested checkout is not the original input checkpoint"
        );
        ensure!(
            matches!(sandboxes, [SandboxState::Stopped]),
            "expected one stopped sandbox for that attempt"
        );
    } else {
        ensure!(
            held_reason == Some("sandbox_absent"),
            "slot is not held for an absent sandbox"
        );
        ensure!(sandboxes.is_empty(), "attempt sandbox still exists");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn quarantined_recovery_requires_original_source_and_a_single_stopped_sandbox() {
        let reason = Some("credential_in_workspace");
        let original = Some("original-head");
        let stopped = [SandboxState::Stopped];
        assert!(
            validate_recovery_source(true, reason, original, "original-head", &stopped).is_ok()
        );
        assert!(
            validate_recovery_source(true, reason, original, "different-head", &stopped).is_err()
        );
        assert!(
            validate_recovery_source(
                true,
                Some("sandbox_absent"),
                original,
                "original-head",
                &stopped
            )
            .is_err()
        );
        assert!(validate_recovery_source(true, reason, original, "original-head", &[]).is_err());
        assert!(
            validate_recovery_source(
                true,
                reason,
                original,
                "original-head",
                &[SandboxState::Running]
            )
            .is_err()
        );
        assert!(
            validate_recovery_source(
                true,
                reason,
                original,
                "original-head",
                &[SandboxState::Stopped, SandboxState::Stopped]
            )
            .is_err()
        );
        assert!(
            validate_recovery_source(false, Some("sandbox_absent"), None, "another-head", &[])
                .is_ok()
        );
    }
}
