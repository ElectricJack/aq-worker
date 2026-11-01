//! An actual execution reply is an observation, not a durable host permit.
use super::*;
use aq_admission::preparation_v2::{ExecutionDelivery, PrepareRequest};

/// No Serde constructor or conversion to a legacy Grant/host permission.
/// The driver must retain actual PreparedPhaseInputs and durably record this
/// observation before any separately qualified execution owner can consume it.
pub struct ExecutionObservation {
    delivery: ExecutionDelivery,
    request_digest: Digest,
    accepted_digest: Digest,
    sample: DeadlineSample,
    sent_ns: u64,
    received_ns: u64,
    first_send_deadline_ns: u64,
    first: Option<Arc<FirstExecutionTransmission>>,
}
struct FirstExecutionTransmission {
    delivery: ExecutionDelivery,
    sent_ns: u64,
    received_ns: u64,
    deadline_ns: u64,
    hard_deadline_ns: u64,
}
impl ExecutionObservation {
    pub fn delivery(&self) -> &ExecutionDelivery {
        &self.delivery
    }
    pub fn request_digest(&self) -> &Digest {
        &self.request_digest
    }
    pub fn accepted_digest(&self) -> &Digest {
        &self.accepted_digest
    }
    pub fn deadline_ns(&self) -> u64 {
        self.sample.deadline_ns()
    }
    pub fn hard_deadline_ns(&self) -> u64 {
        self.sample.hard_deadline_ns()
    }
    pub fn sent_ns(&self) -> u64 {
        self.sent_ns
    }
    pub fn received_ns(&self) -> u64 {
        self.received_ns
    }
    pub fn first_send_deadline_ns(&self) -> u64 {
        self.first_send_deadline_ns
    }
    /// Exact first network observation, including its original local clocks.
    /// Retrying before durable recording must not replace the first send time.
    pub fn first_transmission(&self) -> (&ExecutionDelivery, u64, u64, u64, u64) {
        self.first.as_ref().map_or(
            (
                &self.delivery,
                self.sent_ns,
                self.received_ns,
                self.deadline_ns(),
                self.hard_deadline_ns(),
            ),
            |first| {
                (
                    &first.delivery,
                    first.sent_ns,
                    first.received_ns,
                    first.deadline_ns,
                    first.hard_deadline_ns,
                )
            },
        )
    }
}
impl NativeClient {
    /// First send requires the actual still-live Accepted observation. A retry
    /// after preparation expiry requires the original opaque execution reply;
    /// absence/timeout is unknown and never authorizes a late first send.
    pub async fn prepare_execution(
        &self,
        proposed: &ProposedPreparation,
        acceptance: &AcceptRequest,
        accepted: &AcceptedPreparation,
        request: &PrepareRequest,
        previous: Option<&ExecutionObservation>,
    ) -> Result<ExecutionObservation> {
        self.scope(&request.original.binding)?;
        ensure!(
            acceptance.original.identity == proposed.identity
                && proposed.identity.node_id == self.node
                && accepted.request_digest == acceptance.digest()?
                && accepted.proposal_digest == proposed.proposal.digest()?,
            "phase execution original owner association"
        );
        accepted.delivery.validate_for(
            acceptance,
            &proposed.proposal,
            accepted.delivery.delivery.nonce,
        )?;
        request.validate_for(&accepted.delivery.receipt, acceptance, &proposed.proposal)?;
        let request_digest = request.digest()?;
        let accepted_digest = accepted.delivery.receipt.digest()?;
        let send_by = if let Some(old) = previous {
            ensure!(
                old.request_digest == request_digest && old.accepted_digest == accepted_digest,
                "phase execution replay changed operation"
            );
            old.delivery.validate_for(
                request,
                &accepted.delivery.receipt,
                acceptance,
                &proposed.proposal,
                old.delivery.delivery.nonce,
            )?;
            old.deadline_ns()
        } else {
            accepted.deadline_ns()
        };
        live(send_by)?;
        let got = self
            .phase_reply(
                "prepare",
                &Document::Prepare(Box::new(request.clone())),
                send_by,
            )
            .await?;
        let Document::ExecutionDelivery(delivery) = got.document else {
            anyhow::bail!("unexpected phase execution response");
        };
        let sample = delivery.sample(
            request,
            &accepted.delivery.receipt,
            acceptance,
            &proposed.proposal,
            got.nonce,
            got.sent_ns,
            got.received_ns,
            previous.map(|p| &p.sample),
        )?;
        live(sample.deadline_ns())?;
        let first = previous.map(|old| {
            old.first.clone().unwrap_or_else(|| {
                Arc::new(FirstExecutionTransmission {
                    delivery: old.delivery.clone(),
                    sent_ns: old.sent_ns,
                    received_ns: old.received_ns,
                    deadline_ns: old.deadline_ns(),
                    hard_deadline_ns: old.hard_deadline_ns(),
                })
            })
        });
        Ok(ExecutionObservation {
            delivery: *delivery,
            request_digest,
            accepted_digest,
            sample,
            sent_ns: got.sent_ns,
            received_ns: got.received_ns,
            first_send_deadline_ns: previous.map_or(send_by, |p| p.first_send_deadline_ns),
            first,
        })
    }
}
