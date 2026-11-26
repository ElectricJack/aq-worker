use super::*;
use aq_session_protocol::{ArtifactRef, Digest, Text};
use aq_worker::native::journal::{Journal, RuntimeIdentity};
fn slot_offer(base: &NativeOffer, identity: &WorkerIdentity, id: Uuid) -> NativeOffer {
    let mut offer = base.clone();
    let mut session = offer.offered.offer.binding.session.clone();
    if let aq_session_protocol::SessionIdentity::Remote {
        node_id,
        boot_id,
        attempt_id,
        session_id,
        ..
    } = &mut session
    {
        *node_id = identity.node_id;
        *boot_id = identity.boot_id;
        *attempt_id = id;
        *session_id = id;
    }
    offer.offered.offer.binding.attempt_id = id;
    offer.offered.offer.binding.session = session.clone();
    offer.offered.offer.manifest.bindings.attempt_id = id;
    offer.offered.offer.manifest.bindings.session = session;
    offer.offered.offer.binding.launch_manifest_digest =
        aq_session_protocol::canonical_digest(&offer.offered.offer.manifest).unwrap();
    offer.offered.lifecycle_binding =
        aq_admission::transport::lifecycle_binding(&offer.offered.offer).unwrap();
    offer.offered.reservation_ttl_ms = 60_000;
    offer
}
type Slot = (std::path::PathBuf, WorkerIdentity, Uuid, Journal);

fn reserve_slots(directory: &std::path::Path) -> Vec<Slot> {
    let base: NativeOffer = serde_json::from_value(fixture()["native_offer"].clone()).unwrap();
    let reference = |name: &str| ArtifactRef {
        id: Text::new(name).unwrap(),
        version: Text::new("1").unwrap(),
        digest: Digest::of(name.as_bytes()),
    };
    let mut journals = Vec::new();
    for node in 1..=2 {
        let identity = WorkerIdentity {
            node_id: Uuid::from_u128(1000 + node),
            boot_id: Uuid::from_u128(2000 + node),
        };
        for slot in 1..=3 {
            let id = Uuid::from_u128(3000 + node * 10 + slot);
            let offer = slot_offer(&base, &identity, id);
            let root = directory.join(format!("node-{node}-slot-{slot}"));
            let mut journal =
                Journal::create_identity(&root, identity.clone(), preparation_failure::limits())
                    .unwrap();
            journal
                .reserve(
                    offer,
                    RuntimeIdentity {
                        runtime: reference("synthetic.runtime"),
                        qualification: reference("synthetic.qualification"),
                    },
                    boot_ns() + 50_000_000_000,
                    1024,
                )
                .unwrap();
            journals.push((root, identity.clone(), id, journal));
        }
    }
    journals
}
/// Separate journals under a shared boot keep every attempt and its replies in its own slot.
#[test]
fn six_agent_journals_preserve_node_slot_and_old_boot_recovery_fences() {
    let dir = tempfile::tempdir().unwrap();
    let mut journals = reserve_slots(dir.path());
    for i in 0..journals.len() {
        let sibling_attempt = journals[(i + 1) % journals.len()].2;
        let (_, identity, id, journal) = &mut journals[i];
        let before = journal.state().revision;
        assert!(
            journal.begin_accept(sibling_attempt, vec![]).is_err(),
            "an earlier or sibling attempt cannot mutate this slot"
        );
        assert_eq!(journal.state().revision, before);
        let accepted = journal
            .begin_accept(
                *id,
                request::<Accept>("accept")
                    .acceptance
                    .advertised_capabilities,
            )
            .unwrap();
        assert_eq!(accepted.identity, *identity);
        assert_eq!(accepted.acceptance.binding.attempt_id, *id);
    }
    for (root, identity, id, journal) in journals {
        drop(journal);
        let reopened =
            Journal::open(&root, identity.node_id, preparation_failure::limits()).unwrap();
        assert!(reopened.recovery_required());
        assert_eq!(reopened.state().identity, identity);
        assert_eq!(
            reopened
                .state()
                .attempts
                .keys()
                .copied()
                .collect::<Vec<_>>(),
            vec![id]
        );
        assert!(
            reopened.available().is_err(),
            "old boot recovery cannot silently admit new work"
        );
    }
}
