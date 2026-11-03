use aq_observation::lifecycle::Text;
use aq_observation::*;
use serde_json::json;
use std::{fs, path::PathBuf};
use uuid::Uuid;
fn source(format: Format) -> Source {
    Source{binding:serde_json::from_value(json!({"project_id":Uuid::from_u128(1),"task_id":Uuid::from_u128(2),"lineage_id":Uuid::from_u128(3),"attempt_id":Uuid::from_u128(4),"attempt_generation":1,"session":{"kind":"remote","node_id":Uuid::from_u128(5),"boot_id":Uuid::from_u128(6),"session_id":Uuid::from_u128(7),"attempt_id":Uuid::from_u128(4),"assignment_generation":1}})).unwrap(),source_id:Uuid::from_u128(8),generation:1,native_session_key:Text::new("native-fixture").unwrap(),format,provider:Text::new("fixture-provider").unwrap(),account_reference:Text::new("fixture-account").unwrap(), registered_session_key: None }
}
pub fn documents() -> Vec<(&'static str, serde_json::Value)> {
    let formats = [
        Format::CodexRollout67f0994,
        Format::ClaudeV1Jsonl9a9b8e8,
        Format::GeminiRecording6814e00,
        Format::OpenCodeSessionDto6c24ec9,
    ];
    let fixtures: serde_json::Value =
        serde_json::from_str(include_str!("../tests/fixtures/native-synthetic.json")).unwrap();
    let mut normalized = vec![];
    for (harness, format) in ["codex", "claude", "gemini", "opencode"]
        .into_iter()
        .zip(formats.clone())
    {
        let configured = source(format.clone());
        let mut parser = Normalizer::new(configured.clone(), 128).unwrap();
        if format == Format::OpenCodeSessionDto6c24ec9 {
            parser = parser.bind_scoped_record_channel("native-fixture").unwrap();
        }
        let mut events = vec![];
        let mut offset = 0;
        for record in fixtures[harness].as_array().unwrap() {
            let mut bytes = serde_json::to_vec(record).unwrap();
            bytes.push(b'\n');
            let frame = aq_observation::framing::Frame {
                start: offset,
                bytes,
                oversized: false,
            };
            offset = frame.end().unwrap();
            let (next, facts) = parser.apply(&frame, 100).unwrap();
            parser = next;
            events.extend(facts);
        }
        normalized.push(json!({"harness":harness,"source":configured,"events":events,"mutations":parser.mutations}));
    }
    let caps = formats
        .into_iter()
        .map(|f| capabilities(&source(f)))
        .collect::<Vec<_>>();
    vec![
        ("source.schema.json", source_schema()),
        ("reparse.schema.json", reparse_schema()),
        (
            "capabilities.json",
            json!({"fixture_provenance":"synthetic; no actual selected CLI qualification","parsers":caps}),
        ),
        (
            "normalized-golden.json",
            json!({"fixture_provenance":"synthetic; no released CLI qualification or provider/billing evidence","cases":normalized}),
        ),
    ]
}
fn main() {
    let check = std::env::args().any(|v| v == "--check");
    let base = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    for (name, value) in documents() {
        let content = format!("{}\n", serde_json::to_string_pretty(&value).unwrap());
        let path = base.join("artifacts").join(name);
        if check {
            assert_eq!(
                fs::read_to_string(path).unwrap(),
                content,
                "generated artifact drift"
            );
        } else {
            fs::write(path, content).unwrap();
        }
    }
    let root = base.join("../..");
    if root
        .join("docs/implementation/WRK-15-source-formats.json")
        .exists()
    {
        let content = fs::read_to_string(base.join("artifacts/source-formats.json")).unwrap();
        let path = root.join("docs/implementation/WRK-15-source-formats.json");
        if check {
            assert_eq!(
                fs::read_to_string(path).unwrap(),
                content,
                "format inventory mirror drift"
            );
        } else {
            fs::write(path, content).unwrap();
        }
    }
}
