//! Trusted authenticated transport seam. B2 owns endpoint/authentication semantics;
//! this module owns correlation and client deadline validation, not server truth.
use super::{RetainedAttempt, StoppedAttempt};
use crate::config::WorkerConfig;
use anyhow::Result;
use aq_admission_sandbox::sandbox::{PermitContext, RunnerRenewalRequest, RunnerRenewalResponse};
use std::{future::Future, pin::Pin};
use uuid::Uuid;
pub type TransportFuture<'a, T> = Pin<Box<dyn Future<Output = Result<T>> + Send + 'a>>;

pub struct RenewalDelivery {
    pub response: RunnerRenewalResponse,
    /// Authenticated current grant context before the renewal mutation.
    pub context: PermitContext,
    pub server_now_ms: u64,
}
pub enum RenewalOutcome {
    Delivered(Box<RenewalDelivery>),
    /// Explicit revocation/refusal stops immediately.
    Refused,
    /// A transient loss grants no time; retry the same operation until expiry.
    Unavailable,
}
/// Legacy fixture seam. PermitTransport supplies the dedicated permit client;
/// implementations must never use node credentials in a sandbox. They must
/// validate central receipts and retain immutable source/result bytes before
/// returning receipts. No implementation may treat locally generated IDs as
/// evidence of a server mutation. Fixture implementations are not HTTP wiring.
pub trait RunnerTransport {
    /// Snapshot before a mutable exchange future, so every channel can revoke
    /// authority while renewal is pending. Existing fixture transports opt out.
    fn authority_client(&self) -> Option<super::auth::RunnerHttp> {
        None
    }
    fn preflight(&mut self, worker: &WorkerConfig) -> Result<()>;
    fn renew<'a>(
        &'a mut self,
        worker: &'a WorkerConfig,
        request: &'a RunnerRenewalRequest,
        nonce: Uuid,
    ) -> TransportFuture<'a, RenewalOutcome>;
    fn retain<'a>(
        &'a mut self,
        worker: &'a WorkerConfig,
        stopped: &'a StoppedAttempt,
    ) -> TransportFuture<'a, super::RetentionReceipts>;
    fn finished<'a>(
        &'a mut self,
        worker: &'a WorkerConfig,
        retained: &'a RetainedAttempt,
    ) -> TransportFuture<'a, super::FinishedReceipt>;
}

pub(super) fn validate_renewal(
    delivery: &RenewalDelivery,
    request: &RunnerRenewalRequest,
    nonce: Uuid,
    sent: u64,
    received: u64,
    prior: u64,
    hard: u64,
) -> Result<u64> {
    // Keep B1's evolving validation API behind this single boundary.
    delivery
        .response
        .validate_for(request, &delivery.context, nonce, delivery.server_now_ms)?;
    Ok(delivery
        .response
        .client_deadline_ns(sent, received, prior, hard, request.revision)?)
}
