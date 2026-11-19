//! Codex in the node's docker-local sandbox: the closed recipe (container
//! confinement, so `--sandbox danger-full-access` inside the container), and the
//! mapping of Codex's `exec --json` output to the result-local conclusion.
//!
//! In the docker-local sandbox the AQ result client is not available. The node
//! driver submits the claim from the agent's final message, as it does for
//! Claude. This module turns Codex's JSONL into the same final-result shape the
//! driver already reads for Claude (a `result` event that carries the final
//! message and whether the turn succeeded), so the proven-success rule and the
//! conclusion line are the same for both harnesses. Pure: no filesystem,
//! credential, process or authority operations.
use super::*;

/// The fixed tool `PATH` of the closed recipe (`map_mode`).
const RECIPE_PATH: &str = "shell_environment_policy.set.PATH=\"/usr/local/bin:/usr/bin:/bin\"";

/// Fixed in-sandbox runner for Codex. Public configuration, no secret. The
/// ChatGPT login (`auth.json`) arrives as this attempt's private material and is
/// installed read-only in the attempt's own `CODEX_HOME`: Codex cannot write a
/// refreshed login over it, and nothing under `CODEX_HOME` ever leaves the
/// sandbox (only `/aq/work` is copied back), so no attempt rotates the shared
/// login of another. Codex runs with `--sandbox danger-full-access`: the
/// container is the confinement.
pub const CODEX_RUNNER: &str = r#"set -uo pipefail
umask 077
in=/aq/in; out=/aq/out; m=/aq/private/material
mkdir -p "$out"
[ -s "$m" ] || { echo "aq-runner: material missing"; exit 90; }
while IFS= read -r kv; do [ -n "$kv" ] && export "$kv"; done < "$in/env"
[ -n "${CODEX_HOME:-}" ] || { echo "aq-runner: CODEX_HOME unset"; exit 92; }
mkdir -p "$CODEX_HOME" && cp "$m" "$CODEX_HOME/auth.json" && chmod 0400 "$CODEX_HOME/auth.json" || exit 92
rm -f "$m"
[ -e "$m" ] && { echo "aq-runner: material not wiped"; exit 91; }
set --; while IFS= read -r a; do set -- "$@" "$a"; done < "$in/args"
exe="$(cat "$in/executable")"
cd /aq/work || exit 93
echo "aq-runner: codex $("$exe" --version 2>/dev/null) uid=$(id -u) capeff=$(awk '/^CapEff:/ {print $2}' /proc/self/status) nnp=$(awk '/^NoNewPrivs:/ {print $2}' /proc/self/status)"
"$exe" "$@" < "$in/prompt" 2> "$out/codex.err" | tee "$out/codex.jsonl"
rc=${PIPESTATUS[0]}
rm -f "$CODEX_HOME/auth.json"
echo "$rc" > "$out/rc"
echo "aq-runner: codex exit $rc"
exit "$rc"
"#;

/// Result adapter profile id of this recipe.
pub const PROFILE: &str = "codex-0.166-docker-local-result-local-v1";

