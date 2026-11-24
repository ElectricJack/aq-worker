#[test]
#[ignore = "actual native-exec profile: requires local cc/static ELF and tmux"]
fn native_exec_profile_delivers_exact_sealed_stdin_under_actual_guardian() {
    native_private_input_case(false, false);
}
#[test]
#[ignore = "actual native-exec original material preexec refusal: requires cc/static ELF and tmux"]
fn native_exec_refuses_original_record_changed_after_private_publication() {
    native_private_input_case(true, false);
}
#[test]
#[ignore = "actual scoped private auth/ECHILD: static ELF and tmux only, no agent/provider"]
fn native_exec_scoped_capability_is_published_under_intent_and_cleaned_after_echild() {
    native_private_input_case(false, true);
}
#[test]
#[ignore = "actual scoped original-source refusal: static ELF and tmux only, no agent/provider"]
fn native_exec_scoped_auth_refuses_changed_original_record_at_exec() {
    native_private_input_case(true, true);
}
fn scoped_capability() -> zeroize::Zeroizing<Vec<u8>> {
    zeroize::Zeroizing::new(format!("aq-responses-{}", "a".repeat(64)).into_bytes())
}
fn native_private_input_case(tamper_record: bool, scoped: bool) {
    let mut c = Case::new("unused", false);
    let payload = b"literal startup > argument delivery\nwithout an appended prompt\n".repeat(128);
    c.manifest.context.startup_byte_budget = 1_048_576;
    c.manifest.terminal.input_enabled = false;
    let rendered = rendered_material(&mut c, &payload);
    let source = original_private_files(&c);
    let pending = prepare_native_peer(&mut c, rendered, &source, scoped);
    let auth = check_published_material(&c, scoped);
    assert_eq!(pending.observe().unwrap().phase, HostPhase::Starting);
    let intent = pending.observe().unwrap().guardian.intent;
    let (mut output, baseline) = pending.open_baseline_observer().unwrap();
    if tamper_record {
        std::fs::write(
            source.join("record.json"),
            b"changed original record after auth publication",
        )
        .unwrap();
    }
    let mut released = pending
        .release_after_baseline(&baseline, &mut c.authority)
        .unwrap();
    if tamper_record {
        verify_refused_material(&c, &auth, &intent, &released);
        return;
    }
    let mut handle = wait_named("native sealed exec", || {
        released.poll_started(&mut c.authority).unwrap()
    });
    check_exact_stdin(&mut output, &payload);
    check_native_cleanup(&mut handle, &auth, &source);
}

fn check_native_cleanup(handle: &mut SessionHandle, auth: &Path, source: &Path) {
    assert!(handle.stopped_proof().is_err());
    assert_eq!(handle.observe().unwrap().phase, HostPhase::Running);
    handle.stop().unwrap();
    let stopped = handle.wait_stopped(Duration::from_secs(3)).unwrap();
    assert_eq!(stopped.phase, HostPhase::Stopped);
    assert!(stopped.kernel_wait_echild);
    let generated = auth.parent().unwrap().join("runtime-file");
    std::fs::write(&generated, b"keep unknown generated files").unwrap();
    let proof = handle.stopped_proof().unwrap();
    let cleaned = handle.cleanup_private_inputs(&proof).unwrap();
    assert!(cleaned.auth_removed && !cleaned.home_removed);
    assert_eq!(
        std::fs::read(&generated).unwrap(),
        b"keep unknown generated files"
    );
    std::fs::remove_file(generated).unwrap();
    assert!(!auth.exists());
    assert_eq!(
        std::fs::read(source.join("material.bin")).unwrap(),
        b"sk-private-offline-only"
    );
    assert!(handle.cleanup_private_inputs(&proof).unwrap().home_removed);
}

fn original_private_files(c: &Case) -> PathBuf {
    use std::os::unix::fs::PermissionsExt;
    let source = c.temp.path().join("retained-provider");
    std::fs::create_dir(&source).unwrap();
    std::fs::set_permissions(&source, std::fs::Permissions::from_mode(0o700)).unwrap();
    for (name, bytes) in [
        ("material.bin", b"sk-private-offline-only".as_slice()),
        ("record.json", b"{\"schema_version\":1}".as_slice()),
    ] {
        let path = source.join(name);
        std::fs::write(&path, bytes).unwrap();
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600)).unwrap();
    }
    source
}

fn build_native_peer(c: &Case) -> PathBuf {
    use std::io::Write;
    let program = c.temp.path().join("native-stdin-peer");
    let mut cc = Command::new("/usr/bin/cc")
        .args(["-O2", "-static", "-x", "c", "-o"])
        .arg(&program)
        .arg("-")
        .stdin(Stdio::piped())
        .spawn()
        .unwrap();
    cc.stdin
        .take()
        .unwrap()
        .write_all(include_bytes!("../fixtures/native-stdin-peer.c"))
        .unwrap();
    assert!(cc.wait().unwrap().success());
    assert!(std::fs::metadata(&program).unwrap().len() > observed::MAX_ELF_BYTES);
    program
}

struct NativeFixtureImage {
    path: PathBuf,
    image: observed::NativeExecImage,
    executable: aq_session_protocol::ArtifactRef,
    renderer: aq_session_protocol::ArtifactRef,
}

