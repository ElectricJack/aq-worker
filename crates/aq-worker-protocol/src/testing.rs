//! In-memory conformance peer for independent endpoint tests. Never a durable
//! authority implementation or production coordinator.
use crate::*;
use std::collections::BTreeMap;

#[derive(Clone)]
pub struct FakeCoordinator {
    pub now_ms: u64,
    pub offer: Offer,
    pub cancelled: bool,
    pub active_identity: NodeIdentity,
    latest_report_id: Option<Uuid>,
    grant: Option<(Grant, u64)>,
    acceptances: BTreeMap<Uuid, (String, Grant)>,
    observations: BTreeMap<u64, (String, Ack)>,
    reports: BTreeMap<Uuid, (String, ResultResponse)>,
}
impl FakeCoordinator {
    pub fn new(offer: Offer, now_ms: u64) -> Self {
        Self {
            now_ms,
            active_identity: offer.identity.clone(),
            latest_report_id: None,
            offer,
            cancelled: false,
            grant: None,
            acceptances: BTreeMap::new(),
            observations: BTreeMap::new(),
            reports: BTreeMap::new(),
        }
    }
    fn error(code: ErrorCode) -> ProtocolError {
        ProtocolError {
            code,
            message: "conformance refusal".into(),
            retryable: false,
            operation_id: None,
        }
    }
    pub fn accept(&mut self, r: &AcceptRequest) -> Result<AcceptResponse, ProtocolError> {
        r.validate()
            .map_err(|_| Self::error(ErrorCode::InvalidRequest))?;
        let digest = payload_digest(r);
        if let Some((old, grant)) = self.acceptances.get(&r.operation_id) {
            if *old != digest {
                return Err(Self::error(ErrorCode::IdConflict));
            }
            if self.cancelled {
                return Err(Self::error(ErrorCode::Cancelled));
            }
            let expiry = self.grant.as_ref().unwrap().1;
            if self.now_ms >= expiry {
                return Err(Self::error(ErrorCode::GrantExpired));
            }
            let mut grant = grant.clone();
            grant.valid_for_ms = expiry - self.now_ms;
            return Ok(AcceptResponse { grant });
        }
        if r.identity != self.offer.identity {
            return Err(Self::error(ErrorCode::StaleBoot));
        }
        if r.attempt != self.offer.attempt {
            return Err(Self::error(ErrorCode::StaleGeneration));
        }
        if self.cancelled {
            return Err(Self::error(ErrorCode::Cancelled));
        }
        if self.now_ms >= self.offer.offer_expires_at_ms {
            return Err(Self::error(ErrorCode::OfferExpired));
        }
        if r.offer_id != self.offer.offer_id
            || r.resolved_spec_digest != self.offer.spec.resolved_spec_digest
        {
            return Err(Self::error(ErrorCode::InvalidRequest));
        }
        if self.grant.is_some() {
            return Err(Self::error(ErrorCode::IdConflict));
        }
        let grant = Grant {
            identity: r.identity.clone(),
            attempt: r.attempt.clone(),
            offer_id: r.offer_id,
            reservation_id: r.reservation_id,
            resolved_spec_digest: r.resolved_spec_digest.clone(),
            grant_sequence: 1,
            valid_for_ms: r.reservation_remaining_ms.min(MAX_GRANT_MS),
            reservation_deadline_ms: r.reservation_deadline_ms,
        };
        self.acceptances
            .insert(r.operation_id, (digest, grant.clone()));
        self.grant = Some((grant.clone(), self.now_ms + grant.valid_for_ms));
        Ok(AcceptResponse { grant })
    }
    pub fn observe(&mut self, r: &Observation) -> Result<Ack, ProtocolError> {
        r.validate()
            .map_err(|_| Self::error(ErrorCode::InvalidRequest))?;
        if r.identity != self.offer.identity {
            return Err(Self::error(ErrorCode::StaleBoot));
        }
        if r.attempt != self.offer.attempt {
            return Err(Self::error(ErrorCode::StaleGeneration));
        }
        let digest = payload_digest(r);
        if let Some((old, ack)) = self.observations.get(&r.sequence) {
            return if *old == digest {
                Ok(ack.clone())
            } else {
                Err(Self::error(ErrorCode::IdConflict))
            };
        }
        if self
            .observations
            .keys()
            .last()
            .is_some_and(|n| *n >= r.sequence)
        {
            return Err(Self::error(ErrorCode::InvalidRequest));
        }
        let ack = Ack {
            operation_id: r.operation_id,
        };
        self.observations.insert(r.sequence, (digest, ack.clone()));
        Ok(ack)
    }
    pub fn report(&mut self, r: &ResultReport) -> Result<ResultResponse, ProtocolError> {
        r.validate()
            .map_err(|_| Self::error(ErrorCode::InvalidRequest))?;
        let digest = payload_digest(r);
        if let Some((old, response)) = self.reports.get(&r.report_id) {
            return if *old == digest {
                Ok(response.clone())
            } else {
                Err(Self::error(ErrorCode::IdConflict))
            };
        }
        if r.identity.node_id != self.offer.identity.node_id
            || r.attempt.attempt_id != self.offer.attempt.attempt_id
        {
            return Err(Self::error(ErrorCode::NotFound));
        }
        if !r.artifacts.is_empty() {
            return Err(Self::error(ErrorCode::UnsupportedCapability));
        }
        let (grant, expiry) = self
            .grant
            .as_ref()
            .ok_or_else(|| Self::error(ErrorCode::InvalidRequest))?;
        if r.identity != grant.identity
            || r.attempt != grant.attempt
            || r.resolved_spec_digest != grant.resolved_spec_digest
        {
            return Err(Self::error(ErrorCode::StaleGeneration));
        }
        if r.supersedes_report_id != self.latest_report_id {
            return Err(Self::error(ErrorCode::IdConflict));
        }
        let stopped = matches!(
            r.stop_certainty,
            StopCertainty::ConfirmedStopped | StopCertainty::NotStarted
        );
        let disposition = if !stopped {
            ResultDisposition::Rejected
        } else if self.cancelled {
            ResultDisposition::RetainedCancelled
        } else if self.active_identity != r.identity
            || self.now_ms >= *expiry
            || self.latest_report_id.is_some()
        {
            ResultDisposition::RetainedStale
        } else {
            ResultDisposition::Accepted
        };
        let response = ResultResponse {
            report_id: r.report_id,
            disposition,
            retention_release: stopped && r.pending_operation_ids.is_empty(),
        };
        self.latest_report_id = Some(r.report_id);
        self.reports.insert(r.report_id, (digest, response.clone()));
        Ok(response)
    }
}

