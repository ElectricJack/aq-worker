use super::*;
pub(super) enum Progress {
    Skip,
    Finished,
    Acknowledged,
}
impl Driver<'_> {
    pub(super) async fn handle_offer(
        &mut self,
        offer: Offer,
        identity: &NodeIdentity,
        server_time_ms: u64,
    ) -> Result<Progress> {
        if self
            .runtime
            .journal
            .state()
            .attempts
            .contains_key(&offer.attempt.attempt_id)
        {
            return Ok(Progress::Skip);
        }
        // Server epoch is used only relative to the supplied server epoch.
        if server_time_ms >= offer.offer_expires_at_ms {
            return Ok(Progress::Skip);
        }

        let attempt_id = offer.attempt.attempt_id;
        let source = offer.spec.bindings.as_ref().and_then(|b| b.source.as_ref());
        let admitted = self.reserve(&offer, source);
        let acceptance = match admitted {
            Ok(r) => r,
            Err(e) => {
                let decline = DeclineRequest {
                    identity: identity.clone(),
                    operation_id: Uuid::new_v4(),
                    offer_id: offer.offer_id,
                    reason: e.to_string(),
                };
                self.client.decline(&decline).await?;
                return Ok(Progress::Skip);
            }
        };

        if !self.authorize(&acceptance).await? {
            self.expired(attempt_id).await?;
            return Ok(self.pending_progress());
        }
        self.prepare(attempt_id, source).await?;
        let report = self.run_fixture(attempt_id).await?;
        let report = if source.is_some() {
            let Some(report) = self.collect_source(attempt_id).await? else {
                return Ok(self.pending_progress());
            };
            report
        } else {
            report
        };
        let observation = self.runtime.observation(
            attempt_id,
            "synthetic writers confirmed stopped; immutable report pending".into(),
        )?;
        deliver_stopped_observation(self.client, &mut self.runtime, &observation).await?;
        self.deliver_report(attempt_id, &report).await?;
        Ok(Progress::Acknowledged)
    }
    fn pending_progress(&self) -> Progress {
        if self.options.exit_after_result {
            Progress::Finished
        } else {
            Progress::Skip
        }
    }
    fn reserve(&mut self, offer: &Offer, source: Option<&SourceBinding>) -> Result<AcceptRequest> {
        if offer
            .spec
            .required_capabilities
            .contains(&SOURCE_TRANSFER_CAPABILITY.into())
            && self.transfer.is_none()
        {
            Err(anyhow::anyhow!("source transfer is not configured"))
        } else if let Some(source) = source {
            match self.git {
                Some(git) => {
                    let ready = if self.transfer.is_some() {
                        Ok(())
                    } else {
                        git.base_available(source)
                    };
                    ready.and_then(|()| self.runtime.reserve_source(offer.clone(), git))
                }
                None => Err(anyhow::anyhow!("local source adapter is not configured")),
            }
        } else {
            self.runtime.reserve(offer.clone())
        }
    }
    async fn authorize(&mut self, acceptance: &AcceptRequest) -> Result<bool> {
        let mut sent = self.runtime.clock.now_ms();
        while sent < acceptance.reservation_deadline_ms {
            match self.client.accept(acceptance).await {
                Ok(_) if self.options.drop_acceptance_response_once => {
                    self.options.drop_acceptance_response_once = false;
                }
                Ok(response) => {
                    return self.install_grant(
                        response.grant,
                        sent,
                        acceptance.reservation_deadline_ms,
                    );
                }
                Err(error) if retryable(&error) => retry_delay().await,
                Err(error) => return Err(error),
            }
            sent = self.runtime.clock.now_ms();
        }
        Ok(false)
    }
    fn install_grant(&mut self, grant: Grant, sent: u64, deadline: u64) -> Result<bool> {
        match self.runtime.authorize(grant, sent) {
            Ok(()) => Ok(true),
            Err(_) if self.runtime.clock.now_ms() >= deadline => Ok(false),
            Err(error) => Err(error),
        }
    }
    async fn expired(&mut self, attempt_id: Uuid) -> Result<()> {
        let report = self
            .runtime
            .report_not_started(attempt_id, ProcessOutcome::Expired)?;
        if let Some(response) = submit_report(self.client, &mut self.runtime, &report).await? {
            self.runtime.acknowledge_result(response.clone())?;
            if response.retention_release {
                retire_local(&mut self.runtime, attempt_id, self.git)?;
            }
        }
        println!(
            "{}",
            serde_json::json!({"event":"reservation_expired_not_started","attempt_id":attempt_id})
        );
        Ok(())
    }
}
