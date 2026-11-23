use super::*;
pub(super) struct Work {
    pub(super) id: Uuid,
    pub(super) journal: std::path::PathBuf,
    pub(super) path: std::path::PathBuf,
    pub(super) worker: std::path::PathBuf,
    pub(super) artifact_id: Uuid,
    pub(super) manifest: aq_worker::source::GitManifest,
    pub(super) git: GitClient,
    _guard: aq_worker::source::GitService,
}
impl Work {
    pub(super) fn prepare(fixture: &Fixture, root: &Path) -> (Self, Runtime<FakeClock>) {
        let worker = root.join("worker");
        source_repository::register_worker(&fixture.exe, &worker, fixture.rid);
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
        let out = rt.export_source(id, &git).unwrap();
        let manifest: aq_worker::source::GitManifest =
            serde_json::from_value(out["manifest"].clone()).unwrap();
        rt.prepare_source_upload(id, &git).unwrap();
        let m = rt.journal.state().attempts[&id]
            .source
            .as_ref()
            .unwrap()
            .transfer
            .clone()
            .unwrap();
        (
            Self {
                id,
                journal,
                worker,
                path: work.path,
                artifact_id: m.artifact_id,
                manifest,
                git,
                _guard: guard,
            },
            rt,
        )
    }

    pub(super) async fn finish_upload(
        &self,
        mut rt: Runtime<FakeClock>,
        config: &WorkerConfig,
        state: &Mutex<PeerState>,
    ) -> Runtime<FakeClock> {
        let client = TransferClient::from_config(config, 2).unwrap();
        while let Some(action) = rt.next_source_upload(self.id).unwrap() {
            let result = action.execute(&client).await;
            if matches!(action, UploadAction::Finalize(_)) && !state.lock().unwrap().finalize_lost {
                panic!("finalize did not reach peer");
            }
            match result {
                Ok(status) => rt
                    .acknowledge_source_upload(self.id, &action, status)
                    .unwrap(),
                Err(_) => {
                    rt = self.retry_finalize(rt, action, &client).await;
                }
            }
        }
        assert!(
            rt.retire_source(self.id, &self.git).is_err(),
            "retention is not deletion permission"
        );
        rt
    }

    async fn retry_finalize(
        &self,
        mut rt: Runtime<FakeClock>,
        action: UploadAction,
        client: &TransferClient,
    ) -> Runtime<FakeClock> {
        assert!(matches!(action, UploadAction::Finalize(_)));
        assert!(rt.retire_source(self.id, &self.git).is_err());
        drop(rt);
        rt = runtime(&self.journal);
        let retry = rt.next_source_upload(self.id).unwrap().unwrap();
        match (&action, &retry) {
            (UploadAction::Finalize(a), UploadAction::Finalize(b)) => assert_eq!(a, b),
            _ => panic!("restart changed operation"),
        };
        let status = retry.execute(client).await.unwrap();
        rt.acknowledge_source_upload(self.id, &retry, status)
            .unwrap();
        rt
    }
    pub(super) async fn report_and_retire(
        &self,
        mut rt: Runtime<FakeClock>,
        config: &WorkerConfig,
        state: &Mutex<PeerState>,
    ) {
        let execution = ExecutionClient::from_config(config).unwrap();
        let local_only = rt.journal.state().attempts[&self.id]
            .report
            .clone()
            .unwrap();
        assert!(local_only.supersedes_report_id.is_some());
        assert!(execution.report(&local_only).await.is_err());
        assert!(state.lock().unwrap().report.is_none());
        let report = rt.prepare_source_report(self.id).unwrap();
        assert!(report.supersedes_report_id.is_none());
        let mut unretained = report.clone();
        unretained.report_id = Uuid::new_v4();
        unretained.artifacts[0].central_receipt = None;
        assert!(execution.report(&unretained).await.is_err());
        assert!(state.lock().unwrap().report.is_none());
        assert!(execution.report(&report).await.is_err());
        assert!(state.lock().unwrap().report_lost);
        let mut conflicting = report.clone();
        conflicting.output.push_str("changed after commit");
        assert!(execution.report(&conflicting).await.is_err());
        assert!(rt.retire_source(self.id, &self.git).is_err());
        drop(rt);
        rt = runtime(&self.journal);
        let exact = rt.prepare_source_report(self.id).unwrap();
        assert_eq!(exact, report);
        let response = execution.report(&exact).await.unwrap();
        rt.acknowledge_result(response).unwrap();
        rt.retire_source(self.id, &self.git).unwrap();
        rt.retire(self.id).unwrap();
        assert!(!self.path.exists());
        rt.reclaim_source(self.id, 1, &self.git).unwrap();
        assert!(
            !self
                .worker
                .join("artifacts")
                .join(self.artifact_id.to_string())
                .exists()
        );
    }

    pub(super) fn assert_central_result(&self, fixture: &Fixture) {
        let retained = direct(
            &fixture.exe,
            &fixture.central,
            fixture.rid,
            "workspace.create",
            json!({"base":self.manifest.source}),
            1,
        );
        assert!(
            Path::new(retained["path"].as_str().unwrap())
                .join("result.txt")
                .exists()
        );
    }
}
fn stage_base(fixture: &Fixture, worker: &Path) {
    let artifact = fixture.exported["artifact_handle"].as_str().unwrap();
    let incoming = worker.join("artifacts/incoming").join(artifact);
    std::fs::create_dir_all(&incoming).unwrap();
    for file in ["manifest.json", "objects.bundle"] {
        std::fs::copy(
            fixture.central.join("artifacts").join(artifact).join(file),
            incoming.join(file),
        )
        .unwrap();
    }
    std::fs::write(worker.join("artifact_limits.json"),serde_json::to_vec(&json!({"max_bundle_bytes":64*1024*1024,"max_expanded_bytes":512*1024*1024,"disk_reserve_bytes":16*1024*1024})).unwrap()).unwrap();
}
