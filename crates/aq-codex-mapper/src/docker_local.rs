//! Codex in the node's docker-local sandbox: the closed recipe (container
//! confinement, so `--sandbox danger-full-access` inside the container) for the
//! interactive TUI on the sandbox's terminal.
//!
//! In the docker-local sandbox the AQ result client is not available. The node
//! driver submits the claim from the agent's final message, as it does for
//! Claude: Codex's `notify` program leaves the finished turn's payload (with its
//! last assistant message) in the stop marker the driver waits for. Pure: no
//! filesystem, credential, process or authority operations.
use super::*;

/// The fixed tool `PATH` of the closed recipe (`map_mode`).
const RECIPE_PATH: &str = "shell_environment_policy.set.PATH=\"/usr/local/bin:/usr/bin:/bin\"";

/// Fixed in-sandbox runner for Codex. Public configuration, no secret. The
/// ChatGPT login (`auth.json`) arrives as this attempt's private material and is
/// installed read-only in the attempt's own `CODEX_HOME`: Codex cannot write a
/// refreshed login over it, and nothing under `CODEX_HOME` ever leaves the
/// sandbox (only `/aq/work` is copied back), so no attempt rotates the shared
/// login of another. Codex runs with `--sandbox danger-full-access`: the
/// container is the confinement. It runs as the interactive TUI on the
/// container's terminal, with the startup context as its first prompt. With the
/// node's pinned helper bundle (`/aq/in/codex-runtime`, the same one a hand-started
/// Codex agent gets) Codex runs from a private copy beside its helpers; without
/// them its shell tool does not start.
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
if [ -d "$in/codex-runtime" ]; then
  actual=$(sha256sum "$exe"); actual=${actual%% *}
  [ "$actual" = "$(cat "$in/codex-digest")" ] || { echo "aq-runner: installed Codex does not match its pinned helpers"; exit 94; }
  runtime="$HOME/.codex-runtime"
  mkdir -p "$runtime" && cp -L "$exe" "$runtime/codex" && cp -R "$in/codex-runtime/." "$runtime/" || exit 94
  chmod 700 "$runtime/codex" "$runtime/codex-code-mode-host" "$runtime/codex-path/rg" "$runtime/codex-resources/bwrap"
  exe="$runtime/codex"
  export PATH="$runtime/codex-path:$PATH"
fi
prompt="$(cat "$in/prompt")"
[ "${#prompt}" -le 30000 ] || prompt="Your task is in the file $in/prompt. Read all of it now and carry it out."
TERM=xterm-256color "$exe" "$@" "$prompt"
rc=$?
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
        "result_claim": "node_driver_notify_final_message_v1",
        "terminal": "interactive_tty_v1",
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

/// Where Codex's `notify` program leaves a finished turn's payload, and the
/// program: Codex gives the payload as the last argument.
const NOTIFY: &str = r#"notify=["/bin/bash","-c","printf '%s' \"$1\" > /aq/out/stop.tmp && mv /aq/out/stop.tmp /aq/out/stop.json","aq-stop"]"#;

