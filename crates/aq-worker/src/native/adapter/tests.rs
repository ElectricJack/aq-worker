use super::*;
use std::os::unix::fs::{PermissionsExt, symlink};
fn artifact(id: &str, bytes: &[u8]) -> ArtifactRef {
    ArtifactRef {
        id: aq_session_protocol::Text::new(id).unwrap(),
        version: aq_session_protocol::Text::new("1").unwrap(),
        digest: Digest::of(bytes),
    }
}
fn manifest(root: &std::path::Path) -> AdapterManifest {
    let path = root.join("program");
    if path.exists() {
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o700)).unwrap();
    }
    std::fs::write(&path, b"offline test fixture only").unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o500)).unwrap();
    let pin = FilePin {
        path,
        artifact: artifact("local-file", b"offline test fixture only"),
    };
    AdapterManifest {
        schema_version: 1,
        adapter: artifact("adapter", b"a"),
        runtime: artifact("runtime", b"r"),
        qualification: artifact("qualification", b"q"),
        renderer: artifact("renderer", b"s"),
        handoff: HandoffKind::OpaquePrivateFileV1,
        executable: pin.clone(),
        helper: pin.clone(),
        tmux: pin,
    }
}
fn open(m: AdapterManifest) -> Result<InstalledAdapter> {
    let a = m.adapter.clone();
    let r = m.runtime.clone();
    let q = m.qualification.clone();
    let s = m.renderer.clone();
    let digest = m.digest().unwrap();
    InstalledAdapter::open(
        m,
        ExpectedAdapter {
            manifest_digest: &digest,
            adapter: &a,
            runtime: &r,
            qualification: &q,
            renderer: &s,
        },
    )
}
#[test]
fn actual_files_remain_pinned_and_named_replacement_refuses() {
    let dir = tempfile::tempdir().unwrap();
    let m = manifest(dir.path());
    let mut adapter = open(m.clone()).unwrap();
    adapter.revalidate().unwrap();
    std::fs::rename(&m.executable.path, dir.path().join("old")).unwrap();
    std::fs::write(&m.executable.path, b"offline test fixture only").unwrap();
    std::fs::set_permissions(&m.executable.path, std::fs::Permissions::from_mode(0o500)).unwrap();
    assert!(adapter.revalidate().is_err());
}
#[test]
fn changed_content_mode_symlink_and_pin_refuse() {
    let dir = tempfile::tempdir().unwrap();
    let m = manifest(dir.path());
    let mut adapter = open(m.clone()).unwrap();
    std::fs::set_permissions(&m.executable.path, std::fs::Permissions::from_mode(0o700)).unwrap();
    std::fs::write(&m.executable.path, b"changed").unwrap();
    assert!(adapter.revalidate().is_err());
    assert!(open(m.clone()).is_err());
    let m = manifest(dir.path());
    std::fs::set_permissions(&m.executable.path, std::fs::Permissions::from_mode(0o777)).unwrap();
    assert!(open(m).is_err());
    let mut m = manifest(dir.path());
    let link = dir.path().join("link");
    symlink(&m.executable.path, &link).unwrap();
    m.executable.path = link;
    assert!(open(m).is_err());
}
#[test]
fn independently_chosen_pins_and_closed_wire_shape_required() {
    let dir = tempfile::tempdir().unwrap();
    let m = manifest(dir.path());
    let digest = m.digest().unwrap();
    let wrong = artifact("different", b"a");
    assert!(
        InstalledAdapter::open(
            m.clone(),
            ExpectedAdapter {
                manifest_digest: &digest,
                adapter: &wrong,
                runtime: &m.runtime,
                qualification: &m.qualification,
                renderer: &m.renderer
            }
        )
        .is_err()
    );
    let mut v = serde_json::to_value(&m).unwrap();
    v["handoff"] = serde_json::json!("codex");
    assert!(serde_json::from_value::<AdapterManifest>(v).is_err());
    let mut v = serde_json::to_value(&m).unwrap();
    v["command"] = serde_json::json!("arbitrary");
    assert!(serde_json::from_value::<AdapterManifest>(v).is_err());
    let mut m = m;
    m.schema_version = 2;
    assert!(open(m).is_err());
}
#[test]
fn manifest_bound_and_oversized_sparse_file_refuse_before_hashing() {
    assert!(AdapterManifest::decode(&vec![b' '; MAX_MANIFEST + 1]).is_err());
    let dir = tempfile::tempdir().unwrap();
    let m = manifest(dir.path());
    let digest = m.digest().unwrap();
    let mut changed = m.clone();
    changed.renderer.digest = Digest::of(b"changed");
    assert!(
        InstalledAdapter::open(
            changed,
            ExpectedAdapter {
                manifest_digest: &digest,
                adapter: &m.adapter,
                runtime: &m.runtime,
                qualification: &m.qualification,
                renderer: &m.renderer
            }
        )
        .is_err()
    );
    std::fs::set_permissions(&m.executable.path, std::fs::Permissions::from_mode(0o700)).unwrap();
    OpenOptions::new()
        .write(true)
        .open(&m.executable.path)
        .unwrap()
        .set_len(MAX_EXECUTABLE + 1)
        .unwrap();
    assert!(open(m).is_err());
}
