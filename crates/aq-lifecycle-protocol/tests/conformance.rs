#[allow(dead_code)]
#[path = "../examples/export.rs"]
mod exporter;
use aq_lifecycle_protocol::*;
use serde_json::{Value, json};
use uuid::Uuid;
fn fixture() -> Value {
    serde_json::from_str(include_str!("../artifacts/fixtures.json")).unwrap()
}
fn packet(index: usize) -> Packet {
    serde_json::from_value(fixture()["valid_packets"][index].clone()).unwrap()
}
fn caps() -> Vec<Capability> {
    serde_json::from_value(fixture()["advertised_capabilities"].clone()).unwrap()
}
fn predecessor() -> Binding {
    serde_json::from_value(fixture()["predecessor"].clone()).unwrap()
}
fn successor() -> Binding {
    serde_json::from_value(fixture()["successor"].clone()).unwrap()
}
fn reseal(old: &Packet, message: Message) -> Packet {
    Packet::seal(old.body.message_id, message).unwrap()
}
#[test]
fn public_artifacts_drift_and_all_packets_roundtrip() {
    assert_eq!(exporter::fixtures(), fixture());
    assert_eq!(
        format!(
            "{}\n",
            serde_json::to_string_pretty(&contract_schema()).unwrap()
        ),
        include_str!("../artifacts/contract.schema.json")
    );
    assert_eq!(
        format!(
            "{}\n",
            serde_json::to_string_pretty(&stop_proof_schema()).unwrap()
        ),
        include_str!("../artifacts/stop-proof.schema.json")
    );
    for raw in fixture()["valid_packets"].as_array().unwrap() {
        let encoded = serde_json::to_vec(raw).unwrap();
        let packet = decode(&encoded).unwrap();
        assert_eq!(serde_json::to_value(&packet).unwrap(), *raw);
        assert_eq!(
            canonical_bytes(&packet).unwrap(),
            canonical_bytes(&decode(&canonical_bytes(&packet).unwrap()).unwrap()).unwrap()
        );
    }
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
    if let Some(repo) = root.parent().and_then(std::path::Path::parent)
        && repo.join("crates/aq-lifecycle-protocol") == root
    {
        assert_eq!(
            std::fs::read(repo.join("protocol/lifecycle-v1/contract.schema.json")).unwrap(),
            include_bytes!("../artifacts/contract.schema.json")
        );
        assert_eq!(
            std::fs::read(repo.join("protocol/lifecycle-v1/stop-proof.schema.json")).unwrap(),
            include_bytes!("../artifacts/stop-proof.schema.json")
        );
        assert_eq!(
            std::fs::read(repo.join("protocol/lifecycle-v1/fixtures.json")).unwrap(),
            include_bytes!("../artifacts/fixtures.json")
        );
    }
}
#[test]
fn schema_publishes_finite_batch_and_reference_bounds() {
    let schema = contract_schema();
    assert_eq!(
        schema["$defs"]["ObservationBatch"]["properties"]["events"]["maxItems"],
        MAX_EVENTS
    );
    assert_eq!(
        schema["$defs"]["PacketBody"]["properties"]["required_capabilities"]["maxItems"],
        8
    );
    assert_eq!(
        schema["$defs"]["ParkingReport"]["properties"]["central_receipts"]["maxItems"],
        MAX_REFS
    );
}
#[test]
fn packet_and_source_event_digest_conflicts_are_not_accepted_as_retries() {
    let batch = packet(0);
    let mut peer = ConformancePeer::new(predecessor(), caps()).unwrap();
    let receipt = peer.receive(&batch).unwrap();
    assert_eq!(receipt.through_sequence, Some(2));
    assert_eq!(peer.receive(&batch).unwrap(), receipt);
    let mut changed = batch.body.message.clone();
    if let Message::Batch(value) = &mut changed {
        value.events[0].body.observed_at_ms += 1;
        value.events[0] =
            ObservationRecord::seal(value.events[0].event_id, value.events[0].body.clone())
                .unwrap();
    }
    let changed = reseal(&batch, changed);
    assert_eq!(peer.receive(&changed), Err(ContractError::Conflict));
    assert_eq!(peer.last_sequence(), 2);
    let mut next = match batch.body.message {
        Message::Batch(v) => v,
        _ => unreachable!(),
    };
    next.events.truncate(1);
    next.first_sequence = 3;
    next.last_sequence = 3;
    next.events[0].body.sequence = 3;
    next.events[0] =
        ObservationRecord::seal(next.events[0].event_id, next.events[0].body.clone()).unwrap();
    let next = Packet::seal(Uuid::from_u128(999), Message::Batch(next)).unwrap();
    assert_eq!(peer.receive(&next), Err(ContractError::Conflict));
    assert_eq!(peer.last_sequence(), 2);
}
#[test]
fn old_session_capabilities_and_unknown_required_versions_refuse() {
    let batch = packet(0);
    let old = vec![Capability {
        id: Text::new("aq.profiled-launch").unwrap(),
        version: 1,
    }];
    let mut peer = ConformancePeer::new(predecessor(), old.clone()).unwrap();
    assert_eq!(peer.receive(&batch), Err(ContractError::Unsupported));
    assert_eq!(
        aq_session_protocol::require_capabilities(&batch.body.required_capabilities, &old),
        Err(ContractError::Unsupported)
    );
    let mut raw = serde_json::to_value(batch).unwrap();
    raw["body"]["required_capabilities"][0]["version"] = json!(2);
    assert_eq!(
        decode(&serde_json::to_vec(&raw).unwrap()),
        Err(ContractError::Unsupported)
    );
}
#[test]
fn unknown_zero_cumulative_generations_corrections_and_non_invoice_cost_remain_distinct() {
    let batch = packet(0);
    let Message::Batch(batch) = batch.body.message else {
        unreachable!()
    };
    let ObservationKind::Usage { usage } = &batch.events[0].body.kind else {
        unreachable!()
    };
    assert!(matches!(usage.counters.input, Counter::Unknown { .. }));
    assert_eq!(usage.counters.output, Counter::Known { value: 0 });
    assert!(matches!(
        usage.cost,
        Cost::NonInvoiceEstimate {
            amount_microunits: 0,
            ..
        }
    ));
    assert!(matches!(
        usage.basis,
        CounterBasis::Cumulative {
            counter_generation: 1,
            ..
        }
    ));
    let ObservationKind::Usage { usage: corrected } = &batch.events[1].body.kind else {
        unreachable!()
    };
    assert_eq!(
        corrected.correction_of.as_ref().unwrap().event_id,
        batch.events[0].event_id
    );
    assert_eq!(
        corrected.correction_of.as_ref().unwrap().digest,
        batch.events[0].body_digest
    );
    let mut invalid = batch.events[0].clone();
    if let ObservationKind::Usage { usage } = &mut invalid.body.kind {
        usage.basis = CounterBasis::Cumulative {
            counter_id: Text::new("total").unwrap(),
            counter_generation: 0,
        };
    }
    assert!(ObservationRecord::seal(invalid.event_id, invalid.body).is_err());
    let mut self_correction = batch.events[0].clone();
    if let ObservationKind::Usage { usage } = &mut self_correction.body.kind {
        usage.correction_of = Some(EventRef {
            event_id: self_correction.event_id,
            digest: self_correction.body_digest.clone(),
        });
    }
    assert!(ObservationRecord::seal(self_correction.event_id, self_correction.body).is_err());
}
#[test]
fn missing_raw_source_is_a_gap_and_never_a_fabricated_digest() {
    let mut observation = match packet(6).body.message {
        Message::Observation(v) => v,
        _ => unreachable!(),
    };
    observation.provenance.raw_record = RawRecordEvidence::Unavailable {
        reason: GapReason::SourceMissing,
    };
    assert!(observation.validate().is_err());
    observation.kind = ObservationKind::Gap {
        reason: GapReason::SourceMissing,
        affected: None,
    };
    observation.validate().unwrap();
    let packet = Packet::seal(Uuid::from_u128(444), Message::Observation(observation)).unwrap();
    decode(&canonical_bytes(&packet).unwrap()).unwrap();
}
#[test]
fn mismatched_node_boot_attempt_generation_and_atomic_batch_failure_refuse() {
    let old = packet(0);
    let Message::Batch(mut batch) = old.body.message.clone() else {
        unreachable!()
    };
    if let SessionIdentity::Remote { boot_id, .. } = &mut batch.events[1].body.binding.session {
        *boot_id = Uuid::from_u128(900);
    }
    batch.events[1] =
        ObservationRecord::seal(batch.events[1].event_id, batch.events[1].body.clone()).unwrap();
    assert_eq!(
        Packet::seal(Uuid::from_u128(901), Message::Batch(batch)),
        Err(ContractError::Stale)
    );
    let mut binding = predecessor();
    binding.attempt_generation += 1;
    assert!(binding.validate().is_err());
    let Message::Batch(mut batch) = old.body.message else {
        unreachable!()
    };
    batch.events[1].body.binding.project_id = Uuid::from_u128(888);
    batch.events[1] =
        ObservationRecord::seal(batch.events[1].event_id, batch.events[1].body.clone()).unwrap();
    let packet = Packet::seal(Uuid::from_u128(902), Message::Batch(batch)).unwrap();
    let mut peer = ConformancePeer::new(predecessor(), caps()).unwrap();
    assert_eq!(peer.receive(&packet), Err(ContractError::Stale));
    assert_eq!(peer.last_sequence(), 0);
}
#[test]
fn compute_release_disk_release_and_retained_suspension_are_independent() {
    let Message::Parking(report) = packet(3).body.message else {
        unreachable!()
    };
    assert_eq!(report.disposition, SuspensionDisposition::RetainedSuspended);
    assert!(report.compute_released);
    assert!(!report.protected_disk_released);
    let mut invalid = report.clone();
    invalid.stop = StopObservation::Unknown {
        reason: Text::new("lost ownership").unwrap(),
    };
    assert!(invalid.validate().is_err());
    let mut stopped = report.clone();
    stopped.disposition = SuspensionDisposition::StoppedRetaining;
    stopped.central_receipts.clear();
    stopped.effects = EffectsObservation::Pending {
        operation_ids: vec![Uuid::from_u128(80)],
    };
    stopped.validate().unwrap();
    assert!(stopped.compute_released && !stopped.protected_disk_released);
    let mut invalid = report.clone();
    invalid.protected_disk_released = true;
    assert!(invalid.validate().is_err());
    let mut invalid = report.clone();
    invalid.central_receipts[0].binding_digest = Digest::of(b"other-attempt");
    assert!(invalid.validate().is_err());
    let mut raw = serde_json::to_value(packet(3)).unwrap();
    raw["body"]["message"]["body"]["disposition"] = json!({"kind":"success"});
    assert!(decode(&serde_json::to_vec(&raw).unwrap()).is_err());
}
#[test]
fn checkpoint_is_a_claim_not_stop_proof_or_task_settlement() {
    let checkpoint = packet(1);
    let mut peer = ConformancePeer::new(predecessor(), caps()).unwrap();
    let receipt = peer.receive(&checkpoint).unwrap();
    assert_eq!(receipt.disposition, ReceiptDisposition::CheckpointRecorded);
    assert!(!peer.bootstrap_acknowledged());
    assert_eq!(receipt.through_sequence, None);
    let mut wrong = serde_json::to_value(checkpoint).unwrap();
    wrong["body"]["message"]["body"]["task_succeeded"] = json!(true);
    assert!(decode(&serde_json::to_vec(&wrong).unwrap()).is_err());
}
#[test]
fn fresh_cross_node_continuation_and_exact_bootstrap_ack_are_fenced() {
    let attachment = packet(4);
    let ack = packet(5);
    let mut peer = ConformancePeer::new(successor(), caps()).unwrap();
    assert_eq!(peer.receive(&ack), Err(ContractError::Stale));
    peer.receive(&attachment).unwrap();
    assert!(!peer.bootstrap_acknowledged());
    let mut stale = match ack.body.message.clone() {
        Message::BootstrapAck(v) => v,
        _ => unreachable!(),
    };
    stale.continuation.generation += 1;
    let stale = Packet::seal(Uuid::from_u128(990), Message::BootstrapAck(stale)).unwrap();
    assert_eq!(peer.receive(&stale), Err(ContractError::Stale));
    assert!(!peer.bootstrap_acknowledged());
    let receipt = peer.receive(&ack).unwrap();
    assert_eq!(
        receipt.disposition,
        ReceiptDisposition::BootstrapAcknowledgmentRecorded
    );
    assert!(peer.bootstrap_acknowledged());
    assert_eq!(peer.receive(&ack).unwrap(), receipt);
    let Message::Continuation(mut value) = attachment.body.message else {
        unreachable!()
    };
    value.successor = value.predecessor.clone();
    assert!(value.validate().is_err());
}
#[test]
fn central_binding_needs_no_fake_worker_identity() {
    let mut observation = match packet(6).body.message {
        Message::Observation(v) => v,
        _ => unreachable!(),
    };
    observation.binding.session = SessionIdentity::Central {
        authority_id: Uuid::from_u128(600),
        session_id: Uuid::from_u128(601),
        generation: 9,
    };
    let binding = observation.binding.clone();
    let packet = Packet::seal(Uuid::from_u128(602), Message::Observation(observation)).unwrap();
    let mut peer = ConformancePeer::new(binding, caps()).unwrap();
    peer.receive(&packet).unwrap();
}
#[test]
fn finite_utf8_batches_strict_fields_and_integer_units_refuse() {
    assert!(SummaryText::new("é".repeat(8193)).is_err());
    assert!(decode(&vec![b' '; MAX_BATCH_BYTES + 1]).is_err());
    let Message::Batch(mut batch) = packet(0).body.message else {
        unreachable!()
    };
    batch.events = vec![batch.events[0].clone(); MAX_EVENTS + 1];
    assert!(batch.validate().is_err());
    let mut raw = serde_json::to_value(packet(0)).unwrap();
    raw["body"]["message"]["body"]["events"][0]["body"]["kind"]["usage"]["counters"]["output"]["value"] =
        json!(1.5);
    assert!(decode(&serde_json::to_vec(&raw).unwrap()).is_err());
    let mut raw = serde_json::to_value(packet(0)).unwrap();
    raw["secret_value"] = json!("forbidden");
    assert!(decode(&serde_json::to_vec(&raw).unwrap()).is_err());
}