/// The public, content-addressed description of how the node driver runs Codex in
/// a sandbox. Its bytes are a server Definition (`aq-worker ms1-definitions`).
pub fn recipe_body() -> String {
    let mut value = serde_json::json!({
        "schema_version": 1,
        "profile": PROFILE,
        "cli_version": CODEX_CLI_VERSION,
        "confinement": "container",
        "codex_sandbox": "danger-full-access",
        "runner_digest": aq_session_protocol::Digest::of(CODEX_RUNNER.as_bytes()),
        "login_delivery": "private_material_readonly_auth_json_v1",
        "result_claim": "node_driver_final_message_v1",
        "workspace": "copy_in_copy_out_v1",
    });
    value.sort_all_objects();
    value.to_string()
}
pub fn recipe_artifact() -> aq_session_protocol::ArtifactRef {
    aq_session_protocol::ArtifactRef {
        id: aq_session_protocol::Text::new("aq.codex-docker-local-result-local-recipe")
            .expect("static id"),
        version: aq_session_protocol::Text::new("1").expect("static version"),
        digest: aq_session_protocol::Digest::of(recipe_body().as_bytes()),
    }
}
/// Manifest settings this recipe accepts: Codex 0.166 on an OpenAI model, container
/// confinement, no terminal input, a reasoning effort the mapper knows.
pub fn validate_manifest(m: &aq_session_protocol::LaunchManifest) -> Result<()> {
    let settings = Settings {
        harness: &m.harness,
        model: &m.model,
        terminal_input_enabled: m.terminal.input_enabled,
    };
    ensure!(
        !m.terminal.input_enabled
            && declares(
                settings,
                aq_session_protocol::CONTAINER_CONFINEMENT_CAPABILITY
            )
            && !declares(settings, aq_session_protocol::NATIVE_CONFINEMENT_CAPABILITY),
        "Codex docker-local result recipe unsupported manifest settings"
    );
    validate_settings(settings)
}

/// The node's per-attempt agent channel, as the sandbox sees it.
#[derive(Clone, Copy)]
pub struct AgentChannel<'a> {
    /// `AQ_CHANNEL` value, for example `unix:/aq/channel/aq.sock`.
    pub socket: &'a str,
    /// Directory of the in-sandbox `aq` CLI, put first on the tools' `PATH`.
    pub bin_dir: &'a str,
}

/// The container recipe (`map_container`) for a non-interactive `codex exec` in the
/// docker-local sandbox, reading the ChatGPT login from `CODEX_HOME/auth.json`.
/// With an agent channel, Codex's tool environment (which inherits nothing) gets
/// `AQ_CHANNEL` and the `aq` CLI directory, the way `result_local` adds the result
/// channel variables.
pub fn map_docker_local(
    settings: Settings<'_>,
    paths: Paths<'_>,
    channel: Option<AgentChannel<'_>>,
) -> Result<Recipe> {
    let mut recipe = map_container(settings, paths)?;
    let end = recipe
        .arguments
        .len()
        .checked_sub(1)
        .context("closed Codex stdin marker absent")?;
    ensure!(
        recipe.arguments[end] == "-",
        "closed Codex stdin marker changed"
    );
    let mut additional = vec![
        "--config".to_string(),
        "cli_auth_credentials_store=\"file\"".to_string(),
        "--config".to_string(),
        "forced_login_method=\"chatgpt\"".to_string(),
    ];
    if let Some(AgentChannel { socket, bin_dir }) = channel {
        for value in [socket, bin_dir] {
            ensure!(
                !value.is_empty() && value.len() <= 1024 && !value.contains(['\n', '\0']),
                "agent channel value shape"
            );
        }
        ensure!(
            bin_dir.starts_with('/') && !bin_dir.contains(':'),
            "agent channel bin directory must be absolute"
        );
        let positions: Vec<_> = recipe
            .arguments
            .iter()
            .enumerate()
            .filter(|(_, a)| *a == RECIPE_PATH)
            .map(|(i, _)| i)
            .collect();
        ensure!(
            positions.len() == 1,
            "closed Codex PATH setting absent or duplicated"
        );
        recipe.arguments[positions[0]] = format!(
            "shell_environment_policy.set.PATH={}",
            toml::Value::String(format!("{bin_dir}:/usr/local/bin:/usr/bin:/bin"))
        );
        additional.push("--config".into());
        additional.push(format!(
            "shell_environment_policy.set.AQ_CHANNEL={}",
            toml::Value::String(socket.into())
        ));
    }
    recipe.arguments.splice(end..end, additional);
    ensure!(
        recipe.arguments.len() <= 128 && recipe.arguments.iter().all(|value| value.len() <= 4096),
        "docker-local recipe argument bound"
    );
    Ok(recipe)
}

