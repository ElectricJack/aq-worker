//! Persisted terminal input receipts and their physical delivery boundary.
use super::*;

pub(super) struct InputDelivery<'a> {
    pub(super) command: &'a aq_session_protocol::InputCommand,
    pub(super) digest: Digest,
    pub(super) deadline: u64,
}

impl TerminalHost<'_> {
    pub(super) fn apply_input(&self, state: &mut State, delivery: InputDelivery<'_>) -> Result<()> {
        let InputDelivery {
            command: input,
            digest,
            deadline,
        } = delivery;
        let id = input.operation_id;
        ensure!(
            state.input.len() < MAX_INPUT_RECORDS,
            "terminal input receipt capacity"
        );
        let valid = self.validate_input(state, input, deadline).is_ok();
        let mut receipt = InputReceipt {
            operation_id: id,
            command_digest: digest.clone(),
            terminal: input.lease.terminal.clone(),
            disposition: if valid {
                InputDisposition::Injecting
            } else {
                InputDisposition::Rejected
            },
        };
        state.input.insert(
            id,
            Record {
                digest: digest.clone(),
                receipt: Receipt::Input(receipt.clone()),
            },
        );
        state.save(self.dir)?; // This is the irreversible no-replay boundary.
        if valid {
            test_fault(self.dir, "before-effect")?;
            let result = self.inject(deadline, input);
            test_fault(self.dir, "after-effect")?;
            receipt.disposition = if result.is_ok() {
                InputDisposition::Applied
            } else {
                InputDisposition::DeliveryUnknown
            };
        }
        state.input.insert(
            id,
            Record {
                digest,
                receipt: Receipt::Input(receipt),
            },
        );
        state.save(self.dir)
    }

    fn validate_input(
        &self,
        state: &State,
        input: &aq_session_protocol::InputCommand,
        deadline: u64,
    ) -> Result<()> {
        ensure!(
            !state.unknown(),
            "terminal input paused after uncertain effect"
        );
        ensure!(
            self.config.manifest.terminal().input_enabled
                && input.bytes.len() <= self.config.manifest.terminal().max_input_bytes as usize
                && input.bytes.len()
                    <= self.config.manifest.terminal().max_pending_input_bytes as usize,
            "sealed terminal input policy"
        );
        controller(state, &input.lease, wall_ms()?)?;
        ensure!(
            wall_ms()? >= input.issued_at_ms && wall_ms()? < input.deadline_ms,
            "terminal input deadline"
        );
        self.live_phase(deadline, false)
    }

    fn inject(&self, deadline: u64, input: &aq_session_protocol::InputCommand) -> Result<()> {
        self.live_phase(deadline, false)?;
        let buffer = format!("aq-{}", input.operation_id.simple());
        let bounded_deadline = deadline.min(self.ceiling());
        let timeout = Duration::from_millis(
            bounded_deadline
                .saturating_sub(ownership::raw_boot_ms())
                .min(250),
        );
        bounded_command_input(
            Command::new(&self.config.tmux)
                .args(["-N", "-S"])
                .arg(self.dir.join("tmux.sock"))
                .args(["load-buffer", "-b", &buffer, "-"]),
            Some(&input.bytes),
            timeout,
        )?;
        self.live_phase(deadline, false)?;
        ensure!(
            wall_ms()? < input.deadline_ms && wall_ms()? < input.lease.expires_at_ms,
            "terminal input expired before paste"
        );
        // -r preserves literal LF bytes; no bracketed-paste wrapper or shell exists.
        let result = self.guarded_tmux(
            deadline,
            &[
                "paste-buffer",
                "-r",
                "-d",
                "-b",
                &buffer,
                "-t",
                &self
                    .host
                    .pane
                    .as_ref()
                    .context("terminal pane missing")?
                    .pane_id,
            ],
        );
        // Target only the immutable pane ID, never a user-provided session name.
        result.map(|_| ())
    }
}
fn test_fault(_dir: &Path, _stage: &str) -> Result<()> {
    #[cfg(feature = "test-support")]
    {
        let arm = _dir.join(format!("terminal-fault-{_stage}.arm"));
        if arm.exists() {
            std::fs::remove_file(arm)?;
            durable_new(&_dir.join("terminal-fault.reached"), _stage.as_bytes())?;
            unsafe {
                libc::raise(libc::SIGSTOP);
            }
        }
    }
    Ok(())
}
