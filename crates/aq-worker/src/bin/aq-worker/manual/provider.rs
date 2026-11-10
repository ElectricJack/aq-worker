//! Provider-specific bare interactive startup inside the existing sandbox boundary.
use super::{TreeEntry, file};
use anyhow::{Context, Result, ensure};
use aq_node_service::config::ServiceConfig;
use aq_worker::native::runner::ClaudeDockerSettings;
use serde_json::Value;
use std::path::PathBuf;
const CLAUDE: &str = r#"set -euo pipefail
umask 077
m=/aq/private/material
[ -s "$m" ] || { echo 'Subscription login is unavailable'; exit 90; }
token=$(cat "$m"); rm -f "$m"
export HOME=/aq/home TERM=xterm-256color IS_SANDBOX=1
mkdir -p "$HOME/.claude"
printf '%s' '{"hasCompletedOnboarding":true,"projects":{"/aq/work":{"hasTrustDialogAccepted":true}}}' > "$HOME/.claude.json"
printf '%s' '{"theme":"dark"}' > "$HOME/.claude/settings.json"
cd /aq/work
exe=$(cat /aq/in/executable); model=$(cat /aq/in/model); effort=$(cat /aq/in/effort)
args=(--model "$model" --permission-mode acceptEdits --strict-mcp-config --setting-sources user --append-system-prompt-file /aq/in/context)
if [ "$effort" = off ]; then export MAX_THINKING_TOKENS=0; elif [ "$effort" != default ]; then args+=(--effort "$effort"); fi
CLAUDE_CODE_OAUTH_TOKEN="$token" exec "$exe" "${args[@]}"
"#;
const CODEX: &str = r#"set -euo pipefail
umask 077
export HOME=/aq/home CODEX_HOME=/aq/home/.codex TERM=xterm-256color
mkdir -p "$CODEX_HOME"
[ -s /aq/private/material ] || { echo 'ChatGPT subscription login is unavailable'; exit 90; }
cat /aq/private/material > "$CODEX_HOME/auth.json"
trap 'rm -f "$CODEX_HOME/auth.json"' EXIT
rm -f /aq/private/material
cat /aq/in/codex-config > "$CODEX_HOME/config.toml"
cd /aq/work
exe=$(cat /aq/in/executable)
if [ -d /aq/in/codex-runtime ]; then
  actual=$(sha256sum "$exe"); actual=${actual%% *}
  [ "$actual" = "$(cat /aq/in/codex-digest)" ] || { echo 'Installed Codex does not match its pinned helpers'; exit 91; }
  runtime="$HOME/.codex-runtime"
  mkdir -p "$runtime"
  cp -L "$exe" "$runtime/codex"
  cp -R /aq/in/codex-runtime/. "$runtime/"
  chmod 700 "$runtime/codex" "$runtime/codex-code-mode-host" "$runtime/codex-path/rg" "$runtime/codex-resources/bwrap"
  exe="$runtime/codex"
  export PATH="$runtime/codex-path:$PATH"
fi
model=$(cat /aq/in/model); effort=$(cat /aq/in/effort)
"$exe" --no-daemon --no-alt-screen --sandbox danger-full-access --ask-for-approval never --model "$model" -c "model_reasoning_effort=\"$effort\"" -c 'model_provider="openai"' -c 'forced_login_method="chatgpt"' 
"#;
pub(super) struct Plan {
    pub entries: Vec<TreeEntry>,
    pub secret: PathBuf,
}
pub(super) fn validate_material(spec: &Value, bytes: &[u8]) -> Result<()> {
    if spec["provider"] == "openai" {
        aq_session_host::PrivateLaunchInput::validate_chatgpt_auth_json(bytes)
    } else {
        ensure!(
            !bytes.is_empty()
                && bytes.len() <= 65536
                && std::str::from_utf8(bytes).is_ok_and(
                    |s| !s.trim().is_empty() && s.trim().bytes().all(|b| b.is_ascii_graphic())
                ),
            "subscription login is unavailable"
        );
        Ok(())
    }
}
pub(super) fn available(config: &ServiceConfig) -> Vec<&'static str> {
    let mut out = Vec::new();
    if config
        .measurement
        .token_file
        .as_ref()
        .is_some_and(|p| aq_handoff_note::read_private(p, 65536).is_ok())
    {
        out.push("anthropic");
    }
    if config
        .manual
        .as_ref()
        .and_then(|m| m.codex.as_ref())
        .is_some_and(|c| {
            aq_handoff_note::read_private(&c.auth_file, 65536).is_ok_and(|b| {
                validate_material(&serde_json::json!({"provider":"openai"}), &b).is_ok()
            })
        })
    {
        out.push("openai");
    }
    out
}
pub(super) fn plan(
    config: &ServiceConfig,
    settings: &ClaudeDockerSettings,
    spec: &Value,
) -> Result<Plan> {
    let model = spec["model"].as_str().context("interactive model")?;
    ensure!(
        !model.is_empty()
            && model.len() <= 128
            && !model.starts_with('-')
            && model
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b"-._:/".contains(&b)),
        "invalid interactive model"
    );
    let effort = spec["reasoning"].as_str().unwrap_or("default");
    let (exe, secret, script) = match spec["provider"].as_str() {
        Some("anthropic") => {
            ensure!(
                ["default", "off", "low", "medium", "high", "xhigh", "max"].contains(&effort),
                "unsupported Claude effort"
            );
            (
                settings.host.claude_executable.clone(),
                config
                    .measurement
                    .token_file
                    .clone()
                    .context("Claude subscription file")?,
                CLAUDE,
            )
        }
        Some("openai") => {
            ensure!(
                [
                    "none", "minimal", "low", "medium", "high", "xhigh", "max", "ultra"
                ]
                .contains(&effort),
                "unsupported Codex effort"
            );
            let c = config
                .manual
                .as_ref()
                .and_then(|m| m.codex.as_ref())
                .context("Codex is not installed for manual launch")?;
            (c.executable.clone(), c.auth_file.clone(), CODEX)
        }
        _ => anyhow::bail!("interactive provider is not installed"),
    };
    let mut entries = vec![
        file("model", model.as_bytes()),
        file("effort", effort.as_bytes()),
        file("executable", exe.to_string_lossy().as_bytes()),
        file("agent.sh", script),
    ];
    if spec["provider"] == "openai" {
        entries.push(file("codex-config", codex_config(spec)?));
        if let Some(bundle) = config
            .manual
            .as_ref()
            .and_then(|m| m.codex.as_ref())
            .and_then(|c| c.runtime_bundle.as_ref())
        {
            entries.extend(runtime_bundle(bundle)?);
        }
    }
    Ok(Plan { entries, secret })
}
/// Only these public, exact-version runtime artifacts may enter a session.
fn runtime_bundle(path: &std::path::Path) -> Result<Vec<TreeEntry>> {
    let pins: Value = serde_json::from_str(include_str!(
        "../../../../../../packaging/native-runtime/pins.json"
    ))?;
    let mut entries = vec![file(
        "codex-digest",
        pins["inputs"]["codex"]["sha256"]
            .as_str()
            .context("Codex executable pin")?,
    )];
    for name in [
        "codex-runtime",
        "codex-runtime/codex-path",
        "codex-runtime/codex-resources",
    ] {
        entries.push(TreeEntry::Dir {
            path: name.into(),
            mode: 0o700,
        });
    }
    for name in [
        "codex-code-mode-host",
        "codex-path/rg",
        "codex-resources/bwrap",
    ] {
        let pin = &pins["inputs"][name];
        let expected = pin["bytes"].as_u64().context("Codex helper size")?;
        let file = std::fs::File::open(path.join(name)).context("Codex helper unavailable")?;
        ensure!(
            file.metadata()?.is_file() && file.metadata()?.len() == expected,
            "Codex helper size mismatch"
        );
        let mut bytes = Vec::new();
        use std::io::Read;
        file.take(expected + 1).read_to_end(&mut bytes)?;
        ensure!(
            bytes.len() as u64 == expected
                && aq_session_protocol::Digest::of(&bytes).as_str()
                    == pin["sha256"].as_str().context("Codex helper digest")?,
            "Codex helper digest mismatch"
        );
        entries.push(super::file(&format!("codex-runtime/{name}"), bytes));
    }
    Ok(entries)
}

