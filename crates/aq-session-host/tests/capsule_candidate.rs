#![cfg(feature = "test-support")]
use aq_session_host::{capsule::*, ownership};
use aq_session_protocol::{ArtifactRef, Digest, Text};
use std::{
    fs::File,
    io::{Read, Write},
    os::{
        fd::AsRawFd,
        unix::fs::{MetadataExt, PermissionsExt},
    },
    path::{Path, PathBuf},
    time::{Duration, Instant},
};
const _: () = assert!(!PRODUCTION_ELIGIBLE);
#[test]
#[ignore = "explicit rootless FD-root and real submount canary; no agent/provider"]
fn pinned_root_survives_path_replacement_and_real_submount_is_refused() {
    let scratch = tempfile::tempdir().unwrap();
    std::fs::set_permissions(scratch.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
    let output = std::process::Command::new(env!("CARGO_BIN_EXE_aq-session-capsule-candidate"))
        .arg("probe-root-mount")
        .arg(scratch.path())
        .env_clear()
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        std::fs::read_dir(scratch.path().join("root"))
            .unwrap()
            .next()
            .is_none()
    );
    assert!(
        std::fs::read_dir(scratch.path().join("moved-root"))
            .unwrap()
            .next()
            .is_none()
    );
}
struct Fixture {
    _root: tempfile::TempDir,
    plan: StaticCapsulePlan,
    helper: PinnedHelper,
    workspace: PathBuf,
    _original: File,
}
fn reference(id: &str, path: &Path) -> ArtifactRef {
    ArtifactRef {
        id: Text::new(id).unwrap(),
        version: Text::new("1").unwrap(),
        digest: Digest::of(&std::fs::read(path).unwrap()),
    }
}
fn fixture_with_lifetime(lifetime_ms: u64) -> Fixture {
    let root = tempfile::tempdir().unwrap();
    for name in [
        "root",
        "workspace",
        "auth",
        "allowed",
        "git",
        "private-node",
    ] {
        let path = root.path().join(name);
        std::fs::create_dir(&path).unwrap();
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700)).unwrap();
    }
    let program = root.path().join("peer");
    assert!(
        std::process::Command::new("/usr/bin/cc")
            .args(["-static", "-O2", "-Wall", "-Wextra"])
            .arg(Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/capsule-static-peer.c"))
            .arg("-o")
            .arg(&program)
            .env_clear()
            .env("PATH", "/usr/bin:/bin")
            .status()
            .unwrap()
            .success()
    );
    std::fs::write(
        root.path().join("auth/auth.json"),
        b"derived-local-canary\n",
    )
    .unwrap();
    std::fs::write(root.path().join("allowed/item"), b"allowed-canary\n").unwrap();
    std::fs::write(root.path().join("git/item"), b"git-canary\n").unwrap();
    let source = root.path().join("private-node/original");
    std::fs::write(&source, uuid::Uuid::new_v4().as_bytes()).unwrap();
    let original = File::open(&source).unwrap();
    let metadata = original.metadata().unwrap();
    let helper = PathBuf::from(env!("CARGO_BIN_EXE_aq-session-capsule-candidate"));
    let helper = PinnedHelper::open(
        &std::fs::canonicalize(&helper).unwrap(),
        &reference("source-helper", &helper),
    )
    .unwrap();
    let workspace = root.path().join("workspace");
    let grants = vec![
        MountGrant::open(&workspace, MountTarget::Workspace, true).unwrap(),
        MountGrant::open(&root.path().join("auth"), MountTarget::DerivedAuth, false).unwrap(),
        MountGrant::open(&root.path().join("allowed"), MountTarget::Allowed(0), false).unwrap(),
        MountGrant::open(&root.path().join("git"), MountTarget::GitMetadata, false).unwrap(),
    ];
    let tids = std::fs::read_dir("/proc/self/task")
        .unwrap()
        .map(|entry| entry.unwrap().file_name().into_string().unwrap())
        .collect::<Vec<_>>()
        .join(",");
    let plan = StaticCapsulePlan::new(
        &root.path().join("root"),
        &program,
        &reference("static-peer", &program),
        b"capsule exact stdin bytes\n",
        grants,
        ownership::raw_boot_ms() + lifetime_ms,
    )
    .unwrap()
    .arguments(vec![
        std::process::id().to_string(),
        tids,
        original.as_raw_fd().to_string(),
        source.to_str().unwrap().into(),
        metadata.dev().to_string(),
        metadata.ino().to_string(),
    ])
    .unwrap();
    Fixture {
        _root: root,
        plan,
        helper,
        workspace,
        _original: original,
    }
}
fn fixture() -> Fixture {
    fixture_with_lifetime(8000)
}
fn ready(handle: &mut CapsuleHandle) -> std::net::TcpStream {
    let listener = handle.take_listener().unwrap();
    let until = Instant::now() + Duration::from_secs(3);
    let mut connection = loop {
        match listener.accept() {
            Ok((stream, _)) => break stream,
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                assert!(Instant::now() < until);
                std::thread::sleep(Duration::from_millis(2));
            }
            Err(e) => panic!("{e}"),
        }
    };
    connection
        .set_read_timeout(Some(Duration::from_secs(1)))
        .unwrap();
    let mut bytes = [0u8; 18];
    connection.read_exact(&mut bytes).unwrap();
    assert_eq!(&bytes, b"static-proxy-ready");
    connection.write_all(b"x").unwrap();
    connection
}
fn descendant(pid: u32) -> ownership::ProcessEvidence {
    let children = std::fs::read_to_string(format!("/proc/{pid}/task/{pid}/children")).unwrap();
    let pid = children.split_whitespace().next().unwrap().parse().unwrap();
    ownership::process_evidence(pid).unwrap()
}
#[test]
#[ignore = "explicit rootless namespace/static helper qualification; no Codex/provider"]
fn assembled_static_capsule_has_private_views_trusted_proxy_and_actual_echild() {
    let fixture = fixture();
    let mut handle = CapsuleHandle::spawn(&fixture.helper, fixture.plan).unwrap();
    let witness = handle.witness().clone();
    assert_ne!(witness.guardian.pid, witness.init.pid);
    assert_ne!(witness.init.pid, witness.harness.pid);
    let _proxy = ready(&mut handle);
    assert!(fixture.workspace.join("checks-passed").is_file());
    let descendant = descendant(witness.harness.pid);
    let pidfd = ownership::verified_pidfd(&descendant).unwrap();
    let initfd = ownership::verified_pidfd(&witness.init).unwrap();
    let harnessfd = ownership::verified_pidfd(&witness.harness).unwrap();
    let report = handle.stop().unwrap();
    assert!(report.kernel_echild() && report.reaped_count() > 0);
    assert_eq!(report.witness().init, witness.init);
    assert_eq!(report.witness().harness, witness.harness);
    assert!(!ownership::pidfd_is_live(&pidfd).unwrap());
    assert!(!ownership::pidfd_is_live(&initfd).unwrap());
    assert!(!ownership::pidfd_is_live(&harnessfd).unwrap());
    assert!(ownership::process_evidence(witness.guardian.pid).is_err());
}
#[test]
#[ignore = "explicit rootless namespace/static helper qualification; no Codex/provider"]
fn dropped_handle_closes_control_and_physically_ends_capsule_without_publishing_proof() {
    let fixture = fixture();
    let mut handle = CapsuleHandle::spawn(&fixture.helper, fixture.plan).unwrap();
    let witness = handle.witness().clone();
    let _proxy = ready(&mut handle);
    let descendant = descendant(witness.harness.pid);
    let pidfd = ownership::verified_pidfd(&descendant).unwrap();
    let initfd = ownership::verified_pidfd(&witness.init).unwrap();
    let harnessfd = ownership::verified_pidfd(&witness.harness).unwrap();
    drop(handle);
    assert!(!ownership::pidfd_is_live(&pidfd).unwrap());
    assert!(!ownership::pidfd_is_live(&initfd).unwrap());
    assert!(!ownership::pidfd_is_live(&harnessfd).unwrap());
    assert!(ownership::process_evidence(witness.guardian.pid).is_err());
}

