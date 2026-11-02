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
    /// Local captured-tree verification only. This does not create a successor
    /// Base, launch owner, source release, or authority from serialized history.
    pub(crate) async fn verify_captured_files(
        &self,
        stopped: &DurableStoppedWorkspace,
        tracked: &[u8],
        untracked: &[u8],
    ) -> Result<()> {
        use std::os::unix::fs::OpenOptionsExt;
        ensure!(
            tracked.len() <= 65536 && untracked.len() <= 65536,
            "verification file byte bound"
        );
        let history = self.pipeline.parking_history(stopped).await?;
        ensure!(
            history.intent.suspension.binding == self.binding && history.settled_source_claim()?,
            "verification requires this actual stopped source owner"
        );
        let original = history.digest()?;
        let checkpoint = history
            .source
            .checkpoint
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("captured checkpoint absent"))?;
        let git = &self.source.service.client;
        let create_id = Uuid::new_v4();
        let created = git.exchange(&json!({
            "protocol_version":1,"request_id":create_id,"operation_id":create_id,
            "operation":"workspace.create","repository_id":history.intent.source.repository_id,
            "expected_binding_generation":history.intent.source.binding_generation,
            "params":{"base":checkpoint.source}
        }))?;
        let workspace: crate::source::Workspace = serde_json::from_value(created.clone())?;
        // Never retire the protected original even if a malformed reply tries
        // to alias it. Such uncertainty preserves the fixture roots.
        ensure!(
            workspace.workspace_id != history.intent.workspace.workspace_id,
            "verification workspace aliases protected original"
        );
        let checked = (|| -> Result<()> {
            git.validate_workspace(&workspace)?;
            ensure!(
                created["repository_id"] == history.intent.source.repository_id.to_string()
                    && created["binding_generation"] == history.intent.source.binding_generation
                    && created["base"] == checkpoint.source.hex
                    && created["lifecycle"] == "active"
                    && created["allocating_operation"] == create_id.to_string(),
                "verification exact captured checkpoint workspace"
            );
            let inspect = git.query(
                &history.intent.source,
                "workspace.inspect",
                json!({"workspace_id":workspace.workspace_id}),
            )?;
            ensure!(
                inspect["workspace"] == created
                    && inspect["head"] == serde_json::to_value(&checkpoint.source)?
                    && inspect["clean"] == true,
                "verification workspace is not the captured clean tree"
            );
            let directory = std::fs::OpenOptions::new()
                .read(true)
                .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC)
                .open(&workspace.path)?;
            exact_captured_member(&directory, c"native-input.txt", tracked)?;
            exact_captured_member(&directory, c"wrk16-untracked.txt", untracked)
        })();
        let retired = git.exchange(
            &crate::source::GitAction::Retire {
                workspace_id: workspace.workspace_id,
                generation: workspace.generation,
                head: checkpoint.source.clone(),
            }
            .request(&history.intent.source, Uuid::new_v4()),
        )?;
        ensure!(
            retired["workspace_id"] == workspace.workspace_id.to_string()
                && retired["lifecycle"] == "retired",
            "verification workspace retirement not confirmed"
        );
        ensure!(
            self.pipeline.parking_history(stopped).await?.digest()? == original,
            "local verification changed protected parking history"
        );
        checked
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
fn exact_captured_member(
    directory: &std::fs::File,
    name: &std::ffi::CStr,
    expected: &[u8],
) -> Result<()> {
    use std::{
        io::Read,
        os::{
            fd::{AsRawFd, FromRawFd},
            unix::fs::MetadataExt,
        },
    };
    let fd = unsafe {
        libc::openat(
            directory.as_raw_fd(),
            name.as_ptr(),
            libc::O_RDONLY | libc::O_NOFOLLOW | libc::O_NONBLOCK | libc::O_CLOEXEC,
        )
    };
    ensure!(fd >= 0, "captured fixed file absent/unsafe");
    let mut file = unsafe { std::fs::File::from_raw_fd(fd) };
    let metadata = file.metadata()?;
    ensure!(
        metadata.is_file()
            && metadata.nlink() == 1
            && metadata.uid() == unsafe { libc::geteuid() }
            && metadata.len() == expected.len() as u64,
        "captured fixed file shape/length"
    );
    let mut bytes = Vec::new();
    Read::by_ref(&mut file)
        .take(65537)
        .read_to_end(&mut bytes)?;
    ensure!(bytes == expected, "captured fixed file contents differ");
    Ok(())
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
