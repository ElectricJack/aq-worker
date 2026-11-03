mod support;
use anyhow::{Result, ensure};
use aq_observation::{lifecycle::*, source::*, spool::*, *};
use serde_json::{Value, json};
use std::{collections::BTreeMap, io::Write};
use uuid::Uuid;
fn node(boot: u128) -> Node {
    Node {
        node_id: Uuid::from_u128(5),
        boot_id: Uuid::from_u128(boot),
    }
}
fn source(id: u128, boot: u128, attempt: u128, key: &str) -> Source {
    Source{binding:serde_json::from_value(json!({"project_id":Uuid::from_u128(1),"task_id":Uuid::from_u128(2),"lineage_id":Uuid::from_u128(3),"attempt_id":Uuid::from_u128(attempt),"attempt_generation":1,"session":{"kind":"remote","node_id":Uuid::from_u128(5),"boot_id":Uuid::from_u128(boot),"session_id":Uuid::from_u128(attempt+1),"attempt_id":Uuid::from_u128(attempt),"assignment_generation":1}})).unwrap(),source_id:Uuid::from_u128(id),generation:1,native_session_key:Text::new(key).unwrap(),format:Format::ClaudeV1Jsonl9a9b8e8,provider:Text::new("fixture-provider").unwrap(),account_reference:Text::new("fixture-account").unwrap(), registered_session_key: None }
}
fn row(key: &str, id: &str) -> Vec<u8> {
    let f: Value = serde_json::from_str(include_str!("fixtures/native-synthetic.json")).unwrap();
    let mut row = f["claude"][0].clone();
    row["sessionId"] = key.into();
    row["uuid"] = id.into();
    let mut bytes = serde_json::to_vec(&row).unwrap();
    bytes.push(b'\n');
    bytes
}
fn attach(
    spool: &mut Spool,
    temp: &tempfile::TempDir,
    source: Source,
    name: &str,
    bytes: &[u8],
) -> SourceFile {
    std::fs::write(temp.path().join(name), bytes).unwrap();
    let permit = SourceFile::inspect(
        source.source_id,
        temp.path(),
        name,
        source.native_session_key.clone(),
    )
    .unwrap();
    let file = SourceFile::open(permit.clone()).unwrap();
    spool
        .attach_file(Normalizer::new(source, 128).unwrap(), permit)
        .unwrap();
    file
}
struct Coordinator {
    last: u64,
    receipts: BTreeMap<Uuid, Receipt>,
}
impl Coordinator {
    fn receive(&mut self, packet: &Packet) -> Receipt {
        packet.validate().unwrap();
        if let Some(old) = self.receipts.get(&packet.body.message_id) {
            assert_eq!(old.body_digest, packet.body_digest);
            return old.clone();
        }
        let Message::Batch(batch) = &packet.body.message else {
            panic!()
        };
        assert_eq!(batch.node_id, Uuid::from_u128(5));
        assert!([Uuid::from_u128(6), Uuid::from_u128(66)].contains(&batch.boot_id));
        assert_eq!(batch.first_sequence, self.last + 1);
        self.last = batch.last_sequence;
        let receipt = Receipt {
            schema_version: 1,
            message_id: packet.body.message_id,
            body_digest: packet.body_digest.clone(),
            disposition: ReceiptDisposition::ObservationRecorded,
            through_sequence: Some(batch.last_sequence),
        };
        receipt.validate_against(packet).unwrap();
        self.receipts.insert(receipt.message_id, receipt.clone());
        receipt
    }
}
fn transition(next: u64) -> BootTransition {
    BootTransition {
        operation_id: Uuid::from_u128(500),
        node_id: Uuid::from_u128(5),
        transport_owner_id: Uuid::from_u128(400),
        expected_boot_id: Uuid::from_u128(6),
        new_boot_id: Uuid::from_u128(66),
        expected_next_sequence: next,
        owner_receipt: support::reference("synthetic-central-boot-owner-replacement"),
    }
}
#[test]
fn admitted_boot_preserves_global_sequence_old_facts_and_same_boot_batch_prefixes() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("spool");
    let mut spool = support::initialize(&root, node(6), Limits::default());
    let old = source(8, 6, 40, "native-old");
    let file_a = attach(
        &mut spool,
        &temp,
        old.clone(),
        "old.jsonl",
        &row("native-old", "row-a"),
    );
    spool
        .collect(old.source_id, &file_a, 100, OverflowPolicy::Hold)
        .unwrap();
    let frozen = spool.begin_send(100).unwrap().unwrap();
    assert_eq!(spool.next_sequence(), 3);
    let plan = transition(3);
    let verified =
        VerifiedBootTransition::verify(plan.clone(), &support::FixtureTransportOwner).unwrap();
    spool.adopt_boot(&verified, 100).unwrap();
    spool.adopt_boot(&verified, 100).unwrap();
    assert_eq!(spool.node(), &node(66));
    let new = source(9, 66, 60, "native-new");
    let file_b = attach(
        &mut spool,
        &temp,
        new.clone(),
        "new.jsonl",
        &row("native-new", "row-b"),
    );
    spool
        .collect(new.source_id, &file_b, 101, OverflowPolicy::Hold)
        .unwrap();
    std::fs::OpenOptions::new()
        .append(true)
        .open(temp.path().join("old.jsonl"))
        .unwrap()
        .write_all(&row("native-old", "row-c"))
        .unwrap();
    assert!(
        spool
            .collect(old.source_id, &file_a, 102, OverflowPolicy::Hold)
            .is_err(),
        "old-boot live paths are fenced; only verified immutable retained replay may proceed"
    );
    assert_eq!(spool.next_sequence(), 6);
    assert_eq!(spool.prepare_packet().unwrap().unwrap(), frozen);
    drop(spool);
    assert!(Spool::open(&root, node(6), Limits::default()).is_err());
    let mut spool = Spool::open_existing(&root, node(6).node_id, Limits::default()).unwrap();
    assert_eq!(spool.node(), &node(66));
    let mut central = Coordinator {
        last: 0,
        receipts: BTreeMap::new(),
    };
    let mut boots = vec![];
    for expected_end in [2, 3, 5] {
        let packet = spool.prepare_packet().unwrap().unwrap();
        let Message::Batch(batch) = &packet.body.message else {
            panic!()
        };
        boots.push(batch.boot_id);
        assert_eq!(batch.last_sequence, expected_end);
        let receipt = central.receive(&packet);
        spool.acknowledge(&receipt).unwrap();
        assert_eq!(central.receive(&packet), receipt);
    }
    assert_eq!(
        boots,
        vec![Uuid::from_u128(6), Uuid::from_u128(6), Uuid::from_u128(66)]
    );
    assert_eq!(central.last, 5);
    assert_eq!(spool.health().unwrap().acked_through, 5);
    assert_eq!(spool.next_sequence(), 6);
    let mut reused = plan;
    reused.new_boot_id = Uuid::from_u128(67);
    let verified = VerifiedBootTransition::verify(reused, &support::FixtureTransportOwner).unwrap();
    assert!(spool.adopt_boot(&verified, 100).is_err());
    let fresh_old = source(10, 6, 80, "native-obsolete");
    std::fs::write(
        temp.path().join("obsolete.jsonl"),
        row("native-obsolete", "row-d"),
    )
    .unwrap();
    let permit = SourceFile::inspect(
        fresh_old.source_id,
        temp.path(),
        "obsolete.jsonl",
        fresh_old.native_session_key.clone(),
    )
    .unwrap();
    assert!(
        spool
            .attach_file(Normalizer::new(fresh_old, 128).unwrap(), permit)
            .is_err()
    );
}
struct Denied;
impl TransportAuthority for Denied {
    fn admit_initial(&self, _: &InitialTransport) -> Result<()> {
        ensure!(
            false,
            "central owner already has transport history; recovery/gap admission required"
        );
        Ok(())
    }
    fn admit_boot(&self, _: &BootTransition) -> Result<()> {
        ensure!(false, "central owner refused boot");
        Ok(())
    }
}
#[test]
fn missing_or_corrupt_spool_never_silently_reseeds_and_boot_admission_is_not_a_label() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("spool");
    assert!(Spool::open(&root, node(6), Limits::default()).is_err());
    assert!(!root.join("spool.json").exists());
    let spool = support::initialize(&root, node(6), Limits::default());
    drop(spool);
    std::fs::remove_file(root.join("spool.json")).unwrap();
    assert!(Spool::open_existing(&root, node(6).node_id, Limits::default()).is_err());
    assert!(!root.join("spool.json").exists());
    let admission = InitialTransport {
        operation_id: Uuid::from_u128(401),
        node: node(6),
        transport_owner_id: Uuid::from_u128(400),
        initial_sequence: 1,
        owner_receipt: support::reference("reused-old-initial-admission"),
    };
    assert!(VerifiedInitial::verify(admission, &Denied).is_err());
    assert!(VerifiedBootTransition::verify(transition(1), &Denied).is_err());
    let mut invalid = transition(1);
    invalid.new_boot_id = invalid.expected_boot_id;
    assert!(VerifiedBootTransition::verify(invalid, &support::FixtureTransportOwner).is_err());
}
