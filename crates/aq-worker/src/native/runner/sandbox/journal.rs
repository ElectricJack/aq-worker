//! One-shot, append-only local evidence. No restart here may reacquire execution.
use anyhow::Result;
use aq_session_host::{GuardianIdentity, HostObservation, LaunchAuthority, LaunchIntent};
use serde::Serialize;
use std::{
    fs::{self, File},
    os::unix::fs::DirBuilderExt,
    path::{Path, PathBuf},
};

pub(crate) struct Journal {
    pub directory: PathBuf,
    pub intent: Option<LaunchIntent>,
}
impl Journal {
    pub fn create(state: &Path, frozen: &super::FrozenLaunch) -> Result<Self> {
        let directory = state.join("sandbox-run");
        // Atomic create is both the concurrent admission fence and replay fence.
        fs::DirBuilder::new().mode(0o700).create(&directory)?;
        File::open(state)?.sync_all()?;
        let journal = Self {
            directory,
            intent: None,
        };
        journal.record("handoff", frozen)?;
        Ok(journal)
    }
    pub fn record(&self, name: &str, value: &impl Serialize) -> Result<()> {
        aq_session_host::durable_new(
            &self.directory.join(format!("{name}.json")),
            &serde_json::to_vec(value)?,
        )
    }
}
impl LaunchAuthority for Journal {
    fn record_launch_intent(&mut self, intent: &LaunchIntent) -> Result<()> {
        self.record("launch-intent", intent)?;
        self.intent = Some(intent.clone());
        Ok(())
    }
    fn record_guardian(&mut self, identity: &GuardianIdentity) -> Result<()> {
        self.record("guardian", identity)
    }
    fn record_start_intent(
        &mut self,
        intent: &LaunchIntent,
        guardian: &GuardianIdentity,
    ) -> Result<()> {
        self.record("start-intent", &(intent, guardian))
    }
    fn record_started(&mut self, observation: &HostObservation) -> Result<()> {
        self.record("started", observation)
    }
}

impl aq_session_host::observed::ObservedLaunchAuthority for Journal {
    fn record_baseline(
        &mut self,
        baseline: &aq_session_host::observed::VerifiedBlankBaseline,
    ) -> Result<()> {
        self.record("baseline", baseline)
    }
}
