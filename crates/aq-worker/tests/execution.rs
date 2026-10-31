use aq_worker::{
    clock::{Clock, FakeClock},
    journal::{Journal, Limits, WriteFault},
    runtime::Runtime,
};
use aq_worker_protocol::{testing::*, *};
use std::{
    path::Path,
    sync::{Arc, Barrier, Mutex},
};
use tempfile::TempDir;
use uuid::Uuid;

fn limits(slots: u32) -> Limits {
    Limits {
        capacity: Resources {
            slots,
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
fn runtime(root: &Path, slots: u32) -> Runtime<FakeClock> {
    Runtime::new(
        Journal::open(root, sample_offer().identity, limits(slots)).unwrap(),
        FakeClock::new(100),
    )
}
fn changed_offer(n: u128) -> Offer {
    let mut offer = sample_offer();
    offer.offer_id = Uuid::from_u128(n);
    offer.attempt.attempt_id = Uuid::from_u128(n + 1);
    offer.attempt.session_id = Uuid::from_u128(n + 2);
    offer.spec.project_id = Uuid::from_u128(n + 3);
    offer.spec = offer.spec.seal();
    offer
}
fn authorize(runtime: &mut Runtime<FakeClock>, offer: Offer) -> (AcceptRequest, FakeCoordinator) {
    let mut peer = FakeCoordinator::new(offer.clone(), runtime.clock.now_ms());
    let req = runtime.reserve(offer).unwrap();
    let grant = peer.accept(&req).unwrap().grant;
    runtime.authorize(grant, runtime.clock.now_ms()).unwrap();
    (req, peer)
}

#[test]
fn competing_projects_share_one_durable_capacity_budget() {
    let temp = TempDir::new().unwrap();
    let rt = Arc::new(Mutex::new(runtime(temp.path(), 1)));
    let barrier = Arc::new(Barrier::new(3));
    let mut threads = vec![];
    for offer in [sample_offer(), changed_offer(100)] {
        let rt = rt.clone();
        let barrier = barrier.clone();
        threads.push(std::thread::spawn(move || {
            barrier.wait();
            rt.lock().unwrap().reserve(offer).is_ok()
        }));
    }
    barrier.wait();
    assert_eq!(
        threads
            .into_iter()
            .map(|t| t.join().unwrap())
            .filter(|ok| *ok)
            .count(),
        1
    );
    let rt = rt.lock().unwrap();
    assert_eq!(rt.journal.protected_usage().unwrap().slots, 1);
    assert!(!temp.path().join("launches").exists());
}
#[test]
fn admission_replay_and_changed_bytes_are_distinct() {
    let temp = TempDir::new().unwrap();
    let mut rt = runtime(temp.path(), 2);
    let offer = sample_offer();
    let req = rt.reserve(offer.clone()).unwrap();
    assert_eq!(rt.reserve(offer.clone()).unwrap(), req);
    let mut changed = offer;
    changed.spec.fixture_output.push('!');
    changed.spec = changed.spec.seal();
    assert!(rt.reserve(changed).is_err());
    assert_eq!(rt.journal.protected_usage().unwrap().slots, 1);
}
#[test]
fn exclusive_owner_and_unknown_state_format_fail_closed() {
    let temp = TempDir::new().unwrap();
    let rt = runtime(temp.path(), 1);
    assert!(Journal::open(temp.path(), sample_offer().identity, limits(1)).is_err());
    drop(rt);
    let p = temp.path().join("journal.json");
    let mut state: serde_json::Value = serde_json::from_slice(&std::fs::read(&p).unwrap()).unwrap();
    state["format_version"] = serde_json::json!(999);
    std::fs::write(p, serde_json::to_vec(&state).unwrap()).unwrap();
    assert!(Journal::open(temp.path(), sample_offer().identity, limits(1)).is_err());
}
#[test]
fn write_failures_never_acknowledge_unpersisted_admission() {
    for fault in [WriteFault::BeforeWrite, WriteFault::AfterFileSync] {
        let temp = TempDir::new().unwrap();
        let mut rt = runtime(temp.path(), 1);
        rt.journal.inject_write_fault(fault);
        assert!(rt.reserve(sample_offer()).is_err());
        assert_eq!(rt.journal.protected_usage().unwrap().slots, 0);
        drop(rt);
        assert_eq!(
            runtime(temp.path(), 1)
                .journal
                .protected_usage()
                .unwrap()
                .slots,
            0
        );
    }
}
#[test]
fn uncertain_directory_sync_poison_requires_recovery_and_preserves_reservation() {
    let temp = TempDir::new().unwrap();
    let mut rt = runtime(temp.path(), 1);
    rt.journal.inject_write_fault(WriteFault::AfterRename);
    assert!(rt.reserve(sample_offer()).is_err());
    assert!(rt.reserve(changed_offer(100)).is_err());
    drop(rt);
    let recovered = runtime(temp.path(), 1);
    assert_eq!(recovered.journal.protected_usage().unwrap().slots, 1);
    assert_eq!(recovered.journal.state().attempts.len(), 1);
}
#[test]
fn lost_acceptance_and_late_response_never_revive_expired_reservation() {
    let temp = TempDir::new().unwrap();
    let mut rt = runtime(temp.path(), 1);
    let offer = sample_offer();
    let mut peer = FakeCoordinator::new(offer.clone(), 100);
    let req = rt.reserve(offer).unwrap();
    let _lost = peer.accept(&req).unwrap();
    rt.clock.advance(10_001);
    let replay = peer.accept(&req).unwrap();
    assert!(rt.authorize(replay.grant, rt.clock.now_ms()).is_err());
    assert!(rt.begin_launch(req.attempt.attempt_id).is_err());
}
#[test]
fn request_send_time_includes_network_delay_and_suspend() {
    let temp = TempDir::new().unwrap();
    let mut rt = runtime(temp.path(), 1);
    let offer = sample_offer();
    let mut peer = FakeCoordinator::new(offer.clone(), 100);
    let req = rt.reserve(offer).unwrap();
    let grant = peer.accept(&req).unwrap().grant;
    let sent = rt.clock.now_ms();
    rt.clock.advance(250);
    rt.authorize(grant, sent).unwrap();
    assert_eq!(
        rt.journal.state().attempts[&req.attempt.attempt_id].grant_deadline_ms,
        Some(10100)
    );
    rt.clock.advance(10000);
    assert_eq!(rt.expire().unwrap(), vec![req.attempt.attempt_id]);
    assert!(rt.begin_launch(req.attempt.attempt_id).is_err());
}
#[test]
fn launch_intent_crash_never_blindly_relaunches_or_releases_writers() {
    let temp = TempDir::new().unwrap();
    let mut rt = runtime(temp.path(), 1);
    let (req, _) = authorize(&mut rt, sample_offer());
    let launch = rt.begin_launch(req.attempt.attempt_id).unwrap();
    assert!(rt.begin_launch(req.attempt.attempt_id).is_err());
    drop(rt);
    let mut identity = sample_offer().identity;
    identity.boot_id = Uuid::from_u128(999);
    let journal = Journal::open(temp.path(), identity, limits(1)).unwrap();
    let mut restarted = Runtime::new(journal, FakeClock::new(200));
    assert!(restarted.journal.state().reconciliation_required);
    let record = &restarted.journal.state().attempts[&req.attempt.attempt_id];
    assert_eq!(record.launch_id, Some(launch));
    assert_eq!(record.stop_certainty, StopCertainty::StopUncertain);
    assert_eq!(restarted.journal.protected_usage().unwrap().slots, 1);
    assert!(restarted.begin_launch(req.attempt.attempt_id).is_err());
    assert!(restarted.reserve(changed_offer(100)).is_err());
}
#[test]
fn actual_fixture_child_completes_once_and_report_retry_is_identical() {
    let temp = TempDir::new().unwrap();
    let mut rt = runtime(temp.path(), 1);
    let (req, mut peer) = authorize(&mut rt, sample_offer());
    let handle = rt
        .launch_fixture(
            req.attempt.attempt_id,
            Path::new(env!("CARGO_BIN_EXE_aq-fixture-agent")),
        )
        .unwrap();
    assert!(rt.retire(req.attempt.attempt_id).is_err());
    let report = rt.finish_fixture(req.attempt.attempt_id, handle).unwrap();
    assert_eq!(report.outcome, ProcessOutcome::Success);
    assert_eq!(report.output, "synthetic result\n");
    assert_eq!(report.stop_certainty, StopCertainty::ConfirmedStopped);
    assert_eq!(rt.journal.protected_usage().unwrap().slots, 0);
    assert_eq!(rt.journal.protected_usage().unwrap().disk_bytes, 1024);
    let response = peer.report(&report).unwrap();
    assert_eq!(response, peer.report(&report).unwrap());
    rt.acknowledge_result(response).unwrap();
    rt.retire(req.attempt.attempt_id).unwrap();
    assert_eq!(rt.journal.protected_usage().unwrap().disk_bytes, 0);
    assert!(
        rt.launch_fixture(
            req.attempt.attempt_id,
            Path::new(env!("CARGO_BIN_EXE_aq-fixture-agent"))
        )
        .is_err()
    );
    assert_eq!(
        std::fs::read_dir(temp.path().join("launches"))
            .unwrap()
            .count(),
        0
    );
}
#[test]
fn stopped_slots_do_not_bypass_pending_result_storage_bounds() {
    let temp = TempDir::new().unwrap();
    let mut bound = limits(1);
    bound.max_pending_results = 1;
    let journal = Journal::open(temp.path(), sample_offer().identity, bound).unwrap();
    let mut rt = Runtime::new(journal, FakeClock::new(100));
    let (req, _) = authorize(&mut rt, sample_offer());
    let handle = rt
        .launch_fixture(
            req.attempt.attempt_id,
            Path::new(env!("CARGO_BIN_EXE_aq-fixture-agent")),
        )
        .unwrap();
    rt.finish_fixture(req.attempt.attempt_id, handle).unwrap();
    assert_eq!(rt.journal.available().unwrap().slots, 1);
    assert!(rt.reserve(changed_offer(100)).is_err());
}
#[test]
fn observations_and_directive_receipts_cross_durable_boundary() {
    let temp = TempDir::new().unwrap();
    let mut rt = runtime(temp.path(), 1);
    let (req, _) = authorize(&mut rt, sample_offer());
    let obs = rt
        .observation(req.attempt.attempt_id, "ready".into())
        .unwrap();
    let directive = Directive {
        directive_id: Uuid::from_u128(100),
        attempt: req.attempt.clone(),
        kind: DirectiveKind::Cancel,
    };
    let ack = rt.receive_directive(&directive).unwrap();
    assert_eq!(ack, rt.receive_directive(&directive).unwrap());
    let mut changed = directive;
    changed.kind = DirectiveKind::Drain;
    assert!(rt.receive_directive(&changed).is_err());
    drop(rt);
    let rt = runtime(temp.path(), 1);
    let record = &rt.journal.state().attempts[&req.attempt.attempt_id];
    assert_eq!(record.pending_observations.get(&1), Some(&obs));
    assert_eq!(record.phase, Phase::Stopping);
    assert_eq!(
        rt.journal.state().directive_receipts.values().next(),
        Some(&ack)
    );
}
#[test]
fn unsupported_capabilities_and_forged_grants_are_rejected() {
    let temp = TempDir::new().unwrap();
    let mut rt = runtime(temp.path(), 1);
    let mut offer = sample_offer();
    offer.spec.required_capabilities.push("unknown".into());
    offer.spec = offer.spec.seal();
    assert!(rt.reserve(offer).is_err());
    let offer = sample_offer();
    let mut peer = FakeCoordinator::new(offer.clone(), 100);
    let req = rt.reserve(offer).unwrap();
    let mut grant = peer.accept(&req).unwrap().grant;
    grant.attempt.generation += 1;
    assert!(rt.authorize(grant, 100).is_err());
    assert!(rt.begin_launch(req.attempt.attempt_id).is_err());
}

#[test]
fn directive_receipts_are_bounded_and_exact_replay_survives_the_bound() {
    let temp = TempDir::new().unwrap();
    let mut rt = runtime(temp.path(), 1);
    let (req, _) = authorize(&mut rt, sample_offer());
    let first = Directive {
        directive_id: Uuid::from_u128(1000),
        attempt: req.attempt.clone(),
        kind: DirectiveKind::Cancel,
    };
    let original = rt.receive_directive(&first).unwrap();
    for n in 1..aq_worker::journal::MAX_DIRECTIVES_PER_ATTEMPT {
        rt.receive_directive(&Directive {
            directive_id: Uuid::from_u128(1000 + n as u128),
            attempt: req.attempt.clone(),
            kind: DirectiveKind::Cancel,
        })
        .unwrap();
    }
    assert!(
        rt.receive_directive(&Directive {
            directive_id: Uuid::from_u128(9999),
            attempt: req.attempt,
            kind: DirectiveKind::Cancel
        })
        .is_err()
    );
    assert_eq!(rt.receive_directive(&first).unwrap(), original);
    assert_eq!(
        rt.journal.state().directive_receipts.len(),
        aq_worker::journal::MAX_DIRECTIVES_PER_ATTEMPT
    );
}

#[test]
fn oversized_journal_is_refused_before_deserialization() {
    let temp = TempDir::new().unwrap();
    let rt = runtime(temp.path(), 1);
    drop(rt);
    std::fs::OpenOptions::new()
        .write(true)
        .open(temp.path().join("journal.json"))
        .unwrap()
        .set_len(aq_worker::journal::MAX_JOURNAL_BYTES + 1)
        .unwrap();
    assert!(Journal::open(temp.path(), sample_offer().identity, limits(1)).is_err());
}

#[test]
fn orderly_journal_release_does_not_wait_for_an_unrelated_fork_child() {
    use std::{
        io::{Read, Write},
        os::{fd::AsRawFd, unix::net::UnixStream},
    };

    // Model the guardian capability probe's raw fork while another test/runtime
    // owns a journal. CLOEXEC does not close the inherited lock before exec.
    // The child performs only async-signal-safe syscalls and holds its inherited
    // descriptors until the parent explicitly releases it; no scheduler timing
    // or sleeps determine whether the lock inheritance occurs.
    struct Child {
        pid: libc::pid_t,
        control: UnixStream,
    }
    impl Drop for Child {
        fn drop(&mut self) {
            let _ = self.control.write_all(&[1]);
            let mut status = 0;
            loop {
                let result = unsafe { libc::waitpid(self.pid, &mut status, 0) };
                if result != -1
                    || std::io::Error::last_os_error().raw_os_error() != Some(libc::EINTR)
                {
                    break;
                }
            }
        }
    }

    let temp = TempDir::new().unwrap();
    let rt = runtime(temp.path(), 1);
    let (parent, child) = UnixStream::pair().unwrap();
    let parent_fd = parent.as_raw_fd();
    let child_fd = child.as_raw_fd();
    let pid = unsafe { libc::fork() };
    assert!(pid >= 0, "fork: {}", std::io::Error::last_os_error());
    if pid == 0 {
        unsafe {
            libc::close(parent_fd);
            let ready = [1u8];
            if libc::write(child_fd, ready.as_ptr().cast(), 1) != 1 {
                libc::_exit(90);
            }
            let mut release = [0u8];
            let result = libc::read(child_fd, release.as_mut_ptr().cast(), 1);
            libc::_exit(if result == 1 { 0 } else { 91 });
        }
    }
    drop(child);
    let mut holder = Child {
        pid,
        control: parent,
    };
    let mut ready = [0u8];
    holder.control.read_exact(&mut ready).unwrap();
    assert_eq!(ready, [1]);
    assert!(Journal::open(temp.path(), sample_offer().identity, limits(1)).is_err());
    drop(rt);
    // The unrelated child is still blocked with its inherited descriptor open.
    let reopened = Journal::open(temp.path(), sample_offer().identity, limits(1));
    assert!(
        reopened.is_ok(),
        "orderly owner release: {:?}",
        reopened.err()
    );
}

#[test]
fn profiled_tmux_launch_remains_refused_until_authority_integration() {
    let temp = TempDir::new().unwrap();
    let mut rt = runtime(temp.path(), 1);
    let mut offer = sample_offer();
    offer
        .spec
        .required_capabilities
        .push("aq.profiled-launch".into());
    offer
        .spec
        .required_capabilities
        .push(aq_session_host::HOST_PROFILE.into());
    offer.spec = offer.spec.seal();
    assert!(rt.reserve(offer).is_err());
    assert!(rt.journal.state().attempts.is_empty());
}
