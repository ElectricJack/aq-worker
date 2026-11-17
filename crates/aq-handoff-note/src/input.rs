//! Owner input into a running Claude attempt (interactive terminals, design in
//! aq-server `docs/` and `INTERACTIVE-TERMINALS.md` of the 2026-11-17 rollout).
//!
//! Claude Code in print mode with `--input-format stream-json` keeps its
//! session open while its stdin is open: every user message it reads is
//! answered with its own `result` event, and it exits 0 at end of input
//! (checked with Claude Code 2.1.326). So a queued attempt can take messages
//! typed by the owner without becoming a TUI: the startup text is message 1,
//! each line the owner types is a further message, and input is closed once
//! every message sent has its result and nothing more is queued. An attempt
//! nobody types into therefore ends after its first result, as it does today.
//!
//! This module is the pure part: the message framing, the bounds and the
//! rule for when input closes. Nothing here is wired into the driver yet.
use std::collections::VecDeque;

/// The in-sandbox runner for an attempt that takes owner messages. It is the
/// fixed runner (`aq-worker` `sandbox_driver::RUNNER`) with one difference:
/// Claude's stdin is fed from `/aq/in/messages` instead of one prompt file.
/// Message `N` is delivered when `N.ok` exists beside `N.json` (the driver
/// writes the `.ok` last, so a half-written message is never read). The feed
/// ends when `close` exists or Claude has exited. Public, no secret.
pub const INTERACTIVE_RUNNER: &str = r#"set -uo pipefail
umask 077
in=/aq/in; out=/aq/out; m=/aq/private/material
mkdir -p "$out"
[ -s "$m" ] || { echo "aq-runner: material missing"; exit 90; }
token="$(cat "$m")"; rm -f "$m"
[ -e "$m" ] && { echo "aq-runner: material not wiped"; exit 91; }
while IFS= read -r kv; do [ -n "$kv" ] && export "$kv"; done < "$in/env"
mkdir -p "$HOME" && cp "$in/settings.json" "$HOME/settings.json" || exit 92
set --; while IFS= read -r a; do set -- "$@" "$a"; done < "$in/args"
exe="$(cat "$in/executable")"
cd /aq/work || exit 93
echo "aq-runner: claude $("$exe" --version 2>/dev/null) uid=$(id -u) capeff=$(awk '/^CapEff:/ {print $2}' /proc/self/status) nnp=$(awk '/^NoNewPrivs:/ {print $2}' /proc/self/status)"
feed() { n=1; while [ ! -e "$out/rc" ]; do if [ -e "$in/messages/$n.ok" ]; then cat "$in/messages/$n.json"; printf '\n'; n=$((n+1)); elif [ -e "$in/messages/close" ]; then return 0; else sleep 0.5; fi; done; }
feed | { CLAUDE_CODE_OAUTH_TOKEN="$token" IS_SANDBOX=1 "$exe" "$@" 2> "$out/claude.err"; echo "$?" > "$out/rc"; } | tee "$out/claude.jsonl"
unset token
rc="$(cat "$out/rc" 2>/dev/null || echo 94)"
login_file=false; [ -e "$HOME/.credentials.json" ] && login_file=true
echo "aq-runner: claude exit $rc login_file=$login_file"
exit "$rc"
"#;

/// Said to the agent in front of what the owner typed, so a steer mid-task
/// does not lose the reporting contract its result is judged by.
pub const OWNER_PREFIX: &str = "Message from the owner while you work on this task. Take it into \
account and carry on. Your final message must still end with the Changes line you were asked for.\n\n";

/// One stream-json user message line (no trailing newline).
pub fn user_message(text: &str) -> String {
    serde_json::json!({
        "type": "user",
        "message": {"role": "user", "content": [{"type": "text", "text": text}]},
    })
    .to_string()
}