pub fn sample_offer() -> Offer {
    Offer {
        offer_id: Uuid::from_u128(4),
        identity: NodeIdentity {
            node_id: Uuid::from_u128(1),
            boot_id: Uuid::from_u128(2),
        },
        attempt: AttemptRef {
            attempt_id: Uuid::from_u128(5),
            session_id: Uuid::from_u128(6),
            generation: 1,
        },
        spec: ExecutionSpec {
            bindings: None,
            project_id: Uuid::from_u128(3),
            harness: FAKE_HARNESS_CAPABILITY.into(),
            resolved_spec_digest: String::new(),
            fixture_output: "synthetic result\n".into(),
            resources: Resources {
                slots: 1,
                cpu_millis: 1000,
                memory_bytes: 1024,
                disk_bytes: 1024,
            },
            required_capabilities: vec![FAKE_HARNESS_CAPABILITY.into()],
        }
        .seal(),
        offer_expires_at_ms: 100_000,
        reservation_ttl_ms: 10_000,
    }
}
pub fn sample_acceptance(offer: &Offer) -> AcceptRequest {
    AcceptRequest {
        identity: offer.identity.clone(),
        operation_id: Uuid::from_u128(7),
        offer_id: offer.offer_id,
        attempt: offer.attempt.clone(),
        resolved_spec_digest: offer.spec.resolved_spec_digest.clone(),
        reservation_id: Uuid::from_u128(8),
        reservation_deadline_ms: 10_100,
        reservation_remaining_ms: 10_000,
    }
}
pub fn sample_report(offer: &Offer) -> ResultReport {
    ResultReport {
        identity: offer.identity.clone(),
        report_id: Uuid::from_u128(9),
        supersedes_report_id: None,
        attempt: offer.attempt.clone(),
        resolved_spec_digest: offer.spec.resolved_spec_digest.clone(),
        outcome: ProcessOutcome::Success,
        stop_certainty: StopCertainty::ConfirmedStopped,
        claimed_success: true,
        output: offer.spec.fixture_output.clone(),
        artifacts: vec![],
        pending_operation_ids: vec![],
    }
}
