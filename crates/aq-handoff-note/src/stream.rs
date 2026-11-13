//! Reading Claude's stream-json transcript: the proven-success rule, the
//! bounded conclusion line and the pane rendering of one event.
use crate::claim::{blocked, claimed_changes, field, names_paths};
use crate::holds_secret;
use std::io::Write;

/// The last stream-json `result` event Claude printed, if any.
fn last_result(stream: &[u8]) -> Option<serde_json::Value> {
    stream
        .split(|b| *b == b'\n')
        .rev()
        .filter_map(|line| serde_json::from_slice::<serde_json::Value>(line).ok())
        .find(|v| v["type"] == "result")
}

/// The text of the last stream-json `result` event, if any.
pub fn final_text(stream: &[u8]) -> Option<String> {
    last_result(stream).and_then(|r| r["result"].as_str().map(str::to_owned))
}
/// MS1 review B1: a Claude exit 0 alone never proves the work was done
/// (Claude can end its turn with a question, a refusal or "couldn't find X").
/// The driver records success only when all of these hold; otherwise the
/// executor reports Failed and D3 sends the task to rework (a charged failure).
///
/// - the runner exited 0 (backend-observed);
/// - the last stream-json `result` event has `subtype == "success"` and
///   `is_error == false` (auth failures, max-turns, budget and errors set
///   `is_error`);
/// - the agent's final text contains its own `Changes:` line;
/// - it does not say `Outcome: blocked`;
/// - when its `Changes:` value names a path, changes were copied back (review
///   I-2: a consistency check between the claim and the evidence; a value that
///   names no path is no claim, R1-I1).
///
/// An empty change set is not a failure (owner decision 2026-11-13): a task
/// may be read-only, and its result is the agent's final message.
pub fn proven_success(
    stream: &[u8],
    rc: Option<i32>,
    changed: &[String],
) -> std::result::Result<(), &'static str> {
    if rc != Some(0) {
        return Err("runner exit not 0");
    }
    let result = last_result(stream).ok_or("no final result event")?;
    if result["subtype"] != "success" || result["is_error"] != false {
        return Err("final result is not a success");
    }
    let text = result["result"].as_str().ok_or("no final message")?;
    let claim = claimed_changes(text).ok_or("final message has no Changes: line")?;
    if blocked(text) {
        return Err("the agent reported Outcome: blocked");
    }
    if names_paths(claim) && changed.is_empty() {
        return Err("the agent reported changes, but none were copied back");
    }
    Ok(())
}

/// Bound of the conclusion line (the result report's label holds 512 bytes).
const CONCLUSION_BYTES: usize = 500;
const CLIPPED: &str = " [clipped]";
const CLIPPED_TO_COMMENT: &str = " [clipped; the full message is posted to the task comments]";

/// One-line, bounded conclusion (review I-1, R1-M2): the agent's `Outcome`
/// (first, so clipping never removes it), its own `Changes:` claim, the
/// copied-back files, the exit code, then its final message (without the Changes
/// and Outcome lines), folded and clipped with an explicit marker. `in_comment`:
/// the whole final message is posted as a task comment after the claim.
pub fn conclusion(stream: &[u8], rc: Option<i32>, changed: &[String], in_comment: bool) -> String {
    let text = final_text(stream).unwrap_or_else(|| "no final message".into());
    // R2-M3: no control, bidi or invisible format characters reach the summary.
    let text: String = text
        .chars()
        .filter(|c| crate::visible(*c) || *c == '\n')
        .collect();
    let outcome = if blocked(&text) { "blocked" } else { "done" };
    let claim = claimed_changes(&text)
        .filter(|c| !c.is_empty())
        .unwrap_or("unknown");
    let keep = |l: &&str| field(l, "changes").is_none() && field(l, "outcome").is_none();
    let message: Vec<&str> = text.lines().filter(keep).collect();
    let rest = text
        .lines()
        .rev()
        .find(|l| field(l, "changes").is_some())
        .and_then(|l| l.split_once(';').map(|(_, rest)| rest.trim()))
        .unwrap_or_default();
    let message = format!("{} {rest}", message.join(" "));
    let message = message.split_whitespace().collect::<Vec<_>>().join(" ");
    let copied = match changed.join(",") {
        files if files.is_empty() => "none".to_string(),
        files if files.len() > 120 => format!("{} files", changed.len()),
        files => files,
    };
    let rc = rc.map_or("unknown".to_string(), |c| c.to_string());
    let message = if message.is_empty() {
        "none".to_string()
    } else {
        message
    };
    let line = format!(
        "Outcome: {outcome}; Changes: {claim}; Copied: {copied}; Claude exit {rc}; Result: {message}"
    );
    let mut line = line.split_whitespace().collect::<Vec<_>>().join(" ");
    if line.len() > CONCLUSION_BYTES {
        let marker = if in_comment {
            CLIPPED_TO_COMMENT
        } else {
            CLIPPED
        };
        let mut cut = CONCLUSION_BYTES - marker.len();
        while !line.is_char_boundary(cut) {
            cut -= 1;
        }
        line.truncate(cut);
        line.push_str(marker);
    }
    line
}

