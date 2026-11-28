use super::*;

fn secs(times: &[Instant], since: Instant) -> Vec<u64> {
    times.iter().map(|t| (*t - since).as_secs()).collect()
}
/// The laptop's loop: an offer the server refused (409) cost the slot 30 s to 300 s with no
/// report. Now the slot is settled and up again at once, reports while it waits the short
/// pause, and takes the next offer; the control poll keeps its schedule throughout.
#[tokio::test(start_paused = true)]
async fn a_refused_offer_frees_the_slot_after_the_short_pause() {
    let rig = rig(1);
    let backend = Arc::new(FakeBackend {
        retained_empty: true,
        ..Default::default()
    });
    script(&backend, 1, vec![Step::Fail(REFUSED), Step::Hang]);
    let ctl = control(vec![reply(vec![], None)]);
    let began = Instant::now();
    let (handle, tx) = start(&rig, &backend, &ctl);
    sleep(Duration::from_secs(12)).await;
    let starts = backend.starts.lock().unwrap().clone();
    let pause = Duration::from_millis(rig.config.retry.offer_pause_ms);
    assert_eq!(pause, Duration::from_secs(3), "the default");
    assert_eq!(
        starts.iter().map(|s| s.1).collect::<Vec<_>>(),
        [Duration::ZERO, pause],
        "up again at once, asking for no offer during the pause"
    );
    assert_eq!(starts[1].0, starts[0].0, "it reports from the failure on");
    let polled = backend.run_times.lock().unwrap().clone();
    assert_eq!(
        secs(&polled, starts[0].0),
        [0, 3],
        "the next offer is taken"
    );
    assert_eq!(secs(&ctl.polls.lock().unwrap(), began), [0, 5, 10]);
    assert_eq!(count(rig.layout.archive_dir(1)), 0, "nothing was admitted");
    stop(handle, tx).await;
}
/// Both branches at once: a node with two agent slots, where the offer of slot 1 is refused
/// (409) while slot 2 runs an attempt. Slot 2 is not interrupted, the node is registered
/// once under one boot, its control poll and its single inventory stream go on (slot 1
/// reports into it while it waits), and slot 1 takes the next offer after the short pause.
#[tokio::test(start_paused = true)]
async fn a_refused_offer_on_one_slot_leaves_its_sibling_and_the_node_as_they_were() {
    let mut rig = rig(1);
    rig.layout.node = Some(1);
    let backend = Arc::new(FakeBackend {
        retained_empty: true,
        ..Default::default()
    });
    let (done, rx) = watch::channel(false);
    script(
        &backend,
        1,
        vec![Step::Fail(REFUSED), Step::Until(rx.clone())],
    );
    script(&backend, 2, vec![Step::Until(rx)]);
    let ctl = control(vec![slots_reply(2)]);
    let began = Instant::now();
    let (handle, stop_tx) = start(&rig, &backend, &ctl);
    sleep(Duration::from_secs(12)).await;
    let contexts = backend.contexts.lock().unwrap().clone();
    let starts = backend.starts.lock().unwrap().clone();
    let of = |slot: u32| -> Vec<(u64, u64)> {
        let mine = contexts.iter().zip(&starts).filter(|(c, _)| c.slot == slot);
        mine.map(|(_, (at, hold))| ((*at - began).as_secs(), hold.as_secs()))
            .collect()
    };
    assert_eq!(of(1), [(0, 0), (0, 3)], "up at once, holding the 3 s");
    assert_eq!(of(2), [(0, 0)], "the sibling was started once");
    assert_sibling_identity(&rig, &contexts);
    let polled = backend.run_times.lock().unwrap().clone();
    assert_eq!(
        secs(&polled, began),
        [0, 0, 3],
        "slot 1 takes an offer after the pause"
    );
    assert_node_inventory(&backend, &ctl, began);
    done.send_replace(true);
    spin().await;
    stop(handle, stop_tx).await;
}
fn assert_sibling_identity(rig: &Rig, contexts: &[SlotContext]) {
    let running = contexts.iter().find(|c| c.slot == 2).unwrap();
    assert!(running.state_dir.exists(), "and still runs its attempt");
    assert_ne!(running.state_dir, contexts[0].state_dir);
    assert_ne!(running.git_socket, contexts[0].git_socket);
    assert!(
        contexts
            .iter()
            .all(|c| c.node_id == Uuid::from_u128(1)
                && c.enrollment == rig.layout.slot_enrollment(1)),
        "one node identity"
    );
}