/// What Codex's `exec --json` stream says about its one turn.
#[derive(Debug, PartialEq, Eq)]
pub struct Outcome {
    /// A `turn.completed` was seen and no `turn.failed` or `error` event.
    pub succeeded: bool,
    /// The text of the last completed `agent_message` item, else the last error message.
    pub message: Option<String>,
}

fn events(stream: &[u8]) -> impl Iterator<Item = serde_json::Value> + '_ {
    stream
        .split(|b| *b == b'\n')
        .filter_map(|line| serde_json::from_slice::<serde_json::Value>(line).ok())
}
fn agent_message(event: &serde_json::Value) -> Option<&str> {
    (event["type"] == "item.completed" && event["item"]["type"] == "agent_message")
        .then(|| event["item"]["text"].as_str())
        .flatten()
}
fn error_message(event: &serde_json::Value) -> Option<&str> {
    match event["type"].as_str()? {
        "turn.failed" => event["error"]["message"].as_str(),
        "error" => event["message"].as_str(),
        _ => None,
    }
}

/// None when the stream holds no terminal event (`turn.completed`, `turn.failed`
/// or `error`): the run's end is unknown, never a success.
pub fn outcome(stream: &[u8]) -> Option<Outcome> {
    let (mut completed, mut failed) = (false, false);
    let (mut message, mut error) = (None, None);
    for event in events(stream) {
        match event["type"].as_str() {
            Some("turn.completed") => completed = true,
            Some("turn.failed" | "error") => {
                failed = true;
                error = error_message(&event).map(str::to_owned).or(error);
            }
            _ => {}
        }
        if let Some(text) = agent_message(&event) {
            message = Some(text.to_owned());
        }
    }
    (completed || failed).then(|| Outcome {
        succeeded: completed && !failed,
        message: message.or(error),
    })
}

