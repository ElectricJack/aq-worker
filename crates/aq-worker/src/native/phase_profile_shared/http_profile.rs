macro_rules! http_profile {
    ($wire:path, $reply:ident, $propose:ident, $accept:ident, $prepare:ident) => {
// Opt-in phase transport. Actual network observations, never journal or host authority.
// First Accept must be sent within the original locally observed offer window.
// Its separate preparation lease is not capped by that old window. Exact retries
// require a still-live opaque observation and can only shorten its local cap.
use super::*;
use $wire as phase;
use aq_admission::{Digest,startup::NativeOffer,transport::WorkerIdentity};
use phase::{AcceptRequest,AcceptedDelivery,DeadlineSample,Document,Negotiation,Proposal};
pub use execution::ExecutionObservation;

pub struct ProposedPreparation {
    proposal: Proposal,
    identity: WorkerIdentity,
    original_deadline_ns: u64,
}
impl ProposedPreparation {
    pub fn proposal(&self) -> &Proposal {
        &self.proposal
    }
    pub fn original_deadline_ns(&self) -> u64 {
        self.original_deadline_ns
    }
}

/// No Deserialize/Serialize or conversion to the legacy Preparation type.
pub struct AcceptedPreparation {
    delivery: AcceptedDelivery,
    request_digest: Digest,
    proposal_digest: Digest,
    sample: DeadlineSample,
    sent_ns: u64,
    received_ns: u64,
    first: Option<Arc<FirstAcceptedTransmission>>,
}
struct FirstAcceptedTransmission {
    delivery: AcceptedDelivery,
    sent_ns: u64,
    received_ns: u64,
    deadline_ns: u64,
}
impl AcceptedPreparation {
    pub fn delivery(&self) -> &AcceptedDelivery {
        &self.delivery
    }
    pub fn request_digest(&self) -> &Digest {
        &self.request_digest
    }
    pub fn proposal_digest(&self) -> &Digest {
        &self.proposal_digest
    }
    pub fn deadline_ns(&self) -> u64 {
        self.sample.deadline_ns()
    }
    pub fn sent_ns(&self) -> u64 {
        self.sent_ns
    }
    pub fn received_ns(&self) -> u64 {
        self.received_ns
    }
    pub(crate) fn first_transmission(&self) -> (&AcceptedDelivery, u64, u64, u64) {
        self.first.as_ref().map_or(
            (
                &self.delivery,
                self.sent_ns,
                self.received_ns,
                self.deadline_ns(),
            ),
            |first| {
                (
                    &first.delivery,
                    first.sent_ns,
                    first.received_ns,
                    first.deadline_ns,
                )
            },
        )
    }
}

#[derive(Serialize)]
struct ProposalRequest<'a> {
    identity: &'a WorkerIdentity,
    binding: &'a LaunchBinding,
    negotiation: &'a Negotiation,
}
fn live(deadline: u64) -> Result<()> {
    ensure!(boot_ns()? < deadline, "phase local observation expired");
    Ok(())
}
fn same(a: &impl Serialize, b: &impl Serialize) -> Result<bool> {
    Ok(serde_json::to_value(a)? == serde_json::to_value(b)?)
}
/// Poll may carry a later delivery sample for the same immutable publication.
/// Compare those two actual records without changing either stored clock or the
/// driver's previously observed local deadline. Only this sample may differ.
pub(crate) fn validate_original_delivery(
    original: &NativeOffer,
    delivered: &NativeOffer,
) -> Result<()> {
    original.validate()?;
    delivered.validate()?;
    ensure!(
        original.offered.clock.expires_at_ms == delivered.offered.clock.expires_at_ms
            && original.offered.clock.server_time_ms <= delivered.offered.clock.server_time_ms,
        "phase original/delivered offer clock association"
    );
    let mut comparable = delivered.clone();
    comparable.offered.clock = original.offered.clock.clone();
    ensure!(
        same(original, &comparable)?,
        "phase original/delivered offer content changed"
    );
    Ok(())
}
struct Received {
    document: Document,
    nonce: Uuid,
    sent_ns: u64,
    received_ns: u64,
}
impl NativeClient {
    async fn $reply(
        &self,
        path: &str,
        request: &impl Serialize,
        send_by_ns: u64,
    ) -> Result<Received> {
        let bytes = serde_json::to_vec(request)?;
        ensure!(bytes.len() <= phase::MAX_BYTES, "phase request bound");
        let _slot = self.control_slots.try_acquire()?;
        let nonce = Uuid::new_v4();
        let sent_ns = boot_ns()?;
        ensure!(sent_ns < send_by_ns, "phase send window expired");
        let response = self
            .control
            .post(format!("{}/v1/worker/profiled-v2/{path}", self.base))
            .header(header::AUTHORIZATION, self.credential.clone())
            .header(header::CONTENT_TYPE, "application/json")
            .header("aq-delivery-nonce", nonce.to_string())
            .body(bytes)
            .send()
            .await
            .map_err(|_| anyhow::anyhow!("phase transport failed; disposition unknown"))?;
        if !response.status().is_success() { return Err(refused(response).await); }
        let bytes = body(response, phase::MAX_BYTES).await?;
        let received_ns = boot_ns()?;
        let document = phase::decode(&bytes)?;
        Ok(Received {
            document,
            nonce,
            sent_ns,
            received_ns,
        })
    }

    /// The original deadline must come from the driver's actual Poll/reservation
    /// owner. A proposal response contains historical clocks, not a new delivery
    /// window, so this method never samples them against a fresh client time.
    pub async fn $propose(
        &self,
        identity: &WorkerIdentity,
        original: &NativeOffer,
        negotiation: &Negotiation,
        original_deadline_ns: u64,
    ) -> Result<ProposedPreparation> {
        original.validate()?;
        identity.validate()?;
        negotiation.validate()?;
        self.scope(&original.offered.offer.binding)?;
        ensure!(
            identity.node_id == self.node && !identity.boot_id.is_nil(),
            "phase node/boot scope"
        );
        ensure!(
            matches!(&original.offered.offer.binding.session,
            SessionIdentity::Remote { node_id, boot_id, .. }
                if *node_id == identity.node_id && *boot_id == identity.boot_id),
            "phase offer boot scope"
        );
        live(original_deadline_ns)?;
        let request = ProposalRequest {
            identity,
            binding: &original.offered.offer.binding,
            negotiation,
        };
        let got = self
            .$reply("proposal", &request, original_deadline_ns)
            .await?;
        let Document::Proposal(proposal) = got.document else {
            anyhow::bail!("unexpected phase proposal response");
        };
        proposal.validate()?;
        validate_original_delivery(&proposal.original, original)?;
        ensure!(
            same(&proposal.negotiation, negotiation)?,
            "phase proposal changed negotiation"
        );
        live(original_deadline_ns)?;
        Ok(ProposedPreparation {
            proposal: *proposal,
            identity: identity.clone(),
            original_deadline_ns,
        })
    }

    /// The caller must retain its exact reservation and fsync Accept intent before
    /// sending. This transport does not persist it or infer receipt after timeout.
    pub async fn $accept(
        &self,
        proposed: &ProposedPreparation,
        request: &AcceptRequest,
        previous: Option<&AcceptedPreparation>,
    ) -> Result<AcceptedPreparation> {
        request.validate_for(&proposed.proposal)?;
        self.scope(&request.original.acceptance.binding)?;
        ensure!(
            request.original.identity == proposed.identity
                && proposed.identity.node_id == self.node,
            "phase accepted identity"
        );
        let request_digest = request.digest()?;
        let proposal_digest = proposed.proposal.digest()?;
        if let Some(old) = previous {
            ensure!(
                old.request_digest == request_digest && old.proposal_digest == proposal_digest,
                "phase replay operation changed"
            );
            old.delivery
                .validate_for(request, &proposed.proposal, old.delivery.delivery.nonce)?;
            live(old.deadline_ns())?;
        } else {
            live(proposed.original_deadline_ns)?;
        }
        let send_by_ns = previous.map_or(proposed.original_deadline_ns, |v| v.deadline_ns());
        let got = self
            .$reply(
                "accept",
                &Document::Accept(Box::new(request.clone())),
                send_by_ns,
            )
            .await?;
        let Document::AcceptedDelivery(delivery) = got.document else {
            anyhow::bail!("unexpected phase accepted response");
        };
        let sample = delivery.sample(
            request,
            &proposed.proposal,
            got.nonce,
            got.sent_ns,
            got.received_ns,
            previous.map(|v| &v.sample),
        )?;
        live(sample.deadline_ns())?;
        let first = previous.map(|old| {
            old.first.clone().unwrap_or_else(|| {
                Arc::new(FirstAcceptedTransmission {
                    delivery: old.delivery.clone(),
                    sent_ns: old.sent_ns,
                    received_ns: old.received_ns,
                    deadline_ns: old.deadline_ns(),
                })
            })
        });
        Ok(AcceptedPreparation {
            delivery: *delivery,
            request_digest,
            proposal_digest,
            sample,
            sent_ns: got.sent_ns,
            received_ns: got.received_ns,
            first,
        })
    }
}

mod execution {
// An actual execution reply is an observation, not a durable host permit.
use super::*;
use super::phase::{ExecutionDelivery, PrepareRequest};

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
    pub async fn $prepare(
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
            .$reply(
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

}

    };
}
pub(crate) use http_profile;