/// Why a typed line was not queued.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Refused {
    /// Input closed: the agent has finished.
    Closed,
    Empty,
    /// Longer than the terminal policy's `max_input_bytes`.
    TooLong,
    /// More than `max_pending_input_bytes` are waiting to be delivered.
    Backlog,
}
impl Refused {
    pub fn reason(self) -> &'static str {
        match self {
            Self::Closed => "the agent has finished; it takes no more input",
            Self::Empty => "an empty line is not sent",
            Self::TooLong => "the line is longer than this terminal accepts",
            Self::Backlog => "earlier lines are still waiting for the agent",
        }
    }
}

/// The paths (under `/aq/in`) and bytes of one message, in the order they
/// must be written: the `.ok` marker last.
pub fn message_files(number: u32, line: &str) -> [(String, Vec<u8>); 2] {
    [
        (format!("messages/{number}.json"), line.as_bytes().to_vec()),
        (format!("messages/{number}.ok"), Vec::new()),
    ]
}
/// The marker that ends the feed.
pub const CLOSE_FILE: &str = "messages/close";

/// What has been sent to the agent, what it has answered, and what waits.
/// The bounds come from the launch manifest's terminal policy.
#[derive(Debug)]
pub struct Feed {
    sent: u32,
    results: u32,
    pending: VecDeque<String>,
    pending_bytes: usize,
    closed: bool,
    partial: Vec<u8>,
    max_input_bytes: usize,
    max_pending_bytes: usize,
}
impl Feed {
    /// The startup text is message 1 and counts as sent.
    pub fn new(max_input_bytes: usize, max_pending_bytes: usize) -> Self {
        Self {
            sent: 1,
            results: 0,
            pending: VecDeque::new(),
            pending_bytes: 0,
            closed: false,
            partial: Vec::new(),
            max_input_bytes,
            max_pending_bytes,
        }
    }
    /// A line the owner typed. Queued as it is; control characters other than
    /// tab are dropped, since a terminal sends them for editing keys.
    pub fn typed(&mut self, line: &str) -> Result<(), Refused> {
        if self.closed {
            return Err(Refused::Closed);
        }
        let text: String = line
            .chars()
            .filter(|c| *c == '\t' || !c.is_control())
            .collect();
        let text = text.trim();
        if text.is_empty() {
            return Err(Refused::Empty);
        }
        if text.len() > self.max_input_bytes {
            return Err(Refused::TooLong);
        }
        if self.pending_bytes + text.len() > self.max_pending_bytes {
            return Err(Refused::Backlog);
        }
        self.pending_bytes += text.len();
        self.pending.push_back(text.to_owned());
        Ok(())
    }
    /// Runner output as it arrives: counts complete `result` events.
    pub fn observe(&mut self, bytes: &[u8]) {
        self.partial.extend_from_slice(bytes);
        while let Some(end) = self.partial.iter().position(|b| *b == b'\n') {
            let line: Vec<u8> = self.partial.drain(..=end).collect();
            let event = serde_json::from_slice::<serde_json::Value>(&line[..line.len() - 1]);
            if event.is_ok_and(|v| v["type"] == "result") {
                self.results += 1;
            }
        }
    }
    /// The next message to deliver: its number, the owner's text (for the
    /// pane) and the stream-json line (for the agent).
    pub fn next(&mut self) -> Option<(u32, String, String)> {
        if self.closed {
            return None;
        }
        let text = self.pending.pop_front()?;
        self.pending_bytes -= text.len();
        self.sent += 1;
        let line = user_message(&format!("{OWNER_PREFIX}{text}"));
        Some((self.sent, text, line))
    }
    /// Every message sent has its result and nothing waits: close the input.
    /// Returns true once; afterwards [`Feed::typed`] refuses.
    pub fn finish(&mut self) -> bool {
        if self.closed || !self.pending.is_empty() || self.results < self.sent {
            return false;
        }
        self.closed = true;
        true
    }
    pub fn closed(&self) -> bool {
        self.closed
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn result() -> Vec<u8> {
        b"{\"type\":\"result\",\"subtype\":\"success\"}\n".to_vec()
    }

    #[test]
    fn user_message_is_one_json_line_with_the_text() {
        let line = user_message("say \"hi\"\nthen stop");
        assert!(!line.contains('\n'));
        let v: serde_json::Value = serde_json::from_str(&line).unwrap();
        assert_eq!(v["type"], "user");
        assert_eq!(v["message"]["content"][0]["text"], "say \"hi\"\nthen stop");
    }

    #[test]
    fn an_unattended_attempt_closes_after_its_first_result() {
        let mut feed = Feed::new(4096, 32768);
        assert!(!feed.finish(), "no result yet");
        feed.observe(b"{\"type\":\"assistant\"}\n");
        assert!(!feed.finish());
        feed.observe(&result());
        assert!(feed.finish());
        assert!(!feed.finish(), "closes once");
        assert_eq!(feed.typed("too late"), Err(Refused::Closed));
        assert!(feed.next().is_none());
    }

    #[test]
    fn a_typed_line_keeps_input_open_until_its_own_result() {
        let mut feed = Feed::new(4096, 32768);
        feed.typed("  also update the README \u{1b}[A ").unwrap();
        assert!(!feed.finish(), "a line is waiting");
        let (number, text, line) = feed.next().unwrap();
        assert_eq!((number, text.as_str()), (2, "also update the README [A"));
        let v: serde_json::Value = serde_json::from_str(&line).unwrap();
        let sent = v["message"]["content"][0]["text"].as_str().unwrap();
        assert!(sent.starts_with(OWNER_PREFIX) && sent.ends_with("also update the README [A"));
        feed.observe(&result());
        assert!(!feed.finish(), "message 2 has no result yet");
        feed.observe(&result());
        assert!(feed.finish());
    }

    #[test]
    fn a_result_split_across_reads_is_counted_once() {
        let mut feed = Feed::new(4096, 32768);
        let bytes = result();
        feed.observe(&bytes[..10]);
        assert!(!feed.finish());
        feed.observe(&bytes[10..]);
        feed.observe(b"not json\n{\"type\":\"result\"");
        assert!(feed.finish(), "one complete result; the partial one waits");
    }

    #[test]
    fn bounds_and_empty_lines_are_refused_by_name() {
        let mut feed = Feed::new(8, 12);
        assert_eq!(feed.typed(" \u{7}\r"), Err(Refused::Empty));
        assert_eq!(feed.typed("123456789"), Err(Refused::TooLong));
        feed.typed("12345678").unwrap();
        assert_eq!(feed.typed("12345"), Err(Refused::Backlog));
        feed.typed("1234").unwrap();
        let _ = feed.next().unwrap();
        feed.typed("12345").unwrap();
        for refused in [
            Refused::Closed,
            Refused::Empty,
            Refused::TooLong,
            Refused::Backlog,
        ] {
            assert!(!refused.reason().is_empty());
        }
    }

    #[test]
    fn a_message_is_written_before_its_marker() {
        let [(json, body), (ok, empty)] = message_files(3, "{}");
        assert_eq!(
            (json.as_str(), body.as_slice()),
            ("messages/3.json", &b"{}"[..])
        );
        assert_eq!((ok.as_str(), empty.len()), ("messages/3.ok", 0));
    }

    /// The interactive runner differs from the fixed runner only in how
    /// Claude's stdin is fed and how its exit code is read; the token handling
    /// lines are the same and in the same order.
    #[test]
    fn the_interactive_runner_keeps_the_token_handling_of_the_fixed_runner() {
        let read = INTERACTIVE_RUNNER.find("token=\"$(cat \"$m\")\"").unwrap();
        let wipe = INTERACTIVE_RUNNER.find("rm -f \"$m\"").unwrap();
        let exec = INTERACTIVE_RUNNER
            .find("CLAUDE_CODE_OAUTH_TOKEN=\"$token\"")
            .unwrap();
        assert!(read < wipe && wipe < exec);
        assert!(!INTERACTIVE_RUNNER.contains("export CLAUDE_CODE_OAUTH_TOKEN"));
        assert!(!INTERACTIVE_RUNNER.contains("--dangerously-skip-permissions"));
        assert!(!INTERACTIVE_RUNNER.contains("echo \"$token"));
        assert!(INTERACTIVE_RUNNER.contains("messages/$n.ok"));
        assert!(INTERACTIVE_RUNNER.contains("messages/close"));
    }
}
