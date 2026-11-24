use super::*;

#[test]
fn recorded_handoff_reads_the_marker_of_the_attempt() {
    let rig = rig(1);
    let (a, b) = (Uuid::from_u128(1), Uuid::from_u128(2));
    let (da, _db) = (driver(&rig, 1, a), driver(&rig, 1, b));
    let (host, state) = (rig.layout.host_root(1), rig.layout.current(1));
    assert_eq!(recorded_handoff(&host, &state, a), None);
    assert!(mark_handoff(&rig.layout, &[1], a, HandoffReason::ServiceStop).unwrap());
    assert!(da.join("handoff").exists());
    assert_eq!(
        [a, b, Uuid::from_u128(3)].map(|id| recorded_handoff(&host, &state, id)),
        [Some(HandoffReason::ServiceStop), None, None]
    );
}

#[tokio::test(start_paused = true)]
async fn starts_one_loop_per_slot_and_archives_each_attempt() {
    let rig = rig(2);
    let backend = Arc::new(FakeBackend::default());
    for slot in 1..=2 {
        script(
            &backend,
            slot,
            vec![
                Step::Outcome(AttemptOutcome::AttemptStopped),
                Step::Outcome(AttemptOutcome::ShutdownBeforeAdmission),
            ],
        );
    }
    let (handle, tx) = start(&rig, &backend, &control(vec![reply(vec![], None)]));
    sleep(Duration::from_secs(60)).await;
    assert!(!handle.is_finished(), "the service outlives its slot loops");
    stop(handle, tx).await;
    for slot in 1..=2 {
        assert_eq!(count(rig.layout.archive_dir(slot)), 2);
        assert!(!rig.layout.current(slot).exists());
    }
}

#[tokio::test(start_paused = true)]
async fn recovers_existing_current_before_new_work() {
    let rig = rig(1);
    std::fs::create_dir_all(rig.layout.current(1)).unwrap();
    std::fs::write(rig.layout.current(1).join("journal"), b"j").unwrap();
    let backend = Arc::new(FakeBackend::default());
    script(
        &backend,
        1,
        vec![Step::Outcome(AttemptOutcome::ShutdownBeforeAdmission)],
    );
    let (handle, tx) = start(&rig, &backend, &control(vec![reply(vec![], None)]));
    sleep(Duration::from_secs(1)).await;
    stop(handle, tx).await;
    assert_eq!(*backend.calls.lock().unwrap(), ["recover:1", "run:1"]);
}

#[tokio::test(start_paused = true)]
async fn held_recovery_disables_only_that_slot() {
    let rig = rig(2);
    for slot in 1..=2 {
        std::fs::create_dir_all(rig.layout.current(slot)).unwrap();
        std::fs::write(rig.layout.current(slot).join("journal"), b"j").unwrap();
    }
    let backend = Arc::new(FakeBackend::default());
    backend.recovery.lock().unwrap().insert(
        1,
        [Ok(RecoverOutcome::Held("sandbox_absent".into()))].into(),
    );
    let (handle, tx) = start(&rig, &backend, &control(vec![reply(vec![], None)]));
    sleep(Duration::from_secs(1)).await;
    stop(handle, tx).await;
    let calls = backend.calls.lock().unwrap().clone();
    assert!(
        calls.contains(&"run:2".to_string()) && !calls.contains(&"run:1".to_string()),
        "{calls:?}"
    );
    assert!(
        rig.layout.current(1).join("journal").exists(),
        "a held slot keeps its state"
    );
    // R1: the holding reason is recorded for `service status`, only for the held slot.
    assert_eq!(
        [1, 2].map(|slot| rig.layout.held_reason(slot)),
        [Some("sandbox_absent".into()), None]
    );
}

#[tokio::test(start_paused = true)]
async fn owner_handoff_wakes_held_slot_and_rechecks_journal() {
    let rig = rig(1);
    journal(&rig, 1);
    let backend = Arc::new(FakeBackend::default());
    backend.recovery.lock().unwrap().insert(
        1,
        [
            Ok(RecoverOutcome::Held("sandbox_absent".into())),
            Ok(RecoverOutcome::Salvaged),
        ]
        .into(),
    );
    script(&backend, 1, vec![Step::Hang]);
    let (handle, tx) = start(&rig, &backend, &control(vec![reply(vec![], None)]));
    sleep(Duration::from_secs(1)).await;
    assert_eq!(rig.layout.held_reason(1).as_deref(), Some("sandbox_absent"));
    write_private(&rig.layout.recovery_recorded(1), b"reported").unwrap();
    sleep(CONTROL_INTERVAL + Duration::from_secs(1)).await;
    assert_eq!(
        *backend.calls.lock().unwrap(),
        ["recover:1", "recover:1", "run:1"]
    );
    assert_eq!(rig.layout.held_reason(1), None);
    assert!(!rig.layout.recovery_recorded(1).exists());
    stop(handle, tx).await;
}

