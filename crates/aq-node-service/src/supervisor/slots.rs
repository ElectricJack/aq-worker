use super::*;

enum SlotAction {
    Attempt,
    Retry,
    Stop,
    Retire,
}

struct SlotRun {
    slot: u32,
    node_id: Uuid,
    stopping: watch::Receiver<bool>,
    pause: watch::Receiver<bool>,
    failures: Failures,
    // A held attempt reports during its delay. If it cannot stay up, wait here.
    resume: tokio::time::Instant,
    holds: bool,
}

impl<B: SlotBackend, C: ControlSource> Shared<B, C> {
    async fn await_enrollment(
        &self,
        slot: u32,
        stopping: &mut watch::Receiver<bool>,
    ) -> Option<Uuid> {
        let retry = &self.sup.config.retry;
        let mut delay = Duration::from_millis(retry.fault_backoff_start_ms);
        let maximum = Duration::from_millis(retry.fault_backoff_max_ms);
        let enrollment = self.sup.layout.slot_enrollment(slot);
        loop {
            match EnrollmentFile::load(&enrollment) {
                Ok(f) => return Some(f.node_id),
                Err(e) => tracing::error!(slot, "slot waits for its enrollment: {e:#}"),
            }
            if !back_off(stopping, &mut delay, maximum).await {
                return None;
            }
        }
    }

    pub(super) async fn slot_loop(self: Arc<Self>, slot: u32) -> bool {
        let resume = tokio::time::Instant::now();
        let mut stopping = self.stopping.subscribe();
        let pause = self.pause.tx.subscribe();
        let Some(node_id) = self.await_enrollment(slot, &mut stopping).await else {
            return false;
        };
        let mut run = SlotRun {
            slot,
            node_id,
            stopping,
            pause,
            failures: Failures::new(&self.sup.config.retry),
            resume,
            holds: true,
        };
        run.run(&self).await
    }
}

impl SlotRun {
    async fn run<B: SlotBackend, C: ControlSource>(&mut self, shared: &Shared<B, C>) -> bool {
        while !*self.stopping.borrow() {
            match self.next_action(shared).await {
                SlotAction::Retire => return true,
                SlotAction::Stop => return false,
                SlotAction::Retry => continue,
                SlotAction::Attempt => {}
            }
            let remaining = self.remaining();
            if !self.holds && !wait(&mut self.stopping, remaining).await {
                return false;
            }
            let result = shared
                .attempt(self.slot, self.node_id, self.remaining())
                .await;
            if !self.record(shared, result) {
                return false;
            }
        }
        false
    }

    fn remaining(&self) -> Duration {
        self.resume
            .saturating_duration_since(tokio::time::Instant::now())
    }

    async fn next_action<B: SlotBackend, C: ControlSource>(
        &mut self,
        shared: &Shared<B, C>,
    ) -> SlotAction {
        match self.settle(shared).await {
            SlotAction::Attempt => self.await_capacity(shared).await,
            other => other,
        }
    }

    async fn settle<B: SlotBackend, C: ControlSource>(
        &mut self,
        shared: &Shared<B, C>,
    ) -> SlotAction {
        let slot = self.slot;
        match shared.settle(slot, self.node_id).await {
            Ok(true) => SlotAction::Attempt,
            Ok(false) => {
                // Recheck the journal once after an operator records a handoff. Recover
                // still decides whether this slot can be archived and take new work.
                if !await_recovery_record(
                    &shared.sup.layout.recovery_recorded(slot),
                    &mut self.stopping,
                )
                .await
                {
                    return SlotAction::Stop;
                }
                SlotAction::Retry
            }
            Err(e) => {
                tracing::warn!(
                    slot,
                    "recovery failed, retrying in {}s: {e:#}",
                    self.failures.fault.as_secs()
                );
                if !back_off(
                    &mut self.stopping,
                    &mut self.failures.fault,
                    Duration::from_millis(shared.sup.config.retry.fault_backoff_max_ms),
                )
                .await
                {
                    return SlotAction::Stop;
                }
                SlotAction::Retry
            }
        }
    }

    async fn await_capacity<B: SlotBackend, C: ControlSource>(
        &mut self,
        shared: &Shared<B, C>,
    ) -> SlotAction {
        loop {
            if self.slot > *shared.target.borrow() {
                return SlotAction::Retire;
            }
            if *self.stopping.borrow() {
                return SlotAction::Stop;
            }
            let mut target = shared.target.subscribe();
            let paused = *self.pause.borrow();
            let ready = async {
                match paused {
                    true => std::future::pending().await,
                    false => shared.sup.backend.ready(self.node_id).await,
                }
            };
            tokio::select! { _ = ready => return SlotAction::Attempt, _ = self.pause.changed() => {}, _ = self.stopping.changed() => {}, _ = target.changed() => {} }
        }
    }

    fn record<B: SlotBackend, C: ControlSource>(
        &mut self,
        shared: &Shared<B, C>,
        result: Result<AttemptOutcome>,
    ) -> bool {
        let slot = self.slot;
        match result {
            Ok(AttemptOutcome::AttemptStopped) => {
                (self.failures, self.holds) = (Failures::new(&shared.sup.config.retry), true);
            }
            Ok(AttemptOutcome::Paused) => self.holds = true,
            Ok(AttemptOutcome::ShutdownBeforeAdmission) => return false,
            Err(e) if !self.remaining().is_zero() => {
                self.holds = false;
                tracing::warn!(
                    slot,
                    "the slot could not stay up while it waited; it waits {}s without reporting: {e:#}",
                    self.remaining().as_secs()
                );
            }
            Err(e) => {
                let text = format!("{e:#}");
                let kind = FailureKind::of(&text);
                let delay = self.failures.pause(kind, &shared.sup.config.retry);
                tracing::warn!(
                    slot,
                    ?kind,
                    "attempt failed, retrying in {}s: {text}",
                    delay.as_secs()
                );
                // The server answered (a refusal, not a lost reply): recovery reads this.
                if ["returned 4", "returned 503"]
                    .iter()
                    .any(|s| text.contains(&format!("native endpoint {s}")))
                {
                    let _ = write_private(
                        &shared.sup.layout.current(slot).join(SERVER_REFUSED),
                        text.as_bytes(),
                    );
                }
                self.resume = tokio::time::Instant::now() + delay;
            }
        }
        true
    }
}
