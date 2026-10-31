use serde_json::Value;
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
