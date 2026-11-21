//! The harness the node driver runs in a docker-local sandbox, and the secrets of a
//! Codex ChatGPT login that must never leave it.
use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use zeroize::Zeroizing;

/// Claude is the default and is never written to the driver configuration, so a
/// Claude configuration keeps its bytes.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DriverHarness {
    #[default]
    Claude,
    Codex,
}
impl DriverHarness {
    pub fn is_claude(&self) -> bool {
        *self == Self::Claude
    }
    /// The transcript file beside the driver configuration.
    pub fn transcript_file(&self) -> &'static str {
        match self {
            Self::Claude => "claude.jsonl",
            Self::Codex => "codex.jsonl",
        }
    }
    /// Where the harness writes its session transcripts inside the sandbox: Claude's
    /// project directory of `/aq/work` in its config directory `/aq/home`, Codex's
    /// `sessions` in its `CODEX_HOME` `/aq/home/codex`. The only part of the private
    /// harness home the node mounts.
    pub fn transcript_dir(&self) -> &'static str {
        match self {
            Self::Claude => "/aq/home/projects/-aq-work",
            Self::Codex => "/aq/home/codex/sessions",
        }
    }
    /// The harness's name in the conclusion line (`Codex exit 0`).
    pub fn label(&self) -> &'static str {
        match self {
            Self::Claude => "Claude",
            Self::Codex => "Codex",
        }
    }
}

/// The id, access and refresh tokens of a ChatGPT login document (`auth.json`),
/// each refused wherever it would leave the sandbox. The caller validates the
/// document's shape first.
pub fn codex_login_tokens(auth: &[u8]) -> Result<Vec<Zeroizing<Vec<u8>>>> {
    let parsed: serde_json::Value = serde_json::from_slice(auth).context("Codex login JSON")?;
    let tokens = ["id_token", "access_token", "refresh_token"].map(|name| {
        parsed["tokens"][name]
            .as_str()
            .filter(|t| !t.is_empty())
            .map(|t| Zeroizing::new(t.as_bytes().to_vec()))
            .context("Codex login token absent")
    });
    tokens.into_iter().collect()
}

/// Where the harness's stop hook leaves the hook JSON of a finished turn.
pub const STOP_MARKER: &str = "/aq/out/stop.json";
/// The stop hook command: the hook JSON (stdin) atomically replaces the marker.
pub const STOP_HOOK: &str = include_str!("claude-stop.sh");

/// Fixed in-sandbox runner of interactive Claude Code (I4a pattern). Public
/// configuration, no secret. Claude runs on the container's terminal, which the
/// node driver attaches to its pane; the startup dialogs (onboarding, workspace
/// trust, bypass-mode confirmation) are answered in the private home beforehand.
pub const CLAUDE_RUNNER: &str = r#"set -uo pipefail
umask 077
in=/aq/in; out=/aq/out; m=/aq/private/material
mkdir -p "$out"
[ -s "$m" ] || { echo "aq-runner: material missing"; exit 90; }
token="$(cat "$m")"; rm -f "$m"
[ -e "$m" ] && { echo "aq-runner: material not wiped"; exit 91; }
while IFS= read -r kv; do [ -n "$kv" ] && export "$kv"; done < "$in/env"
mkdir -p "$HOME" && cp "$in/settings.json" "$HOME/settings.json" || exit 92
printf '%s' '{"hasCompletedOnboarding":true,"bypassPermissionsModeAccepted":true,"projects":{"/aq/work":{"hasTrustDialogAccepted":true,"hasCompletedProjectOnboarding":true}}}' > "$HOME/.claude.json" || exit 92
set --; while IFS= read -r a; do set -- "$@" "$a"; done < "$in/args"
exe="$(cat "$in/executable")"
prompt="$(cat "$in/prompt")"
[ "${#prompt}" -le 30000 ] || prompt="Your task is in the file $in/prompt. Read all of it now and carry it out."
cd /aq/work || exit 93
echo "aq-runner: claude $("$exe" --version 2>/dev/null) uid=$(id -u) capeff=$(awk '/^CapEff:/ {print $2}' /proc/self/status) nnp=$(awk '/^NoNewPrivs:/ {print $2}' /proc/self/status)"
CLAUDE_CODE_OAUTH_TOKEN="$token" IS_SANDBOX=1 TERM=xterm-256color "$exe" "$@" -- "$prompt"
rc=$?
unset token
echo "$rc" > "$out/rc"
login_file=false; [ -e "$HOME/.credentials.json" ] && login_file=true
echo "aq-runner: claude exit $rc login_file=$login_file"
exit "$rc"
"#;

