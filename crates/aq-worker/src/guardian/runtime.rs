use super::*;

pub(super) struct Guardian<'a> {
    directory: &'a Path,
    binding: LaunchBinding,
    state: GuardianState,
    // Keep Child alive for ownership metadata; all reaping uses waitpid.
    root_child: Option<Child>,
    root_pid: Option<u32>,
    last_now: u64,
    stop_started: Option<u64>,
}
impl<'a> Guardian<'a> {
    pub(super) fn new(directory: &'a Path, binding: LaunchBinding, state: GuardianState) -> Self {
        Self {
            directory,
            binding,
            state,
            root_child: None,
            root_pid: None,
            last_now: 0,
            stop_started: None,
        }
    }
    pub(super) fn serve(&mut self, listener: UnixListener, token: &str) -> Result<()> {
        loop {
            let now = self.observe_clock();
            self.expire(now)?;
            if self.reaped_all(now)? {
                return Ok(());
            }
            if self.state.phase == GuardianPhase::Stopping && self.stop_children()? {
                return Ok(());
            }
            self.serve_control(&listener, token)?;
            std::thread::sleep(Duration::from_millis(5));
        }
    }
    fn serve_control(&mut self, listener: &UnixListener, token: &str) -> Result<()> {
        for _ in 0..8 {
            if self.state.phase == GuardianPhase::Stopping || self.expired_before_control()? {
                break;
            }
            match listener.accept() {
                Ok((mut stream, _)) => self.handle_stream(&mut stream, token)?,
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => break,
                Err(error) => return Err(error.into()),
            }
        }
        Ok(())
    }
    fn observe_clock(&mut self) -> u64 {
        let now = match logical_now(&self.binding, &mut self.last_now) {
            Ok(now) => now,
            Err(_) => {
                self.state.stop_reason = Some(ProcessOutcome::Expired);
                self.state.phase = GuardianPhase::Stopping;
                self.state.clock_uncertain = true;
                self.last_now
            }
        };
        self.state.observed_at_ms = now;
        now
    }
    fn expire(&mut self, now: u64) -> Result<()> {
        if now >= self.state.deadline_ms
            && !matches!(
                self.state.phase,
                GuardianPhase::Stopped | GuardianPhase::Stopping
            )
        {
            self.state.phase = GuardianPhase::Stopping;
            self.state.stop_reason = Some(ProcessOutcome::Expired);
            self.save()?;
        }
        Ok(())
    }
    fn reaped_all(&mut self, now: u64) -> Result<bool> {
        if reap(&mut self.state, self.root_pid)? && self.state.phase != GuardianPhase::Waiting {
            self.state.phase = GuardianPhase::Stopped;
            self.state.kernel_wait_echild = true;
            self.state.stopped_at_ms = Some(now);
            self.save()?;
            return Ok(true);
        }
        // A root exiting with surviving descendants is not task completion.
        // Stop the adopted leftovers before publishing any stopped proof.
        if self.state.root_status.is_some() && self.state.phase == GuardianPhase::Running {
            self.state.phase = GuardianPhase::Stopping;
            self.state.stop_reason = Some(ProcessOutcome::Failed);
            self.save()?;
        }
        Ok(false)
    }
    fn stop_children(&mut self) -> Result<bool> {
        // Termination budgets use independent raw BOOTTIME even when the
        // authority/fixture clock is unreadable or has regressed.
        let raw_now = crate::clock::raw_boot_ms();
        let started = *self.stop_started.get_or_insert(raw_now);
        let sig = if raw_now.saturating_sub(started) < TERM_GRACE_MS {
            libc::SIGTERM
        } else {
            libc::SIGKILL
        };
        signal_children(sig)?;
        if raw_now.saturating_sub(started) >= STOP_LIMIT_MS {
            self.state.phase = GuardianPhase::StopUncertain;
            self.save()?;
            return Ok(true);
        }
        Ok(false)
    }
    fn expired_before_control(&mut self) -> Result<bool> {
        let fresh = logical_now(&self.binding, &mut self.last_now);
        if fresh
            .as_ref()
            .map_or(true, |now| *now >= self.state.deadline_ms)
        {
            self.state.phase = GuardianPhase::Stopping;
            self.state.stop_reason = Some(ProcessOutcome::Expired);
            self.save()?;
            return Ok(true);
        }
        Ok(false)
    }
    fn handle_stream(&mut self, stream: &mut UnixStream, token: &str) -> Result<()> {
        let read_ms = 20;
        #[cfg(feature = "test-support")]
        let read_ms = std::env::var("AQ_GUARDIAN_TEST_READ_MS")
            .ok()
            .map(|v| v.parse::<u64>().unwrap().clamp(1, 500))
            .unwrap_or(read_ms);
        stream.set_read_timeout(Some(Duration::from_millis(read_ms)))?;
        #[cfg(feature = "test-support")]
        if let Some(path) = std::env::var_os("AQ_GUARDIAN_TEST_READ_MARKER") {
            durable_replace(Path::new(&path), b"reading control body\n")?;
        }
        stream.set_write_timeout(Some(Duration::from_millis(20)))?;
        let received = read_bounded(stream, Duration::from_millis(read_ms));
        let Ok(bytes) = received else {
            return Ok(());
        };
        if peer_pid(stream).is_err() {
            return Ok(());
        }
        let Ok(request) = serde_json::from_slice::<Control>(&bytes) else {
            return Ok(());
        };
        if request.token != token || request.identity != self.state.identity {
            return Ok(());
        }
        let result = self.apply(request.operation);
        let reply = Reply {
            state: self.state.clone(),
            error: result.err().map(|e| e.to_string()),
        };
        let _ = write_bounded(
            stream,
            &serde_json::to_vec(&reply)?,
            Duration::from_millis(20),
        );
        Ok(())
    }
    fn apply(&mut self, operation: Operation) -> Result<()> {
        // A read may stall. Never validate Start/Renew against
        // the stale timestamp from before receiving its bytes.
        let now = logical_now(&self.binding, &mut self.last_now)?;
        self.state.observed_at_ms = now;
        match operation {
            Operation::Status => Ok(()),
            Operation::Stop { reason } => self.stop(reason),
            Operation::Start {
                grant_sequence,
                deadline_ms,
            } => self.start(now, grant_sequence, deadline_ms),
            Operation::Renew {
                grant_sequence,
                deadline_ms,
            } => self.renew(now, grant_sequence, deadline_ms),
        }
    }
    fn stop(&mut self, reason: ProcessOutcome) -> Result<()> {
        ensure!(
            matches!(
                reason,
                ProcessOutcome::Cancelled | ProcessOutcome::Expired | ProcessOutcome::Failed
            ),
            "invalid stop reason"
        );
        self.state.stop_reason = Some(reason);
        self.state.phase = GuardianPhase::Stopping;
        self.save()
    }
    fn start(&mut self, now: u64, grant_sequence: u64, deadline_ms: u64) -> Result<()> {
        ensure!(
            self.state.phase == GuardianPhase::Waiting && self.root_pid.is_none(),
            "launch already started or terminal"
        );
        ensure!(
            now < self.state.deadline_ms
                && now < deadline_ms
                && deadline_ms <= self.binding.initial_deadline_ms
                && grant_sequence == self.binding.initial_grant_sequence,
            "expired/mismatched start grant"
        );
        self.state.phase = GuardianPhase::Running;
        self.state.deadline_ms = deadline_ms;
        self.save()?;
        let spawn_now = logical_now(&self.binding, &mut self.last_now)?;
        ensure!(
            spawn_now < self.state.deadline_ms,
            "grant expired before writer spawn"
        );
        let child = Command::new(&self.binding.executable)
            .current_dir(self.binding.workspace.as_deref().unwrap_or(self.directory))
            .arg("--launch-dir")
            .arg(self.directory)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()?;
        self.root_pid = Some(child.id());
        self.state.root_process = Some(crate::fixture::process_evidence(child.id())?);
        self.root_child = Some(child);
        self.save()
    }
    fn renew(&mut self, now: u64, grant_sequence: u64, deadline_ms: u64) -> Result<()> {
        ensure!(
            self.state.phase == GuardianPhase::Running
                && now < self.state.deadline_ms
                && now < deadline_ms
                && deadline_ms <= now.saturating_add(MAX_GRANT_MS)
                && grant_sequence > self.state.grant_sequence,
            "late/stale guardian renewal"
        );
        self.state.grant_sequence = grant_sequence;
        self.state.deadline_ms = deadline_ms;
        self.save()
    }
    fn save(&self) -> Result<()> {
        save(self.directory, &self.state)
    }
}
