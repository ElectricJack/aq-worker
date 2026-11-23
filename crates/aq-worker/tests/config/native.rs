use aq_session_protocol::Digest;
use aq_worker::native::{
    adapter::{AdapterManifest, FilePin, HandoffKind},
    journal::{Limits, RuntimeIdentity},
    runner::{ConfiguredPreparation, NativeConfig},
};
use std::os::unix::fs::PermissionsExt;
#[path = "../support/native_runner_config.rs"]
mod fixture;
pub(super) use fixture::config;
use fixture::write;

#[test]
fn native_config_rejects_unsupported_capacity_and_changed_executable() {
    let (_dir, mut c) = config();
    c.capacity.slots = aq_protocol::MAX_NODE_SLOTS + 1;
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
fn previous_single_slot_template_and_larger_capacity_both_load() {
    let (_dir, mut config) = config();
    config.validate().unwrap();
    config.capacity.slots = 3;
    config.validate().unwrap();
}
