use super::*;
struct TerminalDisposition {
    outcome: ProcessOutcome,
    certainty: StopCertainty,
}

impl<C: Clock> Runtime<C> {
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
        let report = self.record_terminal(
            attempt_id,
            TerminalDisposition {
                outcome,
                certainty: StopCertainty::ConfirmedStopped,
            },
            output,
        )?;
        crate::fault::checkpoint(
            self.journal.root(),
            "report-durable",
            Some(handle_launch_id),
        )?;
        Ok(report)
    }
    fn record_terminal(
        &mut self,
        attempt_id: Uuid,
        result: TerminalDisposition,
        output: String,
    ) -> Result<ResultReport> {
        ensure!(
            output.len() <= MAX_TEXT,
            "fixture output exceeds wire bound"
        );
        self.journal.transaction(|s| {
            let r = s.attempts.get_mut(&attempt_id).context("unknown attempt")?;
            r.phase = Phase::Collecting;
            r.stop_certainty = result.certainty;
            let report = ResultReport {
                identity: r.offer.identity.clone(),
                report_id: Uuid::new_v4(),
                supersedes_report_id: None,
                attempt: r.offer.attempt.clone(),
                resolved_spec_digest: r.offer.spec.resolved_spec_digest.clone(),
                outcome: result.outcome,
                stop_certainty: r.stop_certainty,
                claimed_success: result.outcome == ProcessOutcome::Success && r.source.is_none(),
                output,
                artifacts: vec![],
                pending_operation_ids: vec![],
            };
            r.report = Some(report.clone());
            r.phase = Phase::Reporting;
            Ok(report)
        })
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
            TerminalDisposition {
                outcome,
                certainty: StopCertainty::NotStarted,
            },
            "not started: durable writer Start boundary was never crossed".into(),
        )
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
}
