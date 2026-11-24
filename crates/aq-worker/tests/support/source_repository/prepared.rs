use super::*;
pub(crate) struct PreparedSource {
    pub(crate) id: Uuid,
    pub(crate) worker: std::path::PathBuf,
    pub(crate) work: aq_worker::source::Workspace,
    pub(crate) checkpoint: aq_worker::source::Checkpoint,
    pub(crate) guard: aq_worker::source::GitService,
    pub(crate) journal: std::path::PathBuf,
}
impl PreparedSource {
    pub(crate) fn new(fixture: &Fixture, root: &Path) -> (Self, Runtime<FakeClock>) {
        let worker = root.join("worker");
        register_worker(&fixture.exe, &worker, fixture.rid);
        stage_base(fixture, &worker);
        let exe = &fixture.exe;
        assert!(aq_worker::source::GitService::start(exe, &"0".repeat(64), &worker).is_err());
        let guard = aq_worker::source::GitService::start(exe, &fixture.pin, &worker).unwrap();
        let git = guard.client.clone();
        let journal = root.join("journal");
        let mut rt = runtime(&journal);
        let mut o = offer();
        o.spec.bindings.as_mut().unwrap().source = Some(fixture.binding.clone());
        o.spec = o.spec.seal();
        let id = o.attempt.attempt_id;
        let mut coordinator = FakeCoordinator::new(o.clone(), 100);
        let a = rt.reserve_source(o, &git).unwrap();
        rt.authorize(coordinator.accept(&a).unwrap().grant, 100)
            .unwrap();
        let artifact = fixture.exported["artifact_handle"].as_str().unwrap();
        rt.import_base(id, Uuid::parse_str(artifact).unwrap(), &git)
            .unwrap();
        let work = rt.prepare_source(id, &git).unwrap();
        std::fs::write(work.path.join("result.txt"), "retained source").unwrap();
        let h = rt
            .launch_fixture(id, Path::new(env!("CARGO_BIN_EXE_aq-fixture-agent")))
            .unwrap();
        rt.finish_fixture(id, h).unwrap();
        let checkpoint = rt.checkpoint_source(id, &git).unwrap();
        assert_ne!(checkpoint.source.hex, fixture.binding.base.hex);
        (
            Self {
                id,
                worker,
                work,
                checkpoint,
                guard,
                journal,
            },
            rt,
        )
    }
}
fn stage_base(fixture: &Fixture, worker: &Path) {
    let artifact = fixture.exported["artifact_handle"].as_str().unwrap();
    copy_artifact(&fixture.central, worker, artifact);
    std::fs::write(worker.join("artifact_limits.json"),serde_json::to_vec(&json!({"max_bundle_bytes":64*1024*1024,"max_expanded_bytes":512*1024*1024,"disk_reserve_bytes":16*1024*1024})).unwrap()).unwrap();
}
