use anyhow::Result;
use aq_session_host::observed::{MAX_NATIVE_EXEC_BYTES, NativeExecPool};
use aq_session_protocol::Text;
use aq_session_protocol::{ArtifactRef, Digest};
use aq_worker::native::adapter::{
    AdapterManifest, ExpectedAdapter, FilePin, HandoffKind,
    native_exec::{NativeExecInstallation, NativeExecKind, NativeExecManifest},
};
use std::os::unix::fs::PermissionsExt;
fn manifest(root: &std::path::Path) -> NativeExecManifest {
    let mut bytes = vec![0u8; 128];
    bytes[..7].copy_from_slice(b"\x7fELF\x02\x01\x01");
    bytes[16] = 2;
    bytes[18] = if cfg!(target_arch = "aarch64") {
        183
    } else {
        62
    };
    let path = root.join("program");
    if path.exists() {
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o700)).unwrap();
    }
    std::fs::write(&path, &bytes).unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o500)).unwrap();
    let reference = ArtifactRef {
        id: Text::new("fixture").unwrap(),
        version: Text::new("1").unwrap(),
        digest: Digest::of(&bytes),
    };
    let pin = FilePin {
        path,
        artifact: reference.clone(),
    };
    NativeExecManifest {
        schema_version: 1,
        profile: NativeExecKind::Codex0166ExecStdinV1,
        executable_bytes: bytes.len() as u64,
        installation: AdapterManifest {
            schema_version: 1,
            adapter: reference.clone(),
            runtime: reference.clone(),
            qualification: reference.clone(),
            renderer: reference,
            handoff: HandoffKind::OpaquePrivateFileV1,
            executable: pin.clone(),
            helper: pin.clone(),
            tmux: pin,
        },
    }
}
fn open(m: NativeExecManifest) -> Result<NativeExecInstallation> {
    let r = m.installation.adapter.clone();
    let digest = m.digest().unwrap();
    NativeExecInstallation::open(
        m,
        ExpectedAdapter {
            manifest_digest: &digest,
            adapter: &r,
            runtime: &r,
            qualification: &r,
            renderer: &r,
        },
        &NativeExecPool::new(1024, 1).unwrap(),
    )
}
#[test]
fn explicit_profile_preserves_exact_size_digest_and_independent_identity() {
    let dir = tempfile::tempdir().unwrap();
    let m = manifest(dir.path());
    let path = m.installation.executable.path.clone();
    let digest = m.digest().unwrap();
    let bytes = m.executable_bytes;
    let mut installed = open(m).unwrap();
    assert_eq!(installed.digest(), &digest);
    assert_eq!(installed.executable_bytes(), bytes);
    installed.revalidate().unwrap();
    std::fs::rename(&path, dir.path().join("old")).unwrap();
    std::fs::write(&path, b"offline installation fixture").unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o500)).unwrap();
    assert!(installed.revalidate().is_err());
}
#[test]
fn undeclared_profile_or_actual_size_mismatch_refuse() {
    let dir = tempfile::tempdir().unwrap();
    let mut m = manifest(dir.path());
    m.executable_bytes += 1;
    assert!(open(m).is_err());
    let mut v = serde_json::to_value(manifest(dir.path())).unwrap();
    v["profile"] = serde_json::json!("large-any-executable");
    assert!(NativeExecManifest::decode(&serde_json::to_vec(&v).unwrap()).is_err());
    let mut m = manifest(dir.path());
    m.executable_bytes = MAX_NATIVE_EXEC_BYTES + 1;
    assert!(open(m).is_err());
    let m = manifest(dir.path());
    let r = m.installation.adapter.clone();
    let wrong = Digest::of(b"different independently selected profile");
    assert!(
        NativeExecInstallation::open(
            m,
            ExpectedAdapter {
                manifest_digest: &wrong,
                adapter: &r,
                runtime: &r,
                qualification: &r,
                renderer: &r
            },
            &NativeExecPool::new(1024, 1).unwrap()
        )
        .is_err()
    );
}
