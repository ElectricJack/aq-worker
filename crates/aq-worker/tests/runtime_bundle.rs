use serde_json::Value;
use sha2::{Digest, Sha256};
use std::{fs, path::Path, process::Command};

/// The Claude Code CLI the C6a recipe (`adapter/claude.rs` `CLAUDE_CLI_VERSION`)
/// admits; any other manifest `cli_version` is refused there.
const CLAUDE_CLI_VERSION: &str = "2.1.318";
const GEMINI_CLI_VERSION: &str = aq_worker::native::adapter::gemini::GEMINI_CLI_VERSION;

fn lowercase_hex(value: &Value, length: usize) -> bool {
    value.as_str().is_some_and(|s| {
        s.len() == length
            && s.bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
    })
}

fn assert_digests(pin: &Value, name: &str, fields: &[&str]) {
    for &field in fields {
        assert!(
            lowercase_hex(&pin[field], 64),
            "{name} {field} must be lowercase hex SHA-256"
        );
    }
}

fn assert_harness_pins(pins: &Value) {
    assert_eq!(
        pins["inputs"]["codex"]["version"],
        aq_codex_mapper::CODEX_CLI_VERSION
    );
    let claude_pin = &pins["inputs"]["claude"];
    assert_eq!(claude_pin["version"], CLAUDE_CLI_VERSION);
    assert_eq!(
        claude_pin["url"],
        format!(
            "https://registry.npmjs.org/@anthropic-ai/claude-code-linux-x64/-/claude-code-linux-x64-{CLAUDE_CLI_VERSION}.tgz"
        )
    );
    assert_eq!(claude_pin["archive_member"], "package/claude");
    assert_digests(claude_pin, "claude", &["archive_sha256", "sha256"]);
    let opencode_pin = &pins["inputs"]["opencode"];
    let opencode_version = aq_worker::native::adapter::opencode::OPENCODE_CLI_VERSION;
    assert_eq!(opencode_pin["version"], opencode_version);
    assert_eq!(
        opencode_pin["url"],
        format!(
            "https://registry.npmjs.org/opencode-linux-x64/-/opencode-linux-x64-{opencode_version}.tgz"
        )
    );
    assert_eq!(opencode_pin["archive_member"], "package/bin/opencode");
    assert_digests(opencode_pin, "opencode", &["archive_sha256", "sha256"]);
    let gemini_pin = &pins["inputs"]["gemini.tgz"];
    assert_eq!(gemini_pin["version"], GEMINI_CLI_VERSION);
    assert_eq!(
        gemini_pin["url"],
        format!(
            "https://registry.npmjs.org/@google/gemini-cli/-/gemini-cli-{GEMINI_CLI_VERSION}.tgz"
        )
    );
    assert_eq!(gemini_pin["bundle_member"], "package/bundle/gemini.js");
    assert_digests(gemini_pin, "gemini", &["sha256", "bundle_sha256"]);
}

fn assert_agent_pin(pins: &Value) {
    // The agent's CLI is aq-server's static aqv2 (spec 0006 section 9 step 3),
    // pinned by aq-server commit and SHA-256 until a release asset exists.
    let aqv2_pin = &pins["inputs"]["aqv2"];
    assert_eq!(
        aqv2_pin["source"],
        "https://github.com/ElectricJack/aq-server"
    );
    assert!(
        lowercase_hex(&aqv2_pin["source_revision"], 40),
        "aqv2 is pinned by a full aq-server commit"
    );
    assert_digests(aqv2_pin, "aqv2", &["sha256"]);
}

fn write_bundle(root: &Path, pins: &Value) -> Value {
    for (path, contents) in [
        ("bin/aq-worker", "runner"),
        ("bin/aq-session-host", "host"),
        ("bin/aq", "aq"),
        ("bin/aq-agent", "#!/bin/sh\nexec /opt/aq/bin/aq \"$@\"\n"),
        ("bin/aq-git", "git"),
        ("harnesses/codex/codex", "codex"),
        ("harnesses/claude/claude", "claude"),
        ("harnesses/opencode/opencode", "opencode"),
        ("harnesses/gemini/bundle/gemini.js", "gemini"),
        ("node/bin/node", "node"),
    ] {
        fs::create_dir_all(root.join(path).parent().unwrap()).unwrap();
        fs::write(root.join(path), contents).unwrap();
    }
    fs::write(root.join("pins.json"), serde_json::to_vec(pins).unwrap()).unwrap();
    let source = serde_json::json!({
        "revision": "a".repeat(40),
        "runner_version": env!("CARGO_PKG_VERSION"),
        "session_host_version": "0.1.5",
        "archive_sha256": "b".repeat(64)
    });
    fs::write(
        root.join("source.json"),
        serde_json::to_vec(&source).unwrap(),
    )
    .unwrap();
    source
}

fn generate_manifest(root: &Path, repo: &Path) -> Value {
    let status = Command::new("python3")
        .arg(repo.join("packaging/sandbox/bundle_manifest.py"))
        .arg(root)
        .status()
        .unwrap();
    assert!(status.success());
    serde_json::from_slice(&fs::read(root.join("manifest.json")).unwrap()).unwrap()
}

