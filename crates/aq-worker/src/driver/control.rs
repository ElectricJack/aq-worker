use super::*;
pub(super) async fn control_fixture(
    runtime: &mut Runtime<BootClock>,
    client: &ExecutionClient,
    attempt_id: Uuid,
    handle: &mut crate::fixture::FixtureHandle,
) -> Result<()> {
    let next_poll = runtime.clock.now_ms();
    let now = runtime.clock.now_ms();
    let next_renew = now.saturating_add(
        runtime.journal.state().attempts[&attempt_id]
            .grant_deadline_ms
            .unwrap()
            .saturating_sub(now)
            / 3,
    );
    Control {
        runtime,
        client,
        attempt_id,
        handle,
        next_poll,
        next_renew,
        renewal: None,
        kill_sent: false,
    }
    .run()
    .await
}
struct Control<'a> {
    runtime: &'a mut Runtime<BootClock>,
    client: &'a ExecutionClient,
    attempt_id: Uuid,
    handle: &'a mut crate::fixture::FixtureHandle,
    next_poll: u64,
    next_renew: u64,
    renewal: Option<RenewRequest>,
    kill_sent: bool,
}
impl Control<'_> {
    async fn run(&mut self) -> Result<()> {
        while self.handle.try_wait()?.is_none() {
            self.tick().await?;
        }
        Ok(())
    }
    async fn tick(&mut self) -> Result<()> {
        let now = self.runtime.clock.now_ms();
        self.runtime.expire()?;
        let record = &self.runtime.journal.state().attempts[&self.attempt_id];
        if record.phase == Phase::Stopping {
            if !self.kill_sent {
                self.handle
                    .stop(record.stop_reason.unwrap_or(ProcessOutcome::Cancelled))?;
                self.kill_sent = true;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
            return Ok(());
        }
        let deadline = record.grant_deadline_ms.context("missing grant deadline")?;
        let timeout = Duration::from_millis(deadline.saturating_sub(now).clamp(1, 500));
        if now >= self.next_poll {
            self.poll(timeout).await?;
            self.next_poll = now.saturating_add(100);
        }
        if self.runtime.journal.state().attempts[&self.attempt_id].phase == Phase::Stopping {
            return Ok(());
        }
        let now = self.runtime.clock.now_ms();
        if now >= self.next_renew && !self.renew(now).await? {
            return Ok(());
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
        Ok(())
    }
    async fn poll(&mut self, timeout: Duration) -> Result<()> {
        let request =
            availability_poll(self.runtime.journal.state().identity.clone(), self.runtime)?;
        match tokio::time::timeout(timeout, self.client.poll(&request)).await {
            Ok(Ok(response)) => {
                self.apply_directives(response.directives).await?;
            }
            Ok(Err(error)) if !retryable(&error) => {
                self.runtime
                    .request_stop(self.attempt_id, ProcessOutcome::Expired)?;
            }
            _ => {}
        }
        Ok(())
    }
    async fn apply_directives(&mut self, directives: Vec<Directive>) -> Result<()> {
        for directive in directives {
            let ack = self.runtime.receive_directive(&directive)?;
            if directive.attempt.attempt_id == self.attempt_id && !self.kill_sent {
                self.handle.kill()?;
                self.kill_sent = true;
            }
            // Receipt is durable; killing the owned writer precedes
            // waiting for any network acknowledgement.
            let remaining = self.runtime.journal.state().attempts[&self.attempt_id]
                .grant_deadline_ms
                .unwrap()
                .saturating_sub(self.runtime.clock.now_ms());
            if remaining > 0 {
                let _ = tokio::time::timeout(
                    Duration::from_millis(remaining.min(100)),
                    self.client.directive_ack(&ack),
                )
                .await;
            }
        }
        Ok(())
    }
    async fn renew(&mut self, now: u64) -> Result<bool> {
        let record = &self.runtime.journal.state().attempts[&self.attempt_id];
        let request = self.renewal.get_or_insert_with(|| RenewRequest {
            identity: self.runtime.journal.state().identity.clone(),
            operation_id: Uuid::new_v4(),
            attempt: record.offer.attempt.clone(),
            grant_sequence: record.grant.as_ref().unwrap().grant_sequence,
        });
        let sent = self.runtime.clock.now_ms();
        let remaining = record.grant_deadline_ms.unwrap().saturating_sub(sent);
        if remaining == 0 {
            return Ok(false);
        }
        match tokio::time::timeout(
            Duration::from_millis(remaining.min(500)),
            self.client.renew(request),
        )
        .await
        {
            Ok(Ok(response)) => {
                self.runtime.renew(response.grant, sent)?;
                let record = &self.runtime.journal.state().attempts[&self.attempt_id];
                self.handle.renew(
                    record.grant.as_ref().unwrap().grant_sequence,
                    record.grant_deadline_ms.unwrap(),
                )?;
                self.renewal = None;
                let record = &self.runtime.journal.state().attempts[&self.attempt_id];
                let now = self.runtime.clock.now_ms();
                self.next_renew =
                    now.saturating_add(record.grant_deadline_ms.unwrap().saturating_sub(now) / 3);
            }
            Ok(Err(error)) if !retryable(&error) => {
                self.runtime
                    .request_stop(self.attempt_id, ProcessOutcome::Expired)?;
            }
            _ => {
                self.next_renew = now.saturating_add(100);
            }
        }
        Ok(true)
    }
}
