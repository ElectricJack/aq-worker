use super::*;
impl Entry {
    pub(super) fn cancel_transport(&self) {
        // Safety cancellation remains available if another owner panicked
        // while holding this mutex. It can revoke IO, never authorize effects.
        let physical = self
            .physical
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some(transport) = &physical.transport {
            transport.cancel();
        }
        if let Some(channel) = &physical.channel {
            channel.close();
        }
    }
    #[cfg(test)]
    pub(super) fn hook(&self, stage: Checkpoint, journal: &mut Journal) -> Result<()> {
        let mut fault = lock(&self.fault)?;
        if fault.as_ref().is_some_and(|(s, _)| *s == stage) {
            journal.inject_fault(fault.take().unwrap().1);
        }
        drop(fault);
        let mut hook = lock(&self.hook)?;
        if hook.as_ref().is_some_and(|(s, _)| *s == stage) {
            journal.set_pause_gate(hook.take().unwrap().1);
        }
        Ok(())
    }

    pub(super) fn current(&self, owner: &Owned) -> Result<()> {
        ensure!(
            !self.stop.load(Ordering::SeqCst),
            "native host safety stop fenced"
        );
        self.profile
            .as_ref()
            .context("recovery cannot authorize host")?
            .root()
            .verify()?;
        self.scope
            .as_ref()
            .context("recovery has no execution scope")?
            .check(owner)?;
        self.profile
            .as_ref()
            .context("recovery has no profile")?
            .root()
            .verify()?;
        ensure!(
            !self.stop.load(Ordering::SeqCst),
            "native host stopped during current checks"
        );
        Ok(())
    }
    pub(super) fn snapshot(&self, journal: &Journal) -> Result<()> {
        *lock(&self.history)? = Some(
            journal
                .state()
                .attempts
                .get(&self.id)
                .and_then(|a| a.native_host.clone())
                .context("native host history absent")?,
        );
        Ok(())
    }
    pub(super) fn stop_now(&self) -> Result<NativeStopOutcome> {
        self.stop.store(true, Ordering::SeqCst);
        self.cancel_transport();
        let h = lock(&self.history)?
            .clone()
            .context("native host has no durable intent")?;
        let intent = h
            .host_intent
            .as_ref()
            .context("native host has no physical configuration")?;
        let root = RootGuard::open(
            &h.intent.host_root,
            Some((h.intent.root_device, h.intent.root_inode)),
        )?;
        root.verify()?;
        let mut handle = SessionHost::recover(&h.intent.host_root, intent)?;
        handle.wait_stopped(Duration::from_secs(2))?;
        let proof = handle.stopped_proof()?;
        let observation = proof.observation().clone();
        if handle.cleanup_private_inputs(&proof).is_err() {
            tracing::warn!(attempt_id=%self.id,"stopped private runtime data retained after cleanup refusal");
        }
        let mut state = lock(&self.physical)?;
        state.stopped = Some(proof);
        state.handle = Some(handle);
        state.outcome = Outcome::Stopped;
        Ok(NativeStopOutcome::StoppedPendingJournal(Box::new(
            observation,
        )))
    }
    pub(super) async fn finish_stopped_transport(&self) -> Result<()> {
        let transport = {
            let mut physical = lock(&self.physical)?;
            if physical.stopped.is_none() {
                return Ok(());
            }
            drop(physical.channel.take()); // removes the socket directory
            drop(physical.observation.take());
            physical.transport.take()
        };
        if let Some(transport) = transport {
            // Cancellation of this wait still aborts the serving task. This
            // closes local IO; it proves no rollback of upstream effects.
            let stopped = tokio::time::timeout(Duration::from_secs(1), transport.stop()).await;
            if !matches!(stopped, Ok(Ok(()))) {
                tracing::warn!(attempt_id=%self.id,"stopped transport task ended without a clean acknowledgement");
            }
        }
        Ok(())
    }
}
