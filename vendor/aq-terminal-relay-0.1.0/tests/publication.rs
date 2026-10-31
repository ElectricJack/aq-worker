mod support;
fn drift(root: &std::path::Path, artifacts: std::collections::BTreeMap<String, String>) {
    let actual: std::collections::BTreeSet<_> = std::fs::read_dir(root)
        .unwrap()
        .map(|p| p.unwrap().file_name().to_string_lossy().into_owned())
        .filter(|p| p.ends_with(".json"))
        .collect();
    assert_eq!(actual, artifacts.keys().cloned().collect());
    for (name, bytes) in artifacts {
        assert_eq!(
            std::fs::read_to_string(root.join(&name)).unwrap(),
            bytes,
            "{name}"
        );
        if name.starts_with("valid-") {
            aq_terminal_relay::draft::decode_document(bytes.as_bytes()).unwrap();
        }
        if name.starts_with("invalid-") {
            assert!(
                aq_terminal_relay::draft::decode_document(bytes.as_bytes()).is_err(),
                "{name}"
            );
        }
    }
}
#[test]
fn deterministic_schema_and_valid_invalid_fixtures_have_no_drift() {
    let manifest = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
    drift(&manifest.join("artifacts"), support::artifacts());
    // Packaged crates qualify from their included public artifacts; the workspace
    // additionally checks the human-readable protocol publication directory.
    let workspace = manifest.join("../../Cargo.toml");
    if std::fs::read_to_string(workspace).is_ok_and(|v| v.contains("\"crates/aq-terminal-relay\""))
    {
        drift(
            &manifest.join("../../protocol/terminal-relay/draft-v1"),
            support::artifacts(),
        );
        drift(
            &manifest.join("../aq-terminal-relay-protocol/artifacts"),
            support::peer_artifacts(),
        );
    }
}