/// R1: an attempt that failed in preparation stays for recovery; recovery settles it at once
/// (the backend reports it never started), the slot is archived, comes up again (it polls
/// after its pause), and a holding reason left by an earlier hold is cleared.
#[tokio::test(start_paused = true)]
async fn a_failed_preparation_is_settled_and_the_slot_polls_again() {
    let rig = rig(1);
    let backend = Arc::new(FakeBackend {
        retained_empty: true,
        ..Default::default()
    });
    write_private(&rig.layout.slot_held(1), b"journal_inconsistent: old").unwrap();
    script(&backend, 1, vec![Step::ErrorAdmitted, Step::Hang]);
    let settled = [Ok(RecoverOutcome::Reported)];
    backend.recovery.lock().unwrap().insert(1, settled.into());
    let (handle, tx) = start(&rig, &backend, &control(vec![reply(vec![], None)]));
    sleep(Duration::from_secs(1)).await;
    assert_preparation_settled(&rig, &backend);
    assert_eq!(backend.run_times.lock().unwrap().len(), 1, "no poll yet");
    sleep(Duration::from_secs(30)).await;
    assert_eq!(backend.run_times.lock().unwrap().len(), 2, "the next poll");
    assert_eq!(rig.layout.held_reason(1), None);
    stop(handle, tx).await;
}

fn assert_preparation_settled(rig: &Rig, backend: &FakeBackend) {
    assert_eq!(
        rig.layout.held_reason(1),
        None,
        "cleared when the slot settles"
    );
    assert_eq!(
        *backend.calls.lock().unwrap(),
        ["run:1", "recover:1", "run:1"],
        "recovered once, then up again"
    );
    assert_eq!(count(rig.layout.archive_dir(1)), 1, "settled and archived");
}

#[tokio::test(start_paused = true)]
async fn handoff_marker_written_when_driver_dir_appears_later() {
    let rig = rig(1);
    let backend = Arc::new(FakeBackend::default());
    script(&backend, 1, vec![Step::Hang]);
    let attempt = Uuid::from_u128(77);
    let (handle, tx) = start(&rig, &backend, &control(vec![reply(vec![attempt], None)]));
    sleep(Duration::from_secs(1)).await;
    assert!(
        !rig.layout.host_root(1).read_dir().unwrap().any(|_| true),
        "no driver dir yet"
    );
    let dir = driver(&rig, 1, attempt);
    assert!(!dir.join("handoff").exists());
    sleep(Duration::from_secs(5)).await;
    let marker: serde_json::Value =
        serde_json::from_slice(&std::fs::read(dir.join("handoff")).unwrap()).unwrap();
    assert_eq!(
        marker,
        serde_json::json!({"schema_version": 1, "reason": "owner_stop"})
    );
    stop(handle, tx).await;
}

#[tokio::test(start_paused = true)]
async fn failing_slot_backs_off_and_prunes_empty_dirs() {
    let rig = rig(1);
    let backend = Arc::new(FakeBackend {
        retained_empty: true,
        ..Default::default()
    });
    script(
        &backend,
        1,
        vec![
            Step::Error,
            Step::Error,
            Step::Error,
            Step::Error,
            Step::Hang,
        ],
    );
    let (handle, tx) = start(&rig, &backend, &control(vec![reply(vec![], None)]));
    sleep(Duration::from_secs(40)).await;
    assert_eq!(
        count(rig.layout.archive_dir(1)),
        0,
        "empty archives are pruned"
    );
    sleep(Duration::from_secs(500)).await;
    let times = backend.run_times.lock().unwrap().clone();
    let gaps: Vec<u64> = times.windows(2).map(|w| (w[1] - w[0]).as_secs()).collect();
    assert_eq!(gaps, [30, 60, 120, 240]);
    assert_eq!(count(rig.layout.archive_dir(1)), 0);
    stop(handle, tx).await;
}

