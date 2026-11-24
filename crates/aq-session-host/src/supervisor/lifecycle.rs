//! Guardian process lifecycle. Each loop observes stop and expiry before starting or renewing work.
use super::*;

pub(super) struct Guardian<'a> {
    pub(super) pane_fd: Option<std::os::fd::OwnedFd>,
    pub(super) _root: Option<Child>,
    pub(super) viewer_owner: reader_lease::Owner,
    pub(super) reader_owner: reader_lease::Owner,
    pub(super) state: HostObservation,
    pub(super) dir: &'a Path,
    pub(super) config: &'a SealedLaunch,
    pub(super) identity: &'a GuardianIdentity,
    pub(super) directory_guard: &'a terminal_io::DirectoryGuard,
    pub(super) observed_owner: &'a mut Option<observed::gate::Owner>,
    pub(super) viewer_revision_highwater: u64,
    pub(super) root_pid: Option<u32>,
    pub(super) stop_started: Option<u64>,
    pub(super) terminal_failed: bool,
    pub(super) terminal_revision_highwater: u64,
    pub(super) last_time: u64,
    pub(super) wall_deadline: u64,
}

impl Guardian<'_> {
    pub(super) fn supervise(&mut self) -> Result<()> {
        while !self.turn()? {
            std::thread::sleep(Duration::from_millis(5));
        }
        Ok(())
    }

    fn turn(&mut self) -> Result<bool> {
        let now = raw_boot_ms();
        self.check_stop(now);
        if self.reaped()? {
            return Ok(true);
        }
        let stopping = self.state.phase == HostPhase::Stopping;
        if stopping && self.stop(now)? {
            return Ok(true);
        }
        if !stopping {
            self.advance()?;
        }
        self.confirm_baseline()?;
        self.tick_readers();
        Ok(false)
    }

    fn check_stop(&mut self, now: u64) {
        let clock_regressed = now < self.last_time;
        self.last_time = now;
        self.state.observed_boot_ms = now;
        // Keep the first stop cause in the final receipt while reaping children.
        if self.state.phase == HostPhase::Stopping {
            return;
        }
        if clock_regressed {
            self.state.phase = HostPhase::Stopping;
            self.state.stop_reason = Some("clock regressed".into());
        }
        // Stop and expiry are evaluated before renewal or any new process effect.
        if self.dir.join("stop.request").exists() {
            match request(self.dir, "stop.request", self.identity) {
                Ok(Some(_)) => {
                    self.state.phase = HostPhase::Stopping;
                    self.state.stop_reason = Some("cancelled".into());
                }
                _ => {
                    self.state.phase = HostPhase::Stopping;
                    self.state.stop_reason = Some("invalid stop directive".into());
                }
            }
        }
        if now >= self.state.permit_deadline_boot_ms || now >= self.wall_deadline {
            self.state.phase = HostPhase::Stopping;
            self.state.stop_reason = Some("execution permit expired".into());
        }
    }

    fn reaped(&mut self) -> Result<bool> {
        let reaped = reap_children()?;
        self.state.reaped_count += reaped.processes.len() as u64;
        for (pid, status) in &reaped.processes {
            if Some(*pid) == self.root_pid {
                self.state.root_status = Some(*status);
            }
        }
        let session_started_or_stopping =
            self.root_pid.is_some() || self.state.phase == HostPhase::Stopping;
        if reaped.kernel_echild && session_started_or_stopping {
            self.state.phase = HostPhase::Stopped;
            self.state.kernel_wait_echild = true;
            save(self.dir, &self.state)?;
            return Ok(true);
        }
        if self.state.root_status.is_some() {
            self.state.phase = HostPhase::Stopping;
            self.state
                .stop_reason
                .get_or_insert("tmux server exited".into());
        }
        Ok(false)
    }

    fn stop(&mut self, now: u64) -> Result<bool> {
        let started = *self.stop_started.get_or_insert(now);
        let grace = self.config.manifest.resources().stop_grace_ms.min(1000);
        signal_children_bounded(if now.saturating_sub(started) < grace {
            libc::SIGTERM
        } else {
            libc::SIGKILL
        })?;
        save(self.dir, &self.state)?;
        if now.saturating_sub(started) >= 5000 {
            self.state.phase = HostPhase::StopUncertain;
            save(self.dir, &self.state)?;
            return Ok(true);
        }
        Ok(false)
    }

    fn advance(&mut self) -> Result<()> {
        let startup = self.start_server().and_then(|()| self.start_pane());
        if let Err(error) = startup {
            self.state.phase = HostPhase::Stopping;
            self.state.stop_reason = Some(format!("startup failed: {error}"));
            save(self.dir, &self.state)?;
        }
        if self.state.phase == HostPhase::Running {
            let running = self.maintain_pane();
            if let Err(error) = running {
                self.state.phase = HostPhase::Stopping;
                self.state.stop_reason = Some(format!("running validation failed: {error}"));
                save(self.dir, &self.state)?;
            }
        }
        Ok(())
    }

    fn may_launch(&self) -> bool {
        raw_boot_ms() < self.state.permit_deadline_boot_ms
            && raw_boot_ms() < self.wall_deadline
            && !self.dir.join("stop.request").exists()
    }

    fn start_server(&mut self) -> Result<()> {
        if self.state.phase != HostPhase::Waiting || !self.dir.join("start.request").exists() {
            return Ok(());
        }
        let permit = request(self.dir, "start.request", self.identity)?.context("start request")?;
        ensure!(permit == self.config.permit, "changed initial start permit");
        self.config.validate_permit_at(
            &permit,
            &self.identity.intent.session,
            &self.identity.intent.manifest_digest,
            raw_boot_ms(),
        )?;
        self.state.phase = HostPhase::Starting;
        save(self.dir, &self.state)?;
        ensure!(
            !self.dir.join("tmux.sock").exists(),
            "tmux socket collision"
        );
        fault_point(self.dir, "before-tmux-spawn")?;
        ensure!(
            self.may_launch(),
            "authority expired/cancelled before tmux spawn"
        );
        let child = Command::new(&self.config.tmux)
            .args(["-D", "-u", "-S"])
            .arg(self.dir.join("tmux.sock"))
            .args(["-f", "/dev/null"])
            .env_clear()
            .env("PATH", "/usr/bin:/bin")
            .env("TERM", "xterm-256color")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()?;
        self.root_pid = Some(child.id());
        fault_point(self.dir, "after-tmux-spawn")?;
        let server = process_evidence(child.id())?;
        self._root = Some(child);
        durable_new(&self.dir.join("server.json"), &serde_json::to_vec(&server)?)?;
        Ok(())
    }

    fn start_pane(&mut self) -> Result<()> {
        // tmux binds its socket before it listens; a client that
        // connects in between fails with "no server running".
        if self.state.phase != HostPhase::Starting || self.state.pane.is_some() {
            return Ok(());
        }
        if std::os::unix::net::UnixStream::connect(self.dir.join("tmux.sock")).is_ok() {
            self.create_pane()?;
            self.record_pane()?;
        }
        Ok(())
    }

    fn create_pane(&self) -> Result<()> {
        fault_point(self.dir, "socket-created")?;
        ensure!(
            self.may_launch(),
            "authority expired/cancelled before pane launch"
        );
        tmux(
            self.config,
            self.dir,
            &["set-option", "-g", "remain-on-exit", "on"],
        )?;
        let helper = self.config.helper.to_str().context("helper UTF-8")?;
        let launch_dir = self.dir.to_str().context("launch directory UTF-8")?;
        let workspace = self.config.workspace.to_str().context("workspace UTF-8")?;
        ensure!(
            self.may_launch(),
            "authority expired/cancelled immediately before pane creation"
        );
        tmux(
            self.config,
            self.dir,
            &["set-option", "-g", "status", "off"],
        )?;
        // Input on an attached viewer is for the pane. Disabling
        // the prefix keeps the tmux command prompt out of that path.
        tmux(
            self.config,
            self.dir,
            &["set-option", "-g", "prefix", "None"],
        )?;
        let columns = self
            .config
            .observed
            .as_ref()
            .map_or(80, |s| s.columns)
            .to_string();
        let rows = self
            .config
            .observed
            .as_ref()
            .map_or(24, |s| s.rows)
            .to_string();
        let mut arguments = vec!["new-session", "-d", "-s", "aq", "-c", workspace];
        arguments.extend(["-x", &columns, "-y", &rows]);
        arguments.extend(["--", helper, "pane", launch_dir]);
        tmux(self.config, self.dir, &arguments)?;
        // After the session exists: tmux 3.4's server exits on `new-session` when
        // `window-size manual` is set globally before the first session.
        tmux(
            self.config,
            self.dir,
            &["set-window-option", "-t", "aq", "window-size", "manual"],
        )?;
        fault_point(self.dir, "pane-created")?;
        Ok(())
    }

    fn record_pane(&mut self) -> Result<()> {
        let (server_pid, pane_pid, dead, pane_id) = pane_status(self.config, self.dir)?;
        ensure!(
            Some(server_pid) == self.root_pid && !dead,
            "tmux/pane identity mismatch"
        );
        ensure!(
            parent(server_pid)? == std::process::id() && descendant(pane_pid, server_pid)?,
            "tmux escaped guardian ancestry"
        );
        let pane = PaneIdentity {
            server: process_evidence(server_pid)?,
            pane: process_evidence(pane_pid)?,
            pane_id,
            session_name: "aq".into(),
        };
        self.pane_fd = Some(verified_pidfd(&pane.pane)?);
        self.state.pane = Some(pane);
        if self.config.observed.is_none() {
            self.state.phase = HostPhase::Running;
        }
        save(self.dir, &self.state)?;
        Ok(())
    }

    fn renew(&mut self) -> Result<()> {
        if let Some(permit) = request(self.dir, "renew.request", self.identity)?
            && permit.sequence > self.state.permit_sequence
        {
            self.config.validate_permit_at(
                &permit,
                &self.identity.intent.session,
                &self.identity.intent.manifest_digest,
                raw_boot_ms(),
            )?;
            ensure!(
                raw_boot_ms() < self.state.permit_deadline_boot_ms,
                "late renewal cannot revive session"
            );
            ensure!(
                permit.expires_boot_ms <= self.wall_deadline,
                "renewal exceeds attempt wall limit"
            );
            if matches!(self.config.manifest, LaunchDocument::Plan(_)) {
                ensure!(
                    self.state.permit_sequence.checked_add(1) == Some(permit.sequence)
                        && permit.expires_boot_ms > self.state.permit_deadline_boot_ms,
                    "host renewal must advance exact live prior permit"
                );
            }
            self.state.permit_sequence = permit.sequence;
            self.state.permit_deadline_boot_ms = permit.expires_boot_ms;
            save(self.dir, &self.state)?;
        }
        Ok(())
    }

    fn maintain_pane(&mut self) -> Result<()> {
        self.renew()?;
        if !ownership::pidfd_is_live(self.pane_fd.as_ref().context("pane pidfd")?)? {
            self.state.phase = HostPhase::Stopping;
            self.state.stop_reason = Some("harness pane exited".into());
            save(self.dir, &self.state)?;
        }
        if self.state.phase != HostPhase::Running || self.terminal_failed {
            return Ok(());
        }
        let terminal = terminal_io::TerminalHost {
            dir: self.dir,
            config: self.config,
            host: &self.state,
            deadline: self.wall_deadline,
        };
        if terminal
            .tick(self.directory_guard, &mut self.terminal_revision_highwater)
            .is_err()
        {
            // Terminal IPC failure must not block independent expiry.
            self.terminal_failed = true;
            let _ = durable_new(
                &self.dir.join("terminal.disabled"),
                b"terminal effects refused\n",
            );
        }
        Ok(())
    }

    fn confirm_baseline(&mut self) -> Result<()> {
        if let Some(owner) = self.observed_owner.as_mut()
            && matches!(self.state.phase, HostPhase::Waiting | HostPhase::Starting)
        {
            match owner.tick(&observed::gate::GateTurn {
                config: self.config,
                host: &self.state,
                directory: self.dir,
                readers: &self.reader_owner,
                guard: self.directory_guard,
            }) {
                Ok(true) => {
                    self.state.phase = HostPhase::Running;
                    self.state.observed_boot_ms = raw_boot_ms();
                    save(self.dir, &self.state)?;
                }
                Ok(false) => {}
                Err(error) => {
                    // Production guardian stderr is /dev/null; test builds keep guardian-test-stderr.log.
                    eprintln!("observed gate failed closed: {error:#}");
                    self.state.phase = HostPhase::Stopping;
                    self.state.stop_reason = Some(format!(
                        "observed gate failed closed; post-grant exec evidence may be unknown: {error:#}"
                    ));
                    save(self.dir, &self.state)?;
                }
            }
        }
        Ok(())
    }

    fn tick_readers(&mut self) {
        let reader_host = terminal_io::TerminalHost {
            dir: self.dir,
            config: self.config,
            host: &self.state,
            deadline: self.wall_deadline,
        };
        self.reader_owner.tick(
            &reader_host,
            reader_lease::Gate {
                guard: self.directory_guard,
                highwater: &mut self.terminal_revision_highwater,
                pending_observation: self
                    .observed_owner
                    .as_ref()
                    .and_then(|o| o.closed_ready())
                    .is_some(),
            },
        );
        self.viewer_owner.tick(
            &reader_host,
            reader_lease::Gate {
                guard: self.directory_guard,
                highwater: &mut self.viewer_revision_highwater,
                pending_observation: false,
            },
        );
    }
}
