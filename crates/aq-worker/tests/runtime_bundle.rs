use serde_json::Value;
use sha2::{Digest, Sha256};
use std::{fs, path::Path, process::Command};

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
    let bundle = tempfile::tempdir().unwrap();
    let root = bundle.path();
    for (path, contents) in [
        ("bin/aq-worker", "runner"),
        ("bin/aq-session-host", "host"),
        ("bin/aq-git", "git"),
        ("harnesses/codex/codex", "codex"),
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
    assert_eq!(manifest["harnesses"].as_array().unwrap().len(), 1);
    let codex = &manifest["harnesses"][0];
    assert_eq!(codex["name"], "codex");
    assert_eq!(codex["version"], pins["inputs"]["codex"]["version"]);
    assert_eq!(
        manifest["aq_git"]["version"],
        pins["inputs"]["aq-git"]["version"]
    );
    assert_eq!(manifest["runner"]["version"], env!("CARGO_PKG_VERSION"));
    assert_eq!(
        manifest["node"]["version"],
        pins["inputs"]["node.tar.xz"]["version"]
    );
    for artifact in [
        codex,
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
