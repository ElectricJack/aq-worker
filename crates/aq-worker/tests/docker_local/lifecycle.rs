// ---------------------------------------------------------------------------
// Material delivery, runner start, bounded reads, stop and destroy proofs.

#[tokio::test]
async fn material_is_delivered_by_archive_only() {
    let mut h = harness_with(ScriptedDocker::new(), |config| {
        config.material_uid = 1000;
        config.material_gid = 1001;
    })
    .await;
    let request = request(1, 2, 3);
    let sandbox = bounded(h.backend.create(&request)).await.unwrap();
    let secret = b"top-secret-material-0123";
    bounded(
        h.backend
            .deliver_private_material(&sandbox, PrivateMaterial::new(secret.to_vec())),
    )
    .await
    .unwrap();
    let container = h.docker.container(&request.name).unwrap();
    let file = &container.layer["/aq/private/material"];
    assert_eq!(file.data, secret);
    assert_eq!((file.mode, file.uid, file.gid), (0o600, 1000, 1001));
    assert!(h.docker.requests().contains(&(
        DockerMethod::Put,
        format!("/v1.44/containers/{}/archive?path=%2F", request.name)
    )));
    let needle = String::from_utf8_lossy(secret).to_string();
    assert!(
        !container.body.to_string().contains(&needle),
        "never env or argv"
    );
    for (_, path) in h.docker.requests() {
        assert!(!path.contains(&needle));
    }
    bounded(h.backend.start_runner(&sandbox)).await.unwrap();
    assert_eq!(
        bounded(
            h.backend
                .deliver_private_material(&sandbox, PrivateMaterial::new(secret.to_vec()))
        )
        .await,
        Err(BackendError::InvalidState),
        "material goes in before the runner starts"
    );
}

#[tokio::test]
async fn preparation_rechecks_a_timed_out_read_without_replaying_the_upload() {
    let mut h = harness_with(ScriptedDocker::new(), |config| config.api_timeout_ms = 100).await;
    let request = request(1, 2, 3);
    let sandbox = bounded(h.backend.create(&request)).await.unwrap();
    h.docker.clear_requests();
    let inspect = format!("/v1.44/containers/{}/json", request.name);
    h.docker.script(Rule {
        method: DockerMethod::Get,
        path_prefix: inspect.clone(),
        effect: Effect::StallBeforeApply,
    });
    bounded(
        h.backend
            .deliver_private_material(&sandbox, PrivateMaterial::new(b"material".to_vec())),
    )
    .await
    .unwrap();
    let requests = h.docker.requests();
    assert_eq!(
        requests
            .iter()
            .filter(|(method, path)| *method == DockerMethod::Get && path == &inspect)
            .count(),
        2
    );
    assert_eq!(
        requests
            .iter()
            .filter(|(method, _)| *method == DockerMethod::Put)
            .count(),
        1
    );
}

#[tokio::test]
async fn exact_running_sandbox_recovers_a_lost_start_reply_without_a_second_start() {
    let mut h = harness_with(ScriptedDocker::new().runner(Runner::StaysRunning), |_| {}).await;
    let request = request(1, 2, 3);
    let sandbox = bounded(h.backend.create(&request)).await.unwrap();
    bounded(
        h.backend
            .deliver_private_material(&sandbox, PrivateMaterial::new(b"material".to_vec())),
    )
    .await
    .unwrap();
    h.docker.clear_requests();
    let start = format!("/v1.44/containers/{}/start", request.name);
    h.docker.script(Rule {
        method: DockerMethod::Post,
        path_prefix: start.clone(),
        effect: Effect::DropReplyAfterApply,
    });
    bounded(h.backend.start_runner(&sandbox)).await.unwrap();
    assert_eq!(h.docker.container(&request.name).unwrap().status, "running");
    assert_eq!(
        h.docker
            .requests()
            .iter()
            .filter(|(method, path)| *method == DockerMethod::Post && path == &start)
            .count(),
        1
    );
}

#[tokio::test]
async fn ambiguous_start_with_only_created_evidence_sends_one_start() {
    let mut h = harness_with(ScriptedDocker::new(), |config| config.api_timeout_ms = 100).await;
    let request = request(1, 2, 3);
    let sandbox = bounded(h.backend.create(&request)).await.unwrap();
    h.docker.clear_requests();
    let start = format!("/v1.44/containers/{}/start", request.name);
    h.docker.script(Rule {
        method: DockerMethod::Post,
        path_prefix: start.clone(),
        effect: Effect::StallBeforeApply,
    });
    assert_eq!(
        bounded(h.backend.start_runner(&sandbox)).await,
        Err(BackendError::Uncertain(UnconfirmedReason::Timeout))
    );
    assert_eq!(h.docker.container(&request.name).unwrap().status, "created");
    assert_eq!(
        h.docker
            .requests()
            .iter()
            .filter(|(method, path)| *method == DockerMethod::Post && path == &start)
            .count(),
        1
    );
}

