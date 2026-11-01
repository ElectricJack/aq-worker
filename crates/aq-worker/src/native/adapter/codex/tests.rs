use super::*;
use aq_session_protocol::Text;
use std::os::unix::fs::PermissionsExt;
fn fixture() -> (NativeOffer, Vec<u8>) {
    let value: serde_json::Value = serde_json::from_slice(include_bytes!(
        "../../../../../../vendor/aq-admission-0.1.9/artifacts/startup.fixture.json"
    ))
    .unwrap();
    let mut offer: NativeOffer = serde_json::from_value(value["native_offer"].clone()).unwrap();
    let m = &mut offer.offered.offer.manifest;
    m.harness.harness = Text::new("codex").unwrap();
    m.harness.cli_version = Text::new(CODEX_CLI_VERSION).unwrap();
    m.model.provider = Text::new("openai").unwrap();
    m.model.reasoning = Text::new("medium").unwrap();
    m.model.spending_budget = None;
    m.terminal.input_enabled = false;
    offer.offered.offer.binding.launch_manifest_digest =
        crate::native::journal::canonical_digest(m).unwrap();
    offer.validate().unwrap();
    (
        offer,
        value["payload_utf8"].as_str().unwrap().as_bytes().to_vec(),
    )
}
const PEER: &[u8] = b"#!/usr/bin/python3\nimport sys,json,hashlib,os\np=sys.stdin.buffer.read(1048577)\nprint(json.dumps({'bytes':len(p),'digest':hashlib.sha256(p).hexdigest(),'args':sys.argv[1:],'ambient_secret':os.environ.get('AQ_TEST_AMBIENT_SECRET')}))\n";
fn installed(dir: &Path, offer: &NativeOffer) -> InstalledAdapter {
    let path = dir.join("executable");
    std::fs::write(&path, PEER).unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o500)).unwrap();
    let reference = ArtifactRef {
        id: Text::new("fixture").unwrap(),
        version: Text::new("1").unwrap(),
        digest: Digest::of(PEER),
    };
    let pin = FilePin {
        path,
        artifact: reference.clone(),
    };
    let m = AdapterManifest {
        schema_version: 1,
        adapter: offer.offered.offer.manifest.harness.adapter.clone(),
        runtime: reference.clone(),
        qualification: reference.clone(),
        renderer: offer.startup.renderer_qualification.clone(),
        handoff: HandoffKind::OpaquePrivateFileV1,
        executable: pin.clone(),
        helper: pin.clone(),
        tmux: pin,
    };
    let digest = m.digest().unwrap();
    InstalledAdapter::open(
        m,
        ExpectedAdapter {
            manifest_digest: &digest,
            adapter: &offer.offered.offer.manifest.harness.adapter,
            runtime: &reference,
            qualification: &reference,
            renderer: &offer.startup.renderer_qualification,
        },
    )
    .unwrap()
}
fn profile(o: &NativeOffer) -> CodexExecProfile {
    let m = &o.offered.offer.manifest;
    CodexExecProfile {
        harness: m.harness.clone(),
        model: m.model.clone(),
        permissions_digest: crate::native::journal::canonical_digest(&m.permissions).unwrap(),
        renderer: o.startup.renderer_qualification.clone(),
    }
}
#[test]
fn exact_offer_maps_closed_argv_and_delivers_one_payload_without_secret_metadata() {
    let root = tempfile::tempdir().unwrap();
    let home = root.path().join("home");
    std::fs::create_dir(&home).unwrap();
    std::fs::set_permissions(&home, std::fs::Permissions::from_mode(0o700)).unwrap();
    let work = root.path().join("work");
    std::fs::create_dir(&work).unwrap();
    let (offer, payload) = fixture();
    let digest = crate::native::journal::canonical_digest(&offer.offered.offer.manifest).unwrap();
    let mut adapter = installed(root.path(), &offer);
    let prepared = profile(&offer)
        .prepare(&mut adapter, &offer, &digest, payload.clone(), &work, &home)
        .unwrap();
    let command = prepared.command();
    assert_eq!(command.arguments.last().unwrap(), "-");
    assert_eq!(
        command
            .arguments
            .iter()
            .filter(|s| s.as_str() == "-")
            .count(),
        1
    );
    assert!(command.arguments.iter().any(|s| s == "--no-daemon"));
    assert!(
        command
            .arguments
            .iter()
            .any(|s| s == "web_search=\"disabled\"")
    );
    for setting in [
        "shell_environment_policy.inherit=\"none\"",
        "shell_environment_policy.set.PATH=\"/usr/bin:/bin\"",
        "features.shell_snapshot=false",
        "features.shell_snapshot_v2=false",
        "allow_login_shell=false",
        "features.plugins=false",
        "features.recommended_plugins=false",
        "features.remote_plugin=false",
        "features.plugin_discovery_prefetch=false",
        "features.apps=false",
        "features.memory_tool=false",
        "memories.generate_memories=false",
    ] {
        assert!(command.arguments.iter().any(|argument| argument == setting));
    }
    assert!(
        !serde_json::to_string(command)
            .unwrap()
            .contains(std::str::from_utf8(&payload).unwrap())
    );
    assert_eq!(
        command.environment.keys().collect::<Vec<_>>(),
        vec!["CODEX_HOME"]
    );
    let mut out = Vec::new();
    prepared.write_startup(&mut out).unwrap();
    assert_eq!(out, payload);
}
#[test]
fn tool_environment_config_starts_empty_and_adds_only_fixed_trusted_path() {
    let root = tempfile::tempdir().unwrap();
    let home = root.path().join("home");
    std::fs::create_dir(&home).unwrap();
    std::fs::set_permissions(&home, std::fs::Permissions::from_mode(0o700)).unwrap();
    let work = root.path().join("work");
    std::fs::create_dir(&work).unwrap();
    let (offer, payload) = fixture();
    let digest = crate::native::journal::canonical_digest(&offer.offered.offer.manifest).unwrap();
    let mut adapter = installed(root.path(), &offer);
    let prepared = profile(&offer)
        .prepare(&mut adapter, &offer, &digest, payload, &work, &home)
        .unwrap();
    let config = prepared
        .command()
        .arguments
        .windows(2)
        .filter(|pair| pair[0] == "--config" && pair[1].starts_with("shell_environment_policy."))
        .map(|pair| pair[1].as_str())
        .collect::<Vec<_>>()
        .join("\n");
    let config: toml::Value = toml::from_str(&config).unwrap();
    let policy = config["shell_environment_policy"].as_table().unwrap();
    assert_eq!(policy.len(), 3);
    assert_eq!(policy["inherit"].as_str(), Some("none"));
    assert_eq!(policy["experimental_use_profile"].as_bool(), Some(false));
    let set = policy["set"].as_table().unwrap();
    assert_eq!(set.len(), 1);
    assert_eq!(set["PATH"].as_str(), Some("/usr/bin:/bin"));
}
#[test]
fn changed_payload_settings_private_mode_and_hard_budget_are_refused() {
    let root = tempfile::tempdir().unwrap();
    let home = root.path().join("home");
    std::fs::create_dir(&home).unwrap();
    std::fs::set_permissions(&home, std::fs::Permissions::from_mode(0o700)).unwrap();
    let (offer, payload) = fixture();
    let digest = crate::native::journal::canonical_digest(&offer.offered.offer.manifest).unwrap();
    let work = root.path().join("work");
    std::fs::create_dir(&work).unwrap();
    let selected = profile(&offer);
    let mut adapter = installed(root.path(), &offer);
    assert!(
        selected
            .prepare(
                &mut adapter,
                &offer,
                &digest,
                b"changed".to_vec(),
                &work,
                &home
            )
            .is_err()
    );
    let mut changed = selected;
    changed.model.reasoning = Text::new("high").unwrap();
    assert!(
        changed
            .prepare(&mut adapter, &offer, &digest, payload.clone(), &work, &home)
            .is_err()
    );
    let mut budgeted = offer.clone();
    budgeted.offered.offer.manifest.model.spending_budget =
        Some(aq_session_protocol::SpendingBudget {
            currency: Text::new("USD").unwrap(),
            basis: aq_session_protocol::BudgetBasis::TaskCumulativeEstimatedCost,
            limit_microunits: 1000,
        });
    let budget_digest =
        crate::native::journal::canonical_digest(&budgeted.offered.offer.manifest).unwrap();
    budgeted.offered.offer.binding.launch_manifest_digest = budget_digest.clone();
    assert!(
        profile(&budgeted)
            .prepare(
                &mut adapter,
                &budgeted,
                &budget_digest,
                payload.clone(),
                &work,
                &home
            )
            .is_err()
    );
    std::fs::set_permissions(&home, std::fs::Permissions::from_mode(0o755)).unwrap();
    assert!(
        profile(&offer)
            .prepare(&mut adapter, &offer, &digest, payload, &work, &home)
            .is_err()
    );
}
#[test]
fn authentication_is_private_bounded_and_cannot_inject_json() {
    let bytes = auth_bytes(b"sk-literal\"quoted").unwrap();
    let v: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(v["OPENAI_API_KEY"], "sk-literal\"quoted");
    assert!(auth_bytes(b"key\nextra").is_err());
    assert!(auth_bytes(&vec![b'a'; 4097]).is_err());
}
#[test]
fn jsonl_readiness_requires_actual_order_and_handles_chunk_boundaries() {
    let id = uuid::Uuid::new_v4();
    let mut p = CodexEvents::default();
    let first = format!("{{\"type\":\"thread.started\",\"thread_id\":\"{id}\"}}\r\n");
    assert!(p.push(&first.as_bytes()[..10]).unwrap().is_empty());
    assert_eq!(
        p.push(&first.as_bytes()[10..]).unwrap(),
        vec![CodexEvent::ThreadStarted(id)]
    );
    assert_eq!(
        p.push(b"{\"type\":\"turn.started\"}\n").unwrap(),
        vec![CodexEvent::TurnStarted(id)]
    );
    p.push(b"{\"type\":\"item.completed\",\"item\":{\"type\":\"agent_message\"}}\n")
        .unwrap();
    assert!(p.finish().is_err());
    p.push(b"{\"type\":\"turn.completed\"}\n").unwrap();
    p.finish().unwrap();
    let mut p = CodexEvents::default();
    assert!(p.push(b"{\"type\":\"turn.started\"}\n").is_err());
    assert!(p.push(first.as_bytes()).is_err());
    let mut p = CodexEvents::default();
    p.push(&vec![b'x'; MAX_EVENT]).unwrap();
    assert!(p.push(b"x").is_err());
    assert!(p.push(first.as_bytes()).is_err());
}

