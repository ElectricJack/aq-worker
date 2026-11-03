use super::*;
use crate::native::adapter::{AdapterManifest, FilePin, HandoffKind};
use aq_admission::preparation_v2::{Negotiation, contract_artifact, required_features};
use aq_session_protocol::{ArtifactRef, Digest, Text};
use std::{
    os::unix::fs::{PermissionsExt, symlink},
    path::Path,
};
fn artifact(id: &str) -> ArtifactRef {
    ArtifactRef {
        id: Text::new(id).unwrap(),
        version: Text::new("1").unwrap(),
        digest: Digest::of(id.as_bytes()),
    }
}
fn write(path: &Path, bytes: &[u8], mode: u32) {
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
    };
    (dir, c)
}
#[tokio::test]
async fn unavailable_executor_refuses_before_any_journal_or_network_admission() {
    let (dir, c) = config();
    let (_, shutdown) = watch::channel(false);
    let worker = WorkerConfig {
        server_url: "http://127.0.0.1:9".into(),
        node_id: Uuid::new_v4(),
        credential: "test-node-credential".into(),
        cf_client_id: None,
        cf_client_secret: None,
    };
    let error = run_once(&worker, c, &mut UnavailableExecutor, shutdown)
        .await
        .unwrap_err();
    assert!(
        error
            .to_string()
            .contains("no qualified native executor installed")
    );
    assert!(!dir.path().join("journal").exists());
    assert!(!dir.path().join("materials").exists());
    // The absent inventory file/Git socket would fail if any admission setup had run.
}
#[test]
fn private_config_refuses_symlink_public_mode_and_oversize() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("config");
    write(&path, b"{}", 0o600);
    let link = dir.path().join("link");
    symlink(&path, &link).unwrap();
    assert!(config::private_file(&link, 10).is_err());
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
    assert!(config::private_file(&path, 10).is_err());
    write(&path, b"too many bytes", 0o600);
    assert!(config::private_file(&path, 10).is_err());
}
#[test]
fn native_config_rejects_unsupported_capacity_and_changed_executable() {
    let (_dir, mut c) = config();
    c.capacity.slots = 2;
    assert!(c.validate().is_err());
    c.capacity.slots = 1;
    assert!(c.installed_adapter().is_ok());
    std::fs::set_permissions(
        c.state_dir.join("program"),
        std::fs::Permissions::from_mode(0o700),
    )
    .unwrap();
    write(&c.state_dir.join("program"), b"tampered", 0o500);
    assert!(c.installed_adapter().is_err());
}

#[test]
fn native_config_load_accepts_exact_long_profiles_and_refuses_changed_contract() {
    let (dir, c) = config();
    let mut value = serde_json::json!({
        "schema_version": c.schema_version,
        "state_dir": c.state_dir,
        "git_root": c.git_root,
        "git_socket": c.git_socket,
        "capacity": c.capacity,
        "runtime": c.runtime,
        "adapter": c.adapter,
        "renderer": c.renderer,
        "adapter_manifest": c.adapter_manifest,
        "adapter_manifest_digest": c.adapter_manifest_digest,
        "capabilities": c.capabilities,
        "features": c.features,
        "negotiation": c.negotiation,
        "source_profile": c.source_profile,
        "maximum_preparation_ms": c.maximum_preparation_ms,
        "inventory_definition": c.inventory_definition,
        "inventory_reference": c.inventory_reference,
        "inventory_adapter": c.inventory_adapter,
        "inventory_evidence": c.inventory_evidence,
        "harness_qualifications": c.harness_qualifications,
        "inventory_ttl_ms": c.inventory_ttl_ms,
        "executor": null
    });
    let path = dir.path().join("native.json");
    for (contract, features, refresh) in [
        (
            aq_admission::preparation_v2::contract_artifact().unwrap(),
            serde_json::to_value(aq_admission::preparation_v2::required_features()).unwrap(),
            false,
        ),
        (
            aq_admission::preparation_v2::long_contract_artifact().unwrap(),
            serde_json::to_value(aq_admission::preparation_v2::required_features()).unwrap(),
            false,
        ),
        (
            aq_admission::preparation_refresh_v1::contract_artifact().unwrap(),
            serde_json::to_value(aq_admission::preparation_refresh_v1::required_features())
                .unwrap(),
            true,
        ),
        (
            aq_admission::preparation_refresh_v1::long_contract_artifact().unwrap(),
            serde_json::to_value(aq_admission::preparation_refresh_v1::required_features())
                .unwrap(),
            true,
        ),
    ] {
        value["negotiation"] = serde_json::json!({"contract": contract, "features": features});
        write(&path, &serde_json::to_vec(&value).unwrap(), 0o600);
        let loaded = NativeConfig::load(&path).unwrap();
        assert_eq!(
            matches!(loaded.negotiation, ConfiguredPreparation::Refresh(_)),
            refresh
        );
        value["negotiation"]["contract"]["digest"] =
            serde_json::to_value(Digest::of(b"changed contract")).unwrap();
        write(&path, &serde_json::to_vec(&value).unwrap(), 0o600);
        assert!(NativeConfig::load(&path).is_err());
    }
}

#[test]
fn existing_native_state_is_opened_for_validated_recovery() {
    let (dir, c) = config();
    let node = Uuid::new_v4();
    let root = dir.path().join("journal");
    let initial = Journal::create(&root, node, c.capacity.clone()).unwrap();
    let old = initial.state().identity.boot_id;
    drop(initial);
    let reopened = open_journal(&c, node).unwrap();
    assert!(reopened.recovery_required());
    assert_eq!(reopened.state().identity.boot_id, old);
}