#[tokio::test]
async fn start_runner_and_bounded_read() {
    let mut h = harness_with(ScriptedDocker::new(), |config| config.max_read_bytes = 1024).await;
    let request = request(1, 2, 3);
    let sandbox = bounded(h.backend.create(&request)).await.unwrap();
    bounded(
        h.backend
            .deliver_private_material(&sandbox, PrivateMaterial::new(b"m".to_vec())),
    )
    .await
    .unwrap();
    bounded(h.backend.start_runner(&sandbox)).await.unwrap();
    assert_eq!(
        bounded(h.backend.read_file(&sandbox, "/aq/work/fixture-output"))
            .await
            .unwrap(),
        b"fixture-complete\n"
    );
    assert_eq!(
        bounded(h.backend.read_file(&sandbox, "/aq/work/missing")).await,
        Err(BackendError::NotFound)
    );
    let cap = Err(BackendError::InvalidRequest("archive exceeds read cap"));
    h.docker
        .put_file(&request.name, "/aq/work/big", vec![7; 1025]);
    assert_eq!(
        bounded(h.backend.read_file(&sandbox, "/aq/work/big")).await,
        cap
    );
    h.docker
        .put_file(&request.name, "/aq/work/huge", vec![7; 1024 * 1024]);
    assert_eq!(
        bounded(h.backend.read_file(&sandbox, "/aq/work/huge")).await,
        cap
    );
}

#[tokio::test]
async fn runner_exit_reports_the_observed_exit_code() {
    let mut h = harness_with(ScriptedDocker::new().runner(Runner::ExitsWith(3)), |_| {}).await;
    let sandbox = bounded(h.backend.create(&request(1, 2, 3))).await.unwrap();
    assert_eq!(
        bounded(h.backend.runner_exit(&sandbox)).await,
        Err(BackendError::InvalidState),
        "a created, unstarted runner has no exit"
    );
    bounded(h.backend.start_runner(&sandbox)).await.unwrap();
    assert_eq!(
        bounded(h.backend.inspect(&sandbox)).await.unwrap().state,
        SandboxState::Stopped
    );
    assert_eq!(
        bounded(h.backend.runner_exit(&sandbox)).await,
        Ok(RunnerExit::Exited(3))
    );
}

#[tokio::test]
async fn stop_is_proven_by_inspect() {
    let mut h = harness_with(ScriptedDocker::new().runner(Runner::StaysRunning), |_| {}).await;
    let running = request(1, 2, 3);
    let sandbox = bounded(h.backend.create(&running)).await.unwrap();
    bounded(h.backend.start_runner(&sandbox)).await.unwrap();
    assert_eq!(
        bounded(h.backend.inspect(&sandbox)).await.unwrap().state,
        SandboxState::Running
    );
    h.docker.script(Rule {
        method: DockerMethod::Post,
        path_prefix: format!("/v1.44/containers/{}/stop", running.name),
        effect: Effect::LieOkWithoutApply,
    });
    assert_eq!(
        bounded(h.backend.stop(&sandbox)).await,
        Confirmation::Unconfirmed(UnconfirmedReason::StillRunning),
        "a 204 is not proof; inspect is"
    );
    let proof = bounded(h.backend.stop(&sandbox)).await;
    assert!(
        matches!(&proof, Confirmation::Confirmed(p) if p.kind() == WriterProofKind::StoppedNoProcesses && p.sandbox() == &sandbox),
        "{proof:?}"
    );
    assert_eq!(h.docker.container(&running.name).unwrap().status, "exited");
    // Already exited: 304 is still confirmed by inspect.
    assert!(bounded(h.backend.stop(&sandbox)).await.is_confirmed());
    let entry = bounded(h.backend.inspect(&sandbox)).await.unwrap();
    assert!(entry.disk_present);
    assert_eq!(
        reconcile(
            None,
            Some(&entry),
            &RetentionState::Unretained,
            Deadline::Active
        ),
        ReconcileAction::Preserve(sandbox.clone())
    );

    // Never started: stopped without being started; the volume stays.
    let idle = request(1, 2, 4);
    let idle_sandbox = bounded(h.backend.create(&idle)).await.unwrap();
    h.docker.clear_requests();
    assert!(bounded(h.backend.stop(&idle_sandbox)).await.is_confirmed());
    assert!(
        !h.docker
            .requests()
            .iter()
            .any(|(_, path)| path.ends_with("/start")),
    );
    assert!(h.docker.volume(&format!("{}-work", idle.name)).is_some());
}

