use aq_observation::{Format, discovery::*, lifecycle::Text};
use uuid::Uuid;
fn discovery(temp: &tempfile::TempDir, format: Format) -> Discovery {
    Discovery {
        source_id: Uuid::from_u128(1),
        root: temp.path().into(),
        native_session_key: Text::new("native-fixture").unwrap(),
        format,
        exact_member: None,
        maximum_entries: 256,
        maximum_candidates: 16,
    }
}
#[test]
fn exact_known_key_never_falls_back_to_newer_or_unrelated_session() {
    let temp = tempfile::tempdir().unwrap();
    let mut d = discovery(&temp, Format::ClaudeV1Jsonl9a9b8e8);
    std::fs::write(
        temp.path().join("newer.jsonl"),
        b"{\"sessionId\":\"other-session\"}\n",
    )
    .unwrap();
    assert_eq!(d.resolve().unwrap(), Resolution::Missing);
    std::fs::write(
        temp.path().join("native-fixture.jsonl"),
        b"{\"sessionId\":\"native-fixture\"}\n",
    )
    .unwrap();
    assert!(matches!(d.resolve().unwrap(), Resolution::Found(_)));
    d.exact_member = Some("absent.jsonl".into());
    assert_eq!(d.resolve().unwrap(), Resolution::Missing);
    d.exact_member = Some("newer.jsonl".into());
    assert_eq!(d.resolve().unwrap(), Resolution::IdentityMismatch);
}
#[test]
fn codex_discovery_refuses_ambiguous_or_exhausted_scan_instead_of_guessing() {
    let temp = tempfile::tempdir().unwrap();
    let mut d = discovery(&temp, Format::CodexRollout67f0994);
    for name in [
        "rollout-a-native-fixture.jsonl",
        "rollout-b-native-fixture.jsonl",
    ] {
        std::fs::write(
            temp.path().join(name),
            b"{\"type\":\"session_meta\",\"payload\":{\"id\":\"native-fixture\"}}\n",
        )
        .unwrap();
    }
    assert_eq!(d.resolve().unwrap(), Resolution::Ambiguous);
    d.maximum_entries = 1;
    assert_eq!(d.resolve().unwrap(), Resolution::BudgetExceeded);
    d.exact_member = Some("rollout-a-native-fixture.jsonl".into());
    assert!(matches!(d.resolve().unwrap(), Resolution::Found(_)));
}
#[test]
fn gemini_short_filename_prefix_is_never_sufficient_without_full_metadata_key() {
    let temp = tempfile::tempdir().unwrap();
    let d = discovery(&temp, Format::GeminiRecording6814e00);
    std::fs::write(
        temp.path().join("session-timestamp-native-f.jsonl"),
        b"{\"sessionId\":\"native-foreign\"}\n",
    )
    .unwrap();
    assert_eq!(d.resolve().unwrap(), Resolution::Missing);
    std::fs::write(
        temp.path().join("session-other-native-f.jsonl"),
        b"{\"sessionId\":\"native-fixture\"}\n",
    )
    .unwrap();
    assert!(matches!(d.resolve().unwrap(), Resolution::Found(_)));
}
