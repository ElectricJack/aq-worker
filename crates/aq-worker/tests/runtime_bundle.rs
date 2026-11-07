use serde_json::Value;
use sha2::{Digest, Sha256};
use std::{fs, path::Path, process::Command};

/// The Claude Code CLI the C6a recipe (`adapter/claude.rs` `CLAUDE_CLI_VERSION`)
/// admits; any other manifest `cli_version` is refused there.
const CLAUDE_CLI_VERSION: &str = "2.1.318";
const GEMINI_CLI_VERSION: &str = aq_worker::native::adapter::gemini::GEMINI_CLI_VERSION;

fn assert_hex_digests(pin: &Value, name: &str) {
    for digest in ["archive_sha256", "sha256"] {
        let value = pin[digest].as_str().expect("digest");
        assert!(
            value.len() == 64
                && value
                    .bytes()
                    .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b)),
            "{name} {digest} must be lowercase hex SHA-256"
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
    assert_hex_digests(claude_pin, "claude");
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
    assert_hex_digests(opencode_pin, "opencode");
    let gemini_pin = &pins["inputs"]["gemini.tgz"];
    assert_eq!(gemini_pin["version"], GEMINI_CLI_VERSION);
    assert_eq!(
        gemini_pin["url"],
        format!(
            "https://registry.npmjs.org/@google/gemini-cli/-/gemini-cli-{GEMINI_CLI_VERSION}.tgz"
        )
    );
    assert_eq!(gemini_pin["bundle_member"], "package/bundle/gemini.js");
    for digest in ["sha256", "bundle_sha256"] {
        let value = gemini_pin[digest].as_str().expect("gemini digest");
        assert!(
            value.len() == 64
                && value
                    .bytes()
                    .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b)),
            "gemini {digest} must be lowercase hex SHA-256"
        );
    }
    // The agent's CLI is aq-server's static aqv2 (spec 0006 section 9 step 3),
    // pinned by aq-server commit and SHA-256 until a release asset exists.
    let aqv2_pin = &pins["inputs"]["aqv2"];
    assert_eq!(
        aqv2_pin["source"],
        "https://github.com/ElectricJack/aq-server"
    );
    let revision = aqv2_pin["source_revision"]
        .as_str()
        .expect("aq-server commit");
    assert!(
        revision.len() == 40
            && revision
                .bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b)),
        "aqv2 is pinned by a full aq-server commit"
    );
    let aqv2_digest = aqv2_pin["sha256"].as_str().expect("aqv2 digest");
    assert!(
        aqv2_digest.len() == 64
            && aqv2_digest
                .bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b)),
        "aqv2 sha256 must be lowercase hex SHA-256"
    );
    let bundle = tempfile::tempdir().unwrap();
    let root = bundle.path();
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
    fs::write(root.join("pins.json"), serde_json::to_vec(&pins).unwrap()).unwrap();
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
    let status = Command::new("python3")
        .arg(repo.join("packaging/sandbox/bundle_manifest.py"))
        .arg(root)
        .status()
        .unwrap();
    assert!(status.success());
    let manifest: Value =
        serde_json::from_slice(&fs::read(root.join("manifest.json")).unwrap()).unwrap();
    assert_eq!(manifest["schema_version"], 1);
    assert_eq!(manifest["source"], source);
    assert_eq!(manifest["harnesses"].as_array().unwrap().len(), 4);
    let codex = &manifest["harnesses"][0];
    assert_eq!(codex["name"], "codex");
    assert_eq!(codex["version"], pins["inputs"]["codex"]["version"]);
    let claude = &manifest["harnesses"][1];
    assert_eq!(claude["name"], "claude");
    assert_eq!(claude["version"], CLAUDE_CLI_VERSION);
    assert_eq!(claude["path"], "/opt/aq/harnesses/claude/claude");
    assert_eq!(claude["bytes"], "claude".len());
    let opencode = &manifest["harnesses"][2];
    assert_eq!(opencode["name"], "opencode");
    assert_eq!(opencode["version"], opencode_pin["version"]);
    assert_eq!(opencode["path"], "/opt/aq/harnesses/opencode/opencode");
    assert_eq!(opencode["bytes"], "opencode".len());
    let gemini = &manifest["harnesses"][3];
    assert_eq!(gemini["name"], "gemini");
    assert_eq!(gemini["version"], GEMINI_CLI_VERSION);
    assert_eq!(gemini["path"], "/opt/aq/harnesses/gemini/bundle/gemini.js");
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
    assert_eq!(manifest["agent_client"]["version"], aqv2_pin["version"]);
    assert_eq!(manifest["agent_client"]["path"], "/opt/aq/bin/aq");
    assert_eq!(manifest["agent_client"]["bytes"], "aq".len());
    assert_eq!(manifest["agent_client_wrapper"]["name"], "aq-agent");
    assert_eq!(
        manifest["agent_client_wrapper"]["path"],
        "/opt/aq/bin/aq-agent"
    );
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
    for artifact in [
        codex,
        claude,
        opencode,
        gemini,
        &manifest["agent_client"],
        &manifest["agent_client_wrapper"],
        &manifest["runner"],
        &manifest["session_host"],
        &manifest["aq_git"],
        &manifest["node"],
    ] {
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
    assert_eq!(manifest["proxies"]["responses"], "in-process:aq-worker");
    assert_eq!(manifest["proxies"]["gemini"], "in-process:aq-worker");
    assert_eq!(manifest["proxies"]["aq_loopback"], "in-process:aq-worker");
}
