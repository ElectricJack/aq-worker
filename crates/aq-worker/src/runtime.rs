//! Deterministic worker lifecycle. Offers, authority, observations and final
//! disposition are separate journal facts; uncertainty always retains resources.
use crate::{
    clock::Clock,
    fixture::FixtureHandle,
    journal::{AttemptRecord, Journal},
};
use anyhow::{Context, Result, ensure};
use aq_worker_protocol::*;
use std::path::Path;
use uuid::Uuid;

pub struct Runtime<C: Clock> {
    pub journal: Journal,
    pub clock: C,
}
impl<C: Clock> Runtime<C> {
    pub fn new(journal: Journal, clock: C) -> Self {
        Self { journal, clock }
    }
    pub fn reserve(&mut self, offer: Offer) -> Result<AcceptRequest> {
        self.reserve_inner(offer, false, 0)
    }
    pub(crate) fn reserve_inner(
        &mut self,
        offer: Offer,
        source_checked: bool,
        source_pending_bytes: u64,
    ) -> Result<AcceptRequest> {
        ensure!(!self.journal.state().local_drain, "node drain is active");
        offer.validate()?;
        ensure!(
            source_checked
                || offer
                    .spec
                    .bindings
                    .as_ref()
                    .and_then(|b| b.source.as_ref())
                    .is_none(),
            "source binding requires configured aq-git admission"
        );
        offer.spec.verify_digest()?;
        ensure!(
            !self.journal.state().reconciliation_required,
            "startup reconciliation required"
        );
        ensure!(
            offer.identity == self.journal.state().identity,
            "stale node/boot"
        );
        ensure!(
            offer.spec.harness == FAKE_HARNESS_CAPABILITY
                && offer
                    .spec
                    .required_capabilities
                    .iter()
                    .all(|c| c == FAKE_HARNESS_CAPABILITY
                        || c == crate::guardian::PROFILE
                        || (source_checked
                            && (c == SOURCE_CAPABILITY || c == SOURCE_TRANSFER_CAPABILITY))),
            "unsupported harness/capability"
        );
        if let Some(record) = self.journal.state().attempts.get(&offer.attempt.attempt_id) {
            ensure!(
                record.offer == offer,
                "attempt ID reused with changed payload"
            );
            ensure!(
                self.clock.now_ms() < record.acceptance.reservation_deadline_ms
                    && record.phase == Phase::Reserved,
                "reservation terminal"
            );
            return Ok(record.acceptance.clone());
        }
        ensure!(
            self.journal.state().attempts.len() < 1024,
            "local history bound reached; maintenance required"
        );
        let pending: Vec<_> = self
            .journal
            .state()
            .attempts
            .values()
            .filter(|r| r.phase != Phase::Retired)
            .collect();
        ensure!(
            pending.len() < self.journal.limits().max_pending_results,
            "pending result count full"
        );
        let pending_bytes = pending
            .iter()
            .try_fold(0u64, |n, r| n.checked_add(r.pending_bytes))
            .context("pending byte accounting overflow")?;
        let projected_bytes = pending_bytes
            .checked_add(if source_checked {
                source_pending_bytes
            } else {
                offer.spec.fixture_output.len() as u64
            })
            .context("pending bytes overflow")?;
        ensure!(
            projected_bytes <= self.journal.limits().max_pending_bytes,
            "pending result bytes full"
        );
        ensure!(
            pending
                .iter()
                .filter(|r| matches!(
                    r.phase,
                    Phase::Reserved | Phase::Authorized | Phase::Preparing
                ))
                .count()
                < self.journal.limits().max_preparing,
            "preparation limit"
        );
        let available = self.journal.available()?;
        let need = &offer.spec.resources;
        ensure!(
            need.slots <= available.slots
                && need.cpu_millis <= available.cpu_millis
                && need.memory_bytes <= available.memory_bytes
                && need.disk_bytes <= available.disk_bytes,
            "local capacity unavailable"
        );
        let free_bytes = fs2::available_space(self.journal.root())?;
        ensure!(
            need.disk_bytes
                <= free_bytes
                    .saturating_sub(self.journal.limits().minimum_free_disk_bytes)
                    .saturating_sub(self.journal.protected_usage()?.disk_bytes),
            "physical disk headroom unavailable"
        );
        let now = self.clock.now_ms();
        let acceptance = AcceptRequest {
            identity: offer.identity.clone(),
            operation_id: Uuid::new_v4(),
            offer_id: offer.offer_id,
            attempt: offer.attempt.clone(),
            resolved_spec_digest: offer.spec.resolved_spec_digest.clone(),
            reservation_id: Uuid::new_v4(),
            reservation_deadline_ms: now
                .checked_add(offer.reservation_ttl_ms)
                .context("deadline overflow")?,
            reservation_remaining_ms: offer.reservation_ttl_ms,
        };
        let record = AttemptRecord {
            source: offer
                .spec
                .bindings
                .as_ref()
                .and_then(|b| b.source.clone())
                .map(crate::source::SourceState::new),
            pending_bytes: if source_checked {
                source_pending_bytes
            } else {
                offer.spec.fixture_output.len() as u64
            },
            stop_reason: None,
            ownership_profile: None,
            guardian: None,
            start_intent: false,
            stopped_proof: None,
            unstarted_decline: None,
            offer,
            acceptance: acceptance.clone(),
            acceptance_sent_at_ms: now,
            grant: None,
            grant_deadline_ms: None,
            phase: Phase::Reserved,
            launch_id: None,
            process: None,
            stop_certainty: StopCertainty::NotStarted,
            observation_sequence: 0,
            pending_observations: Default::default(),
            report: None,
            result: None,
        };
        self.journal.transaction(|s| {
            s.attempts.insert(record.offer.attempt.attempt_id, record);
            Ok(())
        })?;
        Ok(acceptance)
    }
    pub fn authorize(&mut self, grant: Grant, sent_at_ms: u64) -> Result<()> {
        grant.validate()?;
        let now = self.clock.now_ms();
        ensure!(sent_at_ms <= now, "grant send time in future");
        let deadline = sent_at_ms
            .checked_add(grant.valid_for_ms)
            .context("grant deadline overflow")?;
        self.journal.transaction(|s| {
            ensure!(
                !s.reconciliation_required && grant.identity == s.identity,
                "stale incarnation"
            );
            let record = s
                .attempts
                .get_mut(&grant.attempt.attempt_id)
                .context("unknown attempt")?;
            ensure!(
                record.phase == Phase::Reserved || record.phase == Phase::Authorized,
                "grant cannot revive terminal reservation"
            );
            ensure!(
                now < record.acceptance.reservation_deadline_ms && now < deadline,
                "expired local reservation/grant"
            );
            ensure!(
                grant.attempt == record.offer.attempt
                    && grant.offer_id == record.offer.offer_id
                    && grant.reservation_id == record.acceptance.reservation_id
                    && grant.reservation_deadline_ms == record.acceptance.reservation_deadline_ms
                    && grant.resolved_spec_digest == record.offer.spec.resolved_spec_digest,
                "grant binding mismatch"
            );
            ensure!(
                record
                    .grant
                    .as_ref()
                    .is_none_or(|g| grant.grant_sequence >= g.grant_sequence),
                "stale grant sequence"
            );
            record.grant = Some(grant);
            record.grant_deadline_ms = Some(deadline);
            record.acceptance_sent_at_ms = sent_at_ms;
            record.phase = Phase::Authorized;
            Ok(())
        })
    }
    pub fn renew(&mut self, grant: Grant, sent_at_ms: u64) -> Result<()> {
        grant.validate()?;
        let now = self.clock.now_ms();
        self.journal.transaction(|s| {
            ensure!(
                !s.reconciliation_required && grant.identity == s.identity,
                "stale incarnation"
            );
            let r = s
                .attempts
                .get_mut(&grant.attempt.attempt_id)
                .context("unknown attempt")?;
            ensure!(
                matches!(
                    r.phase,
                    Phase::Authorized | Phase::Preparing | Phase::Running
                ),
                "attempt cannot renew"
            );
            ensure!(
                now < r.grant_deadline_ms.context("missing authority")?,
                "expired authority cannot renew"
            );
            let previous = r.grant.as_ref().context("missing grant")?;
            ensure!(
                grant.attempt == previous.attempt
                    && grant.offer_id == previous.offer_id
                    && grant.reservation_id == previous.reservation_id
                    && grant.resolved_spec_digest == previous.resolved_spec_digest
                    && grant.reservation_deadline_ms == previous.reservation_deadline_ms
                    && grant.grant_sequence > previous.grant_sequence,
                "renewal binding/sequence mismatch"
            );
            let deadline = sent_at_ms
                .checked_add(grant.valid_for_ms)
                .context("grant deadline overflow")?;
            ensure!(sent_at_ms <= now && now < deadline, "late renewal");
            r.grant = Some(grant);
            r.grant_deadline_ms = Some(deadline);
            Ok(())
        })
    }
    /// Persist intent before spawn. A crash at/after this point never replays spawn.
    pub fn begin_launch(&mut self, attempt_id: Uuid) -> Result<Uuid> {
        let now = self.clock.now_ms();
        self.journal.transaction(|s| {
            ensure!(
                !s.reconciliation_required,
                "startup reconciliation required"
            );
            let r = s.attempts.get_mut(&attempt_id).context("unknown attempt")?;
            ensure!(
                r.phase == Phase::Authorized && r.launch_id.is_none(),
                "launch already attempted or unauthorized"
            );
            ensure!(
                r.offer.identity == s.identity
                    && now < r.grant_deadline_ms.context("missing authority")?
                    && now < r.acceptance.reservation_deadline_ms,
                "expired/stale launch authority"
            );
            ensure!(
                r.source.as_ref().is_none_or(|s| s.workspace.is_some()),
                "source workspace preparation required"
            );
            let launch_id = Uuid::new_v4();
            r.launch_id = Some(launch_id);
            r.ownership_profile = Some(crate::guardian::PROFILE.into());
            r.phase = Phase::LaunchUncertain;
            r.stop_certainty = StopCertainty::StopUncertain;
            Ok(launch_id)
        })
    }
    pub fn launch_fixture(&mut self, attempt_id: Uuid, executable: &Path) -> Result<FixtureHandle> {
        crate::guardian::probe_profile()?;
        let launch_id = self.begin_launch(attempt_id)?;
        crate::fault::checkpoint(
            self.journal.root(),
            "launch-intent-durable",
            Some(launch_id),
        )?;
        let r = &self.journal.state().attempts[&attempt_id];
        let system_origin_ms = crate::clock::raw_boot_ms();
        let logical_origin_ms = self.clock.now_ms();
        let binding = crate::guardian::LaunchBinding {
            launch_id,
            identity: r.offer.identity.clone(),
            attempt: r.offer.attempt.clone(),
            spec_digest: r.offer.spec.resolved_spec_digest.clone(),
            executable: std::fs::canonicalize(executable)?,
            workspace: r
                .source
                .as_ref()
                .and_then(|s| s.workspace.as_ref().map(|w| w.path.clone())),
            initial_grant_sequence: r.grant.as_ref().unwrap().grant_sequence,
            initial_deadline_ms: r.grant_deadline_ms.unwrap(),
            logical_origin_ms,
            system_origin_ms,
            clock_offset_file: std::env::var_os("AQ_GUARDIAN_CLOCK_OFFSET_FILE").map(Into::into),
            clock_offset_origin: match std::env::var_os("AQ_GUARDIAN_CLOCK_OFFSET_FILE") {
                Some(p) => std::fs::read_to_string(p)?.trim().parse()?,
                None => 0,
            },
        };
        let handle =
            FixtureHandle::spawn(self.journal.root(), binding, &r.offer.spec.fixture_output)?;
        let identity = handle.identity().clone();
        ensure!(
            identity.node_identity == r.offer.identity
                && identity.attempt == r.offer.attempt
                && identity.spec_digest == r.offer.spec.resolved_spec_digest,
            "guardian assignment binding mismatch"
        );
        self.journal.transaction(|s| {
            s.attempts.get_mut(&attempt_id).unwrap().guardian = Some(identity);
            Ok(())
        })?;
        crate::fault::checkpoint(
            self.journal.root(),
            "guardian-identity-durable",
            Some(launch_id),
        )?;
        self.journal.transaction(|s| {
            s.attempts.get_mut(&attempt_id).unwrap().start_intent = true;
            Ok(())
        })?;
        crate::fault::checkpoint(self.journal.root(), "start-intent-durable", Some(launch_id))?;
        let r = &self.journal.state().attempts[&attempt_id];
        let receipt = handle.start(
            self.journal.root(),
            r.grant.as_ref().unwrap().grant_sequence,
            r.grant_deadline_ms.unwrap(),
        )?;
        self.journal.transaction(|s| {
            let r = s.attempts.get_mut(&attempt_id).unwrap();
            r.process = receipt.root_process;
            r.phase = Phase::Running;
            r.stop_certainty = StopCertainty::Running;
            Ok(())
        })?;
        crate::fault::checkpoint(
            self.journal.root(),
            "start-receipt-durable",
            Some(launch_id),
        )?;
        Ok(handle)
    }
    /// Only the owned Child handle can prove this fixture's writers stopped.
    pub fn finish_fixture(
        &mut self,
        attempt_id: Uuid,
        handle: FixtureHandle,
    ) -> Result<ResultReport> {
        let r = self
            .journal
            .state()
            .attempts
            .get(&attempt_id)
            .context("unknown attempt")?;
        ensure!(
            r.launch_id == Some(handle.launch_id()),
            "child ownership mismatch"
        );
        let handle_launch_id = handle.launch_id();
        let (status, output, proof) = handle.wait()?;
        let outcome = if let Some(reason) = proof.stop_reason.or(r.stop_reason) {
            reason
        } else if self.clock.now_ms() >= r.grant_deadline_ms.unwrap_or(0) {
            ProcessOutcome::Expired
        } else if status.success() {
            ProcessOutcome::Success
        } else {
            ProcessOutcome::Failed
        };
        self.journal.transaction(|s| {
            s.attempts.get_mut(&attempt_id).unwrap().stopped_proof = Some(proof);
            Ok(())
        })?;
        let report = self.record_stopped(attempt_id, outcome, output)?;
        crate::fault::checkpoint(
            self.journal.root(),
            "report-durable",
            Some(handle_launch_id),
        )?;
        Ok(report)
    }
    fn record_stopped(
        &mut self,
        attempt_id: Uuid,
        outcome: ProcessOutcome,
        output: String,
    ) -> Result<ResultReport> {
        self.record_terminal(attempt_id, outcome, output, StopCertainty::ConfirmedStopped)
    }
    fn record_terminal(
        &mut self,
        attempt_id: Uuid,
        outcome: ProcessOutcome,
        output: String,
        certainty: StopCertainty,
    ) -> Result<ResultReport> {
        ensure!(
            output.len() <= MAX_TEXT,
            "fixture output exceeds wire bound"
        );
        self.journal.transaction(|s| {
            let r = s.attempts.get_mut(&attempt_id).context("unknown attempt")?;
            r.phase = Phase::Collecting;
            r.stop_certainty = certainty;
            let report = ResultReport {
                identity: r.offer.identity.clone(),
                report_id: Uuid::new_v4(),
                supersedes_report_id: None,
                attempt: r.offer.attempt.clone(),
                resolved_spec_digest: r.offer.spec.resolved_spec_digest.clone(),
                outcome,
                stop_certainty: r.stop_certainty,
                claimed_success: outcome == ProcessOutcome::Success && r.source.is_none(),
                output,
                artifacts: vec![],
                pending_operation_ids: vec![],
            };
            r.report = Some(report.clone());
            r.phase = Phase::Reporting;
            Ok(report)
        })
    }
    pub fn recover_owned_launches(&mut self) -> Result<Vec<Uuid>> {
        let records: Vec<_> = self
            .journal
            .state()
            .attempts
            .iter()
            .filter(|(_, r)| r.phase != Phase::Retired && r.report.is_none())
            .map(|(id, r)| (*id, r.clone()))
            .collect();
        let mut uncertain = vec![];
        for (id, record) in records {
            if record.launch_id.is_none() && record.stop_certainty == StopCertainty::NotStarted {
                self.report_not_started(id, ProcessOutcome::Expired)?;
                continue;
            }
            if record.ownership_profile.as_deref() != Some(crate::guardian::PROFILE) {
                uncertain.push(id);
                continue;
            }
            let launch_id = record
                .launch_id
                .context("guardian attempt missing launch identity")?;
            let directory = self
                .journal
                .root()
                .join("launches")
                .join(launch_id.to_string());
            if !record.start_intent && !directory.join("guardian-identity.json").exists() {
                // This profile cannot send Start before a durable start intent.
                // Missing PID/receipt is not the proof; ordered journal protocol is.
                self.report_not_started(id, ProcessOutcome::Cancelled)?;
                continue;
            }
            let recovered = (|| -> Result<()> {
                let handle = FixtureHandle::recover(
                    self.journal.root(),
                    launch_id,
                    record.guardian.as_ref(),
                )?;
                let identity = handle.identity();
                ensure!(
                    identity.node_identity == record.offer.identity
                        && identity.attempt == record.offer.attempt
                        && identity.spec_digest == record.offer.spec.resolved_spec_digest,
                    "recovery assignment binding mismatch"
                );
                handle.stop(ProcessOutcome::Cancelled)?;
                self.finish_fixture(id, handle)?;
                Ok(())
            })();
            if let Err(error) = recovered {
                eprintln!("launch {launch_id} remains stop_uncertain: {error:#}");
                self.journal.transaction(|s| {
                    let r = s.attempts.get_mut(&id).unwrap();
                    r.phase = Phase::LaunchUncertain;
                    r.stop_certainty = StopCertainty::StopUncertain;
                    Ok(())
                })?;
                uncertain.push(id);
            }
        }
        Ok(uncertain)
    }
    pub fn report_not_started(
        &mut self,
        attempt_id: Uuid,
        outcome: ProcessOutcome,
    ) -> Result<ResultReport> {
        let r = self
            .journal
            .state()
            .attempts
            .get(&attempt_id)
            .context("unknown attempt")?;
        ensure!(
            r.launch_id.is_none()
                || (r.ownership_profile.as_deref() == Some(crate::guardian::PROFILE)
                    && !r.start_intent),
            "absence of writer Start not proven"
        );
        if let Some(report) = &r.report {
            return Ok(report.clone());
        }
        self.record_terminal(
            attempt_id,
            outcome,
            "not started: durable writer Start boundary was never crossed".into(),
            StopCertainty::NotStarted,
        )
    }
    pub fn unstarted_decline(&mut self, attempt_id: Uuid) -> Result<DeclineRequest> {
        self.journal.transaction(|s| {
            let r = s.attempts.get_mut(&attempt_id).context("unknown attempt")?;
            ensure!(
                r.stop_certainty == StopCertainty::NotStarted && !r.start_intent,
                "nonstart not proven"
            );
            if let Some(request) = &r.unstarted_decline {
                return Ok(request.clone());
            }
            let request = DeclineRequest {
                identity: r.offer.identity.clone(),
                operation_id: Uuid::new_v4(),
                offer_id: r.offer.offer_id,
                reason: "proven nonstart after reservation/restart recovery".into(),
            };
            r.unstarted_decline = Some(request.clone());
            Ok(request)
        })
    }
    pub fn acknowledge_unstarted_decline(&mut self, attempt_id: Uuid, ack: Ack) -> Result<()> {
        self.journal.transaction(|s| {
            let r = s.attempts.get_mut(&attempt_id).context("unknown attempt")?;
            ensure!(
                r.stop_certainty == StopCertainty::NotStarted
                    && !r.start_intent
                    && r.grant.is_none()
                    && r.source.as_ref().is_none_or(
                        |source| source.operations.is_empty() && source.workspace.is_none()
                    )
                    && r.unstarted_decline
                        .as_ref()
                        .is_some_and(|d| d.operation_id == ack.operation_id),
                "decline acknowledgement mismatch"
            );
            if let Some(source) = r.source.as_mut() {
                source.retired = true;
            }
            r.phase = Phase::Retired;
            Ok(())
        })
    }
    pub fn acknowledge_result(&mut self, response: ResultResponse) -> Result<()> {
        response.validate()?;
        self.journal.transaction(|s| {
            let record = s
                .attempts
                .values_mut()
                .find(|r| {
                    r.report
                        .as_ref()
                        .is_some_and(|x| x.report_id == response.report_id)
                })
                .context("unknown report acknowledgement")?;
            if let Some(old) = &record.result {
                ensure!(*old == response, "changed result disposition");
                return Ok(());
            }
            ensure!(
                !response.retention_release
                    || record.source.as_ref().is_none_or(|s| s.receipt.is_some()
                        || (!s.operations.contains_key("create")
                            && s.workspace.is_none()
                            && record.launch_id.is_none())),
                "source central retention required for release"
            );
            ensure!(
                matches!(
                    record.stop_certainty,
                    StopCertainty::ConfirmedStopped | StopCertainty::NotStarted
                ),
                "writers not proven stopped"
            );
            record.phase = Phase::Acknowledged;
            record.result = Some(response);
            Ok(())
        })
    }
    pub fn retire(&mut self, attempt_id: Uuid) -> Result<()> {
        let record = self
            .journal
            .state()
            .attempts
            .get(&attempt_id)
            .context("unknown attempt")?;
        ensure!(
            record.phase == Phase::Acknowledged
                && matches!(
                    record.stop_certainty,
                    StopCertainty::ConfirmedStopped | StopCertainty::NotStarted
                )
                && record.result.as_ref().is_some_and(|x| x.retention_release),
            "retention release required"
        );
        ensure!(
            record.source.as_ref().is_none_or(|s| s.retired),
            "aq-git workspace must be retired first"
        );
        // Deletion follows the durable central release and stopped-writer proof.
        // A crash before the retired transition safely repeats this cleanup.
        if let Some(launch_id) = record.launch_id {
            let directory = self
                .journal
                .root()
                .join("launches")
                .join(launch_id.to_string());
            if directory.exists() {
                std::fs::remove_dir_all(&directory)?;
                std::fs::File::open(directory.parent().unwrap())?.sync_all()?;
            }
        }
        self.journal.transaction(|s| {
            let r = s.attempts.get_mut(&attempt_id).context("unknown attempt")?;
            ensure!(
                r.phase == Phase::Acknowledged
                    && matches!(
                        r.stop_certainty,
                        StopCertainty::ConfirmedStopped | StopCertainty::NotStarted
                    )
                    && r.result.as_ref().is_some_and(|x| x.retention_release),
                "retention release required"
            );
            r.phase = Phase::Retired;
            Ok(())
        })
    }
    pub fn receive_directive(&mut self, directive: &Directive) -> Result<DirectiveAck> {
        directive.validate()?;
        self.journal.transaction(|s| {
            if let Some(existing) = s.directive_receipts.get(&directive.directive_id) {
                ensure!(
                    s.directive_contents.get(&directive.directive_id) == Some(directive),
                    "directive ID conflict"
                );
                return Ok(existing.clone());
            }
            ensure!(
                s.directive_contents
                    .values()
                    .filter(|d| d.attempt.attempt_id == directive.attempt.attempt_id)
                    .count()
                    < crate::journal::MAX_DIRECTIVES_PER_ATTEMPT,
                "directive receipt bound reached"
            );
            if directive.kind == DirectiveKind::Drain {
                s.local_drain = true;
            }
            let r = s
                .attempts
                .get_mut(&directive.attempt.attempt_id)
                .context("unknown directive attempt")?;
            ensure!(
                r.offer.attempt == directive.attempt,
                "stale directive generation/session"
            );
            if matches!(
                r.phase,
                Phase::Running
                    | Phase::Preparing
                    | Phase::Authorized
                    | Phase::Reserved
                    | Phase::LaunchUncertain
            ) {
                r.phase = Phase::Stopping;
                r.stop_reason = Some(ProcessOutcome::Cancelled);
            }
            let ack = DirectiveAck {
                identity: s.identity.clone(),
                operation_id: Uuid::new_v4(),
                directive_id: directive.directive_id,
                attempt: directive.attempt.clone(),
                disposition: DirectiveDisposition::Received,
            };
            s.directive_contents
                .insert(directive.directive_id, directive.clone());
            s.directive_receipts
                .insert(directive.directive_id, ack.clone());
            Ok(ack)
        })
    }
    pub fn request_stop(&mut self, attempt_id: Uuid, reason: ProcessOutcome) -> Result<()> {
        ensure!(
            matches!(
                reason,
                ProcessOutcome::Cancelled | ProcessOutcome::Expired | ProcessOutcome::Failed
            ),
            "invalid stop reason"
        );
        self.journal.transaction(|s| {
            let r = s.attempts.get_mut(&attempt_id).context("unknown attempt")?;
            r.phase = Phase::Stopping;
            r.stop_reason = Some(reason);
            if r.launch_id.is_some() {
                r.stop_certainty = StopCertainty::StopUncertain;
            }
            Ok(())
        })
    }
    pub fn expire(&mut self) -> Result<Vec<Uuid>> {
        let now = self.clock.now_ms();
        if !self.journal.state().attempts.values().any(|r| {
            matches!(
                r.phase,
                Phase::Reserved
                    | Phase::Authorized
                    | Phase::Preparing
                    | Phase::Running
                    | Phase::LaunchUncertain
            ) && now
                >= r.grant_deadline_ms
                    .unwrap_or(r.acceptance.reservation_deadline_ms)
        }) {
            return Ok(vec![]);
        }
        self.journal.transaction(|s| {
            let mut stop = vec![];
            for (id, r) in &mut s.attempts {
                if matches!(
                    r.phase,
                    Phase::Reserved
                        | Phase::Authorized
                        | Phase::Preparing
                        | Phase::Running
                        | Phase::LaunchUncertain
                ) && (now
                    >= r.grant_deadline_ms
                        .unwrap_or(r.acceptance.reservation_deadline_ms))
                {
                    r.phase = Phase::Stopping;
                    r.stop_reason = Some(ProcessOutcome::Expired);
                    stop.push(*id);
                    if r.launch_id.is_some() {
                        r.stop_certainty = StopCertainty::StopUncertain;
                    }
                }
            }
            Ok(stop)
        })
    }
    pub fn observation(&mut self, attempt_id: Uuid, detail: String) -> Result<Observation> {
        ensure!(detail.len() <= MAX_TEXT, "observation too large");
        self.journal.transaction(|s| {
            let r = s.attempts.get_mut(&attempt_id).context("unknown attempt")?;
            r.observation_sequence = r
                .observation_sequence
                .checked_add(1)
                .context("observation sequence overflow")?;
            let observation = Observation {
                identity: s.identity.clone(),
                operation_id: Uuid::new_v4(),
                attempt: r.offer.attempt.clone(),
                sequence: r.observation_sequence,
                phase: r.phase,
                stop_certainty: r.stop_certainty,
                detail,
            };
            ensure!(
                r.pending_observations.len() < MAX_BATCH,
                "observation outbound queue full"
            );
            r.pending_observations
                .insert(observation.sequence, observation.clone());
            Ok(observation)
        })
    }
    pub fn acknowledge_observation(&mut self, attempt_id: Uuid, ack: &Ack) -> Result<()> {
        self.journal.transaction(|s| {
            let r = s.attempts.get_mut(&attempt_id).context("unknown attempt")?;
            let sequence = r
                .pending_observations
                .iter()
                .find(|(_, o)| o.operation_id == ack.operation_id)
                .map(|(n, _)| *n)
                .context("unknown observation acknowledgement")?;
            r.pending_observations.remove(&sequence);
            Ok(())
        })
    }
    pub fn reconcile_request(&self) -> ReconcileRequest {
        ReconcileRequest {
            identity: self.journal.state().identity.clone(),
            operation_id: Uuid::new_v4(),
            attempts: self
                .journal
                .state()
                .attempts
                .values()
                .filter(|r| r.phase != Phase::Retired)
                .take(MAX_BATCH)
                .map(|r| ReconcileAttempt {
                    attempt: r.offer.attempt.clone(),
                    previous_boot_id: r.offer.identity.boot_id,
                    phase: r.phase,
                    launch_id: r.launch_id,
                    stop_certainty: r.stop_certainty,
                    report_id: r.report.as_ref().map(|x| x.report_id),
                })
                .collect(),
            cursor: None,
            max_items: MAX_BATCH as u32,
        }
    }
}
