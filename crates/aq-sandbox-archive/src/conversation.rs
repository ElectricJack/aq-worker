//! Project user and assistant messages from provider session JSONL.
use crate::{
    redact::Redactor,
    tar::{InvalidArchive, TreeEntry, read_tree, single_file},
};
use serde::Serialize;
use serde_json::Value;
use uuid::Uuid;

pub const MAX_LOG_BYTES: usize = 16 * 1024 * 1024;
pub const MAX_SNAPSHOT_BYTES: usize = 1024 * 1024;
const MAX_MESSAGES: usize = 1000;

#[derive(Serialize)]
struct Message {
    role: &'static str,
    text: String,
}

#[derive(Default, Serialize)]
pub struct Snapshot {
    messages: Vec<Message>,
    partial: bool,
}

#[derive(Clone, Copy)]
pub enum Provider {
    Claude,
    Codex,
}

impl Provider {
    pub fn log_directory(self) -> &'static str {
        match self {
            Self::Claude => "/aq/home/.claude/projects/-aq-work",
            Self::Codex => "/aq/home/.codex/sessions",
        }
    }

    fn session_id(self, path: &str) -> Option<Uuid> {
        match self {
            Self::Claude => Uuid::parse_str(path.strip_suffix(".jsonl")?).ok(),
            Self::Codex => {
                let file = path.rsplit('/').next()?.strip_suffix(".jsonl")?;
                if !file.starts_with("rollout-") {
                    return None;
                }
                Uuid::parse_str(file.get(file.len().checked_sub(36)?..)?).ok()
            }
        }
    }

    fn message(self, row: &Value, id: Uuid, mut safe: impl FnMut(&str) -> bool) -> Option<Message> {
        let (role, content, block_type) = match self {
            Self::Claude => {
                if row["sessionId"]
                    .as_str()
                    .and_then(|s| Uuid::parse_str(s).ok())
                    != Some(id)
                    || row["isSidechain"] == true
                    || row["isMeta"] == true
                {
                    return None;
                }
                (row["type"].as_str()?, &row["message"]["content"], "text")
            }
            Self::Codex => {
                let p = &row["payload"];
                if row["type"] != "response_item" || p["type"] != "message" {
                    return None;
                }
                let role = p["role"].as_str()?;
                (
                    role,
                    &p["content"],
                    if role == "user" {
                        "input_text"
                    } else {
                        "output_text"
                    },
                )
            }
        };
        let role = match role {
            "user" => "user",
            "assistant" => "assistant",
            _ => return None,
        };
        let visible = |text: &&str| {
            role != "user"
                || match self {
                    Self::Claude => !text.trim_start().starts_with("<local-command-"),
                    Self::Codex => {
                        !text
                            .trim_start()
                            .starts_with("# AGENTS.md instructions for ")
                            && !text.trim_start().starts_with("<environment_context>")
                    }
                }
        };
        let text = if let Some(text) = content.as_str() {
            visible(&text).then_some(text)?.to_owned()
        } else {
            content
                .as_array()?
                .iter()
                .filter(|block| block["type"] == block_type)
                .filter_map(|block| block["text"].as_str())
                .filter(visible)
                .collect::<Vec<_>>()
                .join("\n")
        };
        if !safe(&text) {
            return None;
        }
        let text = sanitize_message(&text);
        (!text.trim().is_empty() && safe(&text)).then_some(Message { role, text })
    }
}