#[test]
fn acknowledgment_schema_and_exact_cursor_pairing_are_independent_of_task_outcome() {
    assert_eq!(
        format!(
            "{}\n",
            serde_json::to_string_pretty(&receipt_schema()).unwrap()
        ),
        include_str!("../artifacts/receipt.schema.json")
    );
    let data = fixture();
    for (packet, receipt) in data["valid_packets"]
        .as_array()
        .unwrap()
        .iter()
        .zip(data["receipts"].as_array().unwrap())
    {
        let packet: Packet = serde_json::from_value(packet.clone()).unwrap();
        let receipt: Receipt = serde_json::from_value(receipt.clone()).unwrap();
        receipt.validate_against(&packet).unwrap();
    }
    let packet = packet(0);
    let mut peer = ConformancePeer::new(predecessor(), caps()).unwrap();
    let mut receipt = peer.receive(&packet).unwrap();
    receipt.through_sequence = Some(3);
    assert!(receipt.validate_against(&packet).is_err());
    receipt.through_sequence = Some(2);
    receipt.body_digest = Digest::of(b"other-payload");
    assert_eq!(
        receipt.validate_against(&packet),
        Err(ContractError::Conflict)
    );
}

#[test]
fn available_raw_bytes_must_agree_with_the_declared_source_span() {
    let mut source = match packet(6).body.message {
        Message::Observation(v) => v.provenance,
        _ => unreachable!(),
    };
    source.raw_record = RawRecordEvidence::Available {
        digest: Digest::of(b"x"),
        byte_length: 0,
    };
    assert!(source.validate().is_err());
    source.raw_record = RawRecordEvidence::Available {
        digest: Digest::of(b"x"),
        byte_length: 21,
    };
    assert!(source.validate().is_err());
    source.cursor = SourceCursor::Record {
        id: Text::new("opaque-record").unwrap(),
    };
    source.validate().unwrap();
    source.cursor = SourceCursor::Bytes { start: 0, end: 21 };
    source.validate().unwrap();
}
#[test]
fn stop_proof_artifact_digest_is_sha256_of_canonical_json() {
    let fixture = &fixture()["stop_proof"];
    let proof: StopProof = serde_json::from_value(fixture["body"].clone()).unwrap();
    let canonical = fixture["canonical_json"].as_str().unwrap().as_bytes();
    // Canonical form: compact, keys sorted, integer units.
    assert_eq!(
        canonical,
        format!(
            "{{\"guardian_instance_id\":\"{}\",\"stopped_at_boot_ms\":{},\"writer_set_digest\":\"{}\"}}",
            proof.guardian_instance_id,
            proof.stopped_at_boot_ms,
            proof.writer_set_digest.as_str()
        )
        .as_bytes()
    );
    assert_eq!(proof.to_bytes().unwrap(), canonical);
    let digest: Digest = serde_json::from_value(fixture["digest"].clone()).unwrap();
    assert_eq!(Digest::of(canonical), digest);
    assert_eq!(proof.digest().unwrap(), digest);
    assert_eq!(StopProof::decode(canonical).unwrap(), proof);
    // The parking fixture names this proof and retains it under role result.
    let Message::Parking(report) = packet(3).body.message else {
        unreachable!()
    };
    let StopObservation::ReportedStopped {
        proof: reference, ..
    } = &report.stop
    else {
        unreachable!()
    };
    assert_eq!(reference.digest, digest);
    let receipt = stop_proof_receipt(&report.stop, &report.central_receipts).unwrap();
    assert_eq!(receipt.role, ArtifactRole::Result);
    assert_eq!(receipt.bytes, canonical.len() as u64);
    assert_eq!(
        serde_json::to_value(receipt).unwrap(),
        fixture["receipt"].clone()
    );
    validate_retention(std::slice::from_ref(receipt), &predecessor()).unwrap();
    // Pretty, reordered or extended bodies are refused: one proof, one digest.
    let pretty = serde_json::to_vec_pretty(&proof).unwrap();
    assert!(StopProof::decode(&pretty).is_err());
    let mut extended = fixture["body"].clone();
    extended["writer_count"] = json!(2);
    let mut extended = serde_json::to_value(&extended).unwrap();
    extended.sort_all_objects();
    assert!(StopProof::decode(&serde_json::to_vec(&extended).unwrap()).is_err());
    assert!(StopProof::decode(&vec![b' '; STOP_PROOF_MAX_BYTES + 1]).is_err());
    let nil = StopProof {
        guardian_instance_id: Uuid::nil(),
        ..proof
    };
    assert!(nil.to_bytes().is_err());
}
#[test]
fn successor_verifies_stop_proof_guardian_instance_and_writer_set() {
    let fixture = &fixture()["stop_proof"];
    let canonical = fixture["canonical_json"].as_str().unwrap().as_bytes();
    let Message::Parking(report) = packet(3).body.message else {
        unreachable!()
    };
    let guardian = Uuid::from_u128(31);
    let writers = Digest::of(b"owned-writer-set");
    let proof = verify_stop_proof(&report.stop, canonical, guardian, &writers).unwrap();
    assert_eq!(proof.stopped_at_boot_ms, 1400);
    // Another guardian instance or writer set is refused.
    assert!(verify_stop_proof(&report.stop, canonical, Uuid::from_u128(99), &writers).is_err());
    assert!(verify_stop_proof(&report.stop, canonical, guardian, &Digest::of(b"other")).is_err());
    // Bytes that are not the referenced artifact are refused.
    let forged = StopProof {
        stopped_at_boot_ms: 1,
        ..proof.clone()
    };
    assert!(
        verify_stop_proof(
            &report.stop,
            &forged.to_bytes().unwrap(),
            guardian,
            &writers
        )
        .is_err()
    );
    // An observation disagreeing with its own proof body is refused.
    let mut stop = report.stop.clone();
    if let StopObservation::ReportedStopped {
        writer_set_digest, ..
    } = &mut stop
    {
        *writer_set_digest = Digest::of(b"other");
    }
    assert!(verify_stop_proof(&stop, canonical, guardian, &Digest::of(b"other")).is_err());
    let unknown = StopObservation::Unknown {
        reason: Text::new("lost ownership").unwrap(),
    };
    assert!(verify_stop_proof(&unknown, canonical, guardian, &writers).is_err());
    assert!(stop_proof_receipt(&unknown, &report.central_receipts).is_none());
}