/// The stream the node driver reads for its claim: one `assistant` event per
/// completed agent message, then one final `result` event (`subtype` `success`
/// with `is_error` false only when the turn succeeded). An unfinished stream has
/// no `result` event.
pub fn result_stream(stream: &[u8]) -> Vec<u8> {
    let mut out = Vec::new();
    for event in events(stream) {
        if let Some(text) = agent_message(&event) {
            let line = serde_json::json!({
                "type": "assistant",
                "message": {"content": [{"type": "text", "text": text}]},
            });
            out.extend_from_slice(line.to_string().as_bytes());
            out.push(b'\n');
        }
    }
    if let Some(Outcome { succeeded, message }) = outcome(stream) {
        let mut line = serde_json::json!({
            "type": "result",
            "subtype": if succeeded { "success" } else { "error" },
            "is_error": !succeeded,
        });
        if let Some(message) = message {
            line["result"] = message.into();
        }
        out.extend_from_slice(line.to_string().as_bytes());
        out.push(b'\n');
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tests::{selected, settings};
    use aq_session_protocol::{Capability, Text};

    fn container_settings() -> (HarnessSettings, ModelSettings) {
        let (mut harness, model) = settings();
        harness.required_capabilities.push(Capability {
            id: Text::new(aq_session_protocol::CONTAINER_CONFINEMENT_CAPABILITY).unwrap(),
            version: 1,
        });
        (harness, model)
    }
    fn paths() -> Paths<'static> {
        Paths {
            executable: Path::new("/opt/aq/bin/codex"),
            workspace: Path::new("/aq/work"),
            private_home: Path::new("/aq/home/codex"),
        }
    }
    fn config_values(recipe: &Recipe) -> Vec<&str> {
        recipe
            .arguments
            .windows(2)
            .filter(|w| w[0] == "--config")
            .map(|w| w[1].as_str())
            .collect()
    }

    #[test]
    fn docker_local_recipe_is_exec_with_full_access_inside_the_container() {
        let (harness, model) = container_settings();
        let recipe = map_docker_local(selected(&harness, &model), paths(), None).unwrap();
        let args = &recipe.arguments;
        assert_eq!(&args[..2], ["--no-daemon", "exec"]);
        let at = |flag: &str| args.iter().position(|a| a == flag).unwrap() + 1;
        assert_eq!(args[at("--sandbox")], "danger-full-access");
        assert_eq!(args[at("--model")], "gpt-5.1-codex-mini");
        assert_eq!(args[at("--cd")], "/aq/work");
        assert_eq!(args.last().map(String::as_str), Some("-"));
        let config = config_values(&recipe);
        for wanted in [
            "model_reasoning_effort=\"medium\"",
            "approval_policy=\"never\"",
            "cli_auth_credentials_store=\"file\"",
            "forced_login_method=\"chatgpt\"",
            RECIPE_PATH,
        ] {
            assert!(config.contains(&wanted), "{wanted} missing: {config:?}");
        }
        assert!(!config.iter().any(|c| c.contains("AQ_CHANNEL")));
        assert_eq!(recipe.environment["CODEX_HOME"], "/aq/home/codex");
        assert_eq!(recipe.environment.len(), 1);
    }

    #[test]
    fn docker_local_recipe_refuses_native_confinement_and_interactive_settings() {
        let (harness, model) = settings();
        assert!(map_docker_local(selected(&harness, &model), paths(), None).is_err());
        let (harness, model) = container_settings();
        let mut interactive = selected(&harness, &model);
        interactive.terminal_input_enabled = true;
        assert!(map_docker_local(interactive, paths(), None).is_err());
        let (harness, mut model) = container_settings();
        model.reasoning = Text::new("max").unwrap();
        assert!(map_docker_local(selected(&harness, &model), paths(), None).is_err());
    }

    #[test]
    fn agent_channel_reaches_the_tool_environment() {
        let (harness, model) = container_settings();
        let channel = AgentChannel {
            socket: "unix:/aq/channel/aq.sock",
            bin_dir: "/opt/aq/bin",
        };
        let recipe = map_docker_local(selected(&harness, &model), paths(), Some(channel)).unwrap();
        let config = config_values(&recipe);
        assert!(config.contains(
            &"shell_environment_policy.set.PATH=\"/opt/aq/bin:/usr/local/bin:/usr/bin:/bin\""
        ));
        assert!(
            config
                .contains(&"shell_environment_policy.set.AQ_CHANNEL=\"unix:/aq/channel/aq.sock\"")
        );
        assert!(!config.contains(&RECIPE_PATH));
        assert_eq!(recipe.arguments.last().map(String::as_str), Some("-"));
        let bad = AgentChannel {
            socket: "unix:/aq/channel/aq.sock",
            bin_dir: "relative/bin",
        };
        assert!(map_docker_local(selected(&harness, &model), paths(), Some(bad)).is_err());
    }

    /// The login is wiped from the material path before Codex starts, installed
    /// read-only in the attempt's own CODEX_HOME, never put in argv or the
    /// environment, and removed after the run; Codex reads the prompt on stdin.
    #[test]
    fn codex_runner_installs_the_login_read_only_and_wipes_the_material_first() {
        let at = |text: &str| CODEX_RUNNER.find(text).unwrap();
        let install = at("cp \"$m\" \"$CODEX_HOME/auth.json\"");
        let readonly = at("chmod 0400 \"$CODEX_HOME/auth.json\"");
        let wipe = at("rm -f \"$m\"");
        let exec = at("\"$exe\" \"$@\" < \"$in/prompt\"");
        let removed = at("rm -f \"$CODEX_HOME/auth.json\"");
        assert!(install < readonly && readonly < wipe && wipe < exec && exec < removed);
        assert!(CODEX_RUNNER.contains("tee \"$out/codex.jsonl\""));
        for absent in [
            "OPENAI_API_KEY",
            "CODEX_API_KEY",
            "token=",
            "echo \"$m",
            "cat \"$m\"",
        ] {
            assert!(!CODEX_RUNNER.contains(absent), "{absent}");
        }
        let body: serde_json::Value = serde_json::from_str(&recipe_body()).unwrap();
        assert_eq!(body["profile"], PROFILE);
        assert_eq!(
            body["runner_digest"],
            aq_session_protocol::Digest::of(CODEX_RUNNER.as_bytes()).as_str()
        );
        assert_eq!(
            recipe_artifact().digest,
            aq_session_protocol::Digest::of(recipe_body().as_bytes())
        );
    }

    fn line(value: serde_json::Value) -> String {
        value.to_string() + "\n"
    }
    fn message(text: &str) -> String {
        line(
            serde_json::json!({"type":"item.completed","item":{"id":"item_1","type":"agent_message","text":text}}),
        )
    }

    #[test]
    fn a_completed_turn_maps_to_a_successful_result_with_the_last_message() {
        let stream = [
            line(serde_json::json!({"type":"thread.started","thread_id":"t"})),
            line(serde_json::json!({"type":"turn.started"})),
            message("Looking."),
            line(serde_json::json!({"type":"item.completed","item":{"type":"command_execution","command":"ls"}})),
            message("Done.\nChanges: src/lib.rs; Checks: cargo test; Limits: none"),
            line(serde_json::json!({"type":"turn.completed","usage":{"input_tokens":1}})),
        ]
        .concat();
        let outcome = outcome(stream.as_bytes()).unwrap();
        assert!(outcome.succeeded);
        assert_eq!(
            outcome.message.as_deref(),
            Some("Done.\nChanges: src/lib.rs; Checks: cargo test; Limits: none")
        );
        let mapped = result_stream(stream.as_bytes());
        let events: Vec<serde_json::Value> = mapped
            .split(|b| *b == b'\n')
            .filter(|l| !l.is_empty())
            .map(|l| serde_json::from_slice(l).unwrap())
            .collect();
        assert_eq!(events.len(), 3);
        assert_eq!(events[0]["type"], "assistant");
        assert_eq!(events[0]["message"]["content"][0]["text"], "Looking.");
        assert_eq!(events[2]["type"], "result");
        assert_eq!(events[2]["subtype"], "success");
        assert_eq!(events[2]["is_error"], false);
        assert_eq!(
            events[2]["result"],
            "Done.\nChanges: src/lib.rs; Checks: cargo test; Limits: none"
        );
    }

    #[test]
    fn a_failed_or_unfinished_turn_is_never_a_success() {
        let failed = [
            line(serde_json::json!({"type":"turn.started"})),
            line(
                serde_json::json!({"type":"turn.failed","error":{"message":"usage limit reached"}}),
            ),
        ]
        .concat();
        let o = outcome(failed.as_bytes()).unwrap();
        assert!(!o.succeeded);
        assert_eq!(o.message.as_deref(), Some("usage limit reached"));
        let mapped = String::from_utf8(result_stream(failed.as_bytes())).unwrap();
        assert!(mapped.contains("\"subtype\":\"error\"") && mapped.contains("\"is_error\":true"));
        // A completion followed by an error event is a failure, with the agent's message kept.
        let late = [
            message("Changes: none; Checks: none; Limits: none"),
            line(serde_json::json!({"type":"turn.completed"})),
            line(serde_json::json!({"type":"error","message":"stream closed"})),
        ]
        .concat();
        let o = outcome(late.as_bytes()).unwrap();
        assert!(!o.succeeded);
        assert_eq!(
            o.message.as_deref(),
            Some("Changes: none; Checks: none; Limits: none")
        );
        // No terminal event: unknown, and no result event at all.
        let unfinished = [
            line(serde_json::json!({"type":"turn.started"})),
            message("half"),
        ]
        .concat();
        assert_eq!(outcome(unfinished.as_bytes()), None);
        assert!(
            !String::from_utf8(result_stream(unfinished.as_bytes()))
                .unwrap()
                .contains("\"result\"")
        );
        assert_eq!(outcome(b"not json\n"), None);
    }
}
