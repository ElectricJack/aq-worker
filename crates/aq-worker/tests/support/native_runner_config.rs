use super::{AdapterManifest, FilePin, HandoffKind, Limits, NativeConfig, RuntimeIdentity};
use aq_admission::preparation_v2::{Negotiation, contract_artifact, required_features};
use aq_session_protocol::{ArtifactRef, Digest, Text};
use std::{os::unix::fs::PermissionsExt, path::Path};
fn artifact(id: &str) -> ArtifactRef {
    ArtifactRef {
        id: Text::new(id).unwrap(),
        version: Text::new("1").unwrap(),
        digest: Digest::of(id.as_bytes()),
    }
}
pub(crate) fn write(path: &Path, bytes: &[u8], mode: u32) {
    std::fs::write(path, bytes).unwrap();
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode)).unwrap();
}
pub(crate) fn config() -> (tempfile::TempDir, NativeConfig) {
    let dir = tempfile::tempdir().unwrap();
    std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
    let pin = |name: &str| {
        let path = dir.path().join(name);
        write(&path, name.as_bytes(), 0o500);
        let mut reference = artifact(name);
        reference.digest = Digest::of(name.as_bytes());
        FilePin {
            path,
            artifact: reference,
        }
    };
    let manifest = AdapterManifest {
        schema_version: 1,
        adapter: artifact("adapter"),
        runtime: artifact("runtime"),
        qualification: artifact("qualification"),
        renderer: artifact("renderer"),
        handoff: HandoffKind::OpaquePrivateFileV1,
        executable: pin("program"),
        helper: pin("helper"),
        tmux: pin("tmux"),
    };
    let manifest_path = dir.path().join("manifest.json");
    write(
        &manifest_path,
        &serde_json::to_vec(&manifest).unwrap(),
        0o600,
    );
    let c = NativeConfig {
        schema_version: 1,
        state_dir: dir.path().into(),
        git_root: dir.path().join("git"),
        git_socket: dir.path().join("git.sock"),
        capacity: Limits {
            slots: 1,
            memory_bytes: 4096,
            cpu_millis: 1000,
            disk_bytes: 4096,
        },
        runtime: RuntimeIdentity {
            runtime: manifest.runtime.clone(),
            qualification: manifest.qualification.clone(),
        },
        adapter: manifest.adapter.clone(),
        renderer: manifest.renderer.clone(),
        adapter_manifest: manifest_path,
        adapter_manifest_digest: manifest.digest().unwrap(),
        capabilities: vec![],
        features: vec![],
        negotiation: Negotiation {
            contract: contract_artifact().unwrap(),
            features: required_features(),
        }
        .into(),
        source_profile: artifact("source"),
        maximum_preparation_ms: 1000,
        inventory_definition: dir.path().join("not-created-inventory-definition"),
        inventory_reference: artifact("inventory"),
        inventory_adapter: artifact("inventory-adapter"),
        inventory_evidence: artifact("inventory-evidence"),
        harness_qualifications: vec![artifact("harness-qualification")],
        inventory_ttl_ms: 1000,
        executor: None,
        node_git: None,
    };
    (dir, c)
}