#[test]
fn actual_offline_process_receives_only_exact_stdin_and_explicit_environment() {
    let root = tempfile::tempdir().unwrap();
    let home = root.path().join("home");
    std::fs::create_dir(&home).unwrap();
    std::fs::set_permissions(&home, std::fs::Permissions::from_mode(0o700)).unwrap();
    let work = root.path().join("work");
    std::fs::create_dir(&work).unwrap();
    let (offer, payload) = fixture();
    let digest = crate::native::journal::canonical_digest(&offer.offered.offer.manifest).unwrap();
    let mut adapter = installed(root.path(), &offer);
    let prepared = profile(&offer)
        .prepare(&mut adapter, &offer, &digest, payload.clone(), &work, &home)
        .unwrap();
    let c = prepared.command();
    let mut child = std::process::Command::new(&c.executable)
        .args(&c.arguments)
        .current_dir(&work)
        .env_clear()
        .envs(&c.environment)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    prepared
        .write_startup(&mut child.stdin.take().unwrap())
        .unwrap();
    let out = child.wait_with_output().unwrap();
    assert!(out.status.success());
    let actual: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(actual["bytes"], payload.len());
    assert_eq!(actual["digest"], Digest::of(&payload).as_str());
    assert!(actual["ambient_secret"].is_null());
    assert_eq!(actual["args"].as_array().unwrap().last().unwrap(), "-");
}
#[test]
fn fatal_or_empty_stream_cannot_report_completion_or_later_readiness() {
    let mut p = CodexEvents::default();
    assert!(p.finish().is_err());
    let id = uuid::Uuid::new_v4();
    let events = format!(
        "{{\"type\":\"thread.started\",\"thread_id\":\"{id}\"}}\n{{\"type\":\"error\"}}\n{{\"type\":\"turn.started\"}}\n"
    );
    assert_eq!(
        p.push(events.as_bytes()).unwrap(),
        vec![CodexEvent::ThreadStarted(id), CodexEvent::Failed]
    );
    assert!(p.finish().is_err());
    let mut p = CodexEvents::default();
    assert!(p.push(&vec![b'x'; MAX_EVENT + 1]).is_err());
    assert!(p.push(events.as_bytes()).is_err());
}

