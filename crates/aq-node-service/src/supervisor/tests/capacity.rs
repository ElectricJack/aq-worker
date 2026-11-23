use super::*;

#[tokio::test(start_paused = true)]
async fn node_slots_grow_and_shrink_without_interrupting_running_attempts() {
    let mut rig = rig(1);
    rig.layout.node = Some(1);
    let backend = Arc::new(FakeBackend::default());
    let (done, rx) = watch::channel(false);
    for slot in 1..=3 {
        script(&backend, slot, vec![Step::Until(rx.clone())]);
    }
    let ctl = control(vec![slots_reply(1)]);
    let (handle, stop_tx) = start(&rig, &backend, &ctl);
    spin().await;
    assert_eq!(backend.contexts.lock().unwrap().len(), 1);
    *ctl.replies.lock().unwrap() = vec![slots_reply(3)].into();
    tokio::time::advance(CONTROL_INTERVAL).await;
    spin().await;
    assert_eq!(backend.contexts.lock().unwrap().len(), 3);
    let registered = || backend.registrations.lock().unwrap().clone();
    let counts = |sent: &[(Uuid, Uuid, u32)]| sent.iter().map(|r| r.2).collect::<Vec<_>>();
    assert_eq!(
        counts(&registered()),
        [1, 3],
        "the raised count is registered"
    );
    *ctl.replies.lock().unwrap() = vec![slots_reply(1)].into();
    tokio::time::advance(CONTROL_INTERVAL).await;
    spin().await;
    assert!(
        backend
            .contexts
            .lock()
            .unwrap()
            .iter()
            .all(|c| c.state_dir.exists()),
        "shrink keeps running attempts"
    );
    let sent = registered();
    assert_eq!(counts(&sent), [1, 3], "a lowered count registers nothing");
    assert!(
        sent[0].0 == sent[1].0 && sent[0].1 != sent[1].1,
        "one boot, a fresh operation"
    );
    done.send_replace(true);
    spin().await;
    let contexts = backend.contexts.lock().unwrap().clone();
    assert_eq!(contexts.iter().filter(|c| c.slot > 1).count(), 2);
    assert!(
        contexts
            .iter()
            .all(|c| c.node_id == Uuid::from_u128(1)
                && c.enrollment == rig.layout.slot_enrollment(1))
    );
    stop(handle, stop_tx).await;
}
/// A slot the node has no room for (manual sessions hold its capacity) waits for the permit:
/// no state directory, no aq-git and no attempt until there is room, however long it takes.
#[tokio::test(start_paused = true)]
async fn a_slot_without_room_creates_nothing_until_the_node_has_room() {
    let mut rig = rig(1);
    rig.layout.node = Some(1);
    let (room, rx) = watch::channel(false);
    let backend = Arc::new(FakeBackend {
        room: Some(rx),
        ..FakeBackend::default()
    });
    let (done, until) = watch::channel(false);
    for slot in 1..=2 {
        script(&backend, slot, vec![Step::Until(until.clone())]);
    }
    let (handle, stop_tx) = start(&rig, &backend, &control(vec![slots_reply(2)]));
    spin().await;
    tokio::time::advance(Duration::from_secs(60)).await;
    spin().await;
    assert!(backend.contexts.lock().unwrap().is_empty());
    assert!(!(backend.calls.lock().unwrap().iter()).any(|c| c.starts_with("run")));
    for slot in 1..=2 {
        assert!(
            !rig.layout.current(slot).exists(),
            "slot {slot} created its state"
        );
    }
    room.send_replace(true);
    spin().await;
    assert_eq!(backend.contexts.lock().unwrap().len(), 2);
    done.send_replace(true);
    stop_tx.send_replace(true);
    handle.await.unwrap().unwrap();
}
#[tokio::test(start_paused = true)]
async fn two_nodes_each_run_three_independent_attempts() {
    let mut rig = rig(2);
    let backend = Arc::new(FakeBackend::default());
    let (done, rx) = watch::channel(false);
    for slot in 1..=3 {
        script(
            &backend,
            slot,
            vec![Step::Until(rx.clone()), Step::Until(rx.clone())],
        );
    }
    rig.layout.node = Some(1);
    let (a, a_stop) = start(&rig, &backend, &control(vec![slots_reply(3)]));
    rig.layout.node = Some(2);
    let (b, b_stop) = start(&rig, &backend, &control(vec![slots_reply(3)]));
    spin().await;
    let contexts = backend.contexts.lock().unwrap().clone();
    assert_eq!(contexts.len(), 6);
    let identities: HashSet<_> = contexts.iter().map(|c| (c.node_id, c.slot)).collect();
    assert_eq!(identities.len(), 6);
    assert_eq!(
        contexts
            .iter()
            .map(|c| c.git_socket.clone())
            .collect::<HashSet<_>>()
            .len(),
        6
    );
    for ctx in &contexts {
        assert_eq!(
            EnrollmentFile::load(&ctx.enrollment).unwrap().node_id,
            ctx.node_id
        );
    }
    done.send_replace(true);
    spin().await;
    stop(a, a_stop).await;
    stop(b, b_stop).await;
}
#[tokio::test(start_paused = true)]
async fn held_slot_does_not_block_sibling_or_node_registration() {
    let mut rig = rig(1);
    rig.layout.node = Some(1);
    let backend = Arc::new(FakeBackend::default());
    std::fs::create_dir_all(rig.layout.current(1)).unwrap();
    std::fs::write(rig.layout.current(1).join("journal"), "old").unwrap();
    backend
        .recovery
        .lock()
        .unwrap()
        .insert(1, vec![Ok(RecoverOutcome::Held("unknown".into()))].into());
    script(&backend, 2, vec![Step::Observe]);
    let (handle, stop_tx) = start(&rig, &backend, &control(vec![slots_reply(2)]));
    spin().await;
    assert!(!backend.controls.lock().unwrap().is_empty());
    assert_eq!(
        backend
            .contexts
            .lock()
            .unwrap()
            .iter()
            .map(|c| c.slot)
            .collect::<Vec<_>>(),
        vec![2]
    );
    assert!(rig.layout.slot_held(1).exists());
    stop(handle, stop_tx).await;
}
#[tokio::test(start_paused = true)]
async fn lowering_machine_node_count_retires_six_nodes_using_their_own_identity() {
    let mut rig = rig(8);
    rig.config.nodes = 2;
    let backend = Arc::new(FakeBackend::default());
    let mut handles = Vec::new();
    for node in 3..=8 {
        let view = rig.layout.for_node(node);
        std::fs::create_dir_all(view.current(1)).unwrap();
        std::fs::write(view.current(1).join("journal"), "old").unwrap();
        let sup = Supervisor::new(
            rig.config.clone(),
            view,
            backend.clone(),
            control(vec![slots_reply(3)]),
            Arc::new(FakeGit),
        )
        .retiring(true);
        handles.push(tokio::spawn(sup.run(watch::channel(false).1)));
    }
    spin().await;
    for handle in handles {
        handle.await.unwrap().unwrap();
    }
    assert_retired_recovery(&backend);
    assert_eq!(rig.layout.enrolled_nodes(), (1..=8).collect::<Vec<_>>());
    for node in 3..=8 {
        assert!(!rig.layout.for_node(node).current(1).exists());
    }
}
fn assert_retired_recovery(backend: &FakeBackend) {
    assert!(backend.contexts.lock().unwrap().is_empty());
    assert!(
        backend.controls.lock().unwrap().is_empty(),
        "retired nodes never register new boots"
    );
    assert_eq!(
        backend
            .calls
            .lock()
            .unwrap()
            .iter()
            .filter(|c| c.starts_with("recover:"))
            .count(),
        6
    );
}
