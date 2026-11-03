//! B6a1 integration with a permit-only client. B2's unpublished receipt codecs
//! are deliberately injected; this does not define a new production wire schema.
use super::{
    auth::{RunnerHttp, RunnerRoute},
    *,
};
use anyhow::{Result, ensure};
use aq_admission_sandbox::sandbox::RunnerRenewalRequest;

/// Trusted receipt adapter. Implementations use the supplied RunnerHttp for all
/// AQ routes and must durably retain actual bytes before returning receipts.
/// No node credential or WorkerConfig is supplied to this interface.
pub trait RunnerExchange: Send {
    fn preflight(&mut self, http: &RunnerHttp) -> Result<()>;
    fn renew<'a>(
        &'a mut self,
        http: &'a RunnerHttp,
        request: &'a RunnerRenewalRequest,
        nonce: Uuid,
    ) -> TransportFuture<'a, RenewalOutcome>;
    fn retain<'a>(
        &'a mut self,
        http: &'a RunnerHttp,
        stopped: &'a StoppedAttempt,
    ) -> TransportFuture<'a, RetentionReceipts>;
    fn finished<'a>(
        &'a mut self,
        http: &'a RunnerHttp,
        retained: &'a RetainedAttempt,
    ) -> TransportFuture<'a, FinishedReceipt>;
}
pub struct PermitTransport<E> {
    http: RunnerHttp,
    exchange: E,
}
impl<E: RunnerExchange> PermitTransport<E> {
    pub fn new(http: RunnerHttp, exchange: E) -> Self {
        Self { http, exchange }
    }
}
impl<E: RunnerExchange> RunnerTransport for PermitTransport<E> {
    fn authority_client(&self) -> Option<RunnerHttp> {
        Some(self.http.clone())
    }
    fn preflight(&mut self, worker: &WorkerConfig) -> Result<()> {
        ensure!(
            worker.node_id == self.http.binding().node.node_id,
            "runner node mismatch"
        );
        self.http
            .check_scope(RunnerRoute::Started, &self.http.binding())?;
        self.exchange.preflight(&self.http)
    }
    fn renew<'a>(
        &'a mut self,
        _: &'a WorkerConfig,
        request: &'a RunnerRenewalRequest,
        nonce: Uuid,
    ) -> TransportFuture<'a, RenewalOutcome> {
        Box::pin(async move {
            if self.http.check_renewal_request(request).is_err() {
                self.http.lose_authority();
                return Ok(RenewalOutcome::Refused);
            }
            let sent = boot_ns()?;
            let result = self.exchange.renew(&self.http, request, nonce).await;
            let received = boot_ns()?;
            if self.http.authority_lost() {
                return Ok(RenewalOutcome::Refused);
            }
            match result {
                Ok(RenewalOutcome::Delivered(delivery)) => {
                    if self
                        .http
                        .accept_renewal(request, &delivery, nonce, sent, received)
                        .is_err()
                    {
                        self.http.lose_authority();
                        Ok(RenewalOutcome::Refused)
                    } else {
                        Ok(RenewalOutcome::Delivered(delivery))
                    }
                }
                Ok(RenewalOutcome::Refused) => {
                    self.http.lose_authority();
                    Ok(RenewalOutcome::Refused)
                }
                Ok(RenewalOutcome::Unavailable) | Err(_) => Ok(RenewalOutcome::Unavailable),
            }
        })
    }
    fn retain<'a>(
        &'a mut self,
        _: &'a WorkerConfig,
        stopped: &'a StoppedAttempt,
    ) -> TransportFuture<'a, RetentionReceipts> {
        Box::pin(async move {
            ensure!(
                stopped.key().attempt_id == self.http.binding().attempt_id,
                "runner stopped attempt mismatch"
            );
            self.http
                .check_scope(RunnerRoute::Stopped, &self.http.binding())?;
            self.exchange.retain(&self.http, stopped).await
        })
    }
    fn finished<'a>(
        &'a mut self,
        _: &'a WorkerConfig,
        retained: &'a RetainedAttempt,
    ) -> TransportFuture<'a, FinishedReceipt> {
        Box::pin(async move {
            ensure!(
                retained.stopped().key().attempt_id == self.http.binding().attempt_id,
                "runner result attempt mismatch"
            );
            self.http
                .check_scope(RunnerRoute::Results, &self.http.binding())?;
            self.exchange.finished(&self.http, retained).await
        })
    }
}