#[tokio::test(start_paused = true)]
async fn shutdown_hands_off_running_attempts_then_waits() {
    let rig = rig(1);
    let backend = Arc::new(FakeBackend::default());
    script(&backend, 1, vec![Step::Hang]);
    let ctl = control(vec![reply(vec![], None)]);
    let (handle, tx) = start(&rig, &backend, &ctl);
    sleep(Duration::from_secs(1)).await;
    let attempt = Uuid::from_u128(5);
    let dir = driver(&rig, 1, attempt);
    let signalled_at = Instant::now();
    tx.send_replace(true);
    sleep(Duration::from_secs(1)).await;
    assert_eq!(
        ctl.handoffs
            .lock()
            .unwrap()
            .iter()
            .map(|(slot, n)| (*slot, n.attempt_id, n.reason))
            .collect::<Vec<_>>(),
        [(1, attempt, HandoffReason::ServiceStop)]
    );
    let marker: serde_json::Value =
        serde_json::from_slice(&std::fs::read(dir.join("handoff")).unwrap()).unwrap();
    assert_eq!(marker["reason"], "service_stop");
    assert!(
        backend.released.lock().unwrap().is_none(),
        "the slot keeps running through the grace period"
    );
    handle.await.unwrap().unwrap();
    assert!(backend.released.lock().unwrap().unwrap() - signalled_at >= Duration::from_secs(10));
}

#[tokio::test(start_paused = true)]
async fn revoked_credential_pauses_all_slots() {
    let rig = rig(1);
    let backend = Arc::new(FakeBackend::default());
    script(&backend, 1, vec![Step::Observe, Step::Hang]);
    let ctl = control(vec![reply(vec![], None), Err((401, true))]);
    let (handle, tx) = start(&rig, &backend, &ctl);
    sleep(Duration::from_secs(16)).await;
    assert_eq!(ctl.polls.lock().unwrap().len(), 4);
    assert!(*backend.observed.lock().unwrap().clone().unwrap().borrow());
    sleep(Duration::from_secs(49)).await;
    assert_eq!(
        ctl.polls.lock().unwrap().len(),
        4,
        "the next poll waits 60 s"
    );
    sleep(Duration::from_secs(11)).await;
    assert_eq!(ctl.polls.lock().unwrap().len(), 5);
    assert_eq!(
        backend.calls.lock().unwrap().len(),
        1,
        "no attempt starts while paused"
    );
    stop(handle, tx).await;
}

/// Review #1: an attempt that fails after admission keeps current/journal, and the next
/// round recovers it (handoff notice and salvage) before new work, which waits out the
/// back-off of the failure.
#[tokio::test(start_paused = true)]
async fn failed_admitted_attempt_stays_for_recovery() {
    let rig = rig(1);
    let backend = Arc::new(FakeBackend {
        retained_empty: true,
        ..Default::default()
    });
    script(&backend, 1, vec![Step::ErrorAdmitted, Step::Hang]);
    let (handle, tx) = start(&rig, &backend, &control(vec![reply(vec![], None)]));
    sleep(Duration::from_secs(1)).await;
    assert_eq!(
        *backend.calls.lock().unwrap(),
        ["run:1", "recover:1", "run:1"]
    );
    assert_eq!(
        count(rig.layout.archive_dir(1)),
        1,
        "archived after recovery"
    );
    let starts = backend.starts.lock().unwrap().clone();
    assert_eq!(starts[1].1, BACKOFF_START, "new work waits the back-off");
    assert_eq!(backend.run_times.lock().unwrap().len(), 1);
    stop(handle, tx).await;
}

/// Review #2: with every slot held the service does not return (systemd would restart it
/// every 10 s); it keeps polling control and never recovers the held slots again.
#[tokio::test(start_paused = true)]
async fn all_slots_held_keeps_the_service_running() {
    let rig = rig(2);
    let backend = Arc::new(FakeBackend::default());
    for slot in 1..=2 {
        journal(&rig, slot);
        let held = Ok(RecoverOutcome::Held("sandbox_absent".into()));
        backend.recovery.lock().unwrap().insert(slot, [held].into());
    }
    let ctl = control(vec![reply(vec![], None)]);
    let (handle, tx) = start(&rig, &backend, &ctl);
    sleep(Duration::from_secs(3600)).await;
    assert!(!handle.is_finished());
    let mut calls = backend.calls.lock().unwrap().clone();
    calls.sort();
    assert_eq!(calls, ["recover:1", "recover:2"]);
    assert!(
        ctl.polls.lock().unwrap().len() > 700,
        "control keeps polling"
    );
    assert!(ctl.handoffs.lock().unwrap().is_empty());
    stop(handle, tx).await;
}

