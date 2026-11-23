use super::*;
impl Registry {
    pub(in super::super) fn new(state: &State) -> Self {
        #[cfg(test)]
        let hook = Arc::new(Mutex::new(None));
        #[cfg(test)]
        let fault = Arc::new(Mutex::new(None));
        let mut entries = BTreeMap::new();
        for (id, a) in &state.attempts {
            if let Some(h) = &a.native_host {
                entries.insert(
                    *id,
                    Arc::new(Entry {
                        #[cfg(test)]
                        hook: hook.clone(),
                        #[cfg(test)]
                        fault: fault.clone(),
                        id: *id,
                        scope: None,
                        result_channel: None,
                        profile: None,
                        history: Mutex::new(Some(h.clone())),
                        stop: AtomicBool::new(true),
                        physical: Mutex::new(Physical {
                            handle: None,
                            observer: None,
                            pending_output: Vec::new(),
                            output_taken_without_sequencer: false,
                            stopped: None,
                            transport: None,
                            channel: None,
                            observation: None,
                            outcome: Outcome::Unknown,
                        }),
                    }),
                );
            }
        }
        Self {
            control: Mutex::new(None),
            #[cfg(test)]
            hook,
            #[cfg(test)]
            fault,
            output: Arc::new(Mutex::new(output_relay::Outputs::default())),
            admitted: Mutex::new(state.attempts.keys().copied().collect()),
            entries: Mutex::new(RegistryEntries {
                entries,
                fenced: BTreeSet::new(),
            }),
            bootstrap: Arc::new(Semaphore::new(1)),
            stop_slots: Arc::new(Semaphore::new(1)),
            #[cfg(any(test, feature = "test-support"))]
            pool: aq_session_host::observed::SealedElfPool::new(16 * 1024 * 1024, 4)
                .expect("fixed bounded ELF pool"),
        }
    }
    pub(in super::super) fn admit(&self, id: Uuid) -> Result<()> {
        let mut admitted = lock(&self.admitted)?;
        ensure!(
            admitted.contains(&id) || admitted.len() < 64,
            "native registry admission capacity"
        );
        admitted.insert(id);
        Ok(())
    }
    pub(in super::super) fn result_stopped(
        &self,
        scope: &LaunchExecutionScope,
    ) -> Result<aq_session_host::StoppedProof> {
        let e = self.entry(scope.id)?;
        let actual = e
            .scope
            .as_ref()
            .context("reopened host cannot capture result")?;
        ensure!(
            actual.digest == scope.digest && actual.inputs.same_owner(&scope.inputs),
            "result stopped host owner differs"
        );
        let p = lock(&e.physical)?;
        let proof = p
            .stopped
            .as_ref()
            .context("result requires actual retained StoppedProof")?;
        ensure!(
            matches!(p.outcome, Outcome::Stopped) && proof.observation().kernel_wait_echild,
            "result writers not proven stopped"
        );
        Ok(proof.clone())
    }
    pub(in super::super) fn result_current(&self, scope: &LaunchExecutionScope) -> Result<()> {
        let e = self.entry(scope.id)?;
        let actual = e
            .scope
            .as_ref()
            .context("reopened host has no result authority")?;
        ensure!(
            actual.digest == scope.digest
                && actual.inputs.same_owner(&scope.inputs)
                && !e.stop.load(Ordering::SeqCst),
            "result host owner differs or stopped"
        );
        let physical = lock(&e.physical)?;
        ensure!(
            physical.stopped.is_none() && matches!(physical.outcome, Outcome::Running),
            "result host not live"
        );
        let observation = physical
            .handle
            .as_ref()
            .context("result actual host absent")?
            .observe()?;
        ensure!(
            observation.phase == aq_session_host::HostPhase::Running,
            "result process no longer running"
        );
        Ok(())
    }
    pub(super) fn entry(&self, id: Uuid) -> Result<Arc<Entry>> {
        lock(&self.entries)?
            .entries
            .get(&id)
            .cloned()
            .context("native host attempt absent")
    }
    pub(super) fn fence(&self, id: Uuid) -> Result<Option<Arc<Entry>>> {
        ensure!(
            lock(&self.admitted)?.contains(&id),
            "native stop not admitted"
        );
        let mut state = lock(&self.entries)?;
        state.fenced.insert(id);
        let entry = state.entries.get(&id).cloned();
        if let Some(e) = &entry {
            e.stop.store(true, Ordering::SeqCst);
        }
        drop(state);
        if let Some(e) = &entry {
            e.cancel_transport();
        }
        Ok(entry)
    }
    pub(super) fn claim_profile(
        &self,
        scope: LaunchExecutionScope,
        profile: Profile,
        result_channel: Option<crate::native::result::VerifiedResultChannel>,
    ) -> Result<Arc<Entry>> {
        ensure!(
            lock(&self.admitted)?.contains(&scope.id),
            "native host not admitted"
        );
        let mut state = lock(&self.entries)?;
        ensure!(
            !state.entries.contains_key(&scope.id) && !state.fenced.contains(&scope.id),
            "native host already claimed; no relaunch"
        );
        let e = Arc::new(Entry {
            #[cfg(test)]
            hook: self.hook.clone(),
            #[cfg(test)]
            fault: self.fault.clone(),
            id: scope.id,
            scope: Some(Arc::new(scope)),
            result_channel: result_channel.map(Arc::new),
            profile: Some(profile),
            history: Mutex::new(None),
            stop: AtomicBool::new(false),
            physical: Mutex::new(Physical {
                handle: None,
                observer: None,
                pending_output: Vec::new(),
                output_taken_without_sequencer: false,
                stopped: None,
                transport: None,
                channel: None,
                observation: None,
                outcome: Outcome::Pending,
            }),
        });
        state.entries.insert(e.id, e.clone());
        Ok(e)
    }
}
