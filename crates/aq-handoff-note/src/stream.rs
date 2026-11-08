//! Reading Claude's stream-json transcript: the proven-success rule, the
//! bounded conclusion line and the pane rendering of one event.

/// The last stream-json `result` event Claude printed, if any.
fn last_result(stream: &[u8]) -> Option<serde_json::Value> {
    stream
        .split(|b| *b == b'\n')
        .rev()
        .filter_map(|line| serde_json::from_slice::<serde_json::Value>(line).ok())
        .find(|v| v["type"] == "result")
}

/// MS1 review B1: a Claude exit 0 alone never proves the work was done
/// (Claude can end its turn with a question, a refusal or "couldn't find X").
/// The driver records success only when all of these hold; otherwise the
/// executor reports Failed and D3 sends the task to rework.
///
/// - the runner exited 0 (backend-observed);
/// - the last stream-json `result` event has `subtype == "success"` and
///   `is_error == false` (auth failures, max-turns, budget and errors set
///   `is_error`);
/// - the agent's final text contains its own `Changes:` line;
/// - the copied-back change set is non-empty.
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
    if !text
        .lines()
        .any(|line| line.trim_start().starts_with("Changes:"))
    {
        return Err("final message has no Changes: line");
    }
    if changed.is_empty() {
        return Err("no workspace change was copied back");
    }
    Ok(())
}

/// One-line, bounded conclusion from Claude's stream-json `result` event.
pub fn conclusion(stream: &[u8], rc: Option<i32>, changed: &[String]) -> String {
    let mut text = None;
    for line in stream.split(|b| *b == b'\n') {
        if let Ok(v) = serde_json::from_slice::<serde_json::Value>(line)
            && v["type"] == "result"
        {
            text = v["result"].as_str().map(str::to_owned);
        }
    }
    let text = text.unwrap_or_else(|| "no final message".into());
    let summary = text
        .lines()
        .rev()
        .find(|l| l.trim_start().starts_with("Changes:"))
        .map(|l| l.trim_start().trim_start_matches("Changes:").to_string())
        .unwrap_or(text.clone());
    let summary = summary.split_whitespace().collect::<Vec<_>>().join(" ");
    let files = if changed.is_empty() {
        "none".to_string()
    } else {
        changed.join(",")
    };
    let rc = rc.map_or("unknown".to_string(), |c| c.to_string());
    let mut line = format!("Changes: {files}; Claude exit {rc}; Summary: {summary}");
    if line.len() > 500 {
        let mut cut = 497;
        while !line.is_char_boundary(cut) {
            cut -= 1;
        }
        line.truncate(cut);
        line.push_str("...");
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn success_requires_exit_zero_a_successful_result_a_changes_line_and_changes() {
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
        assert_eq!(
            proven_success(&done, Some(0), &[]),
            Err("no workspace change was copied back")
        );
        // Only the LAST result event counts.
        let mut later = done.clone();
        later.extend(result("error_during_execution", true, "Changes: none"));
        assert_eq!(
            proven_success(&later, Some(0), &files),
            Err("final result is not a success")
        );
    }

    #[test]
    fn conclusion_is_one_bounded_line() {
        let stream = b"{\"type\":\"system\"}\n{\"type\":\"result\",\"result\":\"Added hello.\\nAll good.\"}\n";
        let line = conclusion(stream, Some(0), &["src/lib.rs".into()]);
        assert_eq!(
            line,
            "Changes: src/lib.rs; Claude exit 0; Summary: Added hello. All good."
        );
        let long = format!("{{\"type\":\"result\",\"result\":\"{}\"}}", "é".repeat(400));
        assert!(conclusion(long.as_bytes(), None, &[]).len() <= 512);
    }
}
