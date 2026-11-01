//! Synchronous fsync callbacks use the pre-reserved writer, never async storage.
use super::*;
use aq_session_host::{
    GuardianIdentity, LaunchAuthority, LaunchIntent,
    observed::{ObservedLaunchAuthority, VerifiedBlankBaseline},
};
pub(super) struct Authority {
    pub(super) owner: Arc<Mutex<Owned>>,
    pub(super) entry: Arc<Entry>,
}
impl Authority {
    fn update(
        &self,
        stage: Checkpoint,
        change: impl FnOnce(&mut NativeHostHistory, u64, &State) -> Result<()>,
    ) -> Result<()> {
        let mut owner = lock(&self.owner)?;
        self.entry.current(&owner)?;
        #[cfg(test)]
        self.entry.hook(stage, &mut owner.journal)?;
        #[cfg(not(test))]
        let _ = stage;
        owner.journal.native_update(self.entry.id, change)?;
        self.entry.snapshot(&owner.journal)?;
        self.entry.current(&owner)
    }
}
impl LaunchAuthority for Authority {
    fn record_launch_intent(&mut self, intent: &LaunchIntent) -> Result<()> {
        let intent = intent.clone();
        self.update(Checkpoint::Configured, move |h, _, _| {
            ensure!(
                h.host_intent.is_none(),
                "host configuration already recorded"
            );
            h.host_intent = Some(intent);
            Ok(())
        })
    }
    fn record_guardian(&mut self, guardian: &GuardianIdentity) -> Result<()> {
        let guardian = guardian.clone();
        self.update(Checkpoint::Guardian, move |h, _, _| {
            ensure!(h.guardian.is_none(), "guardian already recorded");
            h.guardian = Some(guardian);
            Ok(())
        })
    }
    fn record_start_intent(
        &mut self,
        intent: &LaunchIntent,
        guardian: &GuardianIdentity,
    ) -> Result<()> {
        let intent = intent.clone();
        let guardian = guardian.clone();
        self.update(Checkpoint::StartIntent, move |h, _, _| {
            ensure!(
                h.host_intent.as_ref() == Some(&intent)
                    && h.guardian.as_ref() == Some(&guardian)
                    && !h.start_intended,
                "start scope changed"
            );
            h.start_intended = true;
            Ok(())
        })
    }
    fn record_started(&mut self, observation: &HostObservation) -> Result<()> {
        let observation = observation.clone();
        self.update(Checkpoint::Started, move |h, revision, state| {
            ensure!(
                h.running.is_none() && h.baseline.is_some(),
                "Running already recorded or baseline absent"
            );
            let mapped: aq_admission::started::RunningObservation =
                serde_json::from_value(serde_json::to_value(&observation)?)?;
            mapped.validate()?;
            let i = &h.intent;
            let request = aq_admission::started::Request {
                schema_version: 1,
                operation_id: i.started_operation_id,
                binding: i.binding.clone(),
                grant_id: i.grant_id,
                guardian_id: i.logical_guardian_id,
                launch_id: i.launch_id,
                reservation_id: i.reservation_id,
                runtime: i.runtime.runtime.clone(),
                qualification: i.runtime.qualification.clone(),
                journal_revision: state.revision,
                journal_digest: crate::native::journal::canonical_digest(state)?,
                observation_digest: mapped.digest()?,
                observation: mapped,
            };
            request.validate()?;
            h.running = Some(crate::native::journal::launch::NativeRunning {
                observation,
                request,
                recorded_revision: revision,
            });
            Ok(())
        })
    }
}
impl ObservedLaunchAuthority for Authority {
    fn record_baseline(&mut self, baseline: &VerifiedBlankBaseline) -> Result<()> {
        let (columns, rows) = baseline.dimensions();
        let b = crate::native::journal::launch::NativeBaseline {
            baseline_id: baseline.baseline_id(),
            terminal: baseline.terminal().clone(),
            renderer: baseline.renderer().clone(),
            guardian: baseline.guardian().clone(),
            columns,
            rows,
            digest: baseline.digest()?,
        };
        self.update(Checkpoint::Baseline, move |h, _, _| {
            ensure!(h.baseline.is_none(), "baseline release already recorded");
            h.baseline = Some(b);
            Ok(())
        })
    }
}
