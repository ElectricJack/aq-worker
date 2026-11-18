//! Save native user and assistant messages from stopped manual sandboxes.
//! This also discovers older sessions that still have their provider logs.
use super::Agent;
use crate::redact::Redactor;
use aq_node_service::{enrollment_file::write_private, link::ServerLink};
use aq_worker::backend::{
    BackendError,
    docker_local::{
        DockerLocalBackend, DockerLocalConfig,
        tar::{TreeEntry, read_tree},
    },
};
use serde::Serialize;
use serde_json::Value;
use std::{
    path::{Path, PathBuf},
    time::Duration,
};
use tokio::sync::watch;
use uuid::Uuid;

const MAX_LOG_BYTES: usize = 16 * 1024 * 1024;
const MAX_SNAPSHOT_BYTES: usize = 1024 * 1024;
const MAX_MESSAGES: usize = 1000;

#[derive(Serialize)]
struct Message {
    role: &'static str,
    text: String,
}

#[derive(Default, Serialize)]
struct Snapshot {
    messages: Vec<Message>,
    partial: bool,
}

#[derive(Clone, Copy)]
enum Provider {
    Claude,
    Codex,
}

impl Provider {
    fn of(agent: &Agent) -> Option<Self> {
        match agent.spec["provider"].as_str()? {
            "anthropic" => Some(Self::Claude),
            "openai" => Some(Self::Codex),
            _ => None,
        }
    }

    fn log_directory(self) -> &'static str {
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

    fn message(self, row: &Value, id: Uuid) -> Option<Message> {
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
        let text = sanitize_message(&text);
        (!text.trim().is_empty()).then_some(Message { role, text })
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

fn parse(provider: Provider, id: Uuid, data: &[u8]) -> Snapshot {
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
        if let Some(message) = provider.message(&row, id) {
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

async fn read_snapshot(agent: &Agent, cfg: &DockerLocalConfig) -> anyhow::Result<Option<Snapshot>> {
    let (Some(provider), Some(sandbox)) = (Provider::of(agent), &agent.sandbox) else {
        return Ok(None);
    };
    let mut backend = DockerLocalBackend::connect_unix(cfg.clone()).await?;
    let raw = match backend
        .get_archive(sandbox, provider.log_directory(), MAX_LOG_BYTES)
        .await
    {
        Ok(raw) => zeroize::Zeroizing::new(raw),
        Err(BackendError::NotFound | BackendError::InvalidRequest(_)) => return Ok(None),
        Err(error) => return Err(error.into()),
    };
    let mut candidate = None;
    for entry in read_tree(&raw, MAX_LOG_BYTES as u64)? {
        if let TreeEntry::File { path, data, .. } = entry {
            let data = zeroize::Zeroizing::new(data);
            if let Some(id) = provider.session_id(&path) {
                let snapshot = parse(provider, id, &data);
                if !snapshot.messages.is_empty() {
                    if candidate.is_some() {
                        return Ok(None);
                    }
                    candidate = Some(snapshot);
                }
            }
        }
    }
    // There should be one root session in a fresh manual sandbox. Do not merge
    // unrelated threads or attribute an arbitrary log to this AQ session.
    Ok(candidate)
}

async fn archive(
    directory: &Path,
    agent: &Agent,
    link: &ServerLink,
    cfg: &DockerLocalConfig,
) -> anyhow::Result<()> {
    let done = directory.join("conversation.done");
    if done.exists() {
        return Ok(());
    }
    let pending = directory.join("conversation.pending.json");
    let body = match std::fs::read(&pending) {
        Ok(body) => body,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            let Some(snapshot) = read_snapshot(agent, cfg).await? else {
                return write_private(&done, b"no native conversation");
            };
            let body = serde_json::to_vec(&snapshot)?;
            write_private(&pending, &body)?;
            body
        }
        Err(error) => return Err(error.into()),
    };
    let receipt = link
        .manual_conversation(agent.id, &serde_json::from_slice(&body)?)
        .await?;
    anyhow::ensure!(
        receipt["session_id"]
            .as_str()
            .and_then(|id| Uuid::parse_str(id).ok())
            == Some(agent.id),
        "conversation acknowledgement did not match session"
    );
    write_private(&done, b"archived")?;
    std::fs::remove_file(pending)?;
    Ok(())
}

pub(super) async fn run(
    root: PathBuf,
    node: Uuid,
    link: ServerLink,
    cfg: DockerLocalConfig,
    mut shutdown: watch::Receiver<bool>,
) {
    let mut warned = std::collections::HashSet::new();
    while !*shutdown.borrow() {
        if let Ok(entries) = std::fs::read_dir(&root) {
            for entry in entries.flatten() {
                let dir = entry.path();
                let Ok(bytes) = std::fs::read(dir.join("state.json")) else {
                    continue;
                };
                let Ok(agent) = serde_json::from_slice::<Agent>(&bytes) else {
                    continue;
                };
                if agent.node_id != node
                    || agent.status != "stopped"
                    || dir.file_name().and_then(|s| s.to_str()) != Some(&agent.id.to_string())
                {
                    continue;
                }
                tokio::select! {
                    result = archive(&dir, &agent, &link, &cfg) => {
                        if result.is_err() && warned.insert(agent.id) {
                            tracing::warn!(session_id = %agent.id, "native conversation archive will be retried");
                        }
                    }
                    _ = shutdown.changed() => return,
                }
            }
        }
        tokio::select! {
            _ = tokio::time::sleep(Duration::from_secs(10)) => {},
            _ = shutdown.changed() => return,
        }
    }
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
        let result = serde_json::to_value(parse(Provider::Claude, id, &native)).unwrap();
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
        let result = serde_json::to_value(parse(Provider::Codex, id, &native)).unwrap();
        assert_eq!(
            result,
            json!({"messages":[
            {"role":"user","text":"What changed?"},
            {"role":"assistant","text":"File changed. aqa_**********!"}
        ],"partial":false})
        );
        assert!(
            parse(Provider::Codex, Uuid::new_v4(), &native)
                .messages
                .is_empty()
        );
    }
}