/// The container recipe (`map_container_interactive`) for the Codex TUI in the
/// docker-local sandbox, reading the ChatGPT login from `CODEX_HOME/auth.json`.
/// The arguments end with `--`; the runner appends the startup prompt.
/// With an agent channel, Codex's tool environment (which inherits nothing) gets
/// `AQ_CHANNEL` and the `aq` CLI directory, the way `result_local` adds the result
/// channel variables.
pub fn map_docker_local(
    settings: Settings<'_>,
    paths: Paths<'_>,
    channel: Option<AgentChannel<'_>>,
) -> Result<Recipe> {
    let home = paths.private_home.to_str().context("Codex home UTF8")?;
    let mut recipe = map_container_interactive(settings, paths)?;
    let end = recipe
        .arguments
        .len()
        .checked_sub(1)
        .context("closed Codex prompt marker absent")?;
    ensure!(
        recipe.arguments[end] == "--",
        "closed Codex prompt marker changed"
    );
    let mut additional = vec![
        "--config".to_string(),
        "forced_login_method=\"chatgpt\"".to_string(),
        "--config".to_string(),
        "check_for_update_on_startup=false".to_string(),
        "--config".to_string(),
        NOTIFY.to_string(),
    ];
    // The tools' PATH: the `aq` CLI first (with a channel), the template's fixed
    // toolchain, then the helper bundle's programs (`rg`) where the runner puts them.
    let mut path = format!("/usr/local/bin:/usr/bin:/bin:{home}/.codex-runtime/codex-path");
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
        path = format!("{bin_dir}:{path}");
        additional.push("--config".into());
        additional.push(format!(
            "shell_environment_policy.set.AQ_CHANNEL={}",
            toml::Value::String(socket.into())
        ));
    }
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
        toml::Value::String(path)
    );
    recipe.arguments.splice(end..end, additional);
    ensure!(
        recipe.arguments.len() <= 128 && recipe.arguments.iter().all(|value| value.len() <= 4096),
        "docker-local recipe argument bound"
    );
    Ok(recipe)
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
    fn docker_local_recipe_is_the_tui_with_full_access_inside_the_container() {
        let (harness, model) = container_settings();
        let recipe = map_docker_local(selected(&harness, &model), paths(), None).unwrap();
        let args = &recipe.arguments;
        assert_eq!(args[0], "--no-daemon");
        assert!(!args.iter().any(|a| a == "exec" || a == "--json"));
        let at = |flag: &str| args.iter().position(|a| a == flag).unwrap() + 1;
        assert_eq!(args[at("--sandbox")], "danger-full-access");
        assert_eq!(args[at("-m")], "gpt-5.1-codex-mini");
        assert_eq!(args[at("--cd")], "/aq/work");
        assert_eq!(args.last().map(String::as_str), Some("--"));
        let config = config_values(&recipe);
        // The notify program is valid TOML and writes the stop marker atomically.
        let notify: toml::Value = toml::from_str(NOTIFY).unwrap();
        assert_eq!(notify["notify"].as_array().unwrap().len(), 4);
        assert!(NOTIFY.contains("/aq/out/stop.json") && config.contains(&NOTIFY));
        for wanted in [
            "model_reasoning_effort=\"medium\"",
            "approval_policy=\"never\"",
            "cli_auth_credentials_store=\"file\"",
            "forced_login_method=\"chatgpt\"",
            "shell_environment_policy.set.PATH=\"/usr/local/bin:/usr/bin:/bin:/aq/home/codex/.codex-runtime/codex-path\"",
        ] {
            assert!(config.contains(&wanted), "{wanted} missing: {config:?}");
        }
        assert!(!config.iter().any(|c| c.contains("AQ_CHANNEL")));
        assert_eq!(recipe.environment["CODEX_HOME"], "/aq/home/codex");
        assert_eq!(recipe.environment["HOME"], "/aq/home/codex");
        assert_eq!(recipe.environment.len(), 2);
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
            &"shell_environment_policy.set.PATH=\"/opt/aq/bin:/usr/local/bin:/usr/bin:/bin:/aq/home/codex/.codex-runtime/codex-path\""
        ));
        assert!(
            config
                .contains(&"shell_environment_policy.set.AQ_CHANNEL=\"unix:/aq/channel/aq.sock\"")
        );
        assert!(!config.contains(&RECIPE_PATH));
        assert_eq!(recipe.arguments.last().map(String::as_str), Some("--"));
        let bad = AgentChannel {
            socket: "unix:/aq/channel/aq.sock",
            bin_dir: "relative/bin",
        };
        assert!(map_docker_local(selected(&harness, &model), paths(), Some(bad)).is_err());
    }

    /// The login is wiped from the material path before Codex starts, installed
    /// read-only in the attempt's own CODEX_HOME, never put in argv or the
    /// environment, and removed after the run; the prompt is the last argument.
    #[test]
    fn codex_runner_installs_the_login_read_only_and_wipes_the_material_first() {
        let at = |text: &str| CODEX_RUNNER.find(text).unwrap();
        let install = at("cp \"$m\" \"$CODEX_HOME/auth.json\"");
        let readonly = at("chmod 0400 \"$CODEX_HOME/auth.json\"");
        let wipe = at("rm -f \"$m\"");
        let exec = at("\"$exe\" \"$@\" \"$prompt\"");
        let removed = at("rm -f \"$CODEX_HOME/auth.json\"");
        assert!(install < readonly && readonly < wipe && wipe < exec && exec < removed);
        for absent in [
            "tee ",
            "codex.jsonl",
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
}