/// Review #3: a recovery error other than Held is retried with backoff; the slot then runs.
#[tokio::test(start_paused = true)]
async fn transient_recovery_error_retries_then_runs() {
    let rig = rig(1);
    journal(&rig, 1);
    // R1: a reason left by an earlier hold is not shown while recovery retries.
    write_private(&rig.layout.slot_held(1), b"journal_unreadable: node: old").unwrap();
    let backend = Arc::new(FakeBackend::default());
    let outcomes = [Err(()), Ok(RecoverOutcome::Salvaged)];
    backend.recovery.lock().unwrap().insert(1, outcomes.into());
    script(&backend, 1, vec![Step::Hang]);
    let (handle, tx) = start(&rig, &backend, &control(vec![reply(vec![], None)]));
    sleep(Duration::from_secs(29)).await;
    assert_eq!(*backend.calls.lock().unwrap(), ["recover:1"]);
    assert_eq!(rig.layout.held_reason(1), None, "retrying, not held");
    sleep(Duration::from_secs(2)).await;
    assert_eq!(
        *backend.calls.lock().unwrap(),
        ["recover:1", "recover:1", "run:1"]
    );
    stop(handle, tx).await;
}

/// Review M1: setting a flag to its current value wakes nobody.
#[test]
fn pause_notifies_only_on_change() {
    let pause = Pause {
        tx: watch::channel(false).0,
        flags: Mutex::new([false; 3]),
    };
    let mut rx = pause.tx.subscribe();
    pause.set(STOPPING, false);
    assert!(!rx.has_changed().unwrap());
    pause.set(STOPPING, true);
    assert!(rx.has_changed().unwrap() && *rx.borrow_and_update());
    pause.set(STOPPING, true);
    pause.set(REVOKED, true);
    pause.set(STOPPING, false);
    assert!(!rx.has_changed().unwrap(), "still paused by REVOKED");
    pause.set(REVOKED, false);
    assert!(rx.has_changed().unwrap() && !*rx.borrow_and_update());
}

/// Review M5: an edge's redirect or bare 403 backs the control poll off without pausing;
/// AQ's own 403 is a revoke.
#[tokio::test(start_paused = true)]
async fn edge_refusal_backs_off_without_pausing() {
    for (status, aq) in [(302, false), (403, false)] {
        let rig = rig(1);
        let backend = Arc::new(FakeBackend::default());
        script(&backend, 1, vec![Step::Observe, Step::Hang]);
        let ctl = control(vec![reply(vec![], None), Err((status, aq))]);
        let (handle, tx) = start(&rig, &backend, &ctl);
        sleep(Duration::from_secs(1000)).await;
        assert!(
            !*backend.observed.lock().unwrap().clone().unwrap().borrow(),
            "{status}"
        );
        let times = ctl.polls.lock().unwrap().clone();
        let gaps: Vec<u64> = times.windows(2).map(|w| (w[1] - w[0]).as_secs()).collect();
        assert_eq!(gaps[..7], [5, 10, 20, 40, 80, 160, 300], "{status}");
        stop(handle, tx).await;
    }
    let rig = rig(1);
    let backend = Arc::new(FakeBackend::default());
    script(&backend, 1, vec![Step::Observe, Step::Hang]);
    let ctl = control(vec![reply(vec![], None), Err((403, true))]);
    let (handle, tx) = start(&rig, &backend, &ctl);
    sleep(Duration::from_secs(16)).await;
    assert!(*backend.observed.lock().unwrap().clone().unwrap().borrow());
    stop(handle, tx).await;
}

