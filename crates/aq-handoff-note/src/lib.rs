//! The handoff marker the service writes beside a driver, and the handoff note
//! the driver (or a restarted node's salvage) claims for a stopped attempt.
use serde::Deserialize;
use std::path::Path;
mod preserve;
mod secret_file;
pub use preserve::{CopyBackIncomplete, PRESERVED_MARKER, preserved, preserved_claim, write_exit};
mod stream;
pub use secret_file::{parse_token, read_private};
pub use stream::{conclusion, pane_line, print_logs, proven_success};

/// Folds whitespace and bounds a claim to 500 bytes.
pub fn fold_claim(text: String) -> String {
    let mut text: String = text.split_whitespace().collect::<Vec<_>>().join(" ");
    if text.len() > 500 {
        let mut cut = 497;
        while !text.is_char_boundary(cut) {
            cut -= 1;
        }
        text.truncate(cut);
        text.push_str("...");
    }
    text
}

/// File beside `driver.json`, written by the service, asking the driver to stop and hand off.
pub const HANDOFF_MARKER: &str = "handoff";
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HandoffMarker {
    pub schema_version: u32,
    pub reason: aq_protocol::HandoffReason,
}
/// None when absent; None (and a println) when unreadable or invalid; never panics.
pub fn read_handoff_marker(path: &Path) -> Option<aq_protocol::HandoffReason> {
    let bytes = match std::fs::read(path) {
        Ok(bytes) => bytes,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return None,
        Err(e) => {
            println!("aq-sandbox-harness: handoff marker unreadable: {e}");
            return None;
        }
    };
    match serde_json::from_slice::<HandoffMarker>(&bytes) {
        Ok(m) if m.schema_version == 1 => Some(m.reason),
        _ => {
            println!("aq-sandbox-harness: handoff marker invalid; ignored");
            None
        }
    }
}

/// The text of the last stream-json `assistant` event, if it has any.
fn last_assistant_text(stream: &[u8]) -> Option<String> {
    let event = stream
        .split(|b| *b == b'\n')
        .rev()
        .filter_map(|line| serde_json::from_slice::<serde_json::Value>(line).ok())
        .find(|v| v["type"] == "assistant")?;
    let text: String = event["message"]["content"]
        .as_array()?
        .iter()
        .filter_map(|part| part["text"].as_str())
        .collect();
    (!text.trim().is_empty()).then_some(text)
}

/// The claim text for a stopped-and-handed-off attempt (at most 500 characters).
pub fn handoff_note(
    reason: aq_protocol::HandoffReason,
    changed: &[String],
    stream: &[u8],
    token: &[u8],
) -> String {
    use aq_protocol::HandoffReason::*;
    let words = match reason {
        OwnerStop => "owner stop",
        ServiceStop => "service stop",
        Restart => "node restart",
    };
    let paths = if changed.is_empty() {
        "none".to_string()
    } else {
        let more = changed.len().saturating_sub(20);
        let shown = changed[..changed.len() - more].join(", ");
        if more > 0 {
            format!("{shown} (+{more} more)")
        } else {
            shown
        }
    };
    let mut note = format!(
        "Handoff ({words}): the agent was stopped before it finished; its work so far is saved. Changed: {paths}."
    );
    if let Some(text) = last_assistant_text(stream).filter(|t| !holds_secret(t.as_bytes(), token)) {
        let text = text.split_whitespace().collect::<Vec<_>>().join(" ");
        note.push_str(&format!(
            " Last agent message: {}.",
            text.chars().take(200).collect::<String>()
        ));
    }
    fold_claim(note)
}

/// Whether `haystack` contains the token's exact bytes.
pub fn holds_token(haystack: &[u8], token: &[u8]) -> bool {
    !token.is_empty()
        && haystack.len() >= token.len()
        && haystack.windows(token.len()).any(|w| w == token)
}

/// The exact login token, or anything shaped like an AQ agent credential
/// (`aqa_…`, 0006 §8: no AQ secret exists in the sandbox, so any is a leak).
pub fn holds_secret(haystack: &[u8], token: &[u8]) -> bool {
    holds_token(haystack, token) || aq_agent_channel::holds_agent_credential(haystack)
}

#[cfg(test)]
mod tests {
    use super::*;
    use aq_protocol::HandoffReason::*;
    fn assistant(text: &str) -> Vec<u8> {
        serde_json::json!({"type":"assistant","message":{"content":[{"type":"text","text":text}]}})
            .to_string()
            .into_bytes()
    }
    #[test]
    fn handoff_marker_parses_and_ignores_garbage() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(HANDOFF_MARKER);
        assert_eq!(read_handoff_marker(&path), None);
        std::fs::write(&path, br#"{"schema_version":1,"reason":"owner_stop"}"#).unwrap();
        assert_eq!(read_handoff_marker(&path), Some(OwnerStop));
        std::fs::write(&path, br#"{"schema_version":2,"reason":"owner_stop"}"#).unwrap();
        assert_eq!(read_handoff_marker(&path), None);
        std::fs::write(&path, b"not json").unwrap();
        assert_eq!(read_handoff_marker(&path), None);
    }
    #[test]
    fn handoff_note_lists_changes_and_last_message() {
        let changed: Vec<String> = (0..25).map(|i| format!("src/f{i}.rs")).collect();
        let note = handoff_note(
            ServiceStop,
            &changed,
            &assistant("Working on the parser"),
            b"tok",
        );
        assert!(note.starts_with("Handoff (service stop): the agent was stopped before it finished; its work so far is saved. Changed: src/f0.rs"));
        assert!(note.contains("src/f19.rs") && !note.contains("src/f20.rs"));
        assert!(note.contains("(+5 more)"));
        assert!(note.contains("Last agent message: Working on the parser."));
        assert!(note.len() <= 500);
    }
    #[test]
    fn handoff_note_never_carries_a_secret() {
        let note = handoff_note(
            Restart,
            &[],
            &assistant("my token is sekrit-token-1"),
            b"sekrit-token-1",
        );
        assert!(!note.contains("Last agent message") && !note.contains("sekrit"));
        assert!(note.contains("node restart"));
    }
    #[test]
    fn handoff_note_without_changes_says_none() {
        let note = handoff_note(OwnerStop, &[], b"", b"tok");
        assert!(note.contains("Changed: none."));
        assert!(!note.contains("Last agent message"));
    }
}