#[tokio::test]
async fn destroy_requires_authorization_and_proves_absence() {
    let mut h = harness().await;
    let request = request(1, 2, 3);
    let sandbox = bounded(h.backend.create(&request)).await.unwrap();
    let other = SandboxIdentity {
        kind: BackendKind::DockerLocal,
        backend_id: self::request(1, 2, 4).name,
        owner: owner(1, 2, 4),
    };
    let wrong = DestroyAuthorization::operator_released(other, "test").unwrap();
    h.docker.clear_requests();
    assert_eq!(
        bounded(h.backend.destroy(&sandbox, &wrong)).await,
        Confirmation::Unconfirmed(UnconfirmedReason::Unauthorized)
    );
    assert!(h.docker.requests().is_empty());

    let release = DestroyAuthorization::operator_released(sandbox.clone(), "test").unwrap();
    h.docker.script(Rule {
        method: DockerMethod::Delete,
        path_prefix: "/v1.44/volumes/".into(),
        effect: Effect::LieOkWithoutApply,
    });
    assert!(
        matches!(
            bounded(h.backend.destroy(&sandbox, &release)).await,
            Confirmation::Unconfirmed(_)
        ),
        "a 204 DELETE with the volume still present is not destroyed"
    );
    assert!(h.docker.volume(&format!("{}-work", request.name)).is_some());

    let proof = bounded(h.backend.destroy(&sandbox, &release)).await;
    assert!(
        matches!(&proof, Confirmation::Confirmed(p) if p.kind() == WriterProofKind::Destroyed && p.sandbox() == &sandbox),
        "{proof:?}"
    );
    assert!(h.docker.container_names().is_empty());
    assert!(h.docker.volume_names().is_empty());
    assert_eq!(
        bounded(h.backend.inspect(&sandbox)).await,
        Err(BackendError::NotFound)
    );
}

#[tokio::test]
async fn unreachable_daemon_is_unknown_never_gone() {
    let mut h = harness_with(
        ScriptedDocker::new().runner(Runner::StaysRunning),
        |config| {
            config.api_timeout_ms = 100;
        },
    )
    .await;
    let request = request(1, 2, 3);
    let sandbox = bounded(h.backend.create(&request)).await.unwrap();
    bounded(h.backend.start_runner(&sandbox)).await.unwrap();
    let release = DestroyAuthorization::operator_released(sandbox.clone(), "test").unwrap();
    h.docker.clear_requests();
    h.docker.pause();
    let timeout = BackendError::Uncertain(UnconfirmedReason::Timeout);
    assert_eq!(
        bounded(h.backend.inspect(&sandbox)).await,
        Err(timeout.clone())
    );
    assert_eq!(
        bounded(h.backend.list_owned(request.owner.node_id)).await,
        Err(timeout)
    );
    let unconfirmed = Confirmation::Unconfirmed(UnconfirmedReason::Timeout);
    assert_eq!(bounded(h.backend.stop(&sandbox)).await, unconfirmed);
    assert_eq!(
        bounded(h.backend.destroy(&sandbox, &release)).await,
        unconfirmed
    );
    h.docker.resume();
    assert!(
        deletes(&h.docker).is_empty(),
        "no destroy while unreachable"
    );
    assert_eq!(h.docker.container(&request.name).unwrap().status, "running");
}