/// Appended to the startup context inside the sandbox.
pub const RUNNER_NOTE: &str = "\n\nRunner note (docker-local sandbox): AQ_AGENT_RESULT_DIR and AQ_AGENT_RESULT_COMMAND are not available here. Do not write request.json or run the \
result client. The node runner submits your final message as the AQ result claim and ends this session when your turn ends. A background build or test does not resume a finished session. Wait for it, inspect its exit status, and continue the task before giving a final answer. End with exactly one line: Changes: ...; Checks: exact commands \
and outcomes; Limits: ... (write Changes: none when you changed no file), and above it one line Outcome: done, or Outcome: blocked when you could not \
do the task. Your worktree is retained as is: keep build outputs out of it (for a Rust crate add a .gitignore with /target, or run cargo clean).\n";
/// Appended when the attempt has an agent channel.
pub const CHANNEL_NOTE: &str = "The aq CLI (/opt/aq/bin/aq, where installed) reaches AQ from this \
sandbox through AQ_CHANNEL; no credential is needed or accepted. `aq commands` lists what you may run.\n";

/// The print-mode arguments the unattended Claude recipe starts with.
const CLAUDE_PRINT: [&str; 6] = [
    "-p",
    "--output-format",
    "stream-json",
    "--verbose",
    "--input-format",
    "text",
];
/// Turns the unattended container recipe (print mode, `settings.json` with all
/// hooks disabled) into the interactive TUI for the docker-local sandbox: the
/// print-mode arguments go (the runner gives the prompt as the last argument),
/// the bypass-mode confirmation dialog is answered, and the only hooks (this file
/// is the only settings source) report the end of the turn, finished or failed,
/// to the node driver. Returns the new `settings.json`.
pub fn claude_interactive(arguments: &mut Vec<String>, settings: &[u8]) -> Result<String> {
    anyhow::ensure!(
        arguments.len() > 6 && arguments[..6] == CLAUDE_PRINT,
        "closed Claude print-mode recipe changed"
    );
    arguments.drain(..6);
    arguments.retain(|a| a != "--no-session-persistence");
    let mut value: serde_json::Value = serde_json::from_slice(settings)?;
    let object = value.as_object_mut().context("Claude settings object")?;
    anyhow::ensure!(
        object.remove("disableAllHooks").is_some() && !object.contains_key("hooks"),
        "closed Claude settings changed"
    );
    let hook = serde_json::json!([{ "hooks": [{ "type": "command", "command": STOP_HOOK }] }]);
    object.insert("skipDangerousModePermissionPrompt".into(), true.into());
    object.insert(
        "hooks".into(),
        serde_json::json!({ "Stop": hook, "StopFailure": hook }),
    );
    Ok(serde_json::to_string_pretty(&value)?)
}

