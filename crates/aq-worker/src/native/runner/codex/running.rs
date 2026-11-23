use super::*;
use crate::native::{
    launch::{LocalPhaseHost, PhaseControlRelay},
    result::{DurableStoppedResult, ResultStopCause},
    runner::{
        profile::TerminalProfile,
        publish_output::{ControlPublisher, OutputPublisher},
    },
};
use crate::transfer_client::TransferClient;
use aq_session_host::terminal_io::QualifiedClock;
use std::time::{Duration, Instant};

#[derive(Default)]
pub(crate) struct DriveOptions {
    pub(crate) transfer: Option<TransferClient>,
    pub(crate) clock: Option<QualifiedClock>,
    pub(crate) local_renewal: bool,
    pub(crate) pre_stop: Option<PreStop>,
}
pub(crate) struct RunningHost {
    pipeline: Arc<InputPipeline>,
    host: LocalPhaseHost,
    deadline_ns: u64,
}
struct Observation {
    control: Option<PhaseControlRelay>,
    clock: Option<QualifiedClock>,
    result_enabled: bool,
    local_renewal: bool,
}
impl RunningHost {
    pub(crate) fn new(
        pipeline: Arc<InputPipeline>,
        host: LocalPhaseHost,
        deadline_ns: u64,
    ) -> Self {
        Self {
            pipeline,
            host,
            deadline_ns,
        }
    }
    pub(crate) async fn drive(
        self,
        relay: RelayHttp,
        mut shutdown: watch::Receiver<bool>,
        options: DriveOptions,
    ) -> Result<()> {
        let mut state = Observation {
            control: None,
            clock: options.clock,
            result_enabled: options.transfer.is_some(),
            local_renewal: options.local_renewal,
        };
        let operation = self.observe(relay, &mut shutdown, &mut state).await;
        let (mut cause, operation) = match operation {
            Ok(cause) => (cause, Ok(())),
            Err(error) => (ResultStopCause::Failure, Err(error)),
        };
        if options.local_renewal
            && matches!(
                cause,
                ResultStopCause::Natural | ResultStopCause::ResultRecorded
            )
        {
            cause = driver_proven_cause(cause, self.host.claude_driver_succeeded());
        }
        let intake = self.stop(options.pre_stop).await?;
        // Flush even after an operation error. Preserve the earliest error if the flush also fails.
        let flush = match state.control {
            Some(control) => control.flush_control_receipts().await,
            None => Ok(()),
        };
        let operation = operation.and(flush);
        let stopped = self.report_stopped().await;
        if let Some(transfer) = options.transfer {
            self.record_result(cause, &transfer, (stopped, intake))
                .await?;
        } else {
            intake?;
            stopped?;
        }
        operation
    }
    async fn observe(
        &self,
        relay_client: RelayHttp,
        shutdown: &mut watch::Receiver<bool>,
        state: &mut Observation,
    ) -> Result<ResultStopCause> {
        let Self {
            pipeline,
            host,
            deadline_ns,
        } = self;
        let (id, deadline_ns) = (host.attempt_id(), *deadline_ns);
        let relay = pipeline.start_phase_output(host, relay_client)?;
        let snapshot = pipeline.snapshot().await?;
        let terminal_profile = TerminalProfile::for_attempt(
            snapshot
                .attempts
                .get(&id)
                .context("native Running attempt absent")?,
        )?;
        tokio::select! {
            receipt = terminal_profile.report_started(pipeline, id) => { receipt?; },
            _ = stop_requested(shutdown, deadline_ns) => {
                anyhow::bail!("stop requested while Started publication outcome is unknown");
            }
        }
        let control = if let Some(clock) = state.clock {
            Some(pipeline.start_phase_control(host, &relay, clock).await?)
        } else {
            None
        };
        state.control = control.clone();
        let controller = control.map(ControlPublisher::start);
        let _publisher = OutputPublisher::start(relay);
        self.monitor(controller, shutdown, state).await
    }
    async fn monitor(
        &self,
        controller: Option<ControlPublisher>,
        shutdown: &mut watch::Receiver<bool>,
        state: &Observation,
    ) -> Result<ResultStopCause> {
        let (pipeline, host, id, deadline_ns) = (
            &self.pipeline,
            &self.host,
            self.host.attempt_id(),
            self.deadline_ns,
        );
        let mut timers = state.local_renewal.then(Renewals::new);
        loop {
            // Node-owned (sandboxed) hosts also observe owner cancellation
            // through the node poll; the session host stops the harness.
            if let Some(timers) = &mut timers
                && timers.cancelled(pipeline, id).await
            {
                return Ok(ResultStopCause::Cancelled);
            }
            if let Some(timers) = &mut timers {
                timers.renew(host, deadline_ns);
            }
            if controller
                .as_ref()
                .is_some_and(|c| c.authentication_required())
            {
                anyhow::bail!("Codex interactive authentication required; session quarantined");
            }
            let cancelled = *shutdown.borrow();
            if let Some(stop) = loop_stop_cause(
                cancelled,
                || Ok(crate::native::http::boot_ns()? >= deadline_ns),
                || Ok(host.observe()?.phase == HostPhase::Running),
                state.result_enabled.then(|| pipeline.poll_result_claim(id)),
            )
            .await?
            {
                return Ok(stop);
            }
            tokio::select! {
                _=tokio::time::sleep(Duration::from_millis(250))=>{},
                changed=shutdown.changed()=>{if changed.is_err(){break;}}
            }
        }
        Ok(ResultStopCause::Failure)
    }
    async fn stop(&self, pre_stop: Option<PreStop>) -> Result<Result<()>> {
        // Stop what the host's proof does not cover (the sandbox) first, so the
        // host's short stop grace cannot leave it running behind a Stopped report.
        let outside_stopped = match pre_stop {
            Some(stop) => stop().await,
            None => Ok(()),
        };
        let (pipeline, id) = (&self.pipeline, self.host.attempt_id());
        // Creating this future fences launch/current transport synchronously.
        // Physical cleanup must not wait for a slow local projection fsync.
        let stop = pipeline.stop_phase_host(id);
        let (intake_closed, stopped_physical) =
            tokio::join!(pipeline.close_result_intake(id), stop);
        // Neither a relay error nor an uncertain ACK can bypass real stop.
        match stopped_physical? {
            NativeStopOutcome::StoppedPendingJournal(_) => {
                pipeline.persist_phase_host_stop(id).await?
            }
            NativeStopOutcome::Unknown => {
                pipeline.persist_phase_host_uncertain(id).await?;
                anyhow::bail!("native stop uncertain; retained history requires reconciliation")
            }
        }
        if let Err(error) = outside_stopped {
            // The host is stopped, but a sandbox may still run: never report
            // Stopped (the server then records no stop, so B4 cannot release the
            // task). reconcile-native replays Stopped once the sandbox stop is
            // confirmed.
            pipeline.persist_phase_host_uncertain(id).await?;
            return Err(
                error.context("sandbox stop uncertain; Stopped withheld for reconciliation")
            );
        }
        Ok(intake_closed)
    }
    async fn report_stopped(&self) -> Result<()> {
        let (pipeline, id) = (&self.pipeline, self.host.attempt_id());
        let state = pipeline.snapshot().await?;
        let terminal_profile = TerminalProfile::for_attempt(
            state
                .attempts
                .get(&id)
                .context("native stopped attempt absent")?,
        )?;
        terminal_profile.report_stopped(pipeline, id).await
    }
    async fn record_result(
        &self,
        cause: ResultStopCause,
        transfer: &TransferClient,
        status: (Result<()>, Result<()>),
    ) -> Result<()> {
        let (stopped, intake_closed) = status;
        let (pipeline, id) = (&self.pipeline, self.host.attempt_id());
        if stopped.is_err() {
            let _ = pipeline
                .result_needs_reconciliation(id, aq_agent_result_local::RecoveryCode::StopUnknown)
                .await;
        }
        stopped?;
        intake_closed?;
        let result = pipeline
            .retain_stopped_result(id, cause)
            .await?
            .context("explicit local result claim absent; stopped attempt retained")?;
        let source = self.transfer_source(&result, transfer).await;
        if source.is_err() {
            let _ = pipeline
                .result_needs_reconciliation(
                    id,
                    aq_agent_result_local::RecoveryCode::SourceRetentionUnknown,
                )
                .await;
        }
        source?;
        let report = pipeline.record_result_report(&result).await;
        if report.is_err() {
            let _ = pipeline
                .result_needs_reconciliation(id, aq_agent_result_local::RecoveryCode::ReportUnknown)
                .await;
        }
        report?;
        pipeline.end_result_source(&result).await;
        Ok(())
    }
    async fn transfer_source(
        &self,
        result: &DurableStoppedResult,
        transfer: &TransferClient,
    ) -> Result<()> {
        let (pipeline, id) = (&self.pipeline, self.host.attempt_id());
        if pipeline.push_result_source(result).await? {
            return Ok(());
        }
        pipeline.capture_result_source(result).await?;
        // Original source limits also bound the number of chunks. Each
        // finite request preserves the exact original operation on error.
        let snapshot = pipeline.snapshot().await?;
        let max_steps = crate::native::result::transfer_bound(
            snapshot.attempts[&id]
                .result
                .as_ref()
                .context("result history absent")?,
        )?;
        for _ in 0..max_steps {
            if pipeline
                .transfer_result_source_step(result, transfer)
                .await?
            {
                return Ok::<(), anyhow::Error>(());
            }
        }
        anyhow::bail!("native result source transfer step bound exhausted")
    }
}