#[test]
fn native_mapping_reuses_preflight_image_after_mutable_program_path_changes() {
    use super::super::native_exec::{NativeExecInstallation, NativeExecKind, NativeExecManifest};
    use aq_session_host::observed::NativeExecPool;
    let root = tempfile::tempdir().unwrap();
    let home = root.path().join("home");
    std::fs::create_dir(&home).unwrap();
    std::fs::set_permissions(&home, std::fs::Permissions::from_mode(0o700)).unwrap();
    let work = root.path().join("work");
    std::fs::create_dir(&work).unwrap();
    let (offer, payload) = fixture();
    let m = &offer.offered.offer.manifest;
    let program = root.path().join("native");
    std::fs::copy(std::fs::canonicalize("/usr/bin/true").unwrap(), &program).unwrap();
    let image = std::fs::read(&program).unwrap();
    let pin = FilePin {
        path: program.clone(),
        artifact: ArtifactRef {
            id: Text::new("offline.true").unwrap(),
            version: Text::new("1").unwrap(),
            digest: Digest::of(&image),
        },
    };
    let helper = root.path().join("helper");
    std::fs::write(&helper, b"unexecuted helper fixture").unwrap();
    std::fs::set_permissions(&helper, std::fs::Permissions::from_mode(0o500)).unwrap();
    let local = FilePin {
        path: helper,
        artifact: ArtifactRef {
            id: Text::new("offline.helper").unwrap(),
            version: Text::new("1").unwrap(),
            digest: Digest::of(b"unexecuted helper fixture"),
        },
    };
    let r = m.harness.adapter.clone();
    let renderer = offer.startup.renderer_qualification.clone();
    let manifest = NativeExecManifest {
        schema_version: 1,
        profile: NativeExecKind::Codex0166ExecStdinV1,
        executable_bytes: image.len() as u64,
        installation: AdapterManifest {
            schema_version: 1,
            adapter: r.clone(),
            runtime: r.clone(),
            qualification: r.clone(),
            renderer: renderer.clone(),
            handoff: HandoffKind::OpaquePrivateFileV1,
            executable: pin,
            helper: local.clone(),
            tmux: local,
        },
    };
    let digest = manifest.digest().unwrap();
    let pool = NativeExecPool::new(1024 * 1024, 1).unwrap();
    let mut installed = NativeExecInstallation::open(
        manifest,
        ExpectedAdapter {
            manifest_digest: &digest,
            adapter: &r,
            runtime: &r,
            qualification: &r,
            renderer: &renderer,
        },
        &pool,
    )
    .unwrap();
    std::fs::write(
        &program,
        b"mutable path changed; sealed image remains exact",
    )
    .unwrap();
    let manifest_digest = crate::native::journal::canonical_digest(m).unwrap();
    let prepared = profile(&offer)
        .prepare_native(
            &mut installed,
            CodexExecInput {
                offer: &offer,
                expected_manifest: &manifest_digest,
                payload: payload.clone(),
                workspace: &work,
                private_home: &home,
            },
        )
        .unwrap();
    let mut received = Vec::new();
    prepared.write_startup(&mut received).unwrap();
    assert_eq!(received, payload);
    assert_eq!(installed.image().unwrap().bytes(), image.len() as u64);
    assert_eq!(pool.charged_bytes().unwrap(), image.len() as u64);
}
