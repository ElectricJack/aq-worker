use super::*;
use aq_session_protocol::{ArtifactRef, Text};
use aq_worker::native::adapter::{
    AdapterManifest, ExpectedAdapter, FilePin, HandoffKind, InstalledAdapter,
};
#[tokio::test]
async fn exact_installed_owner_handoff_detects_tampering_without_second_store() {
    let peer = Peer::new(Uuid::new_v4(), false).await;
    let plan = peer.plan().await;
    let root = private_root();
    let mut store = MaterialStore::open(root.path(), peer.accept.identity.clone(), 1).unwrap();
    let installed = store.install(peer.material().await, &plan).unwrap();
    let receipt = installed.receipt().unwrap();
    let exe = tempfile::tempdir().unwrap();
    let path = exe.path().join("program");
    std::fs::write(&path, b"offline test file").unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o500)).unwrap();
    let reference = ArtifactRef {
        id: Text::new("offline-fixture").unwrap(),
        version: Text::new("1").unwrap(),
        digest: Digest::of(b"offline test file"),
    };
    let pin = FilePin {
        path: path.clone(),
        artifact: reference.clone(),
    };
    let manifest = AdapterManifest {
        schema_version: 1,
        adapter: reference.clone(),
        runtime: reference.clone(),
        qualification: reference.clone(),
        renderer: reference.clone(),
        handoff: HandoffKind::OpaquePrivateFileV1,
        executable: pin.clone(),
        helper: pin.clone(),
        tmux: pin,
    };
    let digest = manifest.digest().unwrap();
    let mut adapter = InstalledAdapter::open(
        manifest,
        ExpectedAdapter {
            manifest_digest: &digest,
            adapter: &reference,
            runtime: &reference,
            qualification: &reference,
            renderer: &reference,
        },
    )
    .unwrap();
    let handoff = adapter
        .material(&installed, &receipt, &receipt.binding)
        .unwrap();
    assert_eq!(
        handoff.read_private().unwrap().as_slice(),
        b"synthetic provider material\n\x00literal"
    );
    assert!(
        !serde_json::to_string(handoff.receipt())
            .unwrap()
            .contains("synthetic provider material")
    );
    // Input access is independent of executable launch checks: no 768MiB
    // binary rehash on every small credential read. No execution is exposed.
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o700)).unwrap();
    std::fs::write(&path, b"changed executable fixture").unwrap();
    assert!(adapter.revalidate().is_err());
    assert!(
        adapter
            .material(&installed, &receipt, &receipt.binding)
            .is_ok()
    );
    let mut wrong = receipt.clone();
    wrong.operation_id = Uuid::new_v4();
    assert!(
        adapter
            .material(&installed, &wrong, &receipt.binding)
            .is_err()
    );
    let mut binding = receipt.binding.clone();
    binding.attempt_id = Uuid::new_v4();
    assert!(adapter.material(&installed, &receipt, &binding).is_err());
    std::fs::write(
        root.path()
            .join(peer.lease.lease_id.to_string())
            .join("material.bin"),
        b"changed",
    )
    .unwrap();
    assert!(handoff.read_private().is_err());
    assert!(
        adapter
            .material(&installed, &receipt, &receipt.binding)
            .is_err()
    );
    assert!(store.reopen_pinned(&receipt).is_err());
    assert_eq!(std::fs::read_dir(root.path()).unwrap().count(), 1);
}
