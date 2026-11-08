use super::*;
use aq_session_protocol::Text;
use std::{
    fs,
    os::unix::fs::{PermissionsExt, symlink},
};

fn fixture() -> serde_json::Value {
    serde_json::from_slice(include_bytes!(
        "../../../../../vendor/aq-admission-0.1.7/artifacts/control.fixture.json"
    ))
    .unwrap()
}
fn offer() -> NativeOffer {
    serde_json::from_value(fixture()["native_offer"].clone()).unwrap()
}
fn identity() -> WorkerIdentity {
    serde_json::from_value(fixture()["requests"]["accept"]["identity"].clone()).unwrap()
}
fn capabilities() -> Vec<Capability> {
    serde_json::from_value(
        fixture()["requests"]["accept"]["acceptance"]["advertised_capabilities"].clone(),
    )
    .unwrap()
}
fn qualification() -> RuntimeIdentity {
    RuntimeIdentity {
        runtime: ArtifactRef {
            id: Text::new("synthetic.runtime").unwrap(),
            version: Text::new("1").unwrap(),
            digest: Digest::of(b"synthetic runtime"),
        },
        qualification: ArtifactRef {
            id: Text::new("synthetic.qualification").unwrap(),
            version: Text::new("1").unwrap(),
            digest: Digest::of(b"synthetic qualification"),
        },
    }
}
fn limits() -> Limits {
    Limits {
        slots: 2,
        memory_bytes: u64::MAX,
        cpu_millis: u64::MAX,
        disk_bytes: 4096,
    }
}
fn create(root: &Path) -> Journal {
    Journal::create_identity(root, identity(), limits()).unwrap()
}
fn reserve(journal: &mut Journal) -> Uuid {
    journal
        .reserve(
            offer(),
            qualification(),
            boot_ns().unwrap() + 400_000_000,
            1024,
        )
        .unwrap()
}
fn rewrite(root: &Path, mutate: impl FnOnce(&mut State)) {
    let file = root.join("journal.json");
    let mut s: Snapshot = serde_json::from_slice(&fs::read(&file).unwrap()).unwrap();
    mutate(&mut s.state);
    s.digest = canonical_digest(&s.state).unwrap();
    fs::write(file, canonical_bytes(&s).unwrap()).unwrap();
}
/// R4: a panic while decoding a journal off the worker's stack is an error, not a crash.
#[test]
fn a_panicking_journal_decode_is_an_error() {
    let e = off_stack(|| -> Result<()> { panic!("synthetic decode panic") }).unwrap_err();
    assert_eq!(e.to_string(), "journal decode thread panicked");
    assert_eq!(off_stack(|| Ok(7)).unwrap(), 7);
}
#[test]
fn reservation_acceptance_survive_restart_without_restoring_live_authority() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("native");
    let mut journal = create(&root);
    let id = reserve(&mut journal);
    assert_eq!(journal.available().unwrap().slots, 1);
    assert_eq!(journal.available().unwrap().disk_bytes, 3072);
    let first = journal.begin_accept(id, capabilities()).unwrap();
    let replay = journal.begin_accept(id, capabilities()).unwrap();
    assert_eq!(
        canonical_bytes(&first).unwrap(),
        canonical_bytes(&replay).unwrap()
    );
    let revision = journal.state().revision;
    drop(journal);
    let mut recovered = Journal::open(&root, identity().node_id, limits()).unwrap();
    assert!(recovered.recovery_required());
    assert!(recovered.available().is_err());
    assert_eq!(recovered.state().identity, identity());
    assert_eq!(recovered.state().revision, revision);
    assert_eq!(
        recovered.state().attempts[&id].reservation.runtime.runtime,
        qualification().runtime
    );
    assert_eq!(
        recovered.state().attempts[&id]
            .reservation
            .runtime
            .qualification,
        qualification().qualification
    );
    assert_eq!(
        canonical_bytes(recovered.state().attempts[&id].acceptance.as_ref().unwrap()).unwrap(),
        canonical_bytes(&first).unwrap()
    );
    assert!(recovered.begin_accept(id, capabilities()).is_err());
    assert!(
        recovered
            .reserve(offer(), qualification(), boot_ns().unwrap() + 10_000_000, 1)
            .is_err()
    );
}
#[test]
fn new_initialization_has_new_logical_boot_and_cannot_overwrite_existing_root() {
    let dir = tempfile::tempdir().unwrap();
    let node = Uuid::new_v4();
    let root = dir.path().join("native");
    let journal = Journal::create(&root, node, limits()).unwrap();
    assert_eq!(journal.state().identity.node_id, node);
    assert!(!journal.recovery_required());
    let old = journal.state().identity.boot_id;
    drop(journal);
    assert!(Journal::create(&root, node, limits()).is_err());
    let another = Journal::create(&dir.path().join("other"), node, limits()).unwrap();
    assert_ne!(old, another.state().identity.boot_id);
}
#[tokio::test]
async fn interrupted_replacement_recovers_only_published_state_and_poisoned_writer_refuses() {
    let _clock = crate::native::http::test_clock::TestClock::new();
    for (fault, published) in [
        (WriteFault::BeforeWrite, false),
        (WriteFault::AfterFileSync, false),
        (WriteFault::AfterRename, true),
        (WriteFault::AfterDirectorySync, true),
    ] {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("native");
        let mut journal = create(&root);
        let id = reserve(&mut journal);
        journal.inject_fault(fault);
        assert!(journal.begin_accept(id, capabilities()).is_err());
        assert!(journal.recovery_required());
        assert!(journal.begin_accept(id, capabilities()).is_err());
        drop(journal);
        let recovered = Journal::open(&root, identity().node_id, limits()).unwrap();
        assert_eq!(
            recovered.state().attempts[&id].acceptance.is_some(),
            published
        );
        assert!(recovered.recovery_required());
        assert!(!root.join("journal.pending").exists());
    }
}
#[test]
fn unknown_attempts_and_changed_acceptance_retries_do_not_consume_revision() {
    let dir = tempfile::tempdir().unwrap();
    let mut journal = create(&dir.path().join("native"));
    let id = reserve(&mut journal);
    journal.begin_accept(id, capabilities()).unwrap();
    let revision = journal.state().revision;
    let mut changed = capabilities();
    changed.push(Capability {
        id: Text::new("extra.synthetic").unwrap(),
        version: 1,
    });
    assert!(journal.begin_accept(id, changed).is_err());
    assert!(
        journal
            .begin_accept(Uuid::new_v4(), capabilities())
            .is_err()
    );
    assert_eq!(journal.state().revision, revision);
}
#[test]
fn expired_reservation_is_still_held_and_cannot_reaccept() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("native");
    let mut journal = create(&root);
    let id = reserve(&mut journal);
    let now = boot_ns().unwrap();
    let mut state = journal.next().unwrap();
    let r = &mut state.attempts.get_mut(&id).unwrap().reservation;
    r.reserved_at_ns = now - 2_000_000;
    r.deadline_ns = now - 1_000_000;
    journal.persist(state).unwrap();
    assert!(journal.begin_accept(id, capabilities()).is_err());
    assert_eq!(journal.state().attempts.len(), 1);
    assert!(
        journal
            .reserve(offer(), qualification(), now + 10_000_000, 1)
            .is_err()
    );
}
#[test]
fn capacity_and_clock_limits_refuse_before_any_new_durable_record() {
    let dir = tempfile::tempdir().unwrap();
    let mut journal = create(&dir.path().join("native"));
    let now = boot_ns().unwrap();
    let revision = journal.state().revision;
    assert!(
        journal
            .reserve(offer(), qualification(), now - 1, 1)
            .is_err()
    );
    assert!(
        journal
            .reserve(offer(), qualification(), u64::MAX, 1)
            .is_err()
    );
    assert!(
        journal
            .reserve(offer(), qualification(), now + 300_000_000, 4097)
            .is_err()
    );
    assert!(
        journal
            .reserve(offer(), qualification(), now + 300_000_000, 0)
            .is_err()
    );
    assert_eq!(journal.state().revision, revision);
    assert!(journal.state().attempts.is_empty());
    reserve(&mut journal);
    drop(journal);
    let mut reduced = limits();
    reduced.disk_bytes = 1;
    assert!(Journal::open(&dir.path().join("native"), identity().node_id, reduced).is_err());
}
#[test]
fn missing_corrupt_newer_and_wrong_node_journals_are_not_empty_nodes() {
    for mode in 0..4 {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("native");
        let journal = create(&root);
        drop(journal);
        match mode {
            0 => fs::remove_file(root.join("journal.json")).unwrap(),
            1 => fs::write(root.join("journal.json"), b"broken").unwrap(),
            2 => rewrite(&root, |s| s.format = FORMAT + 1),
            _ => {}
        }
        let node = if mode == 3 {
            Uuid::new_v4()
        } else {
            identity().node_id
        };
        assert!(Journal::open(&root, node, limits()).is_err());
        assert!(Journal::create(&root, node, limits()).is_err());
    }
}
#[test]
fn checksum_and_structural_association_are_both_checked_on_reopen() {
    for valid_checksum in [false, true] {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("native");
        let mut journal = create(&root);
        let id = reserve(&mut journal);
        journal.begin_accept(id, capabilities()).unwrap();
        drop(journal);
        let file = root.join("journal.json");
        let mut snapshot: Snapshot = serde_json::from_slice(&fs::read(&file).unwrap()).unwrap();
        snapshot
            .state
            .attempts
            .get_mut(&id)
            .unwrap()
            .acceptance
            .as_mut()
            .unwrap()
            .operation_id = Uuid::new_v4();
        if valid_checksum {
            snapshot.digest = canonical_digest(&snapshot.state).unwrap();
        }
        fs::write(file, canonical_bytes(&snapshot).unwrap()).unwrap();
        assert!(Journal::open(&root, identity().node_id, limits()).is_err());
    }
}
#[test]
fn different_kernel_boot_is_historical_without_comparing_local_clocks() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("native");
    let mut journal = create(&root);
    let id = reserve(&mut journal);
    drop(journal);
    let old_boot = Uuid::new_v4();
    rewrite(&root, |s| s.kernel_boot_id = old_boot);
    let mut recovered = Journal::open(&root, identity().node_id, limits()).unwrap();
    assert!(recovered.recovery_required());
    assert_eq!(recovered.state().kernel_boot_id, old_boot);
    assert!(recovered.begin_accept(id, capabilities()).is_err());
}
#[test]
fn exclusive_owner_and_unsafe_paths_refuse() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("native");
    let journal = create(&root);
    assert!(Journal::open(&root, identity().node_id, limits()).is_err());
    drop(journal);
    let alias = dir.path().join("alias");
    symlink(&root, &alias).unwrap();
    assert!(Journal::open(&alias, identity().node_id, limits()).is_err());
    fs::set_permissions(&root, fs::Permissions::from_mode(0o755)).unwrap();
    assert!(Journal::open(&root, identity().node_id, limits()).is_err());
}
#[test]
fn symlink_hardlink_and_nonprivate_members_refuse() {
    for mode in 0..3 {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("native");
        drop(create(&root));
        let file = root.join("journal.json");
        let outside = dir.path().join("outside");
        match mode {
            0 => {
                fs::rename(&file, &outside).unwrap();
                symlink(&outside, &file).unwrap();
            }
            1 => fs::hard_link(&file, &outside).unwrap(),
            _ => fs::set_permissions(&file, fs::Permissions::from_mode(0o644)).unwrap(),
        }
        assert!(Journal::open(&root, identity().node_id, limits()).is_err());
    }
}
#[test]
fn root_replacement_cannot_redirect_an_open_writers_effects() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("native");
    let moved = dir.path().join("held");
    let other = dir.path().join("other");
    let mut journal = create(&root);
    fs::rename(&root, &moved).unwrap();
    fs::create_dir(&other).unwrap();
    symlink(&other, &root).unwrap();
    reserve(&mut journal);
    assert!(!other.join("journal.json").exists());
    assert!(moved.join("journal.json").exists());
}
#[test]
fn oversized_member_and_unknown_root_member_fail_closed() {
    for oversized in [false, true] {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("native");
        drop(create(&root));
        if oversized {
            fs::OpenOptions::new()
                .write(true)
                .open(root.join("journal.json"))
                .unwrap()
                .set_len(MAX_BYTES as u64 + 1)
                .unwrap();
        } else {
            fs::write(root.join("unexpected"), b"x").unwrap();
        }
        assert!(Journal::open(&root, identity().node_id, limits()).is_err());
    }
}
#[tokio::test]
async fn actual_http_preparation_is_recorded_after_exact_acceptance_and_keeps_original_cap() {
    let _clock = crate::native::http::test_clock::TestClock::new();
    use crate::{config::WorkerConfig, native::http::NativeClient};
    use aq_admission::control::{ControlReply, PreparationDelivery};
    use axum::{Json, Router, http::HeaderMap, routing::post};
    async fn handle(headers: HeaderMap, Json(request): Json<Accept>) -> Json<ControlReply> {
        let mut delivery: PreparationDelivery =
            serde_json::from_value(fixture()["replies"][1]["body"].clone()).unwrap();
        delivery.delivery.nonce =
            Uuid::parse_str(headers["aq-delivery-nonce"].to_str().unwrap()).unwrap();
        delivery.original.operation_id = request.operation_id;
        delivery.original.reservation = request.reservation;
        delivery.original.guardian_id = request.guardian_id;
        delivery.original.launch_id = request.launch_id;
        Json(ControlReply::Preparation(delivery))
    }
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let task = tokio::spawn(async move {
        axum::serve(
            listener,
            Router::new().route("/v1/worker/profiled/accept", post(handle)),
        )
        .await
        .unwrap()
    });
    let client = NativeClient::from_config(&WorkerConfig {
        server_url: format!("http://{address}"),
        node_id: identity().node_id,
        credential: "synthetic".into(),
        cf_client_id: None,
        cf_client_secret: None,
    })
    .unwrap();
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("native");
    let mut journal = create(&root);
    let id = reserve(&mut journal);
    let request = journal.begin_accept(id, capabilities()).unwrap();
    let cap = journal.state().attempts[&id].reservation.deadline_ns;
    let observed = client.accept(&request, cap, None).await.unwrap();
    journal.record_preparation(id, &observed).unwrap();
    let first = journal.state().attempts[&id]
        .preparation
        .as_ref()
        .unwrap()
        .deadline_ns;
    let repeated = client.accept(&request, cap, Some(&observed)).await.unwrap();
    journal.record_preparation(id, &repeated).unwrap();
    assert!(
        journal.state().attempts[&id]
            .preparation
            .as_ref()
            .unwrap()
            .deadline_ns
            <= first
    );
    crate::native::http::test_clock::expire(cap).await;
    assert!(client.accept(&request, cap, Some(&observed)).await.is_err());
    drop(journal);
    let recovered = Journal::open(&root, identity().node_id, limits()).unwrap();
    assert!(recovered.recovery_required());
    assert!(recovered.state().attempts[&id].preparation.is_some());
    task.abort();
}

