use super::*;

/// Review M4: a SIGTERM while an attempt prepares (no driver.json yet) marks the slot, so
/// the restarted node reports the attempt as a service stop, not a crash.
#[tokio::test(start_paused = true)]
async fn stop_during_preparation_marks_the_slot_as_service_stop() {
    let rig = rig(1);
    let backend = Arc::new(FakeBackend::default());
    script(&backend, 1, vec![Step::Hang]);
    let (handle, tx) = start(&rig, &backend, &control(vec![reply(vec![], None)]));
    sleep(Duration::from_secs(1)).await;
    let (host, current) = (rig.layout.host_root(1), rig.layout.current(1));
    assert_eq!(recorded_handoff(&host, &current, Uuid::from_u128(9)), None);
    tx.send_replace(true);
    sleep(Duration::from_secs(1)).await;
    assert_eq!(
        recorded_handoff(&host, &current, Uuid::from_u128(9)),
        Some(HandoffReason::ServiceStop)
    );
    handle.await.unwrap().unwrap();
}

/// A held slot's state is not marked as a service stop: it was not interrupted by this stop.
#[tokio::test(start_paused = true)]
async fn held_slot_is_not_marked_on_stop() {
    let rig = rig(1);
    journal(&rig, 1);
    let backend = Arc::new(FakeBackend::default());
    let held = Ok(RecoverOutcome::Held("sandbox_absent".into()));
    backend.recovery.lock().unwrap().insert(1, [held].into());
    let (handle, tx) = start(&rig, &backend, &control(vec![reply(vec![], None)]));
    sleep(Duration::from_secs(1)).await;
    stop(handle, tx).await;
    assert!(!rig.layout.current(1).join(SERVICE_STOP_MARKER).exists());
}

/// Round 2, A: claimed sandboxes are released after every attempt; when that fails the
/// attempt stays in current and the next round's settle releases it before archiving.
#[tokio::test(start_paused = true)]
async fn claimed_sandboxes_are_released_after_the_attempt_or_by_settle() {
    let rig = rig(1);
    let backend = Arc::new(FakeBackend::default());
    *backend.release_failures.lock().unwrap() = 1;
    script(&backend, 1, vec![Step::ErrorAdmitted, Step::Hang]);
    let (handle, tx) = start(&rig, &backend, &control(vec![reply(vec![], None)]));
    sleep(Duration::from_secs(1)).await;
    assert_eq!(
        *backend.releases.lock().unwrap(),
        [1, 1],
        "after the attempt (failed), then by settle before it archives"
    );
    assert_eq!(
        *backend.calls.lock().unwrap(),
        ["run:1", "recover:1", "run:1"]
    );
    assert_eq!(count(rig.layout.archive_dir(1)), 1);
    stop(handle, tx).await;
    // A finished attempt whose release fails is kept too.
    let rig = self::rig(1);
    let backend = Arc::new(FakeBackend::default());
    *backend.release_failures.lock().unwrap() = 1;
    script(&backend, 1, vec![Step::Finished, Step::Hang]);
    let (handle, tx) = start(&rig, &backend, &control(vec![reply(vec![], None)]));
    sleep(Duration::from_secs(1)).await;
    // Kept, then settled at once (a finished attempt resets the backoff).
    assert_eq!(
        *backend.calls.lock().unwrap(),
        ["run:1", "recover:1", "run:1"]
    );
    assert_eq!(*backend.releases.lock().unwrap(), [1, 1]);
    assert_eq!(count(rig.layout.archive_dir(1)), 1);
    stop(handle, tx).await;
}

/// Round 2, C: a stop during the recovery backoff does not mark the attempt that already
/// failed: it was not stopped by the service, so its crash stays charged.
#[tokio::test(start_paused = true)]
async fn stop_during_recovery_backoff_does_not_mark_the_failed_attempt() {
    let rig = rig(1);
    let backend = Arc::new(FakeBackend::default());
    script(&backend, 1, vec![Step::ErrorAdmitted, Step::Hang]);
    backend.recovery.lock().unwrap().insert(1, [Err(())].into());
    let ctl = control(vec![reply(vec![Uuid::from_u128(42)], None)]);
    let (handle, tx) = start(&rig, &backend, &ctl);
    sleep(Duration::from_secs(6)).await;
    stop(handle, tx).await;
    let (host, current) = (rig.layout.host_root(1), rig.layout.current(1));
    assert_recoverable_without_marker(&current, &host);
    assert!(ctl.handoffs.lock().unwrap().is_empty());
    assert_eq!(recorded_handoff(&host, &current, Uuid::from_u128(42)), None);
}

/// Round 2, F: a notice post that never answers cannot hold the stop past the grace plus
/// the stop margin (systemd's TimeoutStopSec).
#[tokio::test(start_paused = true)]
async fn a_hanging_handoff_post_stays_inside_the_stop_budget() {
    let rig = rig(1);
    let backend = Arc::new(FakeBackend::default());
    script(&backend, 1, vec![Step::Hang]);
    let ctl = control(vec![reply(vec![], None)]);
    *ctl.hang_handoffs.lock().unwrap() = true;
    let (handle, tx) = start(&rig, &backend, &ctl);
    sleep(Duration::from_secs(1)).await;
    driver(&rig, 1, Uuid::from_u128(5));
    let signalled = Instant::now();
    stop(handle, tx).await;
    let budget = Duration::from_millis(rig.config.shutdown_grace_ms) + STOP_MARGIN;
    assert!(signalled.elapsed() < budget, "{:?}", signalled.elapsed());
    assert!(
        backend.released.lock().unwrap().is_some(),
        "the slot was forced"
    );
}

/// Round 3: every running attempt is marked before any notice is posted, so a hung post
/// for one slot does not delay another slot's marker.
#[tokio::test(start_paused = true)]
async fn every_stop_marker_precedes_the_first_post() {
    let rig = rig(2);
    let backend = Arc::new(FakeBackend::default());
    for slot in 1..=2 {
        script(&backend, slot, vec![Step::Hang]);
    }
    let ctl = control(vec![reply(vec![], None)]);
    *ctl.hang_handoffs.lock().unwrap() = true;
    let (handle, tx) = start(&rig, &backend, &ctl);
    sleep(Duration::from_secs(1)).await;
    let dirs = [
        driver(&rig, 1, Uuid::from_u128(1)),
        driver(&rig, 2, Uuid::from_u128(2)),
    ];
    tx.send_replace(true);
    sleep(Duration::from_millis(100)).await;
    assert_eq!(
        ctl.handoffs.lock().unwrap().len(),
        1,
        "the first post hangs"
    );
    assert!(
        dirs.iter().all(|d| d.join("handoff").exists()),
        "both marked"
    );
    handle.await.unwrap().unwrap();
}
