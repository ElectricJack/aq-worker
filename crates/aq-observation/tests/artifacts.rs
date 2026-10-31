#[allow(dead_code)]
#[path = "../examples/export.rs"]
mod exporter;
use aq_observation::*;
use serde_json::Value;
use std::path::PathBuf;
#[test]
fn generated_source_reparse_capability_and_normalized_goldens_do_not_drift() {
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    for (name, value) in exporter::documents() {
        let expected: Value =
            serde_json::from_slice(&std::fs::read(root.join("artifacts").join(name)).unwrap())
                .unwrap();
        assert_eq!(value, expected, "{name} drift");
    }
    let mirror = root.join("../../docs/implementation/WRK-15-source-formats.json");
    if mirror.exists() {
        assert_eq!(
            std::fs::read(mirror).unwrap(),
            std::fs::read(root.join("artifacts/source-formats.json")).unwrap()
        );
    }
}
#[test]
fn source_formats_are_exact_and_native_release_or_complete_accounting_is_not_implied() {
    let docs = exporter::documents();
    let caps = &docs
        .iter()
        .find(|(name, _)| *name == "capabilities.json")
        .unwrap()
        .1;
    for entry in caps["parsers"].as_array().unwrap() {
        assert_eq!(entry["selected_released_cli_qualified"], false);
        assert_eq!(entry["complete_accounting_policy_eligible"], false);
        assert!(entry["selected_cli_version"].is_null());
        assert_eq!(entry["format_source_revision"].as_str().unwrap().len(), 40);
    }
    let golden = &docs
        .iter()
        .find(|(name, _)| *name == "normalized-golden.json")
        .unwrap()
        .1;
    let mut source = golden["cases"][0]["source"].clone();
    source["format"] = "latest-codex-auto".into();
    assert!(serde_json::from_value::<Source>(source).is_err());
}
