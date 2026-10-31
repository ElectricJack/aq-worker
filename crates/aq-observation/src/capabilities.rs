//! Format capability advertisement is separate from selected CLI qualification.
use crate::{Format, Source};
use serde_json::{Value, json};
pub fn format_inventory() -> Value {
    serde_json::from_str(include_str!("../artifacts/source-formats.json"))
        .expect("checked-in pinned format evidence")
}
pub fn capabilities(source: &Source) -> Value {
    let (format, harness, basis) = match source.format {
        Format::CodexRollout67f0994 => {
            ("codex-rollout-jsonl-67f0994", "codex", "native_cumulative")
        }
        Format::ClaudeV1Jsonl9a9b8e8 => (
            "claude-jsonl-v1-reference-9a9b8e8",
            "claude",
            "native_message_call",
        ),
        Format::GeminiRecording6814e00 => {
            ("gemini-recording-jsonl-6814e00", "gemini", "native_message")
        }
        Format::OpenCodeSessionDto6c24ec9 => (
            "opencode-public-session-dto-6c24ec9",
            "opencode",
            "scoped_public_message",
        ),
    };
    let inventory = format_inventory();
    let row = inventory["source_formats"]
        .as_array()
        .unwrap()
        .iter()
        .find(|r| r["format_id"] == format)
        .expect("known format evidence");
    json!({"schema_version":1,"harness":harness,"format_id":format,"parser":source.parser(),"format_source_revision":row["source_revision"],"native_usage_when_present":row["native_usage"],"counter_basis":basis,"absent_fields":"unknown_never_zero","selected_cli_version":null,"selected_released_cli_qualified":false,"complete_accounting_policy_eligible":false,"cross_source_accounting":"central_ledger_precedence_required","cost":"unknown_without_qualified_receipt_or_rate_card","raw_content_export":false,"scoped_file_discovery":source.format!=Format::OpenCodeSessionDto6c24ec9,"scoped_public_dto_channel":source.format==Format::OpenCodeSessionDto6c24ec9})
}
pub fn source_schema() -> Value {
    let mut schema = serde_json::to_value(schemars::schema_for!(Source)).expect("source schema");
    schema["x-input-role"] = json!(
        "host-admitted observation source assertion; not authentication or execution authority"
    );
    schema["properties"]["generation"]["minimum"] = 1.into();
    schema["$defs"]["Text"]["maxLength"] = 512.into();
    schema["$defs"]["Text"]["x-utf8-maxBytes"] = 512.into();
    schema
}

pub fn reparse_schema() -> Value {
    let mut schema =
        serde_json::to_value(schemars::schema_for!(crate::ReparsePlan)).expect("reparse schema");
    schema["x-host-authority-required"] = true.into();
    schema["x-max-retained-raw-bytes"] = crate::MAX_REPLAY_BYTES.into();
    schema["properties"]["known"]["maxProperties"] = 128.into();
    schema["properties"]["known"]["minProperties"] = 1.into();
    schema
}
