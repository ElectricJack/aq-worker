//! Native synthetic host lifecycle history. Loading it never grants execution.
use super::{phase_view::PhaseScope, *};
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
    pub request: ClosureRequest,
    pub recorded_revision: u64,
    pub receipt: Option<ClosureReceipt>,
}
/// The closure envelope of the attempt's own terminal profile.
#[derive(Clone, Serialize, Deserialize)]
#[serde(tag = "profile", content = "envelope", rename_all = "snake_case")]
pub enum ClosureRequest {
    Frozen(aq_phase_terminal_protocol::ClosureRequest),
    Refresh(aq_phase_terminal_protocol::refresh_v1::ClosureRequest),
}
#[derive(Clone, Serialize, Deserialize)]
#[serde(tag = "profile", content = "envelope", rename_all = "snake_case")]
pub enum ClosureReceipt {
    Frozen(aq_phase_terminal_protocol::ClosureReceipt),
    Refresh(aq_phase_terminal_protocol::refresh_v1::ClosureReceipt),
}
impl ClosureRequest {
    pub fn original(&self) -> &aq_admission::granted_closure::Request {
        match self {
            Self::Frozen(q) => &q.original,
            Self::Refresh(q) => &q.original,
        }
    }
    pub fn validate(&self) -> Result<()> {
        match self {
            Self::Frozen(q) => q.validate()?,
            Self::Refresh(q) => q.validate()?,
        }
        Ok(())
    }
    pub fn accepts(&self, receipt: &ClosureReceipt) -> Result<()> {
        match (self, receipt) {
            (Self::Frozen(q), ClosureReceipt::Frozen(r)) => r.validate_for(q)?,
            (Self::Refresh(q), ClosureReceipt::Refresh(r)) => r.validate_for(q)?,
            _ => anyhow::bail!("closure receipt profile differs from its request"),
        }
        Ok(())
    }
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
    let Some(history) = &a.native_host else {
        return Ok(());
    };
    let intent = &history.intent;
    ensure!(
        ids.insert(intent.started_operation_id) && ids.insert(intent.stopped_operation_id),
        "native report operation ID reused"
    );
    let execution = a.phase_scope()?;
    intent.validate_scope(&a.reservation, &execution)?;
    intent.validate_bounds(&execution, revision)?;
    history.validate_host(execution.kernel_boot_id)?;
    history.validate_closure(revision)?;
    history.validate_baseline()?;
    history.validate_running(revision)?;
    history.validate_stopped(execution.kernel_boot_id, revision)
}
impl NativeLaunchIntent {
    fn validate_scope(&self, r: &Reservation, execution: &PhaseScope<'_>) -> Result<()> {
        self.binding.validate()?;
        self.terminal.validate()?;
        ensure!(
            self.execution_digest == execution.digest
                && self.binding == r.offer.offered.offer.binding
                && self.grant_id == execution.grant.grant_id
                && self.logical_guardian_id == execution.guardian_id
                && self.launch_id == execution.launch_id
                && self.reservation_id == r.reservation_id,
            "native host exact execution scope"
        );
        ensure!(
            same(&self.runtime, &r.runtime)?
                && self.terminal.session == self.binding.session
                && self.terminal.epoch == 1,
            "native host exact execution scope"
        );
        Ok(())
    }
    fn validate_bounds(&self, execution: &PhaseScope<'_>, revision: u64) -> Result<()> {
        ensure!(
            !self.started_operation_id.is_nil()
                && !self.stopped_operation_id.is_nil()
                && self.started_operation_id != self.stopped_operation_id
                && (execution.record_revision..revision).contains(&self.created_revision)
                && (execution.observed_before_write_ns..self.deadline_ns)
                    .contains(&self.created_ns)
                && self.deadline_ns <= execution.deadline_ns
                && (self.deadline_ns..=execution.hard_deadline_ns).contains(&self.hard_deadline_ns),
            "native host bounds/clock"
        );
        self.validate_geometry()?;
        for reference in [&self.executable, &self.helper, &self.tmux, &self.renderer] {
            reference.id.validate()?;
            reference.version.validate()?;
        }
        Ok(())
    }
    fn validate_geometry(&self) -> Result<()> {
        ensure!(
            self.root_inode > 0
                && self.pid_namespace.len() <= 128
                && self.pid_namespace.starts_with("pid:[")
                && self.pid_namespace.ends_with(']')
                && self.host_root.is_absolute()
                && self.host_root.as_os_str().len() <= 2048,
            "native host bounds/clock"
        );
        ensure!(
            (1..=240).contains(&self.columns)
                && (1..=120).contains(&self.rows)
                && u32::from(self.columns) * u32::from(self.rows) <= 12000,
            "native host bounds/clock"
        );
        Ok(())
    }
    fn contains_revision(&self, saved: u64, current: u64) -> bool {
        saved > self.created_revision && saved <= current
    }
    fn report_ids(&self) -> [Uuid; 4] {
        [
            self.grant_id,
            self.logical_guardian_id,
            self.launch_id,
            self.reservation_id,
        ]
    }
    fn permits(&self, observation: &HostObservation) -> bool {
        observation.permit_deadline_boot_ms
            <= self.deadline_ns.min(self.hard_deadline_ns) / 1_000_000
    }
}
impl NativeHostHistory {
    fn validate_host(&self, boot: Uuid) -> Result<()> {
        self.validate_configuration()?;
        if let Some(g) = &self.guardian {
            ensure!(
                self.host_intent.as_ref() == Some(&g.intent)
                    && g.process.pid > 0
                    && g.process.start_ticks > 0
                    && self.matches_guardian(g, boot)?,
                "physical guardian binding"
            );
        }
        ensure!(
            !self.start_intended || self.guardian.is_some(),
            "start before guardian"
        );
        Ok(())
    }
    fn validate_configuration(&self) -> Result<()> {
        let i = &self.intent;
        if let Some(host) = &self.host_intent {
            ensure!(
                host.launch_id == i.launch_id
                    && host.session == i.binding.session
                    && host.manifest_digest == i.binding.launch_manifest_digest,
                "host configuration binding"
            );
        }
        Ok(())
    }
    fn matches_guardian(&self, guardian: &GuardianIdentity, boot: Uuid) -> Result<bool> {
        Ok(!guardian.nonce.is_nil()
            && guardian.nonce != self.intent.logical_guardian_id
            && guardian.pid_namespace == self.intent.pid_namespace
            && Uuid::parse_str(&guardian.machine_boot_id)? == boot)
    }
    fn before_started(&self) -> bool {
        self.running.is_none()
            && self.started_receipt.is_none()
            && self.terminal.is_none()
            && self.refresh_terminal.is_none()
    }
    fn validate_closure(&self, revision: u64) -> Result<()> {
        let Some(c) = &self.closure else {
            return Ok(());
        };
        let i = &self.intent;
        let q = c.request.original();
        c.request.validate()?;
        ensure!(self.before_started(), "launch closure scope");
        ensure!(
            q.binding == i.binding
                && [q.grant_id, q.guardian_id, q.launch_id, q.reservation_id] == i.report_ids(),
            "launch closure scope"
        );
        ensure!(
            q.journal.revision == c.recorded_revision
                && i.contains_revision(c.recorded_revision, revision),
            "launch closure scope"
        );
        if let Some(receipt) = &c.receipt {
            c.request.accepts(receipt)?;
        }
        Ok(())
    }
    fn validate_baseline(&self) -> Result<()> {
        let Some(b) = &self.baseline else {
            return Ok(());
        };
        let i = &self.intent;
        ensure!(
            self.start_intended
                && self.guardian.as_ref() == Some(&b.guardian)
                && b.terminal == i.terminal
                && b.renderer == i.renderer
                && (b.columns, b.rows) == (i.columns, i.rows)
                && !b.baseline_id.is_nil(),
            "baseline release scope"
        );
        Ok(())
    }
    fn validate_running(&self, revision: u64) -> Result<()> {
        if let Some(v) = &self.running {
            v.request.validate()?;
            ensure!(
                self.baseline.is_some()
                    && self.guardian.as_ref() == Some(&v.observation.guardian)
                    && v.observation.phase == HostPhase::Running,
                "actual Running/Started scope"
            );
            ensure!(
                self.intent.contains_revision(v.recorded_revision, revision)
                    && v.recorded_revision
                        == v.request
                            .journal_revision
                            .checked_add(1)
                            .context("Started revision overflow")?,
                "actual Running/Started scope"
            );
            // A local non-interactive permit can be renewed only within the grant.
            ensure!(
                v.observation.permit_sequence == 1 && self.intent.permits(&v.observation),
                "actual Running/Started scope"
            );
            v.validate_request(&self.intent)?;
        }
        if let Some(receipt) = &self.started_receipt {
            receipt.validate_for(
                &self
                    .running
                    .as_ref()
                    .context("Started ACK without Running")?
                    .request,
            )?;
        }
        Ok(())
    }
    fn validate_stopped(&self, boot: Uuid, revision: u64) -> Result<()> {
        let Some(s) = &self.stopped else {
            return Ok(());
        };
        let o = &s.observation;
        let i = &self.intent;
        ensure!(
            o.phase == HostPhase::Stopped
                && o.kernel_wait_echild
                && self.matches_guardian(&o.guardian, boot)?,
            "actual stopped history"
        );
        ensure!(
            o.permit_sequence >= 1
                && i.permits(o)
                && self.host_intent.as_ref() == Some(&o.guardian.intent)
                && i.contains_revision(s.recorded_revision, revision)
                && s.proof_digest == canonical_digest(&serde_json::json!({"observation":o}))?,
            "actual stopped history"
        );
        self.validate_stop_links(o)
    }
    fn validate_stop_links(&self, o: &HostObservation) -> Result<()> {
        if let Some(g) = &self.guardian {
            ensure!(*g == o.guardian, "stopped foreign guardian");
        }
        if let Some(running) = &self.running {
            ensure!(
                o.pane == running.observation.pane && o.root_status.is_some() && o.reaped_count > 0,
                "stopped prior Running linkage"
            );
        }
        Ok(())
    }
}
impl NativeRunning {
    fn validate_request(&self, i: &NativeLaunchIntent) -> Result<()> {
        let q = &self.request;
        ensure!(
            q.binding == i.binding
                && [q.grant_id, q.guardian_id, q.launch_id, q.reservation_id] == i.report_ids(),
            "actual Running/Started scope"
        );
        ensure!(
            q.runtime == i.runtime.runtime
                && q.qualification == i.runtime.qualification
                && q.operation_id == i.started_operation_id
                && same(&q.observation, &self.observation)?,
            "actual Running/Started scope"
        );
        Ok(())
    }
}
impl State {
    fn native_host_mut(&mut self, id: Uuid) -> Option<&mut NativeHostHistory> {
        self.attempts.get_mut(&id)?.native_host.as_mut()
    }
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
            (intent.deadline_ns, intent.hard_deadline_ns),
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
        let h = next.native_host_mut(id).context("native launch absent")?;
        update(h, revision, &self.state)?;
        self.persist(next)
    }
    fn check_native_writer(&self) -> Result<()> {
        self.storage.check_owner()?;
        ensure!(!self.poisoned, "native writer poisoned");
        Ok(())
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
        self.check_native_writer()?;
        let mut next = self.next()?;
        let h = next
            .native_host_mut(id)
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
        self.check_native_writer()?;
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
            .native_host_mut(id)
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
        build: impl FnOnce(&Attempt, u64, &State) -> Result<ClosureRequest>,
    ) -> Result<ClosureRequest> {
        self.check_native_writer()?;
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
            h.before_started(),
            "a launch with Running history is never closed as unstarted"
        );
        let mut next = self.next()?;
        let revision = next.revision;
        let request = build(old, revision, &self.state)?;
        next.native_host_mut(id)
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
        receipt: ClosureReceipt,
    ) -> Result<()> {
        self.check_native_writer()?;
        let mut next = self.next()?;
        let c = next
            .native_host_mut(id)
            .and_then(|h| h.closure.as_mut())
            .context("native closure absent")?;
        c.request.accepts(&receipt)?;
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
