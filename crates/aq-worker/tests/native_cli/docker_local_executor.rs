//! Epic 0 W1: one docker-local executor with the Claude and Codex harnesses. The node's
//! native configuration lists both, its inventory advertises both, and each offer's
//! `harness.harness` selects the launch.
use aq_session_protocol::{ArtifactRef, Digest};
use aq_worker::native::{
    launch::claude_docker::DockerHarness,
    result::ResultInstallationPins,
    runner::{ExecutorConfig, NativeConfig, launch_pins},
};
use serde_json::{Value, json};
use std::os::unix::fs::PermissionsExt;

fn artifact(id: &str) -> Value {
    json!({"id": id, "version": "1", "digest": Digest::of(id.as_bytes())})
}
fn reference(id: &str) -> ArtifactRef {
    serde_json::from_value(artifact(id)).unwrap()
}
/// The installed Claude docker-local executor (as the service template has it).
fn executor(codex: Option<Value>) -> Value {
    let pinned = |id: &str, path: &str| json!({"artifact": artifact(id), "path": path});
    let template = format!("sha256:{}", "5".repeat(64));
    let mut wire = json!({
        "kind": "claude-2.1.318-docker-local-result-local-v1",
        "composite_definition": "/var/lib/aq/composite.json",
        "settings": {
            "environment": artifact("environment"),
            "host": {
                "claude_executable": "/opt/aq/bin/claude", "columns": 120, "rows": 40,
                "docker_local": {"shared_root_fs": true, "socket": "/run/user/1000/aq-sandbox/podman.sock",
                    "templates": [{"digest": template, "image": format!("registry.invalid/aq-template@{template}")}]},
                "tmux": pinned("aq.host/tmux", "/usr/bin/tmux"),
                "max_workspace_bytes": 268435456, "renderer": artifact("renderer"),
                "root": "/var/lib/aq/host", "template": template
            }
        }
    });
    if let Some(codex) = codex {
        wire["settings"]["codex"] = codex;
    }
    wire
}
fn codex() -> Value {
    json!({
        "adapter": artifact("aq.codex-docker-result-local-adapter"),
        "environment": artifact("environment"),
        "composite_definition": "/var/lib/aq/codex-composite.json",
        "executable": "/opt/aq/bin/codex",
    })
}
/// A whole native configuration of a node with this executor and inventory.
fn native(dir: &std::path::Path, executor: Value, qualifications: &[&str]) -> Value {
    use aq_admission::preparation_v2::{contract_artifact, required_features};
    json!({
        "schema_version": 1,
        "state_dir": dir,
        "git_root": dir.join("git"),
        "git_socket": dir.join("git.sock"),
        "capacity": {"slots": 4, "memory_bytes": 4096, "cpu_millis": 1000, "disk_bytes": 4096},
        "runtime": {"runtime": artifact("runtime"), "qualification": artifact("qualification")},
        "adapter": artifact("aq.claude-result-local-adapter"),
        "renderer": artifact("renderer"),
        "adapter_manifest": dir.join("not-created-manifest"),
        "adapter_manifest_digest": Digest::of(b"manifest"),
        "capabilities": [],
        "features": [],
        "negotiation": {"contract": contract_artifact().unwrap(), "features": required_features()},
        "source_profile": artifact("source"),
        "maximum_preparation_ms": 1000,
        "inventory_definition": dir.join("not-created-inventory"),
        "inventory_reference": artifact("inventory"),
        "inventory_adapter": artifact("inventory-adapter"),
        "inventory_evidence": artifact("inventory-evidence"),
        "harness_qualifications": qualifications.iter().map(|q| artifact(q)).collect::<Vec<_>>(),
        "inventory_ttl_ms": 1000,
        "executor": executor,
    })
}
fn load(dir: &std::path::Path, config: &Value) -> anyhow::Result<NativeConfig> {
    let path = dir.join("native.json");
    let _ = std::fs::remove_file(&path);
    super::write_private(&path, &serde_json::to_vec(config).unwrap());
    NativeConfig::load(&path)
}

