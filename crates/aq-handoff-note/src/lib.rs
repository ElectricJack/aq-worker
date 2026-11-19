//! The handoff marker the service writes beside a driver, and the handoff note
//! the driver (or a restarted node's salvage) claims for a stopped attempt.
use serde::Deserialize;
use std::path::Path;
mod preserve;
mod secret_file;
pub use preserve::{
    CopyBackIncomplete, PRESERVED_MARKER, preserved, preserved_claim, preserved_claim_bytes,
    preserved_conclusion, write_exit,
};
pub mod claim;
pub mod final_message;
pub mod harness;
mod stream;
pub use secret_file::{parse_token, read_private};
pub use stream::{conclusion, conclusion_of, final_text, pane_line, print_logs, proven_success};

/// Review R2-M3, R3-M2: a character the result text keeps. Dropped: control
/// characters other than tab; every Unicode format (Cf) character (bidi embeddings,
/// overrides and isolates, zero-width and joiner characters, marks, the soft hyphen,
/// the BOM, interlinear annotation and prefixed-format characters); the tag block
/// U+E0000-U+E007F; variation selectors; Hangul fillers; U+2028 and U+2029.
pub fn visible(c: char) -> bool {
    (!c.is_control() || c == '\t')
        && !matches!(c,
            '\u{00AD}' | '\u{0600}'..='\u{0605}' | '\u{061C}' | '\u{06DD}' | '\u{070F}'
            | '\u{0890}'..='\u{0891}' | '\u{08E2}' | '\u{115F}' | '\u{1160}' | '\u{180B}'..='\u{180F}'
            | '\u{200B}'..='\u{200F}' | '\u{2028}'..='\u{202E}' | '\u{2060}'..='\u{206F}'
            | '\u{3164}' | '\u{FE00}'..='\u{FE0F}' | '\u{FEFF}' | '\u{FFA0}' | '\u{FFF9}'..='\u{FFFB}'
            | '\u{110BD}' | '\u{110CD}' | '\u{13430}'..='\u{1343F}' | '\u{1BCA0}'..='\u{1BCA3}'
            | '\u{1D173}'..='\u{1D17A}' | '\u{E0000}'..='\u{E007F}' | '\u{E0100}'..='\u{E01EF}')
}

/// Unix milliseconds now (0 before the epoch).
pub fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

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

/// A salvaged handoff's note, with the interrupted host's recorded stop reason, if any.
pub fn salvage_note(
    reason: aq_protocol::HandoffReason,
    changed: &[String],
    stopped: Option<&Option<String>>,
) -> String {
    let note = handoff_note(reason, changed, &[], &[]);
    match stopped.and_then(Option::as_deref) {
        Some(proof) => fold_claim(format!("{note} Host stop: {proof}.")),
        None => note,
    }
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

/// What the driver refuses to copy back, record or print: the exact login token
/// anywhere, and any AQ agent credential shape that the base workspace did not
/// already hold. A credential-shaped string that was in the repository before the
/// agent ran (a test fixture in AQ's own source) is not a leak; the same shape with
/// any other value is, and so is the login token wherever it appears.
pub struct Secrets<'a> {
    tokens: Vec<&'a [u8]>,
    base: std::collections::BTreeSet<Vec<u8>>,
}
impl<'a> Secrets<'a> {
    /// `base`: every path, file content and link target of the workspace as it was
    /// given to the sandbox.
    pub fn new(token: &'a [u8], base: impl IntoIterator<Item = &'a [u8]>) -> Self {
        Self::of_tokens(vec![token], base)
    }
    /// `new` for a login made of several secrets (a Codex ChatGPT login: its id,
    /// access and refresh tokens). Each one is refused wherever it appears.
    pub fn of_tokens(tokens: Vec<&'a [u8]>, base: impl IntoIterator<Item = &'a [u8]>) -> Self {
        let shapes = |bytes| aq_agent_channel::agent_credentials(bytes).map(<[u8]>::to_vec);
        Self {
            tokens,
            base: base.into_iter().flat_map(shapes).collect(),
        }
    }
    pub fn holds(&self, haystack: &[u8]) -> bool {
        self.tokens.iter().any(|token| holds_token(haystack, token))
            || aq_agent_channel::agent_credentials(haystack).any(|c| !self.base.contains(c))
    }
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
    /// The aq-worker repository's own tree holds a credential-shaped fixture
    /// (`manual_stream/redact.rs`), and every attempt on it was refused.
    #[test]
    fn a_credential_shape_already_in_the_base_is_not_a_leak_and_a_new_one_is() {
        let token = b"fixture-login-0123456789abcdef";
        let fixture = b"let sample = b\"x aqa_abcdefghijklmnopqrstuvwxyz0123456789.\";";
        let other = format!("aqa_{}", "Q".repeat(43));
        let base: [&[u8]; 2] = [b"src/redact.rs", fixture];
        let secrets = Secrets::new(token, base);
        // Unchanged, moved into another file, or read into the transcript.
        assert!(!secrets.holds(fixture));
        assert!(!secrets.holds(b"notes: aqa_abcdefghijklmnopqrstuvwxyz0123456789 again"));
        // Any other value of the shape, also a longer or shorter one.
        assert!(secrets.holds(other.as_bytes()));
        assert!(secrets.holds(b"aqa_abcdefghijklmnopqrstuvwxyz0123456789A"));
        assert!(secrets.holds(b"aqa_abcdefghijklmnopqrstuvwxyz012345678"));
        // The login token is refused wherever it is, base or not.
        assert!(secrets.holds(b"export T=fixture-login-0123456789abcdef"));
        let with_token: [&[u8]; 1] = [b"T=fixture-login-0123456789abcdef"];
        assert!(Secrets::new(token, with_token).holds(with_token[0]));
        // Without a base, the same fixture is refused (it was newly written).
        assert!(Secrets::new(token, None).holds(fixture));
        assert!(!Secrets::new(token, None).holds(b"if t.starts_with(\"aqa_\") {}"));
    }
    /// A Codex login is several secrets; each one is refused on its own.
    #[test]
    fn every_token_of_a_multi_token_login_is_refused() {
        let (id, access, refresh) = (
            b"fixture-id-0123456789".as_slice(),
            b"fixture-access-0123456789".as_slice(),
            b"fixture-refresh-0123456789".as_slice(),
        );
        let secrets = Secrets::of_tokens(vec![id, access, refresh], None);
        for token in [id, access, refresh] {
            assert!(secrets.holds(&[b"x=", token].concat()));
        }
        assert!(!secrets.holds(b"fixture-refresh-012345678"));
        assert!(!secrets.holds(b"nothing secret here"));
    }
    #[test]
    fn handoff_note_without_changes_says_none() {
        let note = handoff_note(OwnerStop, &[], b"", b"tok");
        assert!(note.contains("Changed: none."));
        assert!(!note.contains("Last agent message"));
    }
}