// Provider messages are plain text. Drop terminal controls (including payloads
// within control strings) before applying the shared credential masking.
fn sanitize_message(text: &str) -> String {
    enum Escape {
        Text,
        Start,
        Csi,
        String,
        StringEnd,
    }
    let mut state = Escape::Text;
    let mut visible = String::with_capacity(text.len());
    for ch in text.replace("\r\n", "\n").chars() {
        state = match (state, ch) {
            (Escape::Text, '\x1b') => Escape::Start,
            (Escape::Text, '\u{009b}') => Escape::Csi,
            (Escape::Text, '\u{0090}' | '\u{009d}' | '\u{009e}' | '\u{009f}') => Escape::String,
            (Escape::Text, ch) => {
                if !ch.is_control() || matches!(ch, '\n' | '\t') {
                    visible.push(ch);
                } else if ch == '\r' {
                    visible.push('\n');
                }
                Escape::Text
            }
            (Escape::Start, '[') => Escape::Csi,
            (Escape::Start, ']' | 'P' | '^' | '_') => Escape::String,
            (Escape::Start, _) | (Escape::Csi, '@'..='~') => Escape::Text,
            (Escape::Csi, _) => Escape::Csi,
            (Escape::String | Escape::StringEnd, '\x07' | '\u{009c}')
            | (Escape::StringEnd, '\\') => Escape::Text,
            (Escape::String | Escape::StringEnd, '\x1b') => Escape::StringEnd,
            (Escape::String | Escape::StringEnd, _) => Escape::String,
        };
    }
    String::from_utf8(Redactor::default().frame(visible.as_bytes()))
        .expect("masking preserves UTF-8")
}

fn parse(provider: Provider, id: Uuid, data: &[u8], token: Option<&[u8]>) -> Snapshot {
    let mut out = Snapshot::default();
    let mut bytes = 64;
    let mut identified = matches!(provider, Provider::Claude);
    for line in data.split(|&b| b == b'\n').filter(|line| !line.is_empty()) {
        let Ok(row) = serde_json::from_slice::<Value>(line) else {
            out.partial = true;
            continue;
        };
        if matches!(provider, Provider::Codex) && row["type"] == "session_meta" {
            let p = &row["payload"];
            if p["id"].as_str().and_then(|s| Uuid::parse_str(s).ok()) != Some(id)
                || p["cwd"] != "/aq/work"
                || p["source"].get("subagent").is_some()
            {
                return Snapshot::default();
            }
            identified = true;
        }
        if !identified {
            continue;
        }
        if let Some(message) = provider.message(&row, id, |text| {
            let safe =
                token.is_none_or(|token| !aq_handoff_note::holds_secret(text.as_bytes(), token));
            out.partial |= !safe;
            safe
        }) {
            bytes += serde_json::to_vec(&message)
                .expect("message is serializable")
                .len()
                + 1;
            if bytes > MAX_SNAPSHOT_BYTES || out.messages.len() == MAX_MESSAGES {
                out.partial = true;
                break;
            }
            out.messages.push(message);
        }
    }
    out
}

