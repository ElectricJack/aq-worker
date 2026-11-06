use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::path::Path;
#[test]
fn published_archive_and_extracted_dependency_match_the_frozen_inventory() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../vendor");
    let inventory: Value =
        serde_json::from_slice(&std::fs::read(root.join("agent-api-contract.json")).unwrap())
            .unwrap();
    let archive = std::fs::read(root.join("aq-agent-api-0.1.0.crate")).unwrap();
    assert_eq!(
        format!("{:x}", Sha256::digest(&archive)),
        inventory["archive_sha256"]
    );
    let package = root.join("aq-agent-api-0.1.0");
    for (name, expected) in inventory["files"].as_object().unwrap() {
        let bytes = std::fs::read(package.join(name)).unwrap();
        assert_eq!(
            bytes.len() as u64,
            expected["bytes"].as_u64().unwrap(),
            "{name}"
        );
        assert_eq!(
            format!("{:x}", Sha256::digest(&bytes)),
            expected["sha256"],
            "{name}"
        );
    }
    let vcs: Value =
        serde_json::from_slice(&std::fs::read(package.join(".cargo_vcs_info.json")).unwrap())
            .unwrap();
    assert_eq!(vcs["git"]["sha1"], inventory["source_revision"]);
    assert!(vcs["git"].get("dirty").is_none_or(|value| value == false));
}
/// aq-server protocol/agent-comments/v1 is pinned byte for byte, and the
/// client's routes and comment shape follow the pinned files.
#[test]
fn agent_comments_pin_matches_vendored_files_and_client_routes() {
    use aq_agent_client::comments;
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../vendor");
    let pin: Value =
        serde_json::from_slice(&std::fs::read(root.join("agent-comments-v1-source.json")).unwrap())
            .unwrap();
    assert_eq!(pin["producer_repository"], "ElectricJack/aq-server");
    assert_eq!(pin["source_revision"].as_str().unwrap().len(), 40);
    let dir = root.join("agent-comments-v1");
    let mut names: Vec<_> = std::fs::read_dir(&dir)
        .unwrap()
        .map(|e| e.unwrap().file_name().into_string().unwrap())
        .collect();
    names.sort();
    let pinned: Vec<_> = pin["files"].as_object().unwrap().keys().cloned().collect();
    assert_eq!(names, pinned, "every vendored file is pinned");
    for (name, expected) in pin["files"].as_object().unwrap() {
        let bytes = std::fs::read(dir.join(name)).unwrap();
        assert_eq!(
            bytes.len() as u64,
            expected["bytes"].as_u64().unwrap(),
            "{name}"
        );
        assert_eq!(
            format!("{:x}", Sha256::digest(&bytes)),
            expected["sha256"],
            "{name}"
        );
    }
    let routes: Value = serde_json::from_str(comments::ROUTES).unwrap();
    assert_eq!(routes["version"], 1);
    let listed: Vec<_> = routes["routes"]
        .as_array()
        .unwrap()
        .iter()
        .map(|r| (r["method"].as_str().unwrap(), r["path"].as_str().unwrap()))
        .collect();
    assert_eq!(
        listed,
        [("POST", comments::ADD_PATH), ("POST", comments::LIST_PATH)]
    );
    assert!(
        routes["routes"][0]["request"]["body"]
            .as_str()
            .unwrap()
            .contains("1..16384 bytes")
    );
    assert!(
        routes["routes"][1]["request"]["limit?"]
            .as_str()
            .unwrap()
            .contains("1..100")
    );
    for code in [
        "invalid_request",
        "unauthorized",
        "id_conflict",
        "unsupported_api_version",
    ] {
        assert!(routes["errors"].get(code).is_some(), "{code}");
    }
    // The published example thread (owner, tombstone and agent comments) fits
    // the pinned HumanComment shape; a changed shape does not.
    let example: Value = serde_json::from_slice(
        &std::fs::read(dir.join("human-graph-v1.comments.example.json")).unwrap(),
    )
    .unwrap();
    let items = example["data"]["items"].as_array().unwrap();
    assert!(items.iter().any(|c| c["author"]["kind"] == "agent"));
    assert!(items.iter().any(|c| c["body"].is_null()));
    for c in items {
        assert!(comments::valid_comment(c), "{c}");
        let mut extra = c.clone();
        extra["extra"] = json!(1);
        assert!(!comments::valid_comment(&extra));
        let mut no_author = c.clone();
        no_author.as_object_mut().unwrap().remove("author");
        assert!(!comments::valid_comment(&no_author));
    }
}