#[test]
#[ignore = "explicit rootless namespace/static helper refusal; no Codex/provider"]
fn replaced_mount_route_refuses_before_untrusted_exec() {
    let fixture = fixture();
    let original = fixture.workspace.with_file_name("original-workspace");
    std::fs::rename(&fixture.workspace, &original).unwrap();
    std::fs::create_dir(&fixture.workspace).unwrap();
    assert!(CapsuleHandle::spawn(&fixture.helper, fixture.plan).is_err());
    assert!(!fixture.workspace.join("checks-passed").exists());
    assert!(!original.join("checks-passed").exists());
}

#[test]
#[ignore = "explicit rootless namespace/static BOOTTIME expiry; no Codex/provider"]
fn original_deadline_ends_descendants_and_only_actual_queued_echild_counts_as_stop() {
    let fixture = fixture_with_lifetime(1500);
    let mut handle = CapsuleHandle::spawn(&fixture.helper, fixture.plan).unwrap();
    let _proxy = ready(&mut handle);
    let descendant = descendant(handle.witness().harness.pid);
    let pidfd = ownership::verified_pidfd(&descendant).unwrap();
    let until = Instant::now() + Duration::from_secs(4);
    while ownership::pidfd_is_live(&pidfd).unwrap() {
        assert!(Instant::now() < until);
        std::thread::sleep(Duration::from_millis(2));
    }
    let report = handle.stop().unwrap();
    assert!(report.kernel_echild() && report.reaped_count() > 0);
}

#[test]
fn unsupported_roles_write_modes_and_expired_deadlines_refuse_without_launch() {
    let root = tempfile::tempdir().unwrap();
    assert!(MountGrant::open(root.path(), MountTarget::Allowed(4), false).is_err());
    assert!(MountGrant::open(root.path(), MountTarget::DerivedAuth, true).is_err());
    assert!(
        StaticCapsulePlan::new(
            root.path(),
            Path::new("/nonexistent-static-image"),
            &ArtifactRef {
                id: Text::new("unread-static-fixture").unwrap(),
                version: Text::new("1").unwrap(),
                digest: Digest::of(b"not read")
            },
            b"canary",
            vec![],
            ownership::raw_boot_ms() - 1
        )
        .is_err()
    );
}
