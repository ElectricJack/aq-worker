//! Native synthetic host lifecycle history. Loading it never grants execution.
use super::*;
use aq_session_host::{GuardianIdentity, HostObservation, HostPhase, LaunchIntent as HostIntent};
use aq_session_protocol::TerminalIdentity;
#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NativeLaunchIntent {
    pub execution_digest: Digest,
    pub binding: aq_session_protocol::LaunchBinding,
    pub grant_id: Uuid,
    pub logical_guardian_id: Uuid,
    pub launch_id: Uuid,
    pub reservation_id: Uuid,
    pub runtime: RuntimeIdentity,
    pub terminal: TerminalIdentity,
    pub host_root: std::path::PathBuf,
    pub root_device: u64,
    pub root_inode: u64,
    pub pid_namespace: String,
    pub profile_digest: Digest,
    pub executable: ArtifactRef,
    pub helper: ArtifactRef,
    pub tmux: ArtifactRef,
    pub renderer: ArtifactRef,
    pub columns: u16,
    pub rows: u16,
    pub deadline_ns: u64,
    pub hard_deadline_ns: u64,
    pub created_ns: u64,
    pub created_revision: u64,
    pub started_operation_id: Uuid,
    pub stopped_operation_id: Uuid,
}
#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NativeBaseline {
    pub baseline_id: Uuid,
    pub terminal: TerminalIdentity,
    pub renderer: ArtifactRef,
    pub guardian: GuardianIdentity,
    pub columns: u16,
    pub rows: u16,
    pub digest: Digest,
}
#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NativeRunning {
    pub observation: HostObservation,
    pub request: aq_admission::started::Request,
    pub recorded_revision: u64,
}
#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NativeStopped {
    pub observation: HostObservation,
    pub proof_digest: Digest,
    pub recorded_revision: u64,
}
#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NativeHostHistory {
    pub intent: NativeLaunchIntent,
    pub host_intent: Option<HostIntent>,
    pub guardian: Option<GuardianIdentity>,
    pub start_intended: bool,
    pub baseline: Option<NativeBaseline>,
    pub running: Option<NativeRunning>,
    pub started_receipt: Option<aq_admission::started::Receipt>,
    pub stopped: Option<NativeStopped>,
    pub uncertain: Option<LaunchFailure>,
    pub terminal: Option<super::phase_terminal::PhaseTerminalHistory>,
    pub refresh_terminal: Option<super::refresh_terminal::PhaseTerminalHistory>,
    /// Durable no-Started launch closure (B5b/AQ-08). Once recorded the launch
    /// can never start; only the exact request is replayed to the server.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub closure: Option<NativeClosure>,
}
#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NativeClosure {
    pub request: aq_phase_terminal_protocol::ClosureRequest,
    pub recorded_revision: u64,
    pub receipt: Option<aq_phase_terminal_protocol::ClosureReceipt>,
}
#[derive(Clone, Copy, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LaunchFailure {
    IntentPersistence,
    HostPreparation,
    Baseline,
    ExecUnobserved,
    StartedPersistence,
    StopUncertain,
}
impl NativeLaunchIntent {
    pub fn digest(&self) -> Result<Digest> {
        canonical_digest(self)
    }
}
pub(super) fn validate(a: &Attempt, revision: u64, ids: &mut BTreeSet<Uuid>) -> Result<()> {
    let Some(h) = &a.native_host else {
        return Ok(());
    };
    let i = &h.intent;
    ensure!(
        ids.insert(i.started_operation_id) && ids.insert(i.stopped_operation_id),
        "native report operation ID reused"
    );
    let execution = a.phase_scope()?;
    let r = &a.reservation;
    i.binding.validate()?;
    i.terminal.validate()?;
    ensure!(
        i.execution_digest == execution.digest
            && i.binding == r.offer.offered.offer.binding
            && i.grant_id == execution.grant.grant_id
            && i.logical_guardian_id == execution.guardian_id
            && i.launch_id == execution.launch_id
            && i.reservation_id == r.reservation_id
            && same(&i.runtime, &r.runtime)?
            && i.terminal.session == i.binding.session
            && i.terminal.epoch == 1,
        "native host exact execution scope"
    );
    ensure!(
        !i.started_operation_id.is_nil()
            && !i.stopped_operation_id.is_nil()
            && i.started_operation_id != i.stopped_operation_id
            && i.created_revision >= execution.record_revision
            && i.created_revision < revision
            && i.created_ns >= execution.observed_before_write_ns
            && i.created_ns < i.deadline_ns
            && i.deadline_ns <= execution.deadline_ns
            && i.deadline_ns <= i.hard_deadline_ns
            && i.hard_deadline_ns <= execution.hard_deadline_ns
            && i.root_inode > 0
            && i.pid_namespace.len() <= 128
            && i.pid_namespace.starts_with("pid:[")
            && i.pid_namespace.ends_with(']')
            && i.host_root.is_absolute()
            && i.host_root.as_os_str().len() <= 2048
            && i.columns > 0
            && i.columns <= 240
            && i.rows > 0
            && i.rows <= 120
            && u32::from(i.columns) * u32::from(i.rows) <= 12000,
        "native host bounds/clock"
    );
    for reference in [&i.executable, &i.helper, &i.tmux, &i.renderer] {
        reference.id.validate()?;
        reference.version.validate()?;
    }
    if let Some(host) = &h.host_intent {
        ensure!(
            host.launch_id == i.launch_id
                && host.session == i.binding.session
                && host.manifest_digest == i.binding.launch_manifest_digest,
            "host configuration binding"
        );
    }
    if let Some(g) = &h.guardian {
        ensure!(
            h.host_intent.as_ref() == Some(&g.intent)
                && g.process.pid > 0
                && g.process.start_ticks > 0
                && !g.nonce.is_nil()
                && g.nonce != i.logical_guardian_id
                && g.pid_namespace == i.pid_namespace
                && Uuid::parse_str(&g.machine_boot_id)? == execution.kernel_boot_id,
            "physical guardian binding"
        );
    }
    ensure!(
        !h.start_intended || h.guardian.is_some(),
        "start before guardian"
    );
    if let Some(c) = &h.closure {
        let q = &c.request.original;
        c.request.validate()?;
        ensure!(
            h.running.is_none()
                && h.started_receipt.is_none()
                && h.terminal.is_none()
                && h.refresh_terminal.is_none()
                && q.binding == i.binding
                && q.grant_id == i.grant_id
                && q.guardian_id == i.logical_guardian_id
                && q.launch_id == i.launch_id
                && q.reservation_id == i.reservation_id
                && q.journal.revision == c.recorded_revision
                && c.recorded_revision > i.created_revision
                && c.recorded_revision <= revision,
            "launch closure scope"
        );
        if let Some(receipt) = &c.receipt {
            receipt.validate_for(&c.request)?;
        }
    }
    if let Some(b) = &h.baseline {
        ensure!(
            h.start_intended
                && h.guardian.as_ref() == Some(&b.guardian)
                && b.terminal == i.terminal
                && b.renderer == i.renderer
                && b.columns == i.columns
                && b.rows == i.rows
                && !b.baseline_id.is_nil(),
            "baseline release scope"
        );
    }
    if let Some(v) = &h.running {
        v.request.validate()?;
        ensure!(
            h.baseline.is_some()
                && h.guardian.as_ref() == Some(&v.observation.guardian)
                && v.observation.phase == HostPhase::Running
                && v.recorded_revision <= revision
                && v.recorded_revision > i.created_revision
                && v.recorded_revision
                    == v.request
                        .journal_revision
                        .checked_add(1)
                        .context("Started revision overflow")?
                && v.observation.permit_sequence == 1
                // A non-interactive host's local permit is capped below the
                // grant (MAX_PERMIT_MS) and renewed locally; never beyond it.
                && v.observation.permit_deadline_boot_ms
                    <= i.deadline_ns.min(i.hard_deadline_ns) / 1_000_000
                && v.request.binding == i.binding
                && v.request.grant_id == i.grant_id
                && v.request.guardian_id == i.logical_guardian_id
                && v.request.launch_id == i.launch_id
                && v.request.reservation_id == i.reservation_id
                && v.request.runtime == i.runtime.runtime
                && v.request.qualification == i.runtime.qualification
                && v.request.operation_id == i.started_operation_id
                && same(&v.request.observation, &v.observation)?,
            "actual Running/Started scope"
        );
    }
    if let Some(receipt) = &h.started_receipt {
        receipt.validate_for(
            &h.running
                .as_ref()
                .context("Started ACK without Running")?
                .request,
        )?;
    }
    if let Some(s) = &h.stopped {
        ensure!(
            s.observation.phase == HostPhase::Stopped
                && s.observation.kernel_wait_echild
                && s.observation.guardian.pid_namespace == i.pid_namespace
                && Uuid::parse_str(&s.observation.guardian.machine_boot_id)?
                    == execution.kernel_boot_id
                && !s.observation.guardian.nonce.is_nil()
                && s.observation.guardian.nonce != i.logical_guardian_id
                && s.observation.permit_sequence >= 1
                && s.observation.permit_deadline_boot_ms
                    <= i.deadline_ns.min(i.hard_deadline_ns) / 1_000_000
                && h.host_intent.as_ref() == Some(&s.observation.guardian.intent)
                && s.recorded_revision <= revision
                && s.recorded_revision > i.created_revision
                && s.proof_digest
                    == canonical_digest(&serde_json::json!({"observation":s.observation}))?,
            "actual stopped history"
        );
        if let Some(g) = &h.guardian {
            ensure!(*g == s.observation.guardian, "stopped foreign guardian");
        }
        if let Some(running) = &h.running {
            ensure!(
                s.observation.pane == running.observation.pane
                    && s.observation.root_status.is_some()
                    && s.observation.reaped_count > 0,
                "stopped prior Running linkage"
            );
        }
    }
    Ok(())
}
impl Journal {
    pub(crate) fn begin_native_launch(
        &mut self,
        id: Uuid,
        intent: NativeLaunchIntent,
    ) -> Result<()> {
        self.check_native_execution(
            id,
            &intent.execution_digest,
            intent.deadline_ns,
            intent.hard_deadline_ns,
        )?;
        ensure!(
            self.state
                .attempts
                .get(&id)
                .is_some_and(|a| a.native_host.is_none()),
            "native host intent already exists"
        );
        let mut next = self.next()?;
        next.attempts.get_mut(&id).unwrap().native_host = Some(NativeHostHistory {
            intent,
            host_intent: None,
            guardian: None,
            start_intended: false,
            baseline: None,
            running: None,
            started_receipt: None,
            stopped: None,
            uncertain: None,
            terminal: None,
            refresh_terminal: None,
            closure: None,
        });
        self.persist(next)
    }
    pub(crate) fn native_update(
        &mut self,
        id: Uuid,
        update: impl FnOnce(&mut NativeHostHistory, u64, &State) -> Result<()>,
    ) -> Result<()> {
        self.live()?;
        let mut next = self.next()?;
        let revision = next.revision;
        let h = next
            .attempts
            .get_mut(&id)
            .and_then(|a| a.native_host.as_mut())
            .context("native launch absent")?;
        update(h, revision, &self.state)?;
        self.persist(next)
    }
    pub(crate) fn native_started_receipt(
        &mut self,
        id: Uuid,
        receipt: aq_admission::started::Receipt,
    ) -> Result<()> {
        self.live()?;
        let h = self
            .state
            .attempts
            .get(&id)
            .and_then(|a| a.native_host.as_ref())
            .context("native Started history absent")?;
        receipt.validate_for(&h.running.as_ref().context("actual Running absent")?.request)?;
        if let Some(old) = &h.started_receipt {
            ensure!(
                canonical_digest(old)? == canonical_digest(&receipt)?,
                "native Started receipt changed"
            );
            return Ok(());
        }
        self.native_update(id, move |h, _, _| {
            h.started_receipt = Some(receipt);
            Ok(())
        })
    }
    pub(crate) fn native_uncertain(&mut self, id: Uuid, reason: LaunchFailure) -> Result<()> {
        self.storage.check_owner()?;
        ensure!(!self.poisoned, "native writer poisoned");
        let mut next = self.next()?;
        let h = next
            .attempts
            .get_mut(&id)
            .and_then(|a| a.native_host.as_mut())
            .context("native unknown launch absent")?;
        if h.uncertain.is_some() {
            return Ok(());
        }
        h.uncertain = Some(reason);
        self.persist(next)
    }
    pub(crate) fn native_stopped(
        &mut self,
        id: Uuid,
        proof: &aq_session_host::StoppedProof,
    ) -> Result<()> {
        // Safety-only recovery writes never change invocation, grant caps or capacity.
        // An actual opaque host proof is required; serialized history cannot enter.
        self.storage.check_owner()?;
        ensure!(!self.poisoned, "native writer poisoned");
        let mut next = self.next()?;
        let revision = next.revision;
        let old = self
            .state
            .attempts
            .get(&id)
            .context("native stopped attempt absent")?;
        if let Some(stopped) = old.native_host.as_ref().and_then(|h| h.stopped.as_ref()) {
            ensure!(
                stopped.proof_digest == canonical_digest(proof)?,
                "changed stopped proof"
            );
            return Ok(());
        }
        let publication = if old.phase_acceptance.is_some() {
            super::phase_terminal::stopped(old, proof, revision, &self.state)?
        } else {
            None
        };
        let refresh_publication = if old.refresh_acceptance.is_some() {
            super::refresh_terminal::stopped(old, proof, revision, &self.state)?
        } else {
            None
        };
        let h = next
            .attempts
            .get_mut(&id)
            .and_then(|a| a.native_host.as_mut())
            .context("native stopped launch absent")?;
        let stopped = NativeStopped {
            observation: proof.observation().clone(),
            proof_digest: canonical_digest(proof)?,
            recorded_revision: revision,
        };
        if let Some(old) = &h.stopped {
            ensure!(
                old.proof_digest == stopped.proof_digest,
                "changed stopped proof"
            );
            return Ok(());
        }
        h.stopped = Some(stopped);
        if let Some(t) = h.terminal.as_mut() {
            t.stopped = publication;
        }
        if let Some(t) = h.refresh_terminal.as_mut() {
            t.stopped = refresh_publication;
        }
        self.persist(next)
    }
    /// Records the durable no-Started closure once, built from the history at
    /// the next revision. A recorded closure is returned unchanged on replay.
    pub(crate) fn native_closure(
        &mut self,
        id: Uuid,
        build: impl FnOnce(&Attempt, u64, &State) -> Result<aq_phase_terminal_protocol::ClosureRequest>,
    ) -> Result<aq_phase_terminal_protocol::ClosureRequest> {
        self.storage.check_owner()?;
        ensure!(!self.poisoned, "native writer poisoned");
        let old = self
            .state
            .attempts
            .get(&id)
            .context("native closure attempt absent")?;
        let h = old
            .native_host
            .as_ref()
            .context("native closure launch absent")?;
        if let Some(c) = &h.closure {
            return Ok(c.request.clone());
        }
        ensure!(
            h.running.is_none()
                && h.started_receipt.is_none()
                && h.terminal.is_none()
                && h.refresh_terminal.is_none(),
            "a launch with Running history is never closed as unstarted"
        );
        let mut next = self.next()?;
        let revision = next.revision;
        let request = build(old, revision, &self.state)?;
        next.attempts
            .get_mut(&id)
            .and_then(|a| a.native_host.as_mut())
            .context("native closure launch absent")?
            .closure = Some(NativeClosure {
            request: request.clone(),
            recorded_revision: revision,
            receipt: None,
        });
        self.persist(next)?;
        Ok(request)
    }
    pub(crate) fn native_closure_receipt(
        &mut self,
        id: Uuid,
        receipt: aq_phase_terminal_protocol::ClosureReceipt,
    ) -> Result<()> {
        self.storage.check_owner()?;
        ensure!(!self.poisoned, "native writer poisoned");
        let mut next = self.next()?;
        let c = next
            .attempts
            .get_mut(&id)
            .and_then(|a| a.native_host.as_mut())
            .and_then(|h| h.closure.as_mut())
            .context("native closure absent")?;
        receipt.validate_for(&c.request)?;
        if let Some(old) = &c.receipt {
            ensure!(
                canonical_digest(old)? == canonical_digest(&receipt)?,
                "native closure receipt changed"
            );
            return Ok(());
        }
        c.receipt = Some(receipt);
        self.persist(next)
    }
}
