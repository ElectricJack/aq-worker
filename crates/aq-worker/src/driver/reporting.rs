use super::*;
/// Stopped writers need no renewed execution authority to deliver their durable
/// observations. Retrying this exact node exchange must precede report delivery;
/// an ordinary disconnect must not strand an already journaled final report.
pub(super) async fn deliver_stopped_observation<C: Clock>(
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

pub(super) async fn submit_report(
    client: &ExecutionClient,
    runtime: &mut Runtime<BootClock>,
    report: &ResultReport,
) -> Result<Option<ResultResponse>> {
    let record = &runtime.journal.state().attempts[&report.attempt.attempt_id];
    let source_report =
        record.source.is_some() && report.stop_certainty == StopCertainty::ConfirmedStopped;
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
    let prepared;
    let report = if source_report {
        prepared = runtime.prepare_source_report(report.attempt.attempt_id)?;
        &prepared
    } else {
        report
    };
    Ok(Some(client.report(report).await?))
}

pub(super) fn retire_local<C: Clock>(
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