/// Select exactly one visible root conversation from a bounded Docker archive.
pub fn from_archive(
    raw: &[u8],
    provider: Provider,
    expected_session: Option<Uuid>,
    token: Option<&[u8]>,
) -> Result<Option<Snapshot>, InvalidArchive> {
    if let (Provider::Claude, Some(id)) = (provider, expected_session) {
        let data = zeroize::Zeroizing::new(single_file(raw, MAX_LOG_BYTES as u64)?);
        let snapshot = parse(provider, id, &data, token);
        return Ok((!snapshot.messages.is_empty()).then_some(snapshot));
    }
    let mut candidate = None;
    for entry in read_tree(raw, MAX_LOG_BYTES as u64)? {
        if let TreeEntry::File { path, data, .. } = entry {
            let data = zeroize::Zeroizing::new(data);
            if let Some(id) = provider.session_id(&path)
                && expected_session.is_none_or(|expected| id == expected)
            {
                let snapshot = parse(provider, id, &data, token);
                if !snapshot.messages.is_empty() {
                    if candidate.is_some() {
                        return Ok(None);
                    }
                    candidate = Some(snapshot);
                }
            }
        }
    }
    Ok(candidate)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn lines(records: &[Value]) -> Vec<u8> {
        records
            .iter()
            .map(Value::to_string)
            .collect::<Vec<_>>()
            .join("\n")
            .into_bytes()
    }

    #[test]
    fn claude_directory_and_exact_file_use_the_same_message_parser() {
        let id = Uuid::new_v4();
        let archive = crate::tar::tree_archive(
            "project",
            &[TreeEntry::File {
                path: format!("{id}.jsonl"),
                mode: 0o600,
                data: lines(&[
                    json!({"type":"user","sessionId":id,"message":{"content":"Queued prompt"}}),
                    json!({"type":"assistant","sessionId":id,"message":{"content":[{"type":"text","text":"Queued response"}]}}),
                ]),
            }],
        )
        .unwrap();
        let snapshot = |expected: Option<Uuid>| {
            // Docker's file response starts with the file header. Its directory
            // response includes an initial 512-byte directory header.
            let raw = if expected.is_some() {
                &archive[512..]
            } else {
                &archive
            };
            serde_json::to_value(from_archive(raw, Provider::Claude, expected, None).unwrap())
                .unwrap()
        };
        assert_eq!(
            snapshot(Some(id)),
            json!({"messages":[{"role":"user","text":"Queued prompt"},{"role":"assistant","text":"Queued response"}],"partial":false}),
        );
        assert_eq!(snapshot(None), snapshot(Some(id)));
        assert_eq!(snapshot(Some(Uuid::new_v4())), Value::Null);
    }

    #[test]
    fn queued_claude_filters_the_exact_leased_token_from_decoded_messages() {
        let id = Uuid::new_v4();
        let native = format!(
            "{{\"type\":\"assistant\",\"sessionId\":\"{id}\",\"message\":{{\"content\":\"protect-me-\\u0031-2-3\"}}}}\n\
             {{\"type\":\"assistant\",\"sessionId\":\"{id}\",\"message\":{{\"content\":\"Keep this message.\"}}}}"
        );
        let snapshot = parse(
            Provider::Claude,
            id,
            native.as_bytes(),
            Some(b"protect-me-1-2-3"),
        );
        assert!(snapshot.partial);
        assert_eq!(snapshot.messages.len(), 1);
        assert_eq!(snapshot.messages[0].text, "Keep this message.");
    }

    #[test]
    fn claude_extracts_root_messages_and_skips_hidden_blocks() {
        let id = Uuid::new_v4();
        let native = lines(&[
            json!({"type":"user","sessionId":id,"message":{"content":"Explain this\nplease."}}),
            json!({"type":"assistant","sessionId":id,"message":{"content":[
                {"type":"thinking","thinking":"private reasoning"},
                {"type":"tool_use","name":"Read"},
                {"type":"text","text":"Use the existing adapter."},
            ]}}),
            json!({"type":"user","sessionId":id,"message":{"content":[{"type":"tool_result","content":"private output"}]}}),
            json!({"type":"assistant","sessionId":id,"isSidechain":true,"message":{"content":"sidechain"}}),
            json!({"type":"assistant","sessionId":Uuid::new_v4(),"message":{"content":"foreign"}}),
        ]);
        let result = serde_json::to_value(parse(Provider::Claude, id, &native, None)).unwrap();
        assert_eq!(
            result,
            json!({"messages":[
            {"role":"user","text":"Explain this\nplease."},
            {"role":"assistant","text":"Use the existing adapter."},
        ],"partial":false})
        );
    }

    #[test]
    fn codex_skips_injected_context_and_masks_credentials() {
        let id = Uuid::new_v4();
        let meta =
            json!({"type":"session_meta","payload":{"id":id,"cwd":"/aq/work","source":"cli"}});
        let message = |role, content| json!({"type":"response_item","payload":{"type":"message","role":role,"content":content}});
        let native = lines(&[
            meta,
            message(
                "developer",
                json!([{"type":"input_text","text":"private system text"}]),
            ),
            message(
                "user",
                json!([
                    {"type":"input_text","text":"# AGENTS.md instructions for /aq/work"},
                    {"type":"input_text","text":"<environment_context>private context</environment_context>"},
                ]),
            ),
            message(
                "user",
                json!([{"type":"input_text","text":"What changed?"}]),
            ),
            message(
                "assistant",
                json!([{"type":"output_text","text":"File changed. aqa_fake\x1b[31msecret!"}]),
            ),
            json!({"type":"response_item","payload":{"type":"function_call_output","output":"raw result"}}),
        ]);
        let result = serde_json::to_value(parse(Provider::Codex, id, &native, None)).unwrap();
        assert_eq!(
            result,
            json!({"messages":[
            {"role":"user","text":"What changed?"},
            {"role":"assistant","text":"File changed. aqa_**********!"}
        ],"partial":false})
        );
        assert!(
            parse(Provider::Codex, Uuid::new_v4(), &native, None)
                .messages
                .is_empty()
        );
    }
}
