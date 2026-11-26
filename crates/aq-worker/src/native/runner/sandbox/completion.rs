use super::*;
use crate::native::launch::sandbox as host;
pub(super) struct Completion<'a, 'l, S: LoginService> {
    pub(super) key: AttemptKey,
    pub(super) local: &'a LocalEnvironment,
    pub(super) handle: aq_session_host::SessionHandle,
    pub(super) journal: journal::Journal,
    pub(super) output: Option<output_runtime::OutputRuntime>,
    pub(super) active: ActiveLogin<'l, S>,
}
impl<S: LoginService> Completion<'_, '_, S> {
    pub(super) async fn finish<T: RunnerTransport>(
        mut self,
        reason: &'static str,
        deadline: u64,
        connection: (&WorkerConfig, &mut T),
    ) -> Result<RunOutcome> {
        let login_failed = reason == "harness exited"
            && self.active.as_ref().is_some_and(|(login, _)| {
                self.handle.observe().is_ok_and(|observed| {
                    (login.classify)(&observed) == HarnessExit::AuthenticationFailed
                })
            });
        let proof = match host::stop(&mut self.handle) {
            Ok(proof) => proof,
            Err(_) => {
                let wiped = finish_login(&mut self.active, &self.journal.directory, deadline).await;
                return Ok(RunOutcome::Preserved {
                    reason: if wiped.is_ok() {
                        "stop uncertain"
                    } else {
                        "login teardown failed"
                    },
                    stop_proven: false,
                });
            }
        };
        if let Some(runtime) = self.output {
            runtime.stop().await;
        }
        // The harness has stopped writing: write back, then wipe, before any
        // stopped/preserved record, retention or finished outcome.
        let wiped = finish_login(&mut self.active, &self.journal.directory, deadline).await;
        self.journal.record("stopped", &proof)?;
        if wiped.is_err() {
            self.journal.record("preserved", &"login teardown failed")?;
            return Ok(RunOutcome::Preserved {
                reason: "login teardown failed",
                stop_proven: true,
            });
        }
        if login_failed {
            let failure = LoginFailure::CREDENTIAL_INVALID;
            self.journal.record("login-failure", &failure)?;
            self.journal.record("preserved", &"credential_invalid")?;
            return Ok(RunOutcome::LoginFailed {
                failure,
                stop_proven: true,
            });
        }
        if reason != "harness exited" {
            self.journal.record("preserved", &reason)?;
            return Ok(RunOutcome::Preserved {
                reason,
                stop_proven: true,
            });
        }
        let stopped = StoppedAttempt {
            key: self.key,
            workspace: self.local.workspace.clone(),
            evidence: serde_json::to_vec(&proof)?,
        };
        stopped.retain(&self.journal, connection).await
    }
}
impl StoppedAttempt {
    async fn retain<T: RunnerTransport>(
        self,
        journal: &journal::Journal,
        (worker, transport): (&WorkerConfig, &mut T),
    ) -> Result<RunOutcome> {
        let stopped = self;
        let receipts =
            match tokio::time::timeout(Duration::from_secs(30), transport.retain(worker, &stopped))
                .await
            {
                Ok(Ok(receipts)) => receipts,
                _ => {
                    return Ok(RunOutcome::Preserved {
                        reason: "retention unavailable",
                        stop_proven: true,
                    });
                }
            };
        if validate_receipts(&stopped, &receipts).is_err() {
            return Ok(RunOutcome::Preserved {
                reason: "retention receipts incomplete",
                stop_proven: true,
            });
        }
        journal.record("retention", &receipts)?;
        let retained = RetainedAttempt { stopped, receipts };
        let receipt = match tokio::time::timeout(
            Duration::from_secs(30),
            transport.finished(worker, &retained),
        )
        .await
        {
            Ok(Ok(receipt))
                if !receipt.receipt_id.is_nil() && receipt.attempt == retained.stopped.key =>
            {
                receipt
            }
            _ => {
                return Ok(RunOutcome::Preserved {
                    reason: "finished outcome unknown",
                    stop_proven: true,
                });
            }
        };
        journal.record("finished", &receipt)?;
        Ok(RunOutcome::Finished)
    }
}