struct Renewals {
    sequence: u64,
    next_renewal: Instant,
    next_poll: Instant,
}
impl Renewals {
    fn new() -> Self {
        Self {
            sequence: 1,
            next_renewal: Instant::now() + Duration::from_secs(30),
            next_poll: Instant::now() + Duration::from_secs(3),
        }
    }
    async fn cancelled(&mut self, pipeline: &InputPipeline, id: Uuid) -> bool {
        if Instant::now() >= self.next_poll {
            self.next_poll = Instant::now() + Duration::from_secs(5);
            match tokio::time::timeout(Duration::from_secs(4), pipeline.poll_cancellation(id)).await
            {
                Ok(Ok(Some(cancellation))) => {
                    tracing::warn!(attempt_id = %id, %cancellation, "owner cancellation observed; stopping the harness");
                    return true;
                }
                Ok(Ok(None)) => {}
                _ => tracing::debug!(attempt_id = %id, "cancellation poll unavailable"),
            }
        }
        false
    }
    fn renew(&mut self, host: &LocalPhaseHost, deadline_ns: u64) {
        let id = host.attempt_id();
        if Instant::now() >= self.next_renewal {
            self.sequence += 1;
            if let Err(error) = host.renew_local(self.sequence, deadline_ns / 1_000_000) {
                tracing::warn!(attempt_id = %id, "local host permit renewal refused: {error:#}");
            }
            self.next_renewal = Instant::now() + Duration::from_secs(30);
        }
    }
}
