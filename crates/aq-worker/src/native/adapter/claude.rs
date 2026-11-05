//! Pure Claude Code recipe for unattended runs (C6a; owner decision 2026-11-04).
//!
//! The recipe is metadata only: argv, environment, working directory and the
//! `settings.json` bytes the private home must hold. It grants no execution and
//! carries no credential. Claude runs on a subscription login only, so the
//! environment never holds an API key or token variable; leasing the OAuth
//! credential into the attempt is C6b's job, not this recipe's.
//!
//! The confinement mode comes from the frozen manifest (see
//! `runner::sandbox::confinement`):
//! - `native`: Claude's own sandbox is on and fails closed when it cannot
//!   start, unsandboxed commands are refused and the permission mode is
//!   `acceptEdits`, with bypass mode disabled;
//! - `container`: Claude's sandbox is off and the permission mode is
//!   `bypassPermissions`, because the backend sandbox is the boundary.
use crate::native::runner::sandbox::confinement::ConfinementMode;
use anyhow::{Context, Result, ensure};
use aq_session_protocol::{
    CONTAINER_CONFINEMENT_CAPABILITY, HarnessSettings, ModelSettings, NATIVE_CONFINEMENT_CAPABILITY,
};
use std::{
    collections::BTreeMap,
    path::{Component, Path, PathBuf},
};

/// The Claude Code CLI version this recipe was written against (operator spike).
pub const CLAUDE_CLI_VERSION: &str = "2.1.318";
/// Name of the settings file inside the private home.
pub const SETTINGS_FILE: &str = "settings.json";
/// Variables that would replace the subscription login with another credential.
/// None of them may ever appear in a Claude recipe's environment.
pub const FORBIDDEN_ENVIRONMENT: &[&str] = &[
    "ANTHROPIC_API_KEY",
    "ANTHROPIC_AUTH_TOKEN",
    "CLAUDE_CODE_OAUTH_TOKEN",
    "CLAUDE_CODE_USE_BEDROCK",
    "CLAUDE_CODE_USE_VERTEX",
    "CLAUDE_CODE_USE_FOUNDRY",
    // A redirected base URL would send the subscription login to another host.
    "ANTHROPIC_BASE_URL",
    "OPENAI_API_KEY",
    "CODEX_API_KEY",
];

/// Lexical recipe paths, never evidence of installation or confinement.
#[derive(Clone, Copy)]
pub struct ClaudePaths<'a> {
    pub executable: &'a Path,
    /// The process working directory; Claude has no `--cwd` flag.
    pub workspace: &'a Path,
    /// Private agent home: both `HOME` and `CLAUDE_CONFIG_DIR`.
    pub private_home: &'a Path,
}

pub struct ClaudeRecipe {
    pub executable: PathBuf,
    pub arguments: Vec<String>,
    pub environment: BTreeMap<String, String>,
    pub working_directory: PathBuf,
    /// Bytes for `<private_home>/settings.json`.
    pub settings_json: Vec<u8>,
}

/// Crate-private: callers outside the crate reach it only through
/// `ClaudeAgentHome::install`, which requires an `AuthorizedConfinement`.
pub(crate) fn map(
    harness: &HarnessSettings,
    model: &ModelSettings,
    paths: ClaudePaths<'_>,
    mode: ConfinementMode,
) -> Result<ClaudeRecipe> {
    ensure!(
        harness.harness.as_str() == "claude"
            && harness.cli_version.as_str() == CLAUDE_CLI_VERSION
            && model.provider.as_str() == "anthropic"
            && model.spending_budget.is_none(),
        "Claude recipe unsupported settings"
    );
    let declares = |id: &str| {
        harness
            .required_capabilities
            .iter()
            .any(|c| c.id.as_str() == id && c.version == 1)
    };
    let declared = (
        declares(NATIVE_CONFINEMENT_CAPABILITY),
        declares(CONTAINER_CONFINEMENT_CAPABILITY),
    );
    ensure!(
        declared
            == match mode {
                ConfinementMode::Native => (true, false),
                ConfinementMode::Container => (false, true),
            },
        "Claude confinement mode does not match the declared manifest mode"
    );
    let ClaudePaths {
        executable,
        workspace,
        private_home,
    } = paths;
    for path in [executable, workspace, private_home] {
        ensure!(
            path.is_absolute()
                && path
                    .components()
                    .all(|c| matches!(c, Component::RootDir | Component::Normal(_)))
                && !path.as_os_str().as_encoded_bytes().contains(&0),
            "Claude recipe requires absolute normalized paths"
        );
    }
    ensure!(
        !private_home.starts_with(workspace) && !workspace.starts_with(private_home),
        "Claude private home must be disjoint from the workspace"
    );
    let home = private_home.to_str().context("Claude home UTF8")?;
    let settings_path = private_home.join(SETTINGS_FILE);
    let permission_mode = match mode {
        ConfinementMode::Native => "acceptEdits",
        ConfinementMode::Container => "bypassPermissions",
    };
    let arguments = [
        "-p",
        "--output-format",
        "stream-json",
        "--verbose",
        "--input-format",
        "text",
        "--model",
        model.model.as_str(),
        "--permission-mode",
        permission_mode,
        // Only the private user settings; never the workspace's project or
        // local settings, which the agent's repository content controls.
        "--setting-sources",
        "user",
        "--settings",
        settings_path.to_str().context("Claude settings UTF8")?,
        "--strict-mcp-config",
        "--no-session-persistence",
        "--disable-slash-commands",
    ]
    .map(String::from)
    .to_vec();
    let environment = [
        ("HOME", home),
        ("CLAUDE_CONFIG_DIR", home),
        ("PATH", "/usr/local/bin:/usr/bin:/bin"),
        ("DISABLE_AUTOUPDATER", "1"),
        ("DISABLE_TELEMETRY", "1"),
        ("DISABLE_ERROR_REPORTING", "1"),
        ("CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC", "1"),
        ("CLAUDE_CODE_DISABLE_AUTO_MEMORY", "1"),
    ]
    .into_iter()
    .map(|(k, v)| (k.to_owned(), v.to_owned()))
    .collect();
    Ok(ClaudeRecipe {
        executable: executable.to_owned(),
        arguments,
        environment,
        working_directory: workspace.to_owned(),
        settings_json: serde_json::to_vec_pretty(&settings(mode))?,
    })
}

fn settings(mode: ConfinementMode) -> serde_json::Value {
    let mut permissions = serde_json::json!({ "defaultMode": "acceptEdits" });
    let sandbox = match mode {
        ConfinementMode::Native => {
            permissions["disableBypassPermissionsMode"] = "disable".into();
            serde_json::json!({
                "enabled": true,
                "autoAllowBashIfSandboxed": true,
                "allowUnsandboxedCommands": false,
                // Claude's sandbox is fail-open by default: if it cannot start
                // it warns and runs commands unsandboxed (calm-delta-18.9.4).
                "failIfUnavailable": true,
            })
        }
        ConfinementMode::Container => {
            permissions["defaultMode"] = "bypassPermissions".into();
            serde_json::json!({ "enabled": false })
        }
    };
    serde_json::json!({
        "permissions": permissions,
        "sandbox": sandbox,
        "disableAllHooks": true,
        "enableAllProjectMcpServers": false,
        "cleanupPeriodDays": 1,
        "forceLoginMethod": "claudeai",
    })
}

#[cfg(test)]
mod tests;