/// D2: the service is killed while an attempt runs (no stop marker, no archive, its aq-git
/// killed and its socket left behind). The restarted service starts aq-git again on the
/// slot's state, recovers (salvages) the attempt, archives it and takes new work.
#[tokio::test]
async fn a_killed_service_salvages_the_running_attempt_on_restart() {
    let mut rig = rig(1);
    let scripts = tempfile::tempdir().unwrap();
    rig.config.aq_git = crate::git::tests::fake_aq_git(scripts.path(), None);
    let git = || Arc::new(AqGitFactory(rig.config.aq_git.clone()));
    async fn until(what: &str, done: impl Fn() -> bool) {
        for _ in 0..400 {
            if done() {
                return;
            }
            sleep(Duration::from_millis(25)).await;
        }
        panic!("timed out waiting for {what}");
    }
    let backend = Arc::new(FakeBackend::default());
    script(&backend, 1, vec![Step::Running]);
    let ctl = control(vec![reply(vec![], None)]);
    let (handle, _tx) = start_with(&rig, &backend, &ctl, git());
    let socket = rig.layout.git_socket(1);
    until("the attempt", || backend.calls.lock().unwrap().len() == 1).await;
    until("its driver", || {
        rig.layout.host_root(1).join("driver-1").exists()
    })
    .await;
    assert!(std::os::unix::net::UnixStream::connect(&socket).is_ok());
    // SIGKILL: nothing of the graceful stop runs; the aq-git child dies with the service.
    handle.abort();
    let _ = handle.await;
    until("aq-git to die", || {
        std::os::unix::net::UnixStream::connect(&socket).is_err()
    })
    .await;
    assert!(socket.exists(), "the killed aq-git left its socket");
    assert!(rig.layout.current(1).join("journal").exists());

    let backend = Arc::new(FakeBackend::default());
    let salvaged = [Ok(RecoverOutcome::Salvaged)];
    backend.recovery.lock().unwrap().insert(1, salvaged.into());
    script(&backend, 1, vec![Step::Hang]);
    let (handle, tx) = start_with(&rig, &backend, &ctl, git());
    until("recovery and new work", || {
        backend.calls.lock().unwrap().len() == 2
    })
    .await;
    assert_eq!(*backend.calls.lock().unwrap(), ["recover:1", "run:1"]);
    assert_eq!(
        count(rig.layout.archive_dir(1)),
        1,
        "archived after salvage"
    );
    assert!(
        !rig.layout.host_root(1).join("driver-1").exists(),
        "a fresh runtime dir"
    );
    tx.send_replace(true);
    handle.await.unwrap().unwrap();
}

/// After a reboot the runtime dir (the host root and its driver markers) is gone: settle
/// still releases the claimed sandboxes (the backend reads which are preserved from the
/// journal), then archives.
#[tokio::test(start_paused = true)]
async fn settle_releases_claimed_sandboxes_after_the_runtime_dir_is_wiped() {
    let rig = rig(1);
    journal(&rig, 1);
    assert!(
        !rig.layout.host_root(1).exists(),
        "no runtime dir (after a reboot)"
    );
    let backend = Arc::new(FakeBackend::default());
    let salvaged = [Ok(RecoverOutcome::Salvaged)];
    backend.recovery.lock().unwrap().insert(1, salvaged.into());
    script(&backend, 1, vec![Step::Hang]);
    let (handle, tx) = start(&rig, &backend, &control(vec![reply(vec![], None)]));
    sleep(Duration::from_secs(1)).await;
    assert_eq!(*backend.calls.lock().unwrap(), ["recover:1", "run:1"]);
    assert_eq!(
        backend.releases.lock().unwrap()[0],
        1,
        "released before the archive"
    );
    assert_eq!(count(rig.layout.archive_dir(1)), 1);
    stop(handle, tx).await;
}

/// D2 ruling C: a lost attempt released to the server frees its slot: it is archived (its
/// record kept) and the slot takes new work.
#[tokio::test(start_paused = true)]
async fn a_released_lost_attempt_frees_the_slot() {
    let rig = rig(1);
    journal(&rig, 1);
    let backend = Arc::new(FakeBackend::default());
    backend
        .recovery
        .lock()
        .unwrap()
        .insert(1, [Ok(RecoverOutcome::Lost)].into());
    script(&backend, 1, vec![Step::Hang]);
    let (handle, tx) = start(&rig, &backend, &control(vec![reply(vec![], None)]));
    sleep(Duration::from_secs(1)).await;
    assert_eq!(*backend.calls.lock().unwrap(), ["recover:1", "run:1"]);
    assert_eq!(count(rig.layout.archive_dir(1)), 1, "kept in the archive");
    stop(handle, tx).await;
}