/// An installed Claude-only node still loads with no Codex harness; the same executor
/// with a `codex` block, and both harness qualifications in its inventory, offers both.
#[test]
fn native_config_lists_one_docker_local_executor_with_claude_and_codex() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
    let dir = dir.path().canonicalize().unwrap();
    let claude = load(&dir, &native(&dir, executor(None), &["aq.harness/claude"])).unwrap();
    let Some(ExecutorConfig::ClaudeDockerLocalResultLocal { settings, .. }) = &claude.executor
    else {
        panic!("Claude docker-local executor expected");
    };
    assert!(settings.codex.is_none());

    let both = [
        "aq.harness/claude-docker-local",
        "aq.harness/codex-docker-local",
    ];
    let config = load(&dir, &native(&dir, executor(Some(codex())), &both)).unwrap();
    let Some(ExecutorConfig::ClaudeDockerLocalResultLocal { settings, .. }) = &config.executor
    else {
        panic!("Claude docker-local executor expected");
    };
    let codex_settings = settings.codex.as_ref().expect("codex harness");
    assert_eq!(
        codex_settings.executable,
        std::path::Path::new("/opt/aq/bin/codex")
    );
    assert_eq!(
        codex_settings.adapter,
        reference("aq.codex-docker-result-local-adapter")
    );
    assert_eq!(codex_settings.environment, settings.environment);
    assert_eq!(
        config.harness_qualifications,
        both.map(reference).to_vec(),
        "the inventory advertises both harnesses"
    );

    // Closed shapes: an unknown or missing Codex key, or a repeated qualification, is refused.
    let mut extra = codex();
    extra["sandbox"] = json!("workspace-write");
    assert!(load(&dir, &native(&dir, executor(Some(extra)), &both)).is_err());
    let mut missing = codex();
    missing
        .as_object_mut()
        .unwrap()
        .remove("composite_definition");
    assert!(load(&dir, &native(&dir, executor(Some(missing)), &both)).is_err());
    let repeated = [both[1], both[1]];
    assert!(load(&dir, &native(&dir, executor(Some(codex())), &repeated)).is_err());
}

fn pins(adapter: &str) -> ResultInstallationPins {
    ResultInstallationPins {
        adapter: reference(adapter),
        contract: aq_agent_result_local::artifact(),
        client_manifest: reference("aq.agent-result-client-installation"),
        client_executable: reference("client"),
        environment: reference("environment"),
        recipe: reference("recipe"),
        permission_digest: Digest::of(b"plan"),
        reserved_client_bytes: 64 * 1024 * 1024,
    }
}
/// Every recipe a result adapter profile can name is an installable pin, the
/// Codex docker-local one included; an unknown recipe is not.
#[test]
fn installation_pins_accept_each_known_recipe_only() {
    for recipe in [
        aq_codex_mapper::result_local_recipe_artifact(),
        aq_codex_mapper::interactive_result_local_recipe_artifact(),
        aq_worker::native::adapter::claude::docker_local_recipe_artifact(),
        aq_codex_mapper::docker_local::recipe_artifact(),
    ] {
        let mut known = pins("adapter");
        known.recipe = recipe;
        known.validate().unwrap();
    }
    assert!(pins("adapter").validate().is_err());
}
/// The offer's `harness.harness` picks the result pins and so the launch; Codex needs
/// the node's Codex settings, and any other harness is refused before a launch.
#[test]
fn launch_selection_follows_the_offered_harness() {
    let claude = pins("aq.claude-result-local-adapter");
    let codex = pins("aq.codex-docker-result-local-adapter");
    let selected = launch_pins("claude", Some(&claude), Some(&codex)).unwrap();
    assert_eq!(selected, (DockerHarness::Claude, claude.clone()));
    let selected = launch_pins("codex", Some(&claude), Some(&codex)).unwrap();
    assert_eq!(selected, (DockerHarness::Codex, codex.clone()));
    // A Claude-only node: Claude as before, Codex refused.
    assert_eq!(
        launch_pins("claude", Some(&claude), None).unwrap().1,
        claude
    );
    let refused = launch_pins("codex", Some(&claude), None).unwrap_err();
    assert!(
        refused.to_string().contains("no Codex harness"),
        "{refused:#}"
    );
    for other in ["gemini", "opencode", "", "Claude"] {
        assert!(
            launch_pins(other, Some(&claude), Some(&codex)).is_err(),
            "{other}"
        );
    }
    assert!(launch_pins("claude", None, Some(&codex)).is_err());
}

/// Gate 1 (2026-11-18): the driver is the worker's own executable, so the two cannot be
/// out of step. A template that still names a separate driver is refused.
#[test]
fn the_driver_is_the_running_executable_and_cannot_be_configured() {
    use sha2::Digest as _;
    let (driver, bytes) = aq_worker::native::launch::claude_docker::own_driver().unwrap();
    let own = std::fs::canonicalize(std::env::current_exe().unwrap()).unwrap();
    let image = std::fs::read(&own).unwrap();
    assert_eq!(driver.path, own);
    assert_eq!(bytes, image.len() as u64);
    assert_eq!(driver.artifact.id.as_str(), "aq.claude-docker-local-driver");
    assert_eq!(
        driver.artifact.digest.as_str(),
        format!("{:x}", sha2::Sha256::digest(&image))
    );

    assert!(serde_json::from_value::<ExecutorConfig>(executor(None)).is_ok());
    let mut pinned = executor(None);
    pinned["settings"]["host"]["driver"] = json!({
        "artifact": artifact("aq.claude-docker-local-driver"),
        "path": "/var/lib/aq/aq-sandbox-driver",
    });
    let refused = serde_json::from_value::<ExecutorConfig>(pinned)
        .err()
        .unwrap();
    assert!(refused.to_string().contains("driver"), "{refused}");
}
