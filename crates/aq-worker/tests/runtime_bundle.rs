use serde_json::Value;
use sha2::{Digest, Sha256};
use std::{fs, path::Path, process::Command};

/// The Claude Code CLI the C6a recipe (`adapter/claude.rs` `CLAUDE_CLI_VERSION`)
/// admits; any other manifest `cli_version` is refused there.
const CLAUDE_CLI_VERSION: &str = "2.1.318";

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
    for digest in ["archive_sha256", "sha256"] {
        let value = claude_pin[digest].as_str().expect("claude digest");
        assert!(
            value.len() == 64
                && value
                    .bytes()
                    .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b)),
            "claude {digest} must be lowercase hex SHA-256"
        );
    }
    let bundle = tempfile::tempdir().unwrap();
    let root = bundle.path();
    for (path, contents) in [
        ("bin/aq-worker", "runner"),
        ("bin/aq-session-host", "host"),
        ("bin/aq-agent", "agent"),
        ("bin/aq-git", "git"),
        ("harnesses/codex/codex", "codex"),
        ("harnesses/claude/claude", "claude"),
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
        "agent_client_version": "0.1.0",
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
    assert_eq!(manifest["harnesses"].as_array().unwrap().len(), 2);
    let codex = &manifest["harnesses"][0];
    assert_eq!(codex["name"], "codex");
    assert_eq!(codex["version"], pins["inputs"]["codex"]["version"]);
    let claude = &manifest["harnesses"][1];
    assert_eq!(claude["name"], "claude");
    assert_eq!(claude["version"], CLAUDE_CLI_VERSION);
    assert_eq!(claude["path"], "/opt/aq/harnesses/claude/claude");
    assert_eq!(claude["bytes"], "claude".len());
    assert_eq!(
        manifest["aq_git"]["version"],
        pins["inputs"]["aq-git"]["version"]
    );
    assert_eq!(manifest["runner"]["version"], env!("CARGO_PKG_VERSION"));
    assert_eq!(
        manifest["node"]["version"],
        pins["inputs"]["node.tar.xz"]["version"]
    );
    assert_eq!(manifest["agent_client"]["name"], "aq-agent");
    assert_eq!(manifest["agent_client"]["version"], "0.1.0");
    assert_eq!(manifest["agent_client"]["path"], "/opt/aq/bin/aq-agent");
    let rendered = serde_json::to_string(&manifest).unwrap();
    assert!(
        !rendered.contains("\"/opt/aq/bin/aq\""),
        "the sandbox bundle ships aq-agent, never the full aq CLI"
    );
    for artifact in [
        codex,
        claude,
        &manifest["agent_client"],
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
    assert_eq!(manifest["proxies"]["aq_loopback"], "in-process:aq-worker");
}