fn assert_harness_manifest(manifest: &Value, pins: &Value) {
    assert_eq!(manifest["harnesses"].as_array().unwrap().len(), 4);
    let harnesses = manifest["harnesses"].as_array().unwrap();
    for (entry, name, version) in [
        (&harnesses[0], "codex", &pins["inputs"]["codex"]["version"]),
        (
            &harnesses[1],
            "claude",
            &serde_json::json!(CLAUDE_CLI_VERSION),
        ),
        (
            &harnesses[2],
            "opencode",
            &pins["inputs"]["opencode"]["version"],
        ),
        (
            &harnesses[3],
            "gemini",
            &serde_json::json!(GEMINI_CLI_VERSION),
        ),
    ] {
        assert_eq!(entry["name"], name);
        assert_eq!(entry["version"], *version);
    }
    for (entry, path, size) in [
        (
            &harnesses[1],
            "/opt/aq/harnesses/claude/claude",
            Some("claude".len()),
        ),
        (
            &harnesses[2],
            "/opt/aq/harnesses/opencode/opencode",
            Some("opencode".len()),
        ),
        (
            &harnesses[3],
            "/opt/aq/harnesses/gemini/bundle/gemini.js",
            None,
        ),
    ] {
        assert_eq!(entry["path"], path);
        if let Some(size) = size {
            assert_eq!(entry["bytes"], size);
        }
    }
}

fn assert_binary_manifest(manifest: &Value, pins: &Value) {
    assert_eq!(
        manifest["aq_git"]["version"],
        pins["inputs"]["aq-git"]["version"]
    );
    assert_eq!(manifest["runner"]["version"], env!("CARGO_PKG_VERSION"));
    assert_eq!(
        manifest["node"]["version"],
        pins["inputs"]["node.tar.xz"]["version"]
    );
    // B7's "never ship aq" check, inverted (spec 0006 owner decision 5).
    assert_eq!(manifest["agent_client"]["name"], "aq");
    assert_eq!(
        manifest["agent_client"]["version"],
        pins["inputs"]["aqv2"]["version"]
    );
    assert_eq!(manifest["agent_client"]["path"], "/opt/aq/bin/aq");
    assert_eq!(manifest["agent_client"]["bytes"], "aq".len());
    assert_eq!(manifest["agent_client_wrapper"]["name"], "aq-agent");
    assert_eq!(
        manifest["agent_client_wrapper"]["path"],
        "/opt/aq/bin/aq-agent"
    );
}

fn assert_runtime_build(repo: &Path) {
    let definition = fs::read_to_string(repo.join("packaging/sandbox/Dockerfile.runtime")).unwrap();
    for check in [
        "install -o root -g root -m 0755 aqv2 /opt/aq/bin/aq ",
        "test \"$(stat -c '%a %u %g' /opt/aq/bin/aq)\" = \"755 0 0\"",
        "/opt/aq/bin/aq --help | grep -q '^Usage: aq '",
        "exec /opt/aq/bin/aq \"$@\"",
        // The wrapper's `result` reaches `aq result` (spec 0006 §9 step 4).
        "/opt/aq/bin/aq-agent result status 2>&1 >/dev/null | grep -q '\"invalid_contract_or_private_state\"'",
        // The shared target cache needs the touch step (AGENTS.md).
        "find crates vendor protocol packaging -type f -exec touch {} + \\\n    && cargo build --locked",
    ] {
        assert!(definition.contains(check), "bundle build lacks: {check}");
    }
    assert!(
        definition.contains("test \"$(/opt/aq/bin/aq --version)\" = \"aq $("),
        "the bundle build runs aq --version against the pin"
    );
    assert!(
        !definition.contains("test ! -e /opt/aq/bin/aq"),
        "the bundle must ship aq"
    );
}

fn assert_artifact_digests(root: &Path, manifest: &Value) {
    for artifact in manifest["harnesses"].as_array().unwrap().iter().chain([
        &manifest["agent_client"],
        &manifest["agent_client_wrapper"],
        &manifest["runner"],
        &manifest["session_host"],
        &manifest["aq_git"],
        &manifest["node"],
    ]) {
        let path = artifact["path"]
            .as_str()
            .unwrap()
            .strip_prefix("/opt/aq/")
            .unwrap();
        assert_eq!(
            artifact["sha256"],
            format!("{:x}", Sha256::digest(fs::read(root.join(path)).unwrap()))
        );
    }
}

#[test]
fn runtime_bundle_lists_pinned_harness_versions() {
    let repo = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    let pins: Value = serde_json::from_slice(
        &fs::read(repo.join("packaging/sandbox/pins.json")).expect("sandbox pins"),
    )
    .unwrap();
    assert_harness_pins(&pins);
    assert_agent_pin(&pins);
    let bundle = tempfile::tempdir().unwrap();
    let source = write_bundle(bundle.path(), &pins);
    let manifest = generate_manifest(bundle.path(), &repo);
    assert_eq!(manifest["schema_version"], 1);
    assert_eq!(manifest["source"], source);
    assert_harness_manifest(&manifest, &pins);
    assert_binary_manifest(&manifest, &pins);
    assert_runtime_build(&repo);
    assert_artifact_digests(bundle.path(), &manifest);
    assert_eq!(manifest["proxies"]["responses"], "in-process:aq-worker");
    assert_eq!(manifest["proxies"]["gemini"], "in-process:aq-worker");
    assert_eq!(manifest["proxies"]["aq_loopback"], "in-process:aq-worker");
}
