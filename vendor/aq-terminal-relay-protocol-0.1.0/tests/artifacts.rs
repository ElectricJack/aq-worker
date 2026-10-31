use aq_terminal_relay_protocol::*;
#[test]
fn packaged_portable_schema_fixtures_and_uuid_vectors_conform_without_daemon_dependencies() {
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("artifacts");
    let expected =
        serde_json::to_string_pretty(&schemars::schema_for!(WireDocument)).unwrap() + "\n";
    assert_eq!(
        std::fs::read_to_string(root.join("peer-schema.json")).unwrap(),
        expected
    );
    let mut valid = 0;
    let mut invalid = 0;
    for entry in std::fs::read_dir(&root).unwrap() {
        let entry = entry.unwrap();
        let name = entry.file_name().to_string_lossy().into_owned();
        let bytes = std::fs::read(entry.path()).unwrap();
        if name.starts_with("valid-") {
            decode_document(&bytes).unwrap();
            valid += 1;
        }
        if name.starts_with("invalid-") {
            assert!(decode_document(&bytes).is_err(), "{name}");
            invalid += 1;
        }
    }
    assert!(valid >= 10 && invalid >= 7);
    let vectors: serde_json::Value =
        serde_json::from_slice(&std::fs::read(root.join("uuid-vectors.json")).unwrap()).unwrap();
    let permit: epoch::Permit = serde_json::from_value(vectors["permit"].clone()).unwrap();
    for v in vectors["vectors"].as_array().unwrap() {
        assert_eq!(
            epoch::operation_id(&permit, v["sequence"].as_u64().unwrap())
                .unwrap()
                .to_string(),
            v["operation_id"].as_str().unwrap()
        );
    }
    let error = decode_document(br#"{"contract":"private input sentinel","body":{}}"#).unwrap_err();
    assert!(!error.to_string().contains("sentinel"));
}
