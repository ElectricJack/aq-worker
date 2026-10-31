//! Admission margins inside the configured hard logical limit. No history is
//! discarded to replenish them. Ordinary work cannot consume urgent capacity.
use super::*;
pub const META_ALLOWANCE: u64 = 256 * 1024;
pub const DELIVERY_ALLOWANCE: u64 = MAX_BATCH_BYTES as u64 + 8192;
pub const ORDINARY_CONTROL_MARGIN: u64 = 512 * 1024;
pub const BOOT_MARGIN: u64 = 256 * 1024;
pub const SOURCE_CONTROL_GROWTH: u64 = 8192;
pub const MAX_SOURCES: u64 = 16;
pub const FORMAT: u32 = 5;
pub const MIN_ORDINARY_BYTES: u64 = 4096;
impl Limits {
    pub fn data_limit(&self) -> u64 {
        self.logical_bytes
            .saturating_sub(META_ALLOWANCE + DELIVERY_ALLOWANCE)
    }
    pub fn ordinary_limit(&self) -> u64 {
        self.data_limit().saturating_sub(ORDINARY_CONTROL_MARGIN)
    }
    pub fn routine_control_limit(&self) -> u64 {
        self.data_limit().saturating_sub(BOOT_MARGIN)
    }
}
pub(super) fn controls() -> &'static [ArtifactRef] {
    static C: std::sync::LazyLock<Vec<ArtifactRef>> = std::sync::LazyLock::new(|| {
        vec![
            engine_store::loss_parser(),
            reader::control_parser(),
            sessions::parser(),
        ]
    });
    &C
}
pub(super) fn catalog_digest() -> Digest {
    static D: std::sync::LazyLock<Digest> = std::sync::LazyLock::new(|| {
        let mut hash = Sha256::new();
        hash.update(b"observation-format5-policy-v1");
        for bytes in [
            include_bytes!("lib.rs").as_slice(),
            include_bytes!("budget.rs").as_slice(),
            include_bytes!("engine_store.rs").as_slice(),
            include_bytes!("reader.rs").as_slice(),
            include_bytes!("sessions.rs").as_slice(),
            include_bytes!("reparse.rs").as_slice(),
            include_bytes!("local_intent.rs").as_slice(),
        ] {
            hash.update((bytes.len() as u64).to_be_bytes());
            hash.update(bytes);
        }
        Digest::of(&hash.finalize())
    });
    D.clone()
}
pub(super) fn reserved_parser(parser: &ArtifactRef) -> bool {
    matches!(
        parser.id.as_str(),
        "row-spool-loss-monitor" | "row-file-source-monitor" | "row-store-source-fence"
    )
}
pub(super) fn is_urgent(event: &ObservationRecord) -> bool {
    if !matches!(event.body.kind, ObservationKind::Gap { .. }) {
        return false;
    }
    let p = &event.body.provenance.parser;
    if p == &controls()[0] || p == &controls()[2] {
        return true;
    }
    p == &controls()[1]
        && matches!(&event.body.provenance.cursor,SourceCursor::Record{id}if id.as_str().starts_with("source-change:"))
}
pub(super) fn source_policy(row: &SourceRow) -> Result<()> {
    let missing = controls()
        .iter()
        .filter(|p| !row.parsers.contains(p))
        .count();
    ensure!(
        row.parsers.len() + missing <= 16,
        "current control parser slots must remain reserved"
    );
    // Source control changes only these bounded fields. Reserve even the largest
    // encoding, including a previously absent reader anchor and all3 monitors.
    let mut worst = row.clone();
    for p in controls() {
        if !worst.parsers.contains(p) {
            worst.parsers.push(p.clone());
        }
    }
    worst.hold = Some(HoldReason::SourceChange);
    worst.last_control_sequence = Some(u64::MAX);
    worst.cursor = u64::MAX;
    if let Some(e) = &mut worst.engine {
        e.revision = u64::MAX;
        e.context.next_sequence = u64::MAX;
    }
    if let Some(r) = &mut worst.reader {
        r.cursor.offset = u64::MAX;
        r.cursor.anchor = Some(aq_observation::source::Anchor {
            end: u64::MAX,
            bytes: 64,
            digest: Digest::of(b"bounded anchor"),
        });
        r.last_change = Some(aq_observation::source::SourceChange::AnchorChanged);
    }
    ensure!(
        encode(&worst)?.len().saturating_sub(encode(row)?.len()) as u64 <= SOURCE_CONTROL_GROWTH,
        "source configuration cannot reserve bounded control metadata"
    );
    Ok(())
}
pub(super) fn source_growth(old: &[u8], row: &SourceRow) -> Result<()> {
    ensure!(
        encode(row)?.len().saturating_sub(old.len()) as u64 <= SOURCE_CONTROL_GROWTH,
        "control source metadata reserve exceeded"
    );
    source_policy(row)
}
pub(super) fn live(row: &SourceRow) -> bool {
    row.hold.is_none()
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn parser_ids_or_flags_cannot_spend_control_reserve() {
        let binding:Binding=serde_json::from_value(serde_json::json!({"project_id":Uuid::from_u128(1),"task_id":Uuid::from_u128(2),"lineage_id":Uuid::from_u128(3),"attempt_id":Uuid::from_u128(4),"attempt_generation":1,"session":{"kind":"remote","node_id":Uuid::from_u128(5),"boot_id":Uuid::from_u128(6),"session_id":Uuid::from_u128(7),"attempt_id":Uuid::from_u128(4),"assignment_generation":1}})).unwrap();
        let source = Source {
            binding: binding.clone(),
            source_id: Uuid::from_u128(8),
            generation: 1,
            native_session_key: Text::new("synthetic").unwrap(),
            format: aq_observation::Format::ClaudeV1Jsonl9a9b8e8,
            provider: Text::new("synthetic").unwrap(),
            account_reference: Text::new("synthetic").unwrap(),
        };
        let mut row = SourceRow {
            source,
            cursor: 0,
            engine: None,
            hold: None,
            last_control_sequence: None,
            parsers: vec![],
            generations: vec![1],
            reader: None,
            source_operations: vec![],
            record_cursor: None,
        };
        for n in 0..13 {
            let mut fake = controls()[0].clone();
            fake.digest = Digest::of(&[n]);
            row.parsers.push(fake);
        }
        source_policy(&row).unwrap();
        let mut fake = controls()[0].clone();
        fake.digest = Digest::of(b"caller parser ID");
        row.parsers.push(fake.clone());
        assert!(source_policy(&row).is_err());
        let body = Observation {
            binding,
            provenance: Provenance {
                source_id: Uuid::from_u128(8),
                source_generation: 1,
                source_kind: SourceKind::NativeTranscript,
                native_session_key: Text::new("synthetic").unwrap(),
                cursor: SourceCursor::Bytes { start: 0, end: 1 },
                parser: fake,
                raw_record: RawRecordEvidence::Available {
                    digest: Digest::of(b"x"),
                    byte_length: 1,
                },
            },
            sequence: 1,
            observed_at_ms: 1,
            kind: ObservationKind::Gap {
                reason: GapReason::Malformed,
                affected: None,
            },
        };
        let event = ObservationRecord::seal(Uuid::from_u128(9), body).unwrap();
        assert!(!is_urgent(&event));
    }
    #[test]
    fn exact_public_monitor_refs_are_rejected_on_raw_ingress() {
        let temp = tempfile::tempdir().unwrap();
        let store = Store::create(&temp.path().join("rows"), Limits::fixture()).unwrap();
        let binding:Binding=serde_json::from_value(serde_json::json!({"project_id":Uuid::from_u128(1),"task_id":Uuid::from_u128(2),"lineage_id":Uuid::from_u128(3),"attempt_id":Uuid::from_u128(4),"attempt_generation":1,"session":{"kind":"remote","node_id":Uuid::from_u128(5),"boot_id":Uuid::from_u128(6),"session_id":Uuid::from_u128(7),"attempt_id":Uuid::from_u128(4),"assignment_generation":1}})).unwrap();
        let source = Source {
            binding: binding.clone(),
            source_id: Uuid::from_u128(8),
            generation: 1,
            native_session_key: Text::new("synthetic").unwrap(),
            format: aq_observation::Format::ClaudeV1Jsonl9a9b8e8,
            provider: Text::new("synthetic").unwrap(),
            account_reference: Text::new("synthetic").unwrap(),
        };
        store.attach(source).unwrap();
        let before = store.audit_fingerprint().unwrap();
        for parser in controls() {
            let body = Observation {
                binding: binding.clone(),
                provenance: Provenance {
                    source_id: Uuid::from_u128(8),
                    source_generation: 1,
                    source_kind: SourceKind::NativeTranscript,
                    native_session_key: Text::new("synthetic").unwrap(),
                    cursor: SourceCursor::Bytes { start: 0, end: 1 },
                    parser: parser.clone(),
                    raw_record: RawRecordEvidence::Available {
                        digest: Digest::of(b"x"),
                        byte_length: 1,
                    },
                },
                sequence: 1,
                observed_at_ms: 1,
                kind: ObservationKind::Gap {
                    reason: GapReason::SpoolOverflow,
                    affected: None,
                },
            };
            let event = ObservationRecord::seal(Uuid::from_u128(9), body).unwrap();
            assert!(
                store
                    .append(Uuid::from_u128(8), 0, 1, &[event], &[])
                    .is_err()
            );
            assert_eq!(store.audit_fingerprint().unwrap(), before);
            assert!(
                store
                    .source(Uuid::from_u128(8))
                    .unwrap()
                    .last_control_sequence
                    .is_none()
            );
        }
    }
}
