use super::*;
pub(super) struct ObservationErrors {
    pub(super) output: &'static str,
    pub(super) expired: &'static str,
}
impl authority::Authority {
    pub(super) fn wait_for_running(
        &mut self,
        mut released: aq_session_host::observed::ReleasedObservedSession,
        mut observer: aq_session_host::terminal_output::OutputObserver,
        errors: ObservationErrors,
    ) -> Result<()> {
        let scope = self
            .entry
            .scope
            .as_ref()
            .context("native launch scope absent")?;
        let deadline = scope.deadline.min(scope.hard);
        let until = Instant::now() + Duration::from_secs(3);
        let mut output = Vec::new();
        let mut bytes = 0;
        loop {
            let events = observer.poll()?;
            bytes += events
                .iter()
                .map(|e| match e {
                    aq_session_host::terminal_output::ControlEvent::Bytes(b) => b.len(),
                    _ => 0,
                })
                .sum::<usize>();
            ensure!(
                bytes <= 65536 && output.len() + events.len() <= 64,
                "{}",
                errors.output
            );
            output.extend(events);
            if self.entry.stop.load(Ordering::SeqCst) {
                released.stop()?;
                anyhow::bail!("native host stopped before Running")
            }
            if let Some(handle) = released.poll_started(self)? {
                let mut state = lock(&self.entry.physical)?;
                state.handle = Some(handle);
                state.observer = Some(observer);
                state.pending_output = output;
                state.outcome = Outcome::Running;
                return Ok(());
            }
            ensure!(
                boot_ns()? < deadline && Instant::now() < until,
                "{}",
                errors.expired
            );
            std::thread::sleep(Duration::from_millis(2));
        }
    }
}

pub(super) fn permit(
    scope: &LaunchExecutionScope,
    i: &NativeLaunchIntent,
) -> Result<ExecutionPermit> {
    Ok(ExecutionPermit {
        session: i.binding.session.clone(),
        manifest_digest: i.binding.launch_manifest_digest.clone(),
        sequence: 1,
        issued_boot_ms: boot_ns()? / 1_000_000,
        expires_boot_ms: scope.deadline.min(scope.hard) / 1_000_000,
    })
}
impl Entry {
    pub(super) fn retain_bootstrap_failure(&self, owner: &Arc<Mutex<Owned>>) {
        let _ = self.stop_now();
        if let Ok(mut owned) = lock(owner) {
            let _ = owned.journal.native_update(self.id, |h, _, _| {
                h.uncertain = Some(LaunchFailure::ExecUnobserved);
                Ok(())
            });
            let proof = lock(&self.physical)
                .ok()
                .and_then(|state| state.stopped.clone());
            if let Some(proof) = proof {
                let _ = owned.journal.native_stopped(self.id, &proof);
            }
            let _ = self.snapshot(&owned.journal);
        }
    }
    pub(super) async fn launch_result(
        self: Arc<Self>,
        owner: &Arc<Mutex<Owned>>,
        result: Result<()>,
    ) -> Result<NativeHostLaunchResult> {
        match result {
            Ok(()) => {
                let outcome = lock(&self.physical)?.outcome;
                Ok(match outcome {
                    Outcome::Running => NativeHostLaunchResult::Running(LocalPhaseHost {
                        entry: self,
                        owner: owner.clone(),
                    }),
                    Outcome::Stopped => {
                        NativeHostLaunchResult::StoppedBeforeStarted(LocalStoppedHistory {
                            entry: self,
                        })
                    }
                    _ => NativeHostLaunchResult::Unknown(HeldLaunchHistory { entry: self }),
                })
            }
            Err(_) => {
                let _ = self.stop_now();
                self.finish_stopped_transport().await?;
                let stopped = lock(&self.history)?
                    .as_ref()
                    .is_some_and(|h| h.stopped.is_some() && h.running.is_none());
                lock(&self.physical)?.outcome = if stopped {
                    Outcome::Stopped
                } else {
                    Outcome::Unknown
                };
                if stopped {
                    Ok(NativeHostLaunchResult::StoppedBeforeStarted(
                        LocalStoppedHistory { entry: self },
                    ))
                } else {
                    Ok(NativeHostLaunchResult::Unknown(HeldLaunchHistory {
                        entry: self,
                    }))
                }
            }
        }
    }
}
