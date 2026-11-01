#[test]
fn legacy_artifact_identity_remains_exactly_frozen() {
    let value =
        serde_json::to_value(aq_phase_terminal_protocol::contract_artifact().unwrap()).unwrap();
    let actual = serde_json::to_string_pretty(&value).unwrap() + "\n";
    assert_eq!(actual, include_str!("../artifacts/artifact.json"));
}
