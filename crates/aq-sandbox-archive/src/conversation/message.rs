//! Select and sanitize the visible text of a provider message.
use super::{Message, Provider};
use crate::redact::Redactor;
use serde_json::Value;
use uuid::Uuid;

impl Provider {
    fn content(self, row: &Value, id: Uuid) -> Option<(&str, &Value, &'static str)> {
        Some(match self {
            Self::Claude => {
                let session = row["sessionId"]
                    .as_str()
                    .and_then(|s| Uuid::parse_str(s).ok());
                let hidden = row["isSidechain"] == true || row["isMeta"] == true;
                if session != Some(id) || hidden {
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
        })
    }

    pub(super) fn message(
        self,
        row: &Value,
        id: Uuid,
        mut safe: impl FnMut(&str) -> bool,
    ) -> Option<Message> {
        let (role, content, block_type) = self.content(row, id)?;
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
