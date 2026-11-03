//! Synthetic execution fixture behind the Linux subreaper guardian profile.
use crate::guardian::{
    self, GuardianIdentity, GuardianPhase, GuardianState, LaunchBinding, Operation,
};
use anyhow::{Context, Result, bail, ensure};
use aq_session_host::ownership::ProcessEvidence;
use serde::{Deserialize, Serialize};
use std::{
    fs::File,
    os::unix::process::ExitStatusExt,
    path::{Path, PathBuf},
    process::{Child, Command, ExitStatus},
    time::Duration,
};
use uuid::Uuid;
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct FixtureInput {
    output: String,
}
pub struct FixtureHandle {
    child: Option<Child>,
    launch_id: Uuid,
    directory: PathBuf,
    identity: GuardianIdentity,
}
impl FixtureHandle {
    pub fn spawn(root: &Path, binding: LaunchBinding, output: &str) -> Result<Self> {
        guardian::probe_profile()?;
        let directory = root.join("launches").join(binding.launch_id.to_string());
        std::fs::create_dir_all(&directory)?;
        guardian::durable_new(
            &directory.join("input.json"),
            &serde_json::to_vec(&FixtureInput {
                output: output.into(),
            })?,
        )?;
        guardian::durable_new(
            &directory.join("guardian-launch.json"),
            &serde_json::to_vec(&binding)?,
        )?;
        guardian::durable_new(
            &directory.join("control.token"),
            Uuid::new_v4().to_string().as_bytes(),
        )?;
        File::open(directory.parent().unwrap())?.sync_all()?;
        File::open(root)?.sync_all()?;
        let mut child = Command::new(&binding.executable)
            .arg("--guardian")
            .arg("--launch-dir")
            .arg(&directory)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .context("spawn guardian; launch remains uncertain")?;
        crate::fault::checkpoint(root, "guardian-spawned", Some(binding.launch_id))?;
        let deadline = std::time::Instant::now() + Duration::from_secs(2);
        let identity = loop {
            if let Ok(identity) = guardian::read_identity(&directory) {
                ensure!(
                    identity.launch_id == binding.launch_id,
                    "guardian launch mismatch"
                );
                break identity;
            }
            ensure!(
                child.try_wait()?.is_none(),
                "guardian profile refused or startup failed before identity"
            );
            ensure!(
                std::time::Instant::now() < deadline,
                "guardian identity startup timeout"
            );
            std::thread::sleep(Duration::from_millis(2));
        };
        let handle = Self {
            child: Some(child),
            launch_id: binding.launch_id,
            directory,
            identity,
        };
        handle.query()?;
        Ok(handle)
    }
    pub fn recover(
        root: &Path,
        launch_id: Uuid,
        expected: Option<&GuardianIdentity>,
    ) -> Result<Self> {
        let directory = root.join("launches").join(launch_id.to_string());
        let identity = guardian::read_identity(&directory)?;
        ensure!(
            identity.launch_id == launch_id && expected.is_none_or(|x| *x == identity),
            "recovery guardian identity mismatch"
        );
        Ok(Self {
            child: None,
            launch_id,
            directory,
            identity,
        })
    }
    pub fn identity(&self) -> &GuardianIdentity {
        &self.identity
    }
    pub fn launch_id(&self) -> Uuid {
        self.launch_id
    }
    pub fn query(&self) -> Result<GuardianState> {
        let state = guardian::read_state(&self.directory, &self.identity)?;
        if state.phase == GuardianPhase::Stopped {
            return Ok(state);
        }
        if state.phase == GuardianPhase::Stopping {
            let _live = guardian::validate_identity(&self.identity)?;
            return Ok(state);
        }
        match guardian::control(
            &self.directory,
            &self.identity,
            Operation::Status,
            || Ok(()),
        ) {
            Ok(s) => Ok(s),
            Err(error) => {
                let final_state = guardian::read_state(&self.directory, &self.identity)?;
                if final_state.phase == GuardianPhase::Stopped {
                    Ok(final_state)
                } else {
                    Err(error).context("guardian unavailable; stopped writers not proven")
                }
            }
        }
    }
    pub fn start(
        &self,
        root: &Path,
        grant_sequence: u64,
        deadline_ms: u64,
    ) -> Result<GuardianState> {
        guardian::control(
            &self.directory,
            &self.identity,
            Operation::Start {
                grant_sequence,
                deadline_ms,
            },
            || crate::fault::checkpoint(root, "start-sent-before-receipt", Some(self.launch_id)),
        )
    }
    pub fn renew(&self, grant_sequence: u64, deadline_ms: u64) -> Result<()> {
        guardian::control(
            &self.directory,
            &self.identity,
            Operation::Renew {
                grant_sequence,
                deadline_ms,
            },
            || Ok(()),
        )?;
        Ok(())
    }
    pub fn evidence(&self) -> Result<ProcessEvidence> {
        self.query()?
            .root_process
            .context("writer process not started")
    }
    pub fn stop(&self, reason: aq_worker_protocol::ProcessOutcome) -> Result<()> {
        let state = self.query()?;
        if matches!(
            state.phase,
            GuardianPhase::Stopped | GuardianPhase::Stopping
        ) {
            return Ok(());
        }
        match guardian::control(
            &self.directory,
            &self.identity,
            Operation::Stop { reason },
            || Ok(()),
        ) {
            Ok(_) => Ok(()),
            Err(error) => {
                let final_state = guardian::read_state(&self.directory, &self.identity)?;
                if final_state.phase == GuardianPhase::Stopped {
                    Ok(())
                } else {
                    Err(error)
                }
            }
        }
    }
    pub fn kill(&mut self) -> Result<()> {
        self.stop(aq_worker_protocol::ProcessOutcome::Cancelled)
    }
    pub fn try_wait(&mut self) -> Result<Option<ExitStatus>> {
        let state = self.query()?;
        if let Some(child) = &mut self.child {
            let _ = child.try_wait()?;
        }
        match state.phase {
            GuardianPhase::Stopped => Ok(Some(ExitStatus::from_raw(
                state.root_status.unwrap_or(1 << 8),
            ))),
            GuardianPhase::StopUncertain => {
                bail!("guardian could not prove all writers stopped; resources remain protected")
            }
            _ => Ok(None),
        }
    }
    pub fn wait(mut self) -> Result<(ExitStatus, String, GuardianState)> {
        let deadline = std::time::Instant::now() + Duration::from_secs(6);
        while self.try_wait()?.is_none() {
            ensure!(
                std::time::Instant::now() < deadline,
                "guardian stop proof timeout"
            );
            std::thread::sleep(Duration::from_millis(2));
        }
        let state = self.query()?;
        ensure!(state.kernel_wait_echild, "missing kernel writer-stop proof");
        if let Some(child) = &mut self.child {
            let reap_deadline = std::time::Instant::now() + Duration::from_millis(250);
            while child.try_wait()?.is_none() {
                ensure!(
                    std::time::Instant::now() < reap_deadline,
                    "guardian did not exit after kernel stopped proof"
                );
                std::thread::sleep(Duration::from_millis(1));
            }
        }
        let output = if self.directory.join("output.txt").exists() {
            std::fs::read_to_string(self.directory.join("output.txt"))?
        } else {
            String::new()
        };
        Ok((
            ExitStatus::from_raw(state.root_status.unwrap_or(1 << 8)),
            output,
            state,
        ))
    }
}
/// Fixture-only descendant mode deliberately changes session and double-forks.
/// It stays in the guardian's kernel ancestry and continuously changes a counter.
pub fn run_fixture(directory: &Path) -> Result<()> {
    guardian::durable_new(
        &directory.join("started.claim"),
        b"deterministic-agent-v1\n",
    )?;
    let input: FixtureInput =
        serde_json::from_slice(&std::fs::read(directory.join("input.json"))?)?;
    ensure!(
        input.output.len() <= aq_worker_protocol::MAX_TEXT,
        "fixture input too large"
    );
    guardian::durable_new(
        &directory.join("process.json"),
        &serde_json::to_vec(&process_evidence(std::process::id())?)?,
    )?;
    if std::env::var_os("AQ_FIXTURE_IGNORE_TERM").is_some() {
        unsafe { libc::signal(libc::SIGTERM, libc::SIG_IGN) };
    }
    if std::env::var_os("AQ_FIXTURE_DESCENDANT").is_some() {
        spawn_escaped_writer(directory)?;
        if std::env::var("AQ_FIXTURE_DESCENDANT").ok().as_deref()
            == Some("double-fork-setsid-root-exit")
        {
            let deadline = std::time::Instant::now() + Duration::from_secs(2);
            while !directory.join("descendant-counter").exists() {
                ensure!(
                    std::time::Instant::now() < deadline,
                    "descendant startup timeout"
                );
                std::thread::sleep(Duration::from_millis(2));
            }
            return Ok(());
        }
    }
    while std::env::var_os("AQ_FIXTURE_HOLD").is_some() && !directory.join("release").exists() {
        std::thread::sleep(Duration::from_millis(5));
    }
    guardian::durable_new(&directory.join("output.txt"), input.output.as_bytes())?;
    Ok(())
}
fn spawn_escaped_writer(directory: &Path) -> Result<()> {
    // SAFETY: this dedicated fixture binary is single-threaded; fork branches
    // perform no shared-runtime operations. It is never called inside Tokio.
    let first = unsafe { libc::fork() };
    ensure!(first >= 0, "fixture fork failed");
    if first == 0 {
        if unsafe { libc::setsid() } < 0 {
            unsafe { libc::_exit(20) }
        }
        let second = unsafe { libc::fork() };
        if second < 0 {
            unsafe { libc::_exit(21) }
        }
        if second > 0 {
            unsafe { libc::_exit(0) }
        }
        let result = (|| -> Result<()> {
            guardian::durable_new(
                &directory.join("descendant.json"),
                &serde_json::to_vec(&process_evidence(std::process::id())?)?,
            )?;
            let mut count = 0u64;
            while !directory.join("release-descendant").exists() {
                count += 1;
                guardian::durable_replace(
                    &directory.join("descendant-counter"),
                    count.to_string().as_bytes(),
                )?;
                std::thread::sleep(Duration::from_millis(5));
            }
            Ok(())
        })();
        unsafe { libc::_exit(if result.is_ok() { 0 } else { 22 }) }
    }
    let mut status = 0;
    ensure!(
        unsafe { libc::waitpid(first, &mut status, 0) } == first,
        "fixture intermediate reap failed"
    );
    Ok(())
}
pub fn process_evidence(pid: u32) -> Result<ProcessEvidence> {
    aq_session_host::ownership::process_evidence(pid)
}
