use super::*;
use aq_admission_sandbox::sandbox::{RenewalReply, RunnerRenewalRequest};
pub(super) struct Renewal<'a> {
    pub(super) frozen: &'a FrozenLaunch,
    pub(super) authority: Option<&'a auth::RunnerHttp>,
    pub(super) handle: &'a aq_session_host::SessionHandle,
    pub(super) journal: &'a journal::Journal,
    pub(super) request: RunnerRenewalRequest,
    pub(super) deadline: u64,
    pub(super) hard: u64,
}
impl Renewal<'_> {
    fn check(&self, now: u64) -> Result<(), &'static str> {
        stop_reason(self.handle, now, self.deadline).map_or(Ok(()), Err)
    }
    fn authority_lost(&self) -> bool {
        self.authority.is_some_and(auth::RunnerHttp::authority_lost)
    }
    async fn receive(
        &self,
        mut pending: TransportFuture<'_, RenewalOutcome>,
        shutdown: &mut watch::Receiver<bool>,
    ) -> Result<anyhow::Result<RenewalOutcome>, &'static str> {
        loop {
            tokio::select! {
                biased;
                changed = shutdown.changed() => {
                    if changed.is_err() || *shutdown.borrow() {
                        return Err("cancelled");
                    }
                }
                _ = tokio::time::sleep(Duration::from_millis(25)) => {
                    if self.authority_lost() {
                        return Err("runner authority lost");
                    }
                    self.check(boot_ns().map_err(|_| "clock unavailable")?)?;
                }
                response = &mut pending => return Ok(response),
            }
        }
    }
    pub(super) async fn monitor<T: RunnerTransport>(
        &mut self,
        connection: (&WorkerConfig, &mut T),
        shutdown: &mut watch::Receiver<bool>,
        schedule: (u64, Duration),
    ) -> &'static str {
        let (worker, transport) = connection;
        let (now, interval) = schedule;
        let interval = interval.as_nanos() as u64;
        let mut next = boot_ns().unwrap_or(now).saturating_add(interval);
        loop {
            match self
                .turn((worker, transport), shutdown, (&mut next, interval))
                .await
            {
                Ok(()) => {}
                Err(reason) => return reason,
            }
        }
    }
    async fn turn<T: RunnerTransport>(
        &mut self,
        (worker, transport): (&WorkerConfig, &mut T),
        shutdown: &mut watch::Receiver<bool>,
        (next, interval): (&mut u64, u64),
    ) -> Result<(), &'static str> {
        if *shutdown.borrow() {
            return Err("cancelled");
        }
        if self.authority_lost() {
            return Err("runner authority lost");
        }
        let sent = boot_ns().map_err(|_| "clock unavailable")?;
        self.check(sent)?;
        if sent < *next {
            tokio::select! {
                changed = shutdown.changed() => {
                    if changed.is_err() || *shutdown.borrow() {
                        return Err("cancelled");
                    }
                }
                _ = tokio::time::sleep(Duration::from_millis(25)) => {}
            }
            return Ok(());
        }
        let nonce = Uuid::new_v4();
        let response = self
            .receive(transport.renew(worker, &self.request, nonce), shutdown)
            .await?;
        // Recheck CLOCK_BOOTTIME even if the network future wins after suspend.
        let received = boot_ns().map_err(|_| "clock unavailable")?;
        if self.authority_lost() {
            return Err("runner authority lost");
        }
        if received >= self.deadline {
            return Err("renewal deadline expired");
        }
        *next = received.saturating_add(interval);
        self.apply(response, nonce, (sent, received))
    }
    fn apply(
        &mut self,
        response: anyhow::Result<RenewalOutcome>,
        nonce: Uuid,
        clocks: (u64, u64),
    ) -> Result<(), &'static str> {
        match response {
            Ok(RenewalOutcome::Delivered(delivery)) => self.install(&delivery, nonce, clocks),
            Ok(RenewalOutcome::Unavailable) | Err(_) => Ok(()),
            Ok(RenewalOutcome::Refused) => Err("renewal refused"),
        }
    }
    fn install(
        &mut self,
        delivery: &RenewalDelivery,
        nonce: Uuid,
        (sent, received): (u64, u64),
    ) -> Result<(), &'static str> {
        let renewed = transport::validate_renewal(
            delivery,
            &self.request,
            nonce,
            sent,
            received,
            self.deadline,
            self.hard,
        )
        .map_err(|_| "invalid renewal")?;
        let renewed =
            renewed.min(received.saturating_add(aq_session_host::MAX_PERMIT_MS * 1_000_000));
        let permit = match &delivery.response.reply {
            RenewalReply::AwaitingNodeReadoption { .. } => return Ok(()),
            RenewalReply::Renewed { permit, .. } => permit,
        };
        let local = local_permit(self.frozen, delivery.response.revision, received, renewed)
            .map_err(|_| "invalid renewal deadline")?;
        // Only the local permit can enter evidence. The delegated permit carries a credential.
        self.journal
            .record(&format!("renewal-{}", delivery.response.revision), &local)
            .map_err(|_| "renewal persistence failed")?;
        if self.handle.renew(&local).is_err() {
            // A harness may finish while the guardian processes the renewal.
            if stop_reason(
                self.handle,
                boot_ns().unwrap_or(self.deadline),
                self.deadline,
            ) == Some("harness exited")
            {
                return Err("harness exited");
            }
            return Err("guardian renewal refused");
        }
        self.deadline = renewed;
        self.request.permit = permit.clone();
        self.request.revision = delivery.response.revision;
        self.request.operation_id = Uuid::new_v4();
        Ok(())
    }
}
