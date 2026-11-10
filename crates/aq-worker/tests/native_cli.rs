//! Native CLI refusals expose useful causes before their conservative disposition.
#[path = "native_cli/result_profile.rs"]
mod result_profile;
use aq_admission::preparation_v2::{contract_artifact, required_features};
use aq_session_protocol::{ArtifactRef, Digest, Text};
use aq_worker::config::WorkerConfig;
use serde_json::json;
use std::{os::unix::fs::PermissionsExt, path::Path, process::Command};
use uuid::Uuid;

fn write_private(path: &Path, bytes: &[u8]) {
    std::fs::write(path, bytes).unwrap();
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600)).unwrap();
}

fn assert_refusal(worker: &Path, native: &Path, cause: &str) {
    for (command, disposition) in [
        ("check-native", "native configuration check refused"),
        (
            "run-native",
            "native execution unavailable or outcome unknown; preserve state for reconciliation",
        ),
    ] {
        let output = Command::new(env!("CARGO_BIN_EXE_aq-worker"))
            .arg("--config")
            .arg(worker)
            .arg(command)
            .arg("--native-config")
            .arg(native)
            .output()
            .unwrap();
        assert_eq!(output.status.code(), Some(2));
        let stderr = String::from_utf8(output.stderr).unwrap();
        let cause_position = stderr.find(cause).expect(&stderr);
        let disposition_position = stderr.find(disposition).expect(&stderr);
        assert!(cause_position < disposition_position, "{stderr}");
        for secret in ["private-node-token", "private-cf-id", "private-cf-secret"] {
            assert!(!stderr.contains(secret), "{stderr}");
            assert!(!String::from_utf8_lossy(&output.stdout).contains(secret));
        }
    }
}

#[test]
fn native_commands_report_preflight_failure_before_disposition() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
    let worker = dir.path().join("worker.toml");
    WorkerConfig {
        server_url: "https://example.invalid".into(),
        node_id: Uuid::from_u128(1),
        credential: "private-node-token".into(),
        cf_client_id: None,
        cf_client_secret: None,
    }
    .save(&worker)
    .unwrap();
    let artifact = |id: &str| ArtifactRef {
        id: Text::new(id).unwrap(),
        version: Text::new("1").unwrap(),
        digest: Digest::of(id.as_bytes()),
    };
    let fixture: serde_json::Value = serde_json::from_str(include_str!(
        "../../../vendor/aq-admission-0.1.7/artifacts/control.fixture.json"
    ))
    .unwrap();
    let manifest = &fixture["native_offer"]["offered"]["offer"]["manifest"];
    // The legacy executor deterministically refuses in preflight, before any
    // installation files, journal or network admission are accessed.
    let config = json!({
        "schema_version": 1,
        "state_dir": dir.path(),
        "git_root": dir.path().join("git"),
        "git_socket": dir.path().join("git.sock"),
        "capacity": {"slots": 1, "memory_bytes": 4096, "cpu_millis": 1000, "disk_bytes": 4096},
        "runtime": {"runtime": artifact("runtime"), "qualification": artifact("qualification")},
        "adapter": artifact("adapter"),
        "renderer": artifact("renderer"),
        "adapter_manifest": dir.path().join("not-created-manifest"),
        "adapter_manifest_digest": Digest::of(b"manifest"),
        "capabilities": [],
        "features": [],
        "negotiation": {"contract": contract_artifact().unwrap(), "features": required_features()},
        "source_profile": artifact("source"),
        "maximum_preparation_ms": 1000,
        "inventory_definition": dir.path().join("not-created-inventory"),
        "inventory_reference": artifact("inventory"),
        "inventory_adapter": artifact("inventory-adapter"),
        "inventory_evidence": artifact("inventory-evidence"),
        "harness_qualifications": [artifact("harness-qualification")],
        "inventory_ttl_ms": 1000,
        "executor": {
            "kind": "codex-0.166-exec-stdin-v1",
            "settings": {
                "host_root": dir.path().join("not-created-host"),
                "harness": manifest["harness"],
                "model": manifest["model"],
                "permissions_digest": Digest::of(b"permissions"),
                "columns": 80,
                "rows": 24
            }
        }
    });
    let native = dir.path().join("native.json");
    write_private(&native, &serde_json::to_vec(&config).unwrap());
    assert_refusal(
        &worker,
        &native,
        "legacy sealed-memfd executor is not qualified",
    );
    assert!(!dir.path().join("journal").exists());
    assert!(!dir.path().join("materials").exists());

    let mut malformed = config;
    malformed["executor"]["kind"] = json!("private-cf-secret");
    write_private(&native, &serde_json::to_vec(&malformed).unwrap());
    assert_refusal(
        &worker,
        &native,
        "invalid native configuration: invalid JSON Data",
    );
}

#[test]
fn native_commands_report_worker_config_context_without_private_toml() {
    let dir = tempfile::tempdir().unwrap();
    let worker = dir.path().join("worker.toml");
    let native = dir.path().join("native.json");
    assert_refusal(&worker, &native, "No such file or directory");
    write_private(
        &worker,
        b"credential = 'private-node-token'\ncf_client_id = 'private-cf-id'\ncf_client_secret = 'private-cf-secret'\ninvalid",
    );
    assert_refusal(
        &worker,
        &native,
        "invalid TOML configuration (input redacted)",
    );
}
