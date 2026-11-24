use super::*;
impl<C: Clock> Runtime<C> {
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
}