#[test]
fn changed_private_root_permissions_refuse_further_capacity_or_writes() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("native");
    let mut journal = create(&root);
    fs::set_permissions(&root, fs::Permissions::from_mode(0o755)).unwrap();
    assert!(journal.available().is_err());
    assert!(
        journal
            .reserve(
                offer(),
                qualification(),
                boot_ns().unwrap() + 100_000_000,
                1
            )
            .is_err()
    );
}

#[path = "inputs_scenarios.rs"]
mod inputs_scenarios;

#[path = "hydration_scenarios.rs"]
mod hydration_scenarios;

#[path = "preparation_v2_scenarios.rs"]
mod preparation_v2_scenarios;

#[path = "refresh_scenarios.rs"]
mod refresh_scenarios;

#[path = "physical_fixture.rs"]
mod physical_fixture;
#[allow(dead_code)] // Shared finite fixture exposes legacy fault controls unused by this focused slice.
#[path = "refresh_physical_scenarios.rs"]
mod refresh_physical_scenarios;

/// Review M8: salvage_result is for a restarted owner only, and after a restart
/// update_result continues only a salvaged history.
#[test]
fn salvage_and_restarted_update_gates() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("native");
    let mut live = create(&root);
    let id = Uuid::new_v4();
    let e = live.salvage_result(id, None).err().unwrap();
    assert!(e.to_string().contains("restarted owner only"), "{e:#}");
    drop(live);
    let mut restarted = Journal::open(&root, identity().node_id, limits()).unwrap();
    assert!(restarted.recovery_required());
    let e = restarted.salvage_result(id, None).err().unwrap();
    assert!(
        e.to_string().contains("salvage attempt absent"),
        "past the gate: {e:#}"
    );
    let mut a = crate::native::test_support::profile_history::terminal_attempt(false, false);
    let wrong = Digest::of(b"not the history");
    for salvaged in [None, Some(3)] {
        let mut h = crate::native::result::tests::history();
        h.salvaged = salvaged;
        a.result = Some(h);
        restarted.state.attempts.insert(id, a.clone());
        let e = restarted
            .update_result(id, &wrong, |_, _| Ok(()))
            .err()
            .unwrap();
        let expected = match salvaged {
            None => "requires stop-and-preserve recovery",
            Some(_) => "result CAS changed",
        };
        assert!(e.to_string().contains(expected), "{salvaged:?}: {e:#}");
    }
}
