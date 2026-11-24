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
const PEER: &[u8] = b"#!/usr/bin/python3\nimport sys,json,hashlib,os\np=sys.stdin.buffer.read(1048577)\nprint(json.dumps({'bytes':len(p),'digest':hashlib.sha256(p).hexdigest(),'args':sys.argv[1:],'ambient_secret':os.environ.get('AQ_TEST_AMBIENT_SECRET'),'git':{k:v for k,v in os.environ.items() if k.startswith('GIT_')}}))\n";
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
        permissions_digest: None,
        renderer: o.startup.renderer_qualification.clone(),
        confinement: None,
    }
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
    let author = aq_worker_protocol::git_author::GitAuthor {
        name: "Test Creator".into(),
        email: "creator@example.invalid".into(),
    };
    let prepared = profile(&offer)
        .prepare(&mut adapter, &offer, &digest, payload.clone(), &work, &home)
        .unwrap()
        .with_git_author(Some(&author))
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
    for (key, value) in author.tool_environment() {
        assert_eq!(actual["git"][key], value);
    }
    assert_eq!(actual["args"].as_array().unwrap().last().unwrap(), "-");
}

include!("tests/commands.rs");

include!("tests/confinement.rs");