fn assert_node_inventory(backend: &FakeBackend, ctl: &FakeControl, began: Instant) {
    use std::sync::atomic::Ordering::SeqCst;
    let session = &backend.session;
    assert_eq!(
        *backend.registrations.lock().unwrap(),
        [(session.boot_id, session.registration_id, 2)],
        "registered once, under one boot"
    );
    assert_eq!(secs(&ctl.polls.lock().unwrap(), began), [0, 5, 10]);
    assert_eq!(backend.controls.lock().unwrap().len(), 3);
    let reports = backend.reports.lock().unwrap().clone();
    let sequences: Vec<u64> = reports.iter().map(|r| r.1).collect();
    assert_eq!(
        sequences,
        (1..=reports.len() as u64).collect::<Vec<_>>(),
        "one inventory stream for the node"
    );
    let waiting: Vec<u64> = (reports.iter().filter(|r| r.0 == 1))
        .map(|r| (r.2 - began).as_secs())
        .collect();
    assert_eq!(waiting, [0, 0], "slot 1 reported again as it began to wait");
    assert_eq!(session.inventory.acknowledged.load(SeqCst), 3);
}

/// Repeats of the same kind double the pause up to the configured ceiling; another kind
/// starts at the short pause again, and so does the first failure after a finished attempt.
#[tokio::test(start_paused = true)]
async fn repeated_refusals_stop_at_the_configured_ceiling_and_reset() {
    let mut rig = rig(1);
    rig.config.retry.offer_pause_max_ms = 20_000;
    let backend = Arc::new(FakeBackend {
        retained_empty: true,
        ..Default::default()
    });
    let source = "aq-git refused: \"repository_not_found\" \"Repository is not registered\"";
    let mut steps: Vec<Step> = (0..6).map(|_| Step::Fail(REFUSED)).collect();
    steps.extend([Step::Fail(source), Step::Fail(source), Step::Finished]);
    steps.extend([Step::Fail(REFUSED), Step::Hang]);
    script(&backend, 1, steps);
    let (handle, tx) = start(&rig, &backend, &control(vec![reply(vec![], None)]));
    sleep(Duration::from_secs(300)).await;
    let times = backend.run_times.lock().unwrap().clone();
    let gaps: Vec<u64> = times.windows(2).map(|w| (w[1] - w[0]).as_secs()).collect();
    assert_eq!(gaps, [3, 6, 12, 20, 20, 20, 3, 6, 0, 3]);
    stop(handle, tx).await;
}
/// A fault of this machine keeps the long back-off, and the slot reports during it. A
/// slot that cannot even stay up (it fails as it starts) waits here instead, without
/// reporting, and is not started again before the back-off has passed.
#[tokio::test(start_paused = true)]
async fn a_local_fault_backs_off_long_and_a_slot_that_cannot_stay_up_waits_outside() {
    let rig = rig(2);
    let backend = Arc::new(FakeBackend {
        retained_empty: true,
        ..Default::default()
    });
    let sandbox = "Claude driver host did not establish Running";
    script(&backend, 1, vec![Step::Fail(sandbox), Step::Hang]);
    script(
        &backend,
        2,
        vec![Step::Error, Step::Down, Step::Error, Step::Hang],
    );
    let ctl = control(vec![reply(vec![], None)]);
    let began = Instant::now();
    let (handle, tx) = start(&rig, &backend, &ctl);
    sleep(Duration::from_secs(100)).await;
    let calls = backend.calls.lock().unwrap().clone();
    let starts = backend.starts.lock().unwrap().clone();
    let of = |slot: &str| -> Vec<(u64, u64)> {
        let mine = calls.iter().zip(&starts).filter(|(c, _)| c.ends_with(slot));
        mine.map(|(_, (at, hold))| ((*at - began).as_secs(), hold.as_secs()))
            .collect()
    };
    assert_eq!(of(":1"), [(0, 0), (0, 30)], "up and reporting for the 30 s");
    assert_eq!(of(":2"), [(0, 0), (0, 30), (30, 0), (90, 0)]);
    let polls = secs(&ctl.polls.lock().unwrap(), began);
    assert_eq!(polls, (0..20).map(|n| n * 5).collect::<Vec<u64>>());
    stop(handle, tx).await;
}
/// The texts are the ones the service logged on the laptop and the desktop.
#[test]
fn failures_are_classified_from_their_text() {
    use FailureKind::*;
    for (text, kind) in [
        (REFUSED, Refused),
        ("native endpoint returned 503 Service Unavailable", Refused),
        (
            "source transfer endpoint returned 503 Service Unavailable",
            Refused,
        ),
        ("native endpoint returned 413 Payload Too Large", Refused),
        (
            "aq-git refused: \"repository_not_found\" \"Repository is not registered\"",
            Offer,
        ),
        ("native source bulk lane overloaded", Offer),
        (
            "native control transport failed; disposition unknown",
            Unreachable,
        ),
        ("native endpoint returned 401 Unauthorized", Fault),
        ("native endpoint returned 500 Internal Server Error", Fault),
        ("Claude driver host did not establish Running", Fault),
        ("No space left on device (os error 28)", Fault),
    ] {
        assert_eq!(FailureKind::of(text), kind, "{text}");
    }
}