/// Human-readable pane line for one stream-json event (never tool inputs).
pub fn pane_line(line: &[u8]) -> Option<String> {
    let v: serde_json::Value = serde_json::from_slice(line).ok()?;
    match v["type"].as_str()? {
        "assistant" => {
            let parts: Vec<String> = v["message"]["content"]
                .as_array()?
                .iter()
                .filter_map(|c| match c["type"].as_str()? {
                    "text" => Some(format!("claude: {}", c["text"].as_str()?)),
                    "tool_use" => Some(format!("claude tool: {}", c["name"].as_str()?)),
                    _ => None,
                })
                .collect();
            (!parts.is_empty()).then(|| parts.join("\n"))
        }
        "result" => Some(format!(
            "claude result: {} turns={} cost_usd={}",
            v["subtype"].as_str().unwrap_or("?"),
            v["num_turns"],
            v["total_cost_usd"]
        )),
        "system" => Some(format!(
            "claude session {} model {}",
            v["session_id"].as_str().unwrap_or("?"),
            v["model"].as_str().unwrap_or("?")
        )),
        _ => None,
    }
}

/// Prints complete runner lines to the pane. A line holding the token is
/// never printed (a note is); returns whether that happened.
pub fn print_logs(bytes: &[u8], partial: &mut Vec<u8>, token: &[u8]) -> bool {
    let mut leaked = false;
    partial.extend_from_slice(bytes);
    while let Some(pos) = partial.iter().position(|b| *b == b'\n') {
        let line: Vec<u8> = partial.drain(..=pos).collect();
        let line = &line[..line.len() - 1];
        if holds_secret(line, token) {
            leaked = true;
            println!("aq-sandbox-harness: [a runner output line was withheld: it held a secret]");
            continue;
        }
        if line.starts_with(b"{") {
            if let Some(text) = pane_line(line) {
                println!("{text}");
            }
        } else {
            println!("{}", String::from_utf8_lossy(line));
        }
    }
    let _ = std::io::stdout().flush();
    leaked
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn success_requires_exit_zero_a_successful_result_and_a_changes_line() {
        let result = |subtype: &str, is_error: bool, text: &str| {
            format!(
                "{{\"type\":\"system\"}}\n{}\n",
                serde_json::json!({"type":"result","subtype":subtype,"is_error":is_error,"result":text})
            )
            .into_bytes()
        };
        let done = result(
            "success",
            false,
            "Did it.\nChanges: src/lib.rs; Checks: cargo test; Limits: none",
        );
        let files = vec!["src/lib.rs".to_string()];
        assert_eq!(proven_success(&done, Some(0), &files), Ok(()));
        // Each refusal case alone keeps the attempt failed.
        assert_eq!(
            proven_success(&done, Some(1), &files),
            Err("runner exit not 0")
        );
        assert_eq!(
            proven_success(&done, None, &files),
            Err("runner exit not 0")
        );
        assert_eq!(
            proven_success(b"{\"type\":\"assistant\"}\n", Some(0), &files),
            Err("no final result event")
        );
        for (subtype, is_error) in [
            ("success", true),
            ("error_max_turns", true),
            ("error_during_execution", false),
        ] {
            assert_eq!(
                proven_success(&result(subtype, is_error, "Changes: x"), Some(0), &files),
                Err("final result is not a success"),
                "{subtype} {is_error}"
            );
        }
        // A clarifying question or refusal ends the turn without a Changes line.
        let question = result(
            "success",
            false,
            "Which file should I edit? I could not find src/lib.rs.",
        );
        assert_eq!(
            proven_success(&question, Some(0), &files),
            Err("final message has no Changes: line")
        );
        // A read-only task changes no file and still succeeds.
        let read_only = result(
            "success",
            false,
            "The build passes.\nChanges: none; Checks: cargo test passed; Limits: none",
        );
        assert_eq!(proven_success(&read_only, Some(0), &[]), Ok(()));
        // Only the LAST result event counts.
        let mut later = done.clone();
        later.extend(result("error_during_execution", true, "Changes: none"));
        assert_eq!(
            proven_success(&later, Some(0), &files),
            Err("final result is not a success")
        );
    }

    fn result_of(text: &str) -> Vec<u8> {
        format!(
            "{{\"type\":\"system\"}}\n{}\n",
            serde_json::json!({"type":"result","subtype":"success","is_error":false,"result":text})
        )
        .into_bytes()
    }

    /// Review I-2: a claim of changes that did not arrive is refused; this is a
    /// consistency check, not a file-change requirement.
    #[test]
    fn claimed_changes_that_were_not_copied_back_fail() {
        let edits =
            result_of("Done.\nChanges: src/lib.rs, src/api.rs; Checks: cargo test; Limits: none");
        assert_eq!(
            proven_success(&edits, Some(0), &[]),
            Err("the agent reported changes, but none were copied back")
        );
        assert_eq!(
            proven_success(&edits, Some(0), &["src/lib.rs".into()]),
            Ok(())
        );
        // Review R1-I1: the same table as the claim parser, through proven_success.
        for (claims, value) in crate::claim::tests::cases() {
            let text = format!("Answer.\nChanges: {value}; Checks: none");
            let expected = if claims {
                Err("the agent reported changes, but none were copied back")
            } else {
                Ok(())
            };
            assert_eq!(
                proven_success(&result_of(&text), Some(0), &[]),
                expected,
                "{value:?}"
            );
        }
    }

    /// Owner ruling 4a: `Outcome: blocked` is a failure; a missing or unknown
    /// Outcome line is done, so existing agents keep working.
    #[test]
    fn an_outcome_blocked_line_fails_and_any_other_is_done() {
        for text in [
            "I could not find the config.\nOutcome: blocked\nChanges: none",
            "I could not find the config.\n**Outcome:** Blocked\nChanges: none",
            "I could not find the config.\noutcome: blocked.\nChanges: none",
        ] {
            assert_eq!(
                proven_success(&result_of(text), Some(0), &[]),
                Err("the agent reported Outcome: blocked"),
                "{text}"
            );
        }
        for text in [
            "Answer.\nOutcome: done\nChanges: none",
            "Answer.\nOutcome: maybe\nChanges: none",
            "Answer.\nChanges: none",
        ] {
            assert_eq!(
                proven_success(&result_of(text), Some(0), &[]),
                Ok(()),
                "{text}"
            );
        }
    }

    /// Review I-1, R1-M2: the recorded result is the agent's final message
    /// (bounded), led by its Outcome, its own Changes claim and the copied files.
    #[test]
    fn the_conclusion_keeps_the_final_message() {
        let stream = result_of(
            "CI fails because the lockfile is stale.\nRun cargo update.\nChanges: none; Checks: cargo test (passed); Limits: none",
        );
        assert_eq!(
            conclusion(&stream, Some(0), &[], false),
            "Outcome: done; Changes: none; Copied: none; Claude exit 0; Result: CI fails because the lockfile is stale. Run cargo update. Checks: cargo test (passed); Limits: none"
        );
        let line = conclusion(
            &result_of("Added hello.\nOutcome: done\nChanges: src/lib.rs"),
            Some(0),
            &["src/lib.rs".into()],
            false,
        );
        assert_eq!(
            line,
            "Outcome: done; Changes: src/lib.rs; Copied: src/lib.rs; Claude exit 0; Result: Added hello."
        );
        let long = result_of(&format!(
            "{}\n**Outcome:** blocked\nChanges: none",
            "é".repeat(400)
        ));
        let clipped = conclusion(&long, None, &[], false);
        assert!(
            clipped.len() <= 500 && clipped.ends_with(" [clipped]"),
            "{clipped}"
        );
        assert!(
            clipped.starts_with("Outcome: blocked; Changes: none;"),
            "{clipped}"
        );
        // R2-M3: no bidi or invisible format characters in the summary.
        let bidi = result_of("ab\u{202E}cd\u{2067}ef\nChanges: none");
        assert!(conclusion(&bidi, Some(0), &[], false).ends_with("Result: abcdef"));
        let pointed = conclusion(&long, None, &[], true);
        assert!(pointed.len() <= 500, "{pointed}");
        assert!(pointed.ends_with(" [clipped; the full message is posted to the task comments]"));
        assert!(conclusion(b"", None, &[], false).starts_with(
            "Outcome: done; Changes: unknown; Copied: none; Claude exit unknown; Result: no final message"
        ));
    }
}
