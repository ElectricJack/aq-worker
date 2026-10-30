use aq_worker_protocol::*;
#[test]
fn generated_schema_has_no_drift() {
    assert_eq!(
        schema(),
        include_str!("../fixtures/execution-v2.schema.json")
    );
}
#[test]
fn wire_is_strict_and_payload_digest_is_canonical() {
    let a = serde_json::json!({"a":1,"b":2});
    let b = serde_json::json!({"b":2,"a":1});
    assert_eq!(payload_digest(&a), payload_digest(&b));
    assert!(serde_json::from_value::<Ack>(serde_json::json!({"operation_id":"00000000-0000-0000-0000-000000000001","surprise":true})).is_err());
    assert!(
        Ack {
            operation_id: uuid::Uuid::nil()
        }
        .validate()
        .is_err()
    );
}
#[cfg(feature = "test-support")]
mod scenarios {
    use super::*;
    use aq_worker_protocol::testing::*;
    #[test]
    fn lost_acceptance_replay_reduces_ttl_and_changed_bytes_conflict() {
        let offer = sample_offer();
        let req = sample_acceptance(&offer);
        let mut peer = FakeCoordinator::new(offer, 100);
        let first = peer.accept(&req).unwrap();
        peer.now_ms += 250;
        let replay = peer.accept(&req).unwrap();
        assert_eq!(replay.grant.grant_sequence, first.grant.grant_sequence);
        assert_eq!(replay.grant.valid_for_ms, 9750);
        let mut changed = req.clone();
        changed.reservation_deadline_ms += 1;
        assert_eq!(
            peer.accept(&changed).unwrap_err().code,
            ErrorCode::IdConflict
        );
        peer.now_ms = 10_100;
        assert_eq!(peer.accept(&req).unwrap_err().code, ErrorCode::GrantExpired);
    }
    #[test]
    fn expiry_stale_incarnation_and_generation_are_refused() {
        let offer = sample_offer();
        let req = sample_acceptance(&offer);
        let mut peer = FakeCoordinator::new(offer, 100_000);
        assert_eq!(peer.accept(&req).unwrap_err().code, ErrorCode::OfferExpired);
        let mut changed = req.clone();
        changed.identity.boot_id = uuid::Uuid::from_u128(99);
        assert_eq!(
            peer.accept(&changed).unwrap_err().code,
            ErrorCode::StaleBoot
        );
        changed = req;
        changed.attempt.generation += 1;
        assert_eq!(
            peer.accept(&changed).unwrap_err().code,
            ErrorCode::StaleGeneration
        );
    }
    #[test]
    fn observation_sequences_and_report_replays_are_immutable() {
        let offer = sample_offer();
        let mut peer = FakeCoordinator::new(offer.clone(), 100);
        let mut obs = Observation {
            identity: offer.identity.clone(),
            operation_id: uuid::Uuid::from_u128(10),
            attempt: offer.attempt.clone(),
            sequence: 1,
            phase: Phase::Running,
            stop_certainty: StopCertainty::Running,
            detail: String::new(),
        };
        let ack = peer.observe(&obs).unwrap();
        assert_eq!(ack, peer.observe(&obs).unwrap());
        obs.phase = Phase::Stopping;
        assert_eq!(peer.observe(&obs).unwrap_err().code, ErrorCode::IdConflict);
        let mut report = sample_report(&offer);
        let result = peer.report(&report).unwrap();
        assert_eq!(result, peer.report(&report).unwrap());
        report.output.push('!');
        assert_eq!(
            peer.report(&report).unwrap_err().code,
            ErrorCode::IdConflict
        );
    }
    #[test]
    fn cancellation_race_and_late_generation_retain_evidence() {
        let offer = sample_offer();
        let report = sample_report(&offer);
        let mut peer = FakeCoordinator::new(offer.clone(), 100);
        peer.cancelled = true;
        assert_eq!(
            peer.report(&report).unwrap().disposition,
            ResultDisposition::RetainedCancelled
        );
        let mut peer = FakeCoordinator::new(offer.clone(), 100);
        let first = peer.report(&report).unwrap();
        peer.cancelled = true;
        assert_eq!(peer.report(&report).unwrap(), first);
        let mut late = report;
        late.report_id = uuid::Uuid::from_u128(11);
        late.attempt.generation += 1;
        assert_eq!(
            peer.report(&late).unwrap().disposition,
            ResultDisposition::RetainedStale
        );
    }
    #[test]
    fn spec_tampering_and_response_overflow_are_invalid() {
        let mut offer = sample_offer();
        offer.validate().unwrap();
        offer.spec.fixture_output.push('!');
        assert!(offer.validate().is_err());
        assert!(
            PollResponse {
                server_time_ms: 100,
                offers: vec![sample_offer(); MAX_BATCH + 1],
                directives: vec![]
            }
            .validate()
            .is_err()
        );
        assert!(
            ReconcileRequest {
                identity: sample_offer().identity,
                operation_id: uuid::Uuid::from_u128(12),
                attempts: vec![],
                cursor: Some("x".repeat(257)),
                max_items: 1
            }
            .validate()
            .is_err()
        );
    }
}
