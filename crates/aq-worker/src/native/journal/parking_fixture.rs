//! Test-only physical factory. Calling prepare/launch starts real disposable
//! pinned local processes; compilation alone does not run them. No DTO owner path.
use super::*;
use crate::native::{
    launch::{LocalPhaseHost, NativeHostLaunchResult, SyntheticMode},
    parking::DurableStoppedWorkspace,
    preparation::{DurablePhaseExecution, PhaseExecutionCommit},
};
use anyhow::{Result, ensure};

pub(crate) struct ActualParkingFixture {
    execution: DurablePhaseExecution,
    pipeline: Arc<InputPipeline>,
    binding: aq_lifecycle_protocol::Binding,
    workspace: PathBuf,
    source: ActualGitInputs,
    launched: bool,
    stopped_confirmed: bool,
}
impl ActualParkingFixture {
    pub(crate) async fn prepare(grant_window_ms: u64) -> Self {
        assert!(
            (1000..=20000).contains(&grant_window_ms),
            "explicit bounded original grant window"
        );
        let source = actual_git_inputs(true, true).await;
        let h = &source.h;
        h.grant_window_ms.store(grant_window_ms, Ordering::SeqCst);
        let inputs = phase_join(h, source.service.client.clone()).await;
        let workspace = inputs.workspace().workspace().path.clone();
        let state = h.pipeline().snapshot().await.unwrap();
        let attempt = &state.attempts[&h.id];
        assert!(
            !attempt
                .reservation
                .offer
                .offered
                .offer
                .manifest
                .terminal
                .input_enabled
        );
        let binding =
            aq_admission::transport::lifecycle_binding(&attempt.reservation.offer.offered.offer)
                .unwrap();
        let accepted = attempt.phase_acceptance.as_ref().unwrap();
        let observed = h
            .client
            .prepare_execution(
                h.proposed.as_ref().unwrap(),
                &accepted.request,
                h.phase.as_ref().unwrap(),
                inputs.request(),
                None,
            )
            .await
            .unwrap();
        let PhaseExecutionCommit::Usable(execution) = h
            .pipeline()
            .record_phase_execution(inputs, observed)
            .await
            .unwrap()
        else {
            panic!("actual Git phase execution expired before fixture admission")
        };
        let pipeline = h.pipeline().clone();
        pipeline.check_phase_execution(&execution).await.unwrap();
        Self {
            execution,
            pipeline,
            binding,
            workspace,
            source,
            launched: false,
            stopped_confirmed: false,
        }
    }
    pub(crate) fn pipeline(&self) -> Arc<InputPipeline> {
        self.pipeline.clone()
    }
    pub(crate) fn execution(&self) -> &DurablePhaseExecution {
        &self.execution
    }
    pub(crate) fn binding(&self) -> &aq_lifecycle_protocol::Binding {
        &self.binding
    }
    pub(crate) fn workspace(&self) -> &Path {
        &self.workspace
    }
    pub(crate) async fn launch(&mut self) -> Result<LocalPhaseHost> {
        ensure!(!self.launched, "physical fixture cannot relaunch");
        self.launched = true;
        // Existing root/pin/profile implementation. No ambient executable or
        // synthetic Git peer fallback is selected by this new factory.
        let profile = native_host_scenarios::profile(&self.source.h, SyntheticMode::Wait);
        match self
            .pipeline
            .launch_phase_synthetic(&self.execution, profile)
            .await?
        {
            NativeHostLaunchResult::Running(host) => Ok(host),
            NativeHostLaunchResult::StoppedBeforeStarted(_) => {
                anyhow::bail!("physical fixture stopped before Started")
            }
            NativeHostLaunchResult::Unknown(_) => {
                anyhow::bail!("physical fixture launch uncertain")
            }
        }
    }
    pub(crate) async fn confirm_stopped(
        &mut self,
        stopped: &DurableStoppedWorkspace,
    ) -> Result<()> {
        ensure!(
            stopped.attempt_id() == self.binding.attempt_id,
            "foreign stopped fixture"
        );
        let history = self.pipeline.parking_history(stopped).await?;
        ensure!(
            history.intent.suspension.binding == self.binding && history.stop.is_some(),
            "fixture actual stopped anchor mismatch"
        );
        self.stopped_confirmed = true;
        Ok(())
    }
}
impl Drop for ActualParkingFixture {
    fn drop(&mut self) {
        if self.launched && !self.stopped_confirmed {
            // This call synchronously fences; dropping its async future is NOT
            // a physical stop receipt. Preserve both private/source roots for
            // bounded original-deadline stop/recovery instead of removing them.
            drop(self.pipeline.stop_phase_host(self.binding.attempt_id));
            self.source.h._dir.disable_cleanup(true);
            self.source._dir.disable_cleanup(true);
        }
        // GitService remains the existing real process guard and is dropped
        // before its outer source TempDir. No production disk release is claimed.
    }
}
