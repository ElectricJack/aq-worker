use super::*;
impl Driver<'_> {
    pub(super) async fn recover(&mut self, registration: &RegisterRequest) -> Result<bool> {
        let identity = &registration.identity;
        self.runtime.recover_owned_launches()?;
        if let Some(git) = self.git {
            self.export_stopped(git)?;
            self.upload_stopped().await?;
        }
        let response = self.reconcile().await?;
        println!(
            "{}",
            serde_json::json!({"event":"reconciling","identity":identity,"dispositions":response})
        );
        self.accept_dispositions(&response)?;
        let recovered = self.replay_reports().await?;
        self.retire_released()?;
        let uncertain = self.runtime.journal.state().attempts.values().any(|r| {
            r.phase != Phase::Retired
                && !matches!(
                    r.stop_certainty,
                    StopCertainty::ConfirmedStopped | StopCertainty::NotStarted
                )
        });
        if uncertain {
            println!(
                "{}",
                serde_json::json!({"event":"recovery_hold","registration":registration,"protected_usage":self.runtime.journal.protected_usage()?,"available":self.runtime.journal.available()?})
            );
            bail!(
                "prior writers/reservations require process reconciliation; preserved without relaunch"
            );
        }
        self.runtime.journal.transaction(|s| {
            s.reconciliation_required = false;
            Ok(())
        })?;
        if recovered > 0 {
            if self.options.exit_after_result {
                return Ok(true);
            }
            // Receipt recovery may have supplied the daemon's first durable
            // stopped proof, so acquire a fresh explicit recovery disposition.
            self.client
                .reconcile(&self.runtime.reconcile_request())
                .await?;
        }
        Ok(false)
    }
    async fn reconcile(&self) -> Result<ReconcileResponse> {
        match self
            .client
            .reconcile(&self.runtime.reconcile_request())
            .await
        {
            Ok(response) => Ok(response),
            Err(error)
                if error
                    .downcast_ref::<RemoteError>()
                    .is_some_and(|e| e.error.code == ErrorCode::StaleBoot)
                    && self.runtime.journal.state().attempts.is_empty() =>
            {
                Ok(ReconcileResponse {
                    attempts: vec![],
                    next_cursor: None,
                })
            }
            Err(error) => Err(error),
        }
    }
    fn accept_dispositions(&mut self, response: &ReconcileResponse) -> Result<()> {
        for disposition in &response.attempts {
            if let Some(result) = &disposition.result {
                let exists = self.runtime.journal.state().attempts.values().any(|r| {
                    r.report
                        .as_ref()
                        .is_some_and(|p| p.report_id == result.report_id)
                });
                if exists {
                    self.runtime.acknowledge_result(result.clone())?;
                }
            }
        }
        Ok(())
    }
    async fn replay_reports(&mut self) -> Result<usize> {
        let reports: Vec<_> = self
            .runtime
            .journal
            .state()
            .attempts
            .values()
            .filter_map(|r| {
                if r.phase == Phase::Reporting
                    && r.source.as_ref().is_none_or(|s| {
                        s.receipt.is_some()
                            || (!s.operations.contains_key("create")
                                && s.workspace.is_none()
                                && r.launch_id.is_none())
                    })
                    && matches!(
                        r.stop_certainty,
                        StopCertainty::ConfirmedStopped | StopCertainty::NotStarted
                    )
                {
                    r.report.clone()
                } else {
                    None
                }
            })
            .collect();
        let mut recovered = 0;
        for report in reports {
            if let Some(response) = submit_report(self.client, &mut self.runtime, &report).await? {
                self.runtime.acknowledge_result(response.clone())?;
                if response.retention_release {
                    retire_local(&mut self.runtime, report.attempt.attempt_id, self.git)?;
                }
            }
            recovered += 1;
        }
        Ok(recovered)
    }
    fn retire_released(&mut self) -> Result<()> {
        let released: Vec<_> = self
            .runtime
            .journal
            .state()
            .attempts
            .iter()
            .filter_map(|(id, r)| {
                (r.phase == Phase::Acknowledged
                    && r.result.as_ref().is_some_and(|x| x.retention_release))
                .then_some(*id)
            })
            .collect();
        for id in released {
            retire_local(&mut self.runtime, id, self.git)?;
        }
        Ok(())
    }
}
