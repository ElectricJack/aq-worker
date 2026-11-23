enum FixtureImage {
    Sealed,
    NamedReadonly,
}
enum FixtureInstallation {
    Sealed(NativeExecInstallation),
    NamedReadonly(NamedExecInstallation),
}
fn fixture_manifest(
    original: &aq_admission::startup::NativeOffer,
    runtime: &crate::native::journal::RuntimeIdentity,
) -> AdapterManifest {
    let m = &original.offered.offer.manifest;
    let executable = FilePin::from(pin(
        "AQ_NATIVE_CODEX_BINARY",
        "AQ_NATIVE_CODEX_SHA256",
        "aq.fixture.codex-0.166.0",
    ));
    assert_eq!(executable.artifact.digest.as_str(), CODEX_SHA256);
    assert_eq!(fs::metadata(&executable.path).unwrap().len(), CODEX_BYTES);
    AdapterManifest {
        schema_version: 1,
        adapter: m.harness.adapter.clone(),
        runtime: runtime.runtime.clone(),
        qualification: runtime.qualification.clone(),
        renderer: original.startup.renderer_qualification.clone(),
        handoff: HandoffKind::OpaquePrivateFileV1,
        executable,
        helper: FilePin::from(pin(
            "AQ_NATIVE_HOST_BINARY",
            "AQ_NATIVE_HOST_SHA256",
            "aq.fixture.source-host",
        )),
        tmux: FilePin::from(pin(
            "AQ_NATIVE_TMUX_BINARY",
            "AQ_NATIVE_TMUX_SHA256",
            "aq.fixture.tmux-3.4",
        )),
    }
}
fn preflight_fixture(kind: FixtureImage) -> FixtureInstallation {
    // Independently selected fixture pins are installed before any admission
    // deadline. A named case never retries/falls back through a sealed owner.
    let original = offer();
    let m = &original.offered.offer.manifest;
    let runtime = qualification();
    let installation = fixture_manifest(&original, &runtime);
    match kind {
        FixtureImage::Sealed => {
            let manifest = NativeExecManifest {
                schema_version: 1,
                profile: NativeExecKind::Codex0166ExecStdinV1,
                executable_bytes: CODEX_BYTES,
                installation,
            };
            let pool = NativeExecPool::new(512 * 1024 * 1024, 1).unwrap();
            FixtureInstallation::Sealed(
                NativeExecInstallation::open(
                    manifest.clone(),
                    ExpectedAdapter {
                        manifest_digest: &manifest.digest().unwrap(),
                        adapter: &m.harness.adapter,
                        runtime: &runtime.runtime,
                        qualification: &runtime.qualification,
                        renderer: &original.startup.renderer_qualification,
                    },
                    &pool,
                )
                .unwrap(),
            )
        }
        FixtureImage::NamedReadonly => {
            let manifest = NamedExecManifest {
                schema_version: 1,
                profile: NamedExecKind::Codex0166NamedReadonlyExecStdinV1,
                executable_bytes: CODEX_BYTES,
                environment: m.harness.environment.clone(),
                installation,
            };
            let environment =
                NamedReadonlyEnvironment::inspect(m.harness.environment.clone()).unwrap();
            let pool = NamedReadonlyExecPool::new(512 * 1024 * 1024, 1, environment).unwrap();
            FixtureInstallation::NamedReadonly(
                NamedExecInstallation::open(
                    manifest.clone(),
                    ExpectedAdapter {
                        manifest_digest: &manifest.digest().unwrap(),
                        adapter: &m.harness.adapter,
                        runtime: &runtime.runtime,
                        qualification: &runtime.qualification,
                        renderer: &original.startup.renderer_qualification,
                    },
                    &m.harness.environment,
                    &pool,
                )
                .unwrap(),
            )
        }
    }
}
async fn codex_prepared(
    peer: &Provider,
    kind: FixtureImage,
) -> (
    Inputs,
    GitPeer,
    DurablePhaseExecution,
    Arc<NativeCodexHostProfile>,
    aq_session_protocol::TerminalPolicy,
) {
    eprintln!(
        "fixture preflight begin boot_ns={}",
        crate::native::http::boot_ns().unwrap()
    );
    let installed = preflight_fixture(kind);
    eprintln!(
        "fixture preflight complete boot_ns={}",
        crate::native::http::boot_ns().unwrap()
    );
    let export = synthetic();
    let source_manifest = export.manifest.clone();
    let h = Inputs::new_phase_codex(export).await;
    h.host_report_mode.store(1, Ordering::SeqCst);
    h.grant_window_ms.store(20000, Ordering::SeqCst);
    let git = GitPeer::new(h.root.clone(), source_manifest);
    let (profile, policy) = local_profile(&h, installed).await;
    let inputs = phase_join(&h, git.client.clone()).await;
    // The finite Git socket peer provisions a placeholder workspace. Codex exec
    // requires an actual repository, so initialize only this owned fixture path.
    assert!(
        std::process::Command::new("/usr/bin/git")
            .args(["init", "--quiet"])
            .arg(&inputs.workspace().workspace().path)
            .env_clear()
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .status()
            .unwrap()
            .success()
    );
    let state = h.pipeline().snapshot().await.unwrap();
    let intent = state.attempts[&h.id].phase_acceptance.as_ref().unwrap();
    let observed = h
        .client
        .prepare_execution(
            h.proposed.as_ref().unwrap(),
            &intent.request,
            h.phase.as_ref().unwrap(),
            inputs.request(),
            None,
        )
        .await
        .unwrap();
    let PhaseExecutionCommit::Usable(grant) = h
        .pipeline()
        .record_phase_execution(inputs, observed)
        .await
        .unwrap()
    else {
        panic!("actual CLI fixture grant expired")
    };
    let profile = Arc::new(profile.with_development_provider(&peer.origin).unwrap());
    (h, git, grant, profile, policy)
}
async fn local_profile(
    h: &Inputs,
    installed: FixtureInstallation,
) -> (NativeCodexHostProfile, aq_session_protocol::TerminalPolicy) {
    let snapshot = h.pipeline().snapshot().await.unwrap();
    let a = &snapshot.attempts[&h.id];
    let m = &a.reservation.offer.offered.offer.manifest;
    let renderer = a.reservation.offer.startup.renderer_qualification.clone();
    let mapper = CodexExecProfile {
        harness: m.harness.clone(),
        permissions_digest: None,
        renderer,
        confinement: None,
    };
    let policy = m.terminal.clone();
    let host_root = h._dir.path().join("codex-host");
    fs::create_dir(&host_root).unwrap();
    fs::set_permissions(&host_root, fs::Permissions::from_mode(0o700)).unwrap();
    let profile = match installed {
        FixtureInstallation::Sealed(installation) => {
            NativeCodexHostProfile::new(host_root, installation, mapper, 160, 40)
        }
        FixtureInstallation::NamedReadonly(installation) => {
            NativeCodexHostProfile::new_named(host_root, installation, mapper, 160, 40)
        }
    }
    .unwrap();
    (profile, policy)
}
async fn begin_execution(
    peer: &Provider,
    slow_output: bool,
    delayed_started: bool,
) -> (
    Inputs,
    GitPeer,
    watch::Sender<bool>,
    tokio::task::JoinHandle<anyhow::Result<()>>,
    OutputPeer,
) {
    begin_fixture_execution(peer, slow_output, delayed_started, FixtureImage::Sealed).await
}
async fn begin_fixture_execution(
    peer: &Provider,
    slow_output: bool,
    delayed_started: bool,
    kind: FixtureImage,
) -> (
    Inputs,
    GitPeer,
    watch::Sender<bool>,
    tokio::task::JoinHandle<anyhow::Result<()>>,
    OutputPeer,
) {
    let (h, git, grant, profile, policy) = codex_prepared(peer, kind).await;

    if delayed_started {
        h.host_report_mode.store(4, Ordering::SeqCst);
    }
    let (http, mut output) = output_peer(policy, slow_output).await;
    output.execution_deadline_ns = Some(grant.deadline_ns());
    eprintln!(
        "fixture execute begin boot_ns={} original_deadline_ns={}",
        crate::native::http::boot_ns().unwrap(),
        grant.deadline_ns()
    );
    let mut executor = CodexExecutor::development_preflighted(http, profile).unwrap();
    let (tx, rx) = watch::channel(false);
    let pipeline = h.pipeline().clone();
    let task = tokio::spawn(async move { executor.execute(pipeline, grant, rx).await });
    (h, git, tx, task, output)
}