fn retained_native_peer(c: &Case) -> NativeFixtureImage {
    use aq_session_protocol::{ArtifactRef, Digest, Text};
    let program = build_native_peer(c);
    let executable = ArtifactRef {
        id: Text::new("offline.native-stdin-peer").unwrap(),
        version: Text::new("1").unwrap(),
        digest: Digest::of(&std::fs::read(&program).unwrap()),
    };
    let renderer = ArtifactRef {
        id: Text::new("offline.literal-observer").unwrap(),
        version: Text::new("1").unwrap(),
        digest: Digest::of(b"not a screen renderer qualification"),
    };
    let pool = observed::NativeExecPool::new(8 * 1024 * 1024, 1).unwrap();
    let image = pool.load_verified(&program, &executable).unwrap();
    assert_eq!(
        pool.charged_bytes().unwrap(),
        std::fs::metadata(&program).unwrap().len()
    );
    std::fs::write(
        &program,
        b"mutable installation replacement; must never execute",
    )
    .unwrap();
    NativeFixtureImage {
        path: program,
        image,
        executable,
        renderer,
    }
}

fn prepare_native_peer(
    c: &mut Case,
    rendered: RenderedStartup,
    source: &Path,
    scoped: bool,
) -> observed::PendingObservedSession {
    use aq_session_protocol::TerminalIdentity;
    let NativeFixtureImage {
        path: program,
        image,
        executable,
        renderer,
    } = retained_native_peer(c);
    let now = ownership::raw_boot_ms();
    let terminal = TerminalIdentity {
        session: c.manifest.bindings.session.clone(),
        terminal_id: Uuid::new_v4(),
        epoch: 1,
    };
    let startup = observed::ObservedStartup::native_exec(
        rendered,
        terminal,
        image,
        observed::NativeExecObservationProfile::new(renderer.clone(), 80, 24).unwrap(),
        now + 8000,
    )
    .unwrap();
    let adapter = ObservedAdapter {
        arguments: vec![],
        program,
        executable,
        renderer,
        private_source: Some(source.to_owned()),
        scoped_auth: scoped.then(|| private_scope(c, now + 8000)),
    };
    let permit = ExecutionPermit {
        session: c.manifest.bindings.session.clone(),
        manifest_digest: canonical_digest(&c.manifest).unwrap(),
        sequence: 1,
        issued_boot_ms: now,
        expires_boot_ms: now + 8000,
    };
    SessionHost::prepare_observed(
        c.temp.path(),
        c.id,
        &startup,
        permit,
        &c.local,
        &adapter,
        &mut c.authority,
    )
    .unwrap()
}

fn private_scope(c: &Case, deadline: u64) -> ScopedCodexAuthBinding {
    use aq_session_protocol::Digest;
    ScopedCodexAuthBinding {
        launch_id: c.id,
        origin: "http://127.0.0.1:12345".into(),
        deadline_boot_ms: deadline,
        binding: aq_session_protocol::LaunchBinding {
            schema_version: 1,
            session: c.manifest.bindings.session.clone(),
            offer_id: Uuid::new_v4(),
            attempt_id: c.manifest.bindings.attempt_id,
            assignment_generation: c.manifest.bindings.assignment_generation,
            execution_spec_digest: Digest::of(b"static peer only"),
            launch_manifest_digest: canonical_digest(&c.manifest).unwrap(),
            graph_access_profile_digest: c.manifest.bindings.graph_access_profile.digest.clone(),
        },
    }
}
fn check_published_material(c: &Case, scoped: bool) -> PathBuf {
    assert!(c.temp.path().join("caller-intent.json").exists());
    assert!(c.temp.path().join("auth-order.checked").exists());
    let auth = c.adapter.dir.join("codex-home/auth.json");
    let expected_auth = if scoped {
        format!(
            "{{\"OPENAI_API_KEY\":\"{}\"}}",
            std::str::from_utf8(&scoped_capability()).unwrap()
        )
        .into_bytes()
    } else {
        b"{\"OPENAI_API_KEY\":\"sk-private-offline-only\"}".to_vec()
    };
    assert_eq!(std::fs::read(&auth).unwrap(), expected_auth);
    if scoped {
        assert!(!String::from_utf8_lossy(&expected_auth).contains("sk-private"));
        assert!(
            !std::fs::read_to_string(c.adapter.dir.join("launch.json"))
                .unwrap()
                .contains(std::str::from_utf8(&scoped_capability()).unwrap())
        );
    }
    assert!(
        !std::fs::read_to_string(c.adapter.dir.join("launch.json"))
            .unwrap()
            .contains("sk-private")
    );
    auth
}

fn verify_refused_material(
    c: &Case,
    auth: &Path,
    intent: &LaunchIntent,
    released: &observed::ReleasedObservedSession,
) {
    let observed = wait_named("changed original material stops before exec", || {
        let observation = released.observe().unwrap();
        assert_ne!(observation.phase, HostPhase::Running);
        (observation.phase == HostPhase::Stopped).then_some(observation)
    });
    assert!(observed.kernel_wait_echild);
    assert!(!c.temp.path().join("caller-started.json").exists());
    assert!(auth.exists());
    let mut recovered = SessionHost::recover(c.temp.path(), intent).unwrap();
    let proof = recovered.stopped_proof().unwrap();
    assert!(
        recovered
            .cleanup_private_inputs(&proof)
            .unwrap()
            .home_removed
    );
}

fn check_exact_stdin(output: &mut terminal_output::OutputObserver, payload: &[u8]) {
    let hash = payload.iter().fold(14695981039346656037_u64, |h, b| {
        (h ^ u64::from(*b)).wrapping_mul(1099511628211)
    });
    let expected = format!("STDIN:{}:{hash:016x}", payload.len());
    let mut literal = Vec::new();
    wait_named("native exact stdin output", || {
        for event in output.poll().unwrap() {
            if let terminal_output::ControlEvent::Bytes(bytes) = event {
                literal.extend_from_slice(&bytes);
            }
        }
        String::from_utf8_lossy(&literal)
            .contains(&expected)
            .then_some(())
    });
    assert!(String::from_utf8_lossy(&literal).contains("AUTH:exact-private-file"));
}