/// Conformance fixture over the scripted double: faults are injected at the
/// API boundary and the fixture output is read back through the archive API.
struct ScriptedFixture {
    harness: Harness,
    retained: BTreeMap<SandboxIdentity, Vec<u8>>,
}
impl BackendFixture for ScriptedFixture {
    type Backend = DockerLocalBackend<ScriptedDocker>;
    fn backend(&mut self) -> &mut Self::Backend {
        &mut self.harness.backend
    }
    fn request(&self, case: u128) -> CreateRequest {
        request(1, 2, case + 10)
    }
    fn material(&self) -> PrivateMaterial {
        PrivateMaterial::new(b"fixture-private-material".to_vec())
    }
    fn inject(&mut self, fault: Fault) {
        let (method, path_prefix, effect) = match fault {
            Fault::CrashAfterCreate => (
                DockerMethod::Post,
                "/v1.44/containers/create",
                Effect::DropReplyAfterApply,
            ),
            Fault::DestroyFailure => (
                DockerMethod::Delete,
                "/v1.44/containers/",
                Effect::FailBefore(500),
            ),
            other => panic!("scripted fixture does not inject {other:?}"),
        };
        self.harness.docker.script(Rule {
            method,
            path_prefix: path_prefix.into(),
            effect,
        });
    }
    async fn observe_fixture(&mut self, sandbox: &SandboxIdentity) {
        let output = self
            .harness
            .backend
            .read_file(sandbox, "/aq/work/fixture-output")
            .await
            .unwrap();
        assert_eq!(output, b"fixture-complete\n");
    }
    async fn verify_and_retain(&mut self, sandbox: &SandboxIdentity) -> DestroyAuthorization {
        self.observe_fixture(sandbox).await;
        let output = self
            .harness
            .backend
            .read_file(sandbox, "/aq/work/fixture-output")
            .await
            .unwrap();
        self.retained.insert(sandbox.clone(), output);
        assert_eq!(self.retained[sandbox], b"fixture-complete\n");
        DestroyAuthorization::retained(
            sandbox.clone(),
            format!("scripted-retention/{}", sandbox.backend_id),
        )
        .unwrap()
    }
}

#[tokio::test]
async fn scripted_conformance() {
    let mut fixture = ScriptedFixture {
        harness: harness().await,
        retained: BTreeMap::new(),
    };
    bounded(conformance::run_all(&mut fixture)).await;
    assert!(fixture.harness.docker.container_names().is_empty());
    assert!(fixture.harness.docker.volume_names().is_empty());
}

// ---------------------------------------------------------------------------
// WSL suspend/resume: a paused daemon is unknown, never gone.

#[tokio::test]
async fn wsl_suspend_resume_reconciles_sandboxes() {
    let mut h = harness_with(
        ScriptedDocker::new().runner(Runner::StaysRunning),
        |config| {
            config.api_timeout_ms = 100;
        },
    )
    .await;
    let request = request(1, 2, 3);
    let mut journal = LifecycleRecord::intent(BackendKind::DockerLocal, request.clone());
    let sandbox = bounded(h.backend.create(&request)).await.unwrap();
    journal.record_created(sandbox.clone()).unwrap();
    bounded(
        h.backend
            .deliver_private_material(&sandbox, PrivateMaterial::new(b"m".to_vec())),
    )
    .await
    .unwrap();
    bounded(h.backend.start_runner(&sandbox)).await.unwrap();
    let release = DestroyAuthorization::operator_released(sandbox.clone(), "test").unwrap();
    h.docker.clear_requests();

    h.docker.pause();
    suspended_is_unknown(&mut h.backend, &sandbox, &release).await;
    assert!(deletes(&h.docker).is_empty(), "no destroy during the pause");
    h.docker.resume();

    resumed_reconciles(&mut h.backend, &journal, &sandbox).await;
    assert!(
        h.docker
            .volume_names()
            .contains(&format!("{}-work", request.name))
    );
    assert!(bounded(h.backend.stop(&sandbox)).await.is_confirmed());
    assert!(
        bounded(h.backend.destroy(&sandbox, &release))
            .await
            .is_confirmed()
    );
}

async fn suspended_is_unknown<T: DockerTransport>(
    backend: &mut DockerLocalBackend<T>,
    sandbox: &SandboxIdentity,
    release: &DestroyAuthorization,
) {
    assert_eq!(
        bounded(backend.list_owned(sandbox.owner.node_id)).await,
        Err(BackendError::Uncertain(UnconfirmedReason::Timeout))
    );
    assert!(bounded(backend.inspect(sandbox)).await.is_err());
    assert_eq!(
        bounded(backend.destroy(sandbox, release)).await,
        Confirmation::Unconfirmed(UnconfirmedReason::Timeout)
    );
}

async fn resumed_reconciles<T: DockerTransport>(
    backend: &mut DockerLocalBackend<T>,
    journal: &LifecycleRecord,
    sandbox: &SandboxIdentity,
) {
    let owned = bounded(backend.list_owned(sandbox.owner.node_id))
        .await
        .unwrap();
    let entry = owned
        .iter()
        .find(|entry| entry.sandbox == *sandbox)
        .expect("sandbox survives the suspend");
    assert!(entry.disk_present, "work volume survives the suspend");
    let action = reconcile(
        Some(journal),
        Some(entry),
        &RetentionState::Unretained,
        Deadline::Active,
    );
    assert!(
        matches!(&action, ReconcileAction::Adopt(s) | ReconcileAction::Preserve(s) if s == sandbox),
        "{action:?}"
    );
}