/// The final-result stream of an interactive turn, from the stop hook's JSON:
/// Claude's `Stop`/`StopFailure` hook input or Codex's `notify` payload. One
/// `result` event in the shape `proven_success` and `conclusion` read; a failed
/// turn is an error result. None: no (whole) marker yet.
pub fn stop_stream(marker: &[u8]) -> Option<Vec<u8>> {
    let hook: serde_json::Value = serde_json::from_slice(marker).ok()?;
    let failed = hook["hook_event_name"] == "StopFailure";
    let mut line = serde_json::json!({
        "type": "result",
        "subtype": if failed { "error" } else { "success" },
        "is_error": failed,
    });
    let text = hook["last_assistant_message"]
        .as_str()
        .or(hook["last-assistant-message"].as_str());
    // Codex also notifies for its own thread-title turn (`{"title":"..."}`): not the agent's turn.
    let title = |t: &str| {
        serde_json::from_str::<serde_json::Value>(t).is_ok_and(|v| v["title"].is_string())
    };
    if text.is_some_and(title) {
        return None;
    }
    if let Some(text) = text {
        line["result"] = text.into();
    }
    Some(format!("{line}\n").into_bytes())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{final_text, proven_success};
    #[test]
    fn claude_stop_keeps_pending_checks_in_the_same_session() {
        use std::{
            io::Write,
            process::{Command, Stdio},
        };
        let dir = tempfile::tempdir().unwrap();
        let marker = dir.path().join("stop.json");
        let script = STOP_HOOK.replace("/aq/out", dir.path().to_str().unwrap());
        let run = |event: &str, text: &str, continued: bool, pending: bool| {
            let tasks = if pending {
                vec![serde_json::json!({"type":"shell","status":"running"})]
            } else {
                vec![]
            };
            let input = serde_json::json!({"hook_event_name":event,"last_assistant_message":text,
                "stop_hook_active":continued,"background_tasks":tasks});
            let mut child = Command::new("sh")
                .args(["-c", &script])
                .stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .spawn()
                .unwrap();
            child
                .stdin
                .take()
                .unwrap()
                .write_all(input.to_string().as_bytes())
                .unwrap();
            let out = child.wait_with_output().unwrap();
            assert!(out.status.success());
            if marker.exists() {
                assert_eq!(
                    serde_json::from_slice::<serde_json::Value>(&std::fs::read(&marker).unwrap())
                        .unwrap(),
                    input
                );
                std::fs::remove_file(&marker).unwrap();
                return "recorded";
            }
            if out.stdout.is_empty() {
                return "waiting";
            }
            let reply: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
            assert_eq!(reply["decision"], "block");
            "continue"
        };
        assert_eq!(run("Stop", "Waiting for clippy", false, true), "continue");
        assert_eq!(run("Stop", "Waiting for clippy", true, true), "waiting");
        assert_eq!(run("Stop", "Still incomplete", true, false), "recorded");
        assert_eq!(run("Stop", "Changes: none", false, false), "recorded");
        assert_eq!(run("Stop", "Outcome: blocked", false, true), "recorded");
        assert_eq!(run("StopFailure", "Runner failed", false, true), "recorded");
    }

    #[test]
    fn a_finished_turn_is_the_result_and_a_failed_turn_is_not_a_success() {
        let done = "Done.\nOutcome: done\nChanges: none; Checks: none; Limits: none";
        let claude = serde_json::json!({"hook_event_name":"Stop","stop_hook_active":false,"last_assistant_message":done});
        let stream = stop_stream(claude.to_string().as_bytes()).unwrap();
        assert_eq!(final_text(&stream).as_deref(), Some(done));
        assert_eq!(proven_success(&stream, Some(0), &[]), Ok(()));
        let codex = serde_json::json!({"type":"agent-turn-complete","last-assistant-message":done});
        assert_eq!(stop_stream(codex.to_string().as_bytes()).unwrap(), stream);
        let failed =
            serde_json::json!({"hook_event_name":"StopFailure","last_assistant_message":done});
        let stream = stop_stream(failed.to_string().as_bytes()).unwrap();
        assert_eq!(
            proven_success(&stream, Some(0), &[]),
            Err("final result is not a success")
        );
        assert_eq!(
            stop_stream(b"{\"hook_event_name\":\"St"),
            None,
            "half written"
        );
        let title = serde_json::json!({"type":"agent-turn-complete","last-assistant-message":"{\"title\":\"Append a line\"}"});
        assert_eq!(
            stop_stream(title.to_string().as_bytes()),
            None,
            "the title turn"
        );
    }
    #[test]
    fn interactive_claude_drops_print_mode_answers_the_dialog_and_hooks_the_turn_end() {
        let mut arguments: Vec<String> = [
            "-p",
            "--output-format",
            "stream-json",
            "--verbose",
            "--input-format",
            "text",
            "--model",
            "m",
            "--permission-mode",
            "bypassPermissions",
            "--setting-sources",
            "user",
            "--settings",
            "/aq/home/settings.json",
            "--strict-mcp-config",
            "--no-session-persistence",
            "--disable-slash-commands",
            "--effort",
            "high",
        ]
        .map(String::from)
        .to_vec();
        let settings = br#"{"permissions":{"defaultMode":"bypassPermissions"},"sandbox":{"enabled":false},"disableAllHooks":true}"#;
        let json = claude_interactive(&mut arguments, settings).unwrap();
        assert_eq!(
            arguments,
            [
                "--model",
                "m",
                "--permission-mode",
                "bypassPermissions",
                "--setting-sources",
                "user",
                "--settings",
                "/aq/home/settings.json",
                "--strict-mcp-config",
                "--disable-slash-commands",
                "--effort",
                "high"
            ]
        );
        let s: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(s["permissions"]["defaultMode"], "bypassPermissions");
        assert_eq!(s["skipDangerousModePermissionPrompt"], true);
        assert!(s.get("disableAllHooks").is_none());
        assert_eq!(s["hooks"].as_object().unwrap().len(), 2);
        for event in ["Stop", "StopFailure"] {
            assert_eq!(s["hooks"][event][0]["hooks"][0]["command"], STOP_HOOK);
        }
        // A recipe that is not the closed print-mode one is refused, not guessed at.
        let mut other = vec!["--model".to_string(); 8];
        assert!(claude_interactive(&mut other, settings).is_err());
        let mut again = arguments.clone();
        assert!(claude_interactive(&mut again, settings).is_err());
    }
    #[test]
    fn claude_runner_is_valid_bash_keeps_the_token_out_of_argv_and_answers_the_dialogs() {
        let ok = std::process::Command::new("bash")
            .args(["-n", "-c", CLAUDE_RUNNER])
            .status()
            .unwrap();
        assert!(ok.success());
        let at = |text: &str| CLAUDE_RUNNER.find(text).unwrap();
        assert!(at("token=\"$(cat \"$m\")\"") < at("rm -f \"$m\""));
        assert!(at("rm -f \"$m\"") < at("CLAUDE_CODE_OAUTH_TOKEN=\"$token\""));
        assert!(at("hasTrustDialogAccepted") < at("CLAUDE_CODE_OAUTH_TOKEN=\"$token\""));
        for absent in [
            "export CLAUDE_CODE_OAUTH_TOKEN",
            "--dangerously-skip-permissions",
            "echo \"$token",
            "stream-json",
            "tee ",
        ] {
            assert!(!CLAUDE_RUNNER.contains(absent), "{absent}");
        }
    }
}
