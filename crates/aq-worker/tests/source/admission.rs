#[test]
fn source_admission_is_demand_only_and_socket_loss_replays_after_restart() {
    let p = peer(true);
    let temp = TempDir::new().unwrap();
    let mut rt = runtime(temp.path());
    let o = offer();
    let id = o.attempt.attempt_id;
    assert!(rt.reserve(o.clone()).is_err());
    authorize(&mut rt, &p, o);
    assert_no_git_request(&p, "workspace.create");
    assert!(rt.prepare_source(id, &p.client).is_err());
    assert_eq!(rt.journal.state().attempts[&id].phase, Phase::Preparing);
    assert_eq!(rt.journal.protected_usage().unwrap().slots, 1);
    drop(rt);
    let mut rt = runtime(temp.path());
    let first = rt.prepare_source(id, &p.client).unwrap();
    assert_eq!(first.workspace_id, Uuid::from_u128(70));
    assert_create_replayed(&p);
    assert!(rt.retire(id).is_err());
}
fn assert_create_replayed(p: &Peer) {
    let requests = p.requests.lock().unwrap();
    let creates: Vec<_> = requests
        .iter()
        .filter(|r| r["operation"] == "workspace.create")
        .collect();
    assert_eq!(creates.len(), 2);
    assert_eq!(creates[0], creates[1]);
}

fn assert_no_git_request(p: &Peer, operation: &str) {
    assert!(
        p.requests
            .lock()
            .unwrap()
            .iter()
            .all(|r| r["operation"] != operation)
    );
}

#[test]
fn guardian_stopped_writers_gate_checkpoint_and_replay() {
    let p = peer(false);
    let temp = TempDir::new().unwrap();
    let mut rt = runtime(temp.path());
    let o = offer();
    let id = o.attempt.attempt_id;
    authorize(&mut rt, &p, o);
    rt.prepare_source(id, &p.client).unwrap();
    assert!(rt.checkpoint_source(id, &p.client).is_err());
    let handle = rt
        .launch_fixture(id, Path::new(env!("CARGO_BIN_EXE_aq-fixture-agent")))
        .unwrap();
    rt.finish_fixture(id, handle).unwrap();
    assert_same_checkpoint(&mut rt, id, &p);
    assert!(rt.retire_source(id, &p.client).is_err());
    assert!(
        rt.journal.state().attempts[&id]
            .source
            .as_ref()
            .unwrap()
            .receipt
            .is_none()
    );
    assert_eq!(rt.journal.available().unwrap().slots, 2);
    assert!(rt.journal.protected_usage().unwrap().disk_bytes > 0);
}
fn assert_same_checkpoint(rt: &mut Runtime<FakeClock>, id: Uuid, p: &Peer) {
    let cp = rt.checkpoint_source(id, &p.client).unwrap();
    assert_eq!(cp.source.hex, "b".repeat(40));
    assert_eq!(
        rt.checkpoint_source(id, &p.client)
            .unwrap()
            .retention_handle,
        cp.retention_handle
    );
}

#[test]
fn binding_conflicts_and_pending_disk_pressure_refuse_new_work() {
    let p = peer(false);
    let temp = TempDir::new().unwrap();
    let mut rt = runtime(temp.path());
    let mut o = offer();
    o.spec
        .bindings
        .as_mut()
        .unwrap()
        .source
        .as_mut()
        .unwrap()
        .binding_generation = 2;
    o.spec = o.spec.seal();
    assert!(rt.reserve_source(o, &p.client).is_err());
    let mut o = offer();
    o.spec.resources.disk_bytes = 100001;
    o.spec = o.spec.seal();
    assert!(rt.reserve_source(o, &p.client).is_err());
    assert!(rt.journal.state().attempts.is_empty());
    let o = offer();
    let a = rt.reserve_source(o.clone(), &p.client).unwrap();
    assert_eq!(rt.reserve_source(o.clone(), &p.client).unwrap(), a);
    let mut changed = o;
    changed
        .spec
        .bindings
        .as_mut()
        .unwrap()
        .source
        .as_mut()
        .unwrap()
        .base
        .hex = "c".repeat(40);
    changed.spec = changed.spec.seal();
    assert!(rt.reserve_source(changed, &p.client).is_err());
}
#[test]
fn shared_cache_is_charged_once_for_multiple_projects_and_growth_stays_protected() {
    let p = peer(false);
    let temp = TempDir::new().unwrap();
    let mut rt = runtime(temp.path());
    let first = offer();
    let first_id = first.attempt.attempt_id;
    authorize(&mut rt, &p, first.clone());
    let mut second = offer();
    second.offer_id = Uuid::new_v4();
    second.attempt.attempt_id = Uuid::new_v4();
    second.attempt.session_id = Uuid::new_v4();
    second.spec.project_id = Uuid::new_v4();
    second.spec = second.spec.seal();
    rt.reserve_source(second, &p.client).unwrap();
    assert_eq!(
        rt.journal.protected_usage().unwrap().disk_bytes,
        2 * first.spec.resources.disk_bytes + 512 * 1024 * 1024
    );
    let workspace = rt.prepare_source(first_id, &p.client).unwrap();
    let h = rt
        .launch_fixture(first_id, Path::new(env!("CARGO_BIN_EXE_aq-fixture-agent")))
        .unwrap();
    rt.finish_fixture(first_id, h).unwrap();
    let file = std::fs::File::create(workspace.path.join("oversize-sparse-file")).unwrap();
    file.set_len(2 * 1024 * 1024 * 1024).unwrap();
    assert!(rt.checkpoint_source(first_id, &p.client).is_err());
    assert_workspace_growth(&rt, first_id);
    assert_no_git_request(&p, "workspace.checkpoint");
}
fn assert_workspace_growth(rt: &Runtime<FakeClock>, first_id: Uuid) {
    assert!(
        rt.journal.state().attempts[&first_id]
            .source
            .as_ref()
            .unwrap()
            .workspace_bytes
            >= 2 * 1024 * 1024 * 1024
    );
    assert!(rt.journal.protected_usage().unwrap().disk_bytes > 3 * 1024 * 1024 * 1024);
}
