use super::*;
impl<C: Clock> Runtime<C> {
    pub fn launch_fixture(&mut self, attempt_id: Uuid, executable: &Path) -> Result<FixtureHandle> {
        crate::guardian::probe_profile()?;
        let launch_id = self.begin_launch(attempt_id)?;
        crate::fault::checkpoint(
            self.journal.root(),
            "launch-intent-durable",
            Some(launch_id),
        )?;
        let binding = self.launch_binding(attempt_id, launch_id, executable)?;
        let r = &self.journal.state().attempts[&attempt_id];
        let handle =
            FixtureHandle::spawn(self.journal.root(), binding, &r.offer.spec.fixture_output)?;
        let identity = handle.identity().clone();
        ensure!(
            identity.node_identity == r.offer.identity
                && identity.attempt == r.offer.attempt
                && identity.spec_digest == r.offer.spec.resolved_spec_digest,
            "guardian assignment binding mismatch"
        );
        self.record_start_intent(attempt_id, launch_id, identity)?;
        self.start_guardian(attempt_id, launch_id, &handle)?;
        Ok(handle)
    }
    fn launch_binding(
        &self,
        attempt_id: Uuid,
        launch_id: Uuid,
        executable: &Path,
    ) -> Result<crate::guardian::LaunchBinding> {
        let r = &self.journal.state().attempts[&attempt_id];
        let system_origin_ms = crate::clock::raw_boot_ms();
        let logical_origin_ms = self.clock.now_ms();
        Ok(crate::guardian::LaunchBinding {
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
        })
    }
    fn record_start_intent(
        &mut self,
        attempt_id: Uuid,
        launch_id: Uuid,
        identity: crate::guardian::GuardianIdentity,
    ) -> Result<()> {
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
        Ok(())
    }
    fn start_guardian(
        &mut self,
        attempt_id: Uuid,
        launch_id: Uuid,
        handle: &FixtureHandle,
    ) -> Result<()> {
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
        Ok(())
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
            if !self.recover_record(id, &record)? {
                uncertain.push(id);
            }
        }
        Ok(uncertain)
    }
    fn recover_record(&mut self, id: Uuid, record: &AttemptRecord) -> Result<bool> {
        if record.launch_id.is_none() && record.stop_certainty == StopCertainty::NotStarted {
            self.report_not_started(id, record.stop_reason.unwrap_or(ProcessOutcome::Expired))?;
            return Ok(true);
        }
        if record.ownership_profile.as_deref() != Some(crate::guardian::PROFILE) {
            return Ok(false);
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
            return Ok(true);
        }
        if let Err(error) = self.stop_recovered(id, launch_id, record) {
            eprintln!("launch {launch_id} remains stop_uncertain: {error:#}");
            self.journal.transaction(|s| {
                let r = s.attempts.get_mut(&id).unwrap();
                r.phase = Phase::LaunchUncertain;
                r.stop_certainty = StopCertainty::StopUncertain;
                Ok(())
            })?;
            return Ok(false);
        }
        Ok(true)
    }
    fn stop_recovered(&mut self, id: Uuid, launch_id: Uuid, record: &AttemptRecord) -> Result<()> {
        let handle =
            FixtureHandle::recover(self.journal.root(), launch_id, record.guardian.as_ref())?;
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
    }
}
