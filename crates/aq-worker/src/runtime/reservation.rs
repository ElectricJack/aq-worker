use super::*;
impl<C: Clock> Runtime<C> {
    pub(crate) fn reserve_inner(
        &mut self,
        offer: Offer,
        source_checked: bool,
        source_pending_bytes: u64,
    ) -> Result<AcceptRequest> {
        self.validate_offer(&offer, source_checked)?;
        if let Some(record) = self.journal.state().attempts.get(&offer.attempt.attempt_id) {
            return self.replay_reservation(record, &offer);
        }
        let requested = if source_checked {
            source_pending_bytes
        } else {
            offer.spec.fixture_output.len() as u64
        };
        self.check_pending(requested)?;
        self.check_capacity(&offer.spec.resources)?;
        self.reserve_new(offer, requested)
    }
    fn validate_offer(&self, offer: &Offer, source_checked: bool) -> Result<()> {
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
        Ok(())
    }
    fn replay_reservation(&self, record: &AttemptRecord, offer: &Offer) -> Result<AcceptRequest> {
        ensure!(
            record.offer == *offer,
            "attempt ID reused with changed payload"
        );
        ensure!(
            self.clock.now_ms() < record.acceptance.reservation_deadline_ms
                && record.phase == Phase::Reserved,
            "reservation terminal"
        );
        Ok(record.acceptance.clone())
    }
    fn check_pending(&self, requested_bytes: u64) -> Result<()> {
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
            .checked_add(requested_bytes)
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
        Ok(())
    }
    fn check_capacity(&self, need: &Resources) -> Result<()> {
        let available = self.journal.available()?;
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
        Ok(())
    }
    fn reserve_new(&mut self, offer: Offer, pending_bytes: u64) -> Result<AcceptRequest> {
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
            pending_bytes,
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
}
