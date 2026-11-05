//! Sole editable artifact generator. Run from aq-worker root.
use aq_lifecycle_protocol::*;
use serde_json::{Value, json};
use std::{fs, path::Path};
fn id(n: u128) -> uuid::Uuid {
    uuid::Uuid::from_u128(n)
}
fn text(v: &str) -> Text {
    Text::new(v).unwrap()
}
fn reference(v: &str) -> ArtifactRef {
    ArtifactRef {
        id: text(v),
        version: text("1"),
        digest: Digest::of(v.as_bytes()),
    }
}
fn binding(successor: bool) -> Binding {
    Binding {
        project_id: id(6),
        task_id: id(7),
        lineage_id: id(8),
        attempt_id: id(if successor { 11 } else { 3 }),
        attempt_generation: if successor { 5 } else { 4 },
        session: SessionIdentity::Remote {
            node_id: id(if successor { 9 } else { 1 }),
            boot_id: id(if successor { 10 } else { 2 }),
            attempt_id: id(if successor { 11 } else { 3 }),
            assignment_generation: if successor { 5 } else { 4 },
            session_id: id(if successor { 12 } else { 5 }),
        },
    }
}
fn outcome() -> OutcomeRef {
    OutcomeRef {
        outcome_id: id(22),
        wait: wait(),
        revision: 1,
        digest: Digest::of(b"verified-result-reference-only"),
    }
}
fn wait() -> WaitRef {
    WaitRef {
        wait_id: id(20),
        generation: 1,
        predicate_digest: Digest::of(b"typed-wait-predicate"),
    }
}
fn continuation() -> ContinuationRef {
    ContinuationRef {
        continuation_id: id(21),
        generation: 1,
        wait: wait(),
    }
}
fn provenance() -> Provenance {
    Provenance {
        source_id: id(13),
        source_generation: 1,
        source_kind: SourceKind::NativeTranscript,
        native_session_key: text("native-session-1"),
        cursor: SourceCursor::Bytes { start: 0, end: 20 },
        parser: reference("parser-fixture-v1"),
        raw_record: RawRecordEvidence::Available {
            digest: Digest::of(b"raw-provider-record\n"),
            byte_length: 20,
        },
    }
}
fn usage() -> Usage {
    Usage {
        call: ProviderCall {
            provider: text("synthetic"),
            account_reference: text("logical-account"),
            model: text("fixture-model"),
            native_call_id: text("native-call-1"),
        },
        basis: CounterBasis::Cumulative {
            counter_id: text("conversation-total"),
            counter_generation: 1,
        },
        counters: Counters {
            input: Counter::Unknown {
                reason: UnknownReason::NotReported,
            },
            output: Counter::Known { value: 0 },
            cache_read: Counter::Unknown {
                reason: UnknownReason::Unsupported,
            },
            cache_write: Counter::Unknown {
                reason: UnknownReason::Unsupported,
            },
            reasoning: Counter::Unknown {
                reason: UnknownReason::NotApplicable,
            },
        },
        category_semantics: CategorySemantics::MayOverlap {
            policy: reference("provider-category-semantics"),
        },
        cost: Cost::NonInvoiceEstimate {
            amount_microunits: 0,
            currency: text("USD"),
            rate_card: reference("rate-card-r1"),
            basis: EstimateBasis::TokenRates,
            allocation_policy: reference("estimate-policy-r1"),
        },
        completeness: Completeness::ReportedComplete,
        correction_of: None,
    }
}
fn observation(sequence: u64, kind: ObservationKind) -> Observation {
    let mut p = provenance();
    p.cursor = SourceCursor::Bytes {
        start: (sequence - 1) * 20,
        end: sequence * 20,
    };
    Observation {
        binding: binding(false),
        provenance: p,
        sequence,
        observed_at_ms: 1000 + sequence,
        kind,
    }
}
fn retention() -> Vec<RetentionReceipt> {
    [ArtifactRole::Source, ArtifactRole::Checkpoint]
        .into_iter()
        .enumerate()
        .map(|(i, role)| RetentionReceipt {
            receipt_id: id(40 + i as u128),
            owner: text("central-retention-owner"),
            binding_digest: canonical_digest(&binding(false)).unwrap(),
            artifact: reference(if i == 0 {
                "source-snapshot"
            } else {
                "checkpoint-artifact"
            }),
            role,
            bytes: 128,
            retained_at_ms: 1500,
            retained_until_ms: 1000000,
        })
        .collect()
}
/// The guardian stop proof the parking fixture's `ReportedStopped` names.
pub fn stop_proof() -> StopProof {
    StopProof {
        guardian_instance_id: id(31),
        writer_set_digest: Digest::of(b"owned-writer-set"),
        stopped_at_boot_ms: 1400,
    }
}
fn stop_proof_receipt() -> RetentionReceipt {
    let proof = stop_proof();
    RetentionReceipt {
        receipt_id: id(42),
        owner: text("central-retention-owner"),
        binding_digest: canonical_digest(&binding(false)).unwrap(),
        artifact: proof.artifact(text("guardian-stop-proof")).unwrap(),
        role: ArtifactRole::Result,
        bytes: proof.to_bytes().unwrap().len() as u64,
        retained_at_ms: 1500,
        retained_until_ms: 1000000,
    }
}
pub fn fixtures() -> Value {
    let first = ObservationRecord::seal(
        id(101),
        observation(
            1,
            ObservationKind::Usage {
                usage: Box::new(usage()),
            },
        ),
    )
    .unwrap();
    let mut corrected = usage();
    corrected.counters.output = Counter::Known { value: 7 };
    corrected.correction_of = Some(EventRef {
        event_id: first.event_id,
        digest: first.body_digest.clone(),
    });
    let mut corrected_observation = observation(
        2,
        ObservationKind::Usage {
            usage: Box::new(corrected),
        },
    );
    corrected_observation.provenance.cursor = first.body.provenance.cursor.clone();
    corrected_observation.provenance.parser = ArtifactRef {
        id: text("parser-fixture"),
        version: text("2"),
        digest: Digest::of(b"parser-v2"),
    };
    let second = ObservationRecord::seal(id(102), corrected_observation).unwrap();
    let batch = Packet::seal(
        id(100),
        Message::Batch(ObservationBatch {
            node_id: id(1),
            boot_id: id(2),
            first_sequence: 1,
            last_sequence: 2,
            events: vec![first.clone(), second],
        }),
    )
    .unwrap();
    let summary = SummaryClaim {
        text: SummaryText::new(
            "Goal: preserve work.\nUncertainty: effects require reconciliation.",
        )
        .unwrap(),
        context_references: vec![reference("context")],
        pending_operation_ids: vec![id(50)],
    };
    let checkpoint = Packet::seal(
        id(201),
        Message::Checkpoint(Checkpoint {
            binding: binding(false),
            continuation: continuation(),
            claim: CheckpointClaim::Supplied {
                summary_digest: canonical_digest(&summary).unwrap(),
                summary,
            },
        }),
    )
    .unwrap();
    let suspend = Packet::seal(
        id(202),
        Message::Suspend(SuspensionIntent {
            binding: binding(false),
            continuation: continuation(),
            issued_at_ms: 1000,
            deadline_ms: 2000,
            max_summary_bytes: 16384,
            retention_policy: reference("retention-policy"),
        }),
    )
    .unwrap();
    let parking = Packet::seal(
        id(203),
        Message::Parking(ParkingReport {
            binding: binding(false),
            continuation: continuation(),
            disposition: SuspensionDisposition::RetainedSuspended,
            stop: stop_proof()
                .observation(id(30), 2, 1400, text("guardian-stop-proof"))
                .unwrap(),
            local_retained_bytes: 256,
            central_receipts: retention()
                .into_iter()
                .chain([stop_proof_receipt()])
                .collect(),
            effects: EffectsObservation::Reconciled {
                receipt: reference("effect-reconciliation"),
            },
            compute_released: true,
            protected_disk_released: false,
            disk_release_directive: None,
        }),
    )
    .unwrap();
    let attachment = ContinuationAttachment {
        admission_id: id(23),
        continuation: continuation(),
        predecessor: binding(false),
        successor: binding(true),
        fresh_offer_digest: Digest::of(b"new-fenced-offer"),
        outcome: outcome(),
        retained: retention(),
        pending_operation_ids: vec![id(50)],
        held_task_message_ids: vec![id(51)],
        remaining: RemainingBudgets {
            lineage_id: id(8),
            policy: reference("lineage-budget-policy"),
            ledger_checkpoint: reference("minimal-accounting-checkpoint"),
            remaining_failure_retries: 2,
            remaining_elapsed_ms: 60000,
            cost: CostBudget::Unknown {
                reason: UnknownReason::Incomplete,
            },
        },
        native_resume: None,
    };
    let ack = Packet::seal(
        id(205),
        Message::BootstrapAck(BootstrapAck {
            admission_id: id(23),
            continuation: continuation(),
            successor: binding(true),
            attachment_digest: canonical_digest(&attachment).unwrap(),
            outcome: outcome(),
            acknowledgment_receipt: reference("authenticated-aq-bootstrap-ack"),
        }),
    )
    .unwrap();
    let wake = Packet::seal(id(204), Message::Continuation(Box::new(attachment))).unwrap();
    let mut packets = vec![batch, checkpoint, suspend, parking, wake, ack];
    for (i, kind) in [
        ObservationKind::TurnBoundary { in_turn: true },
        ObservationKind::TextReference {
            role: TextRole::Assistant,
            content: reference("scoped-text-artifact"),
            utf8_bytes: 32,
        },
        ObservationKind::ToolStarted {
            call_id: text("tool-call-1"),
            tool_name: text("fixture-tool"),
        },
        ObservationKind::ToolResult {
            call_id: text("tool-call-1"),
            content: reference("tool-result"),
        },
        ObservationKind::Question {
            question_id: text("question-1"),
            content: reference("question-artifact"),
        },
        ObservationKind::ProviderError {
            code: text("rate_limit"),
            retryable: true,
        },
        ObservationKind::Gap {
            reason: GapReason::Truncated,
            affected: Some(SourceCursor::Bytes {
                start: 100,
                end: 200,
            }),
        },
    ]
    .into_iter()
    .enumerate()
    {
        packets.push(
            Packet::seal(
                id(300 + i as u128),
                Message::Observation(observation(3 + i as u64, kind)),
            )
            .unwrap(),
        );
    }
    let capabilities: Vec<Capability> = [
        OBSERVATION_CAPABILITY,
        USAGE_CAPABILITY,
        SUSPENSION_CAPABILITY,
        CONTINUATION_CAPABILITY,
    ]
    .into_iter()
    .map(|name| Capability {
        id: text(name),
        version: 1,
    })
    .collect();
    let mut predecessor_peer = ConformancePeer::new(binding(false), capabilities.clone()).unwrap();
    let mut successor_peer = ConformancePeer::new(binding(true), capabilities.clone()).unwrap();
    let receipts: Vec<Receipt> = packets
        .iter()
        .map(|packet| {
            if matches!(
                packet.body.message,
                Message::Continuation(_) | Message::BootstrapAck(_)
            ) {
                successor_peer.receive(packet).unwrap()
            } else {
                predecessor_peer.receive(packet).unwrap()
            }
        })
        .collect();
    json!({"schema_version":1,"predecessor":binding(false),"successor":binding(true),"advertised_capabilities":capabilities,"valid_packets":packets,"receipts":receipts,"first_usage_event":first,"stop_proof":{"body":stop_proof(),"canonical_json":String::from_utf8(stop_proof().to_bytes().unwrap()).unwrap(),"digest":stop_proof().digest().unwrap(),"receipt":stop_proof_receipt()},"semantics":{"reported_zero":"known value0 is distinct from unknown","billing":"observation claims only; no ledger or exactly-once invoice guarantee","suspended":"distinct attempt disposition; never ordinary success/failure","stop":"reference must be verified by runtime owner","stop_proof":"ReportedStopped.proof names a role-result retained artifact whose bytes are canonical StopProof JSON and whose digest is their SHA-256; the successor SessionHost verifies guardian instance and writer set, the server only lineage","task_mail":"only task-addressed IDs may attach to a successor","bootstrap":"AQ authenticated receipt, not bytes placed in a prompt","legacy_capabilities":"old session0.1.0 behavior preserved"}})
}
fn main() {
    let schema = format!(
        "{}\n",
        serde_json::to_string_pretty(&contract_schema()).unwrap()
    );
    let fixtures = format!("{}\n", serde_json::to_string_pretty(&fixtures()).unwrap());
    let crate_root = Path::new(env!("CARGO_MANIFEST_DIR"));
    let mut dirs = vec![crate_root.join("artifacts")];
    if let Some(repo) = crate_root.parent().and_then(Path::parent)
        && repo.join("crates/aq-lifecycle-protocol") == crate_root
    {
        dirs.push(repo.join("protocol/lifecycle-v1"));
    }
    for dir in dirs {
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("contract.schema.json"), &schema).unwrap();
        fs::write(
            dir.join("receipt.schema.json"),
            format!(
                "{}\n",
                serde_json::to_string_pretty(&receipt_schema()).unwrap()
            ),
        )
        .unwrap();
        fs::write(
            dir.join("stop-proof.schema.json"),
            format!(
                "{}\n",
                serde_json::to_string_pretty(&stop_proof_schema()).unwrap()
            ),
        )
        .unwrap();
        fs::write(dir.join("fixtures.json"), &fixtures).unwrap();
    }
}
