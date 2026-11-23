//! Guardian-side admission and execution of bounded terminal effects.
use super::*;
mod input;
use input::InputDelivery;

const MAILBOXES: [&str; 2] = ["terminal-fence.request", "terminal-effect.request"];

pub(crate) struct TerminalHost<'a> {
    pub(crate) dir: &'a Path,
    pub(crate) config: &'a SealedLaunch,
    pub(crate) host: &'a HostObservation,
    pub(crate) deadline: u64,
}

impl TerminalHost<'_> {
    pub(crate) fn ceiling(&self) -> u64 {
        self.host.permit_deadline_boot_ms.min(self.deadline)
    }

    /// Guardian-only; one fence and one data effect at most per ownership-loop turn.
    pub(crate) fn tick(
        &self,
        directory_guard: &DirectoryGuard,
        revision_highwater: &mut u64,
    ) -> Result<()> {
        if !self.has_bound_request() {
            return Ok(());
        }
        directory_guard.verify()?;
        let Ok(_lock) = RenewalLock::acquire(&self.dir.join("terminal.lock")) else {
            return Ok(());
        };
        let binding: TerminalBinding = private_json(&self.dir.join(BINDING), MAX_REQUEST)?;
        ensure!(
            self.matches_binding(&binding, directory_guard),
            "terminal guardian binding mismatch"
        );
        let mut state = load(self.dir, &binding)?;
        ensure!(
            state.revision >= *revision_highwater,
            "terminal ledger rollback refused"
        );
        *revision_highwater = state.revision;
        for name in MAILBOXES {
            let path = self.dir.join(name);
            if !path.exists() {
                continue;
            }
            self.process_mailbox(&path, &binding, &mut state)?;
            *revision_highwater = state.revision;
            directory_guard.verify()?;
        }
        Ok(())
    }
    fn has_bound_request(&self) -> bool {
        MAILBOXES.iter().any(|path| self.dir.join(path).exists()) && self.dir.join(BINDING).exists()
    }

    fn matches_binding(&self, binding: &TerminalBinding, guard: &DirectoryGuard) -> bool {
        binding.guardian == self.host.guardian
            && self.host.pane.as_ref() == Some(&binding.pane)
            && binding.directory == guard.identity
    }

    fn process_mailbox(
        &self,
        path: &Path,
        binding: &TerminalBinding,
        state: &mut State,
    ) -> Result<()> {
        let request = decrypt(self.dir, binding, &private_bytes(path, MAX_REQUEST)?)?;
        let id = operation_id(&request.effect)?;
        let digest = digest(&request.effect)?;
        if let Some(old) = state.record(id) {
            ensure!(
                old.digest == digest,
                "terminal request changed after admission"
            );
        } else {
            self.apply(state, &request, (id, digest))?;
        }
        std::fs::remove_file(path)?;
        File::open(self.dir)?.sync_all()?;
        Ok(())
    }

    fn active(&self, pending_observation: bool) -> bool {
        self.host.phase == HostPhase::Running
            || pending_observation && self.host.phase == HostPhase::Starting
    }

    pub(super) fn live_phase(&self, deadline: u64, pending_observation: bool) -> Result<()> {
        let now = ownership::raw_boot_ms();
        ensure!(
            self.active(pending_observation)
                && now < self.ceiling().min(deadline)
                && !self.dir.join("stop.request").exists(),
            "terminal authority expired or cancelled"
        );
        let pane = self.host.pane.as_ref().context("terminal pane missing")?;
        ensure!(
            ownership::process_evidence(pane.server.pid)? == pane.server
                && ownership::process_evidence(pane.pane.pid)? == pane.pane
                && supervisor::guardian_live(&self.host.guardian),
            "terminal process identity changed"
        );
        for process in [&pane.server, &pane.pane] {
            ensure!(
                ownership::pidfd_is_live(&ownership::verified_pidfd(process)?)?,
                "terminal process exited"
            );
        }
        Ok(())
    }
    fn apply(&self, state: &mut State, request: &Request, identity: (Uuid, Digest)) -> Result<()> {
        let (id, digest) = identity;
        let result = match &request.effect {
            Effect::Fence(fence) => self.apply_fence(state, fence, request.deadline_boot_ms)?,
            Effect::Input(input) => {
                return self.apply_input(
                    state,
                    InputDelivery {
                        command: input,
                        digest,
                        deadline: request.deadline_boot_ms,
                    },
                );
            }
            Effect::Resize(resize) => Self::refuse_resize(state, resize, id, &digest)?,
            _ => bail!("terminal operation unsupported"),
        };
        state.control.insert(
            id,
            Record {
                digest,
                receipt: result,
            },
        );
        state.save(self.dir)
    }
    fn apply_fence(&self, state: &mut State, fence: &Fence, deadline: u64) -> Result<Receipt> {
        ensure!(
            state.control.len() < MAX_CONTROL_RECORDS,
            "terminal control receipt capacity"
        );
        ensure!(
            state
                .fence
                .as_ref()
                .is_none_or(|old| fence.generation > old.generation),
            "stale terminal fence"
        );
        self.live_phase(deadline, false)?;
        if let Some(lease) = &fence.controller {
            ensure!(
                self.config.manifest.terminal().input_enabled
                    && wall_ms()? >= lease.issued_at_ms
                    && wall_ms()? < lease.expires_at_ms,
                "terminal controller expired or input disabled"
            );
        }
        state.fence = Some(fence.clone());
        state.controller_deadline_boot_ms = deadline;
        state.resize_sequence = 0;
        Ok(Receipt::Fence(FenceAck {
            terminal: fence.terminal.clone(),
            fence_id: fence.fence_id,
            generation: fence.generation,
        }))
    }

    fn refuse_resize(
        state: &mut State,
        resize: &aq_session_protocol::ResizeCommand,
        id: Uuid,
        digest: &Digest,
    ) -> Result<Receipt> {
        ensure!(
            state.control.len() < MAX_CONTROL_RECORDS,
            "terminal control receipt capacity"
        );
        controller(state, &resize.lease, wall_ms()?)?;
        ensure!(
            resize.sequence > state.resize_sequence
                && wall_ms()? >= resize.issued_at_ms
                && wall_ms()? < resize.deadline_ms,
            "terminal resize stale or expired"
        );
        // A controller owns input only. Acknowledge the refusal so the
        // relay can retire a queued request without mutating the pane.
        state.resize_sequence = resize.sequence;
        Ok(Receipt::Resize(ResizeReceipt {
            terminal: resize.lease.terminal.clone(),
            operation_id: id,
            command_digest: digest.clone(),
            sequence: resize.sequence,
            applied: false,
        }))
    }

    pub(crate) fn guarded_tmux(&self, deadline: u64, args: &[&str]) -> Result<Vec<u8>> {
        self.live_phase(deadline, false)?;
        let deadline = deadline.min(self.ceiling());
        let budget =
            Duration::from_millis(deadline.saturating_sub(ownership::raw_boot_ms()).min(250));
        let result = bounded_command_input(
            Command::new(&self.config.tmux)
                .args(["-N", "-S"])
                .arg(self.dir.join("tmux.sock"))
                .args(args),
            None,
            budget,
        )?;
        self.live_phase(deadline, false)?;
        Ok(result)
    }

    pub(crate) fn verify_output_claim(
        &self,
        binding: &TerminalBinding,
        claim: Uuid,
        gate: reader_lease::Gate<'_>,
    ) -> Result<()> {
        gate.guard.verify()?;
        binding.validate()?;
        ensure!(
            !claim.is_nil()
                && self.active(gate.pending_observation)
                && self.matches_binding(binding, gate.guard),
            "reader physical binding"
        );
        let recorded: TerminalBinding = private_json(&self.dir.join(BINDING), MAX_REQUEST)?;
        let marker: TerminalBinding =
            private_json(&self.dir.join("terminal-output.claim"), MAX_REQUEST)?;
        ensure!(
            recorded == *binding && marker == *binding,
            "reader immutable claim binding"
        );
        // Atomic encrypted state replacement gives a coherent version. The claim is
        // immutable; concurrent input receipts need no lock across this read.
        let state = load(self.dir, binding)?;
        ensure!(
            state.output_claim == Some(claim) && state.revision >= *gate.highwater,
            "reader claim or ledger highwater mismatch"
        );
        *gate.highwater = state.revision;
        gate.guard.verify()
    }
}
fn controller(state: &State, lease: &ControllerLease, now: u64) -> Result<()> {
    ensure!(
        state.fence.as_ref().and_then(|v| v.controller.as_ref()) == Some(lease)
            && now >= lease.issued_at_ms
            && now < lease.expires_at_ms
            && ownership::raw_boot_ms() < state.controller_deadline_boot_ms,
        "terminal controller stale or expired"
    );
    Ok(())
}