fn codex_config(spec: &Value) -> Result<String> {
    // JSON strings are valid TOML basic strings; only these known scalar options
    // cross into CLI configuration. Never copy a host config or arbitrary options.
    let quote = |v: &str| serde_json::to_string(v).expect("string serializes");
    let model = spec["model"].as_str().context("model missing")?;
    let effort = spec["reasoning"].as_str().context("effort missing")?;
    let context = "You are an interactive coding agent in an isolated project workspace. Wait for the owner's instructions. Read AGENTS.md and .aq/PRIME.md before project work. Do not change host services, credentials, pools or other projects. This session stays open until the owner stops it.";
    let mut config = format!(
        "model = {}\nmodel_reasoning_effort = {}\nforced_login_method = \"chatgpt\"\ncli_auth_credentials_store = \"file\"\ndeveloper_instructions = {}\n[projects.\"/aq/work\"]\ntrust_level = \"trusted\"\n",
        quote(model),
        quote(effort),
        quote(context)
    );
    if let Some(tier) = spec["options"]["service_tier"].as_str() {
        ensure!(
            ["fast", "flex", "priority", "default", "auto"].contains(&tier),
            "unsupported service tier"
        );
        config = format!("service_tier = {}\n{config}", quote(tier));
    }
    Ok(config)
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn codex_config_retains_model_effort_without_host_configuration_or_prompt() {
        let spec = serde_json::json!({"model":"gpt-test","reasoning":"high","options":{"service_tier":"fast","model_provider":"hostile"}});
        let config = codex_config(&spec).unwrap();
        let value: toml::Value = toml::from_str(&config).unwrap();
        assert_eq!(value["model"].as_str(), Some("gpt-test"));
        assert_eq!(value["model_reasoning_effort"].as_str(), Some("high"));
        assert_eq!(value["service_tier"].as_str(), Some("fast"));
        assert_eq!(value["forced_login_method"].as_str(), Some("chatgpt"));
        assert!(value.get("model_provider").is_none());
        for script in [CODEX, CLAUDE] {
            assert!(
                std::process::Command::new("bash")
                    .args(["-n", "-c", script])
                    .status()
                    .unwrap()
                    .success()
            );
        }
        assert!(CODEX.contains("--no-daemon --no-alt-screen"));
        assert!(!CODEX.contains("exec \"$exe\" exec"));
    }
    #[test]
    fn runtime_bundle_refuses_unpinned_helper_bytes() {
        let root = tempfile::tempdir().unwrap();
        std::fs::write(root.path().join("codex-code-mode-host"), b"wrong helper").unwrap();
        assert!(runtime_bundle(root.path()).is_err());
    }
    #[test]
    fn rejects_api_key_material_and_unsupported_provider_options() {
        assert!(
            validate_material(
                &serde_json::json!({"provider":"openai"}),
                br#"{"auth_mode":"apikey","OPENAI_API_KEY":"fake"}"#
            )
            .is_err()
        );
        assert!(codex_config(&serde_json::json!({"model":"test","reasoning":"high","options":{"service_tier":"\"\nprovider=evil"}})).is_err());
    }
}
