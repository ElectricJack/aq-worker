use aq_worker::{
    clock::FakeClock,
    guardian,
    journal::{Journal, Limits, ProcessEvidence, State},
    runtime::Runtime,
};
use aq_worker_protocol::{testing::*, *};
use tempfile::TempDir;
use uuid::Uuid;
#[allow(dead_code)]
#[path = "fixtures/journal-v1.rs"]
mod legacy;
#[allow(dead_code)]
#[path = "fixtures/journal-v2.rs"]
mod legacy_v2;
fn limits() -> Limits {
    Limits {
        capacity: Resources {
            slots: 1,
            cpu_millis: 10000,
            memory_bytes: 100000,
            disk_bytes: 100000,
        },
        max_pending_results: 16,
        max_pending_bytes: 4096,
        max_preparing: 16,
        minimum_free_disk_bytes: 0,
    }
}
fn legacy_limits() -> legacy::Limits {
    legacy::Limits {
        capacity: limits().capacity,
        max_pending_results: 16,
        max_pending_bytes: 4096,
        max_preparing: 16,
        minimum_free_disk_bytes: 0,
    }
}
fn make_v1(root: &std::path::Path, phase: Phase) -> ResultReport {
    let offer = sample_offer();
    let mut runtime = Runtime::new(
        Journal::open(root, offer.identity.clone(), limits()).unwrap(),
        FakeClock::new(100),
    );
    let accepted = runtime.reserve(offer.clone()).unwrap();
    let report = sample_report(&offer);
    runtime
        .journal
        .transaction(|s| {
            let record = s.attempts.get_mut(&accepted.attempt.attempt_id).unwrap();
            record.phase = phase;
            record.stop_certainty = if phase == Phase::Running {
                StopCertainty::Running
            } else {
                StopCertainty::ConfirmedStopped
            };
            if phase == Phase::Running {
                record.launch_id = Some(Uuid::from_u128(90));
                record.process = Some(ProcessEvidence {
                    pid: u32::MAX,
                    start_ticks: 1,
                });
            } else {
                record.report = Some(report.clone());
                if phase == Phase::Retired {
                    record.result = Some(ResultResponse {
                        report_id: report.report_id,
                        disposition: ResultDisposition::Accepted,
                        retention_release: true,
                    });
                }
            }
            Ok(())
        })
        .unwrap();
    drop(runtime);
    let path = root.join("journal.json");
    let mut value: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
    value["format_version"] = serde_json::json!(1);
    value.as_object_mut().unwrap().remove("local_drain");
    for record in value["attempts"].as_object_mut().unwrap().values_mut() {
        for key in [
            "ownership_profile",
            "guardian",
            "start_intent",
            "stopped_proof",
            "unstarted_decline",
        ] {
            record.as_object_mut().unwrap().remove(key);
        }
    }
    std::fs::write(path, serde_json::to_vec(&value).unwrap()).unwrap();
    report
}
#[test]
fn v1_retired_and_stopped_reports_upgrade_without_reinterpreting_evidence() {
    for phase in [Phase::Retired, Phase::Reporting] {
        let temp = TempDir::new().unwrap();
        let report = make_v1(temp.path(), phase);
        let old = legacy::Journal::open(
            temp.path(),
            sample_offer().identity.clone(),
            legacy_limits(),
        )
        .unwrap();
        drop(old);
        let mut identity = sample_offer().identity;
        identity.boot_id = Uuid::new_v4();
        let mut runtime = Runtime::new(
            Journal::open(temp.path(), identity, limits()).unwrap(),
            FakeClock::new(200),
        );
        assert_eq!(
            runtime.journal.state().format_version,
            aq_worker::journal::STATE_FORMAT
        );
        assert!(runtime.recover_owned_launches().unwrap().is_empty());
        let record = runtime.journal.state().attempts.values().next().unwrap();
        assert_eq!(record.phase, phase);
        assert_eq!(record.report.as_ref(), Some(&report));
        assert!(record.guardian.is_none() && record.stopped_proof.is_none());
        assert_eq!(runtime.journal.protected_usage().unwrap().slots, 0);
    }
}
#[test]
fn legacy_active_launch_without_guardian_stays_uncertain_even_with_missing_pid() {
    let temp = TempDir::new().unwrap();
    make_v1(temp.path(), Phase::Running);
    let mut identity = sample_offer().identity;
    identity.boot_id = Uuid::new_v4();
    let mut runtime = Runtime::new(
        Journal::open(temp.path(), identity, limits()).unwrap(),
        FakeClock::new(200),
    );
    assert_eq!(
        runtime.recover_owned_launches().unwrap(),
        vec![sample_offer().attempt.attempt_id]
    );
    assert_eq!(runtime.journal.protected_usage().unwrap().slots, 1);
    assert!(
        runtime
            .journal
            .state()
            .attempts
            .values()
            .next()
            .unwrap()
            .stopped_proof
            .is_none()
    );
}
#[test]
fn frozen_v1_reader_refuses_current_format_without_modifying_new_state() {
    let temp = TempDir::new().unwrap();
    let journal = Journal::open(temp.path(), sample_offer().identity, limits()).unwrap();
    drop(journal);
    let original = std::fs::read(temp.path().join("journal.json")).unwrap();
    assert!(legacy::Journal::open(temp.path(), sample_offer().identity, legacy_limits()).is_err());
    assert_eq!(
        std::fs::read(temp.path().join("journal.json")).unwrap(),
        original
    );
    let state: State = serde_json::from_slice(&original).unwrap();
    assert_eq!(state.format_version, aq_worker::journal::STATE_FORMAT);
}
#[test]
fn drain_survives_restart_and_does_not_restore_launch_authority() {
    let temp = TempDir::new().unwrap();
    let offer = sample_offer();
    let mut runtime = Runtime::new(
        Journal::open(temp.path(), offer.identity.clone(), limits()).unwrap(),
        FakeClock::new(100),
    );
    let accepted = runtime.reserve(offer).unwrap();
    runtime
        .receive_directive(&Directive {
            directive_id: Uuid::from_u128(99),
            attempt: accepted.attempt,
            kind: DirectiveKind::Drain,
        })
        .unwrap();
    drop(runtime);
    let mut identity = sample_offer().identity;
    identity.boot_id = Uuid::new_v4();
    let mut runtime = Runtime::new(
        Journal::open(temp.path(), identity, limits()).unwrap(),
        FakeClock::new(200),
    );
    assert!(runtime.recover_owned_launches().unwrap().is_empty());
    assert!(runtime.journal.state().local_drain);
    assert_eq!(
        runtime
            .journal
            .state()
            .attempts
            .values()
            .next()
            .unwrap()
            .stop_certainty,
        StopCertainty::NotStarted
    );
    assert!(runtime.reserve(sample_offer()).is_err());
}
#[test]
fn v2_guardian_report_upgrades_exactly_and_old_reader_refuses_v3_unchanged() {
    let temp = TempDir::new().unwrap();
    let offer = sample_offer();
    let id = offer.attempt.attempt_id;
    let identity = offer.identity.clone();
    let mut runtime = Runtime::new(
        Journal::open(temp.path(), identity.clone(), limits()).unwrap(),
        FakeClock::new(100),
    );
    let mut coordinator = FakeCoordinator::new(offer.clone(), 100);
    let accept = runtime.reserve(offer).unwrap();
    runtime
        .authorize(coordinator.accept(&accept).unwrap().grant, 100)
        .unwrap();
    let child = runtime
        .launch_fixture(
            id,
            std::path::Path::new(env!("CARGO_BIN_EXE_aq-fixture-agent")),
        )
        .unwrap();
    runtime.finish_fixture(id, child).unwrap();
    drop(runtime);
    let file = temp.path().join("journal.json");
    let mut v2: serde_json::Value = serde_json::from_slice(&std::fs::read(&file).unwrap()).unwrap();
    v2["format_version"] = serde_json::json!(2);
    std::fs::write(&file, serde_json::to_vec(&v2).unwrap()).unwrap();
    let frozen = legacy_v2::Journal::open(
        temp.path(),
        identity.clone(),
        legacy_v2::Limits {
            capacity: limits().capacity,
            max_pending_results: 16,
            max_pending_bytes: 4096,
            max_preparing: 16,
            minimum_free_disk_bytes: 0,
        },
    )
    .unwrap();
    drop(frozen);
    let current = Journal::open(temp.path(), identity.clone(), limits()).unwrap();
    assert_eq!(current.state().format_version, 3);
    assert!(current.state().attempts[&id].source.is_none());
    drop(current);
    let bytes = std::fs::read(&file).unwrap();
    let v3: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(v2["attempts"], v3["attempts"]);
    assert!(
        legacy_v2::Journal::open(
            temp.path(),
            identity,
            legacy_v2::Limits {
                capacity: limits().capacity,
                max_pending_results: 16,
                max_pending_bytes: 4096,
                max_preparing: 16,
                minimum_free_disk_bytes: 0
            }
        )
        .is_err()
    );
    assert_eq!(std::fs::read(file).unwrap(), bytes);
}
