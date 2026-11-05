//! Pure Gemini CLI recipe for unattended runs (I2; 0005 §8.3 rows in
//! `docs/implementation/I2.md`).
//!
//! The recipe is metadata only: argv, environment, working directory and the
//! system-settings bytes the private home must hold. It never carries the
//! provider key:
//! - [`GeminiAuth::ApiKeyProxy`]: `GOOGLE_GEMINI_BASE_URL` points at the
//!   runner-held loopback proxy and `GEMINI_API_KEY` holds only that proxy's
//!   attempt-local capability (SBX7);
//! - [`GeminiAuth::Subscription`]: "Login with Google", whose refreshable
//!   `oauth_creds.json` C6b leases into `$GEMINI_CLI_HOME/.gemini` (SBX19).
//!
//! The CLI runs from the bundle's private Node, never the project's: the
//! executable is the bundle `node` and the first argument is the pinned
//! `gemini.js`. Settings go to the *system* settings file, which outranks the
//! user and workspace settings the agent's repository could supply.
//!
//! Only container confinement is offered: Gemini's own `--sandbox` needs
//! docker/podman (or macOS `sandbox-exec`), so there is no native mode.
use crate::native::runner::sandbox::confinement::ConfinementMode;
use anyhow::{Context, Result, ensure};
use aq_session_protocol::{
    CONTAINER_CONFINEMENT_CAPABILITY, HarnessSettings, ModelSettings, NATIVE_CONFINEMENT_CAPABILITY,
};
use std::{
    collections::BTreeMap,
    path::{Component, Path, PathBuf},
};

/// The pinned `@google/gemini-cli` npm version (contains commit 6814e00, the
/// recording format the observation reader targets).
pub const GEMINI_CLI_VERSION: &str = "0.67.0";
/// System settings file inside the private home.
pub const SYSTEM_SETTINGS_FILE: &str = "system-settings.json";
/// Directory under `GEMINI_CLI_HOME` that holds the CLI's state.
pub const STATE_DIR: &str = ".gemini";
/// The file-stored Google login inside [`STATE_DIR`].
pub const OAUTH_FILE: &str = "oauth_creds.json";
/// Prefix of the proxy's attempt-local capability.
pub const CAPABILITY_PREFIX: &str = "aq-gemini-";
/// Variables that would select another credential, endpoint or settings
/// source. Only `GEMINI_API_KEY` (the proxy capability) and
/// `GOOGLE_GEMINI_BASE_URL` (the loopback proxy) are ever set, and only in
/// proxy mode.
pub const FORBIDDEN_ENVIRONMENT: &[&str] = &[
    "GEMINI_API_KEY",
    "GOOGLE_API_KEY",
    "GOOGLE_GEMINI_BASE_URL",
    "GOOGLE_VERTEX_BASE_URL",
    "GOOGLE_GENAI_USE_VERTEXAI",
    "GOOGLE_GENAI_USE_GCA",
    "GOOGLE_CLOUD_ACCESS_TOKEN",
    "GOOGLE_APPLICATION_CREDENTIALS",
    "GEMINI_CLI_USE_COMPUTE_ADC",
    "GEMINI_DEFAULT_AUTH_TYPE",
    "GEMINI_SANDBOX",
    "GEMINI_CLI_CUSTOM_HEADERS",
    "GEMINI_API_KEY_AUTH_MECHANISM",
    "CLOUD_SHELL",
    "ANTHROPIC_API_KEY",
    "OPENAI_API_KEY",
];

/// Lexical recipe paths, never evidence of installation or confinement.
#[derive(Clone, Copy)]
pub struct GeminiPaths<'a> {
    /// The bundle's private Node (`/opt/aq/node/bin/node`).
    pub node: &'a Path,
    /// The pinned CLI entry point (`/opt/aq/harnesses/gemini/bundle/gemini.js`).
    pub script: &'a Path,
    /// The process working directory; Gemini has no `--cwd` flag.
    pub workspace: &'a Path,
    /// Private agent home: both `HOME` and `GEMINI_CLI_HOME`.
    pub private_home: &'a Path,
}

/// How the CLI authenticates; neither variant carries the provider key.
pub enum GeminiAuth<'a> {
    /// Runner-held loopback proxy origin (`http://127.0.0.1:<port>`) and its
    /// attempt-local capability.
    ApiKeyProxy { origin: &'a str, capability: &'a str },
    /// File-stored Google login leased by C6b.
    Subscription,
}

pub struct GeminiRecipe {
    pub executable: PathBuf,
    pub arguments: Vec<String>,
    pub environment: BTreeMap<String, String>,
    pub working_directory: PathBuf,
    /// Bytes for `<private_home>/system-settings.json`.
    pub system_settings_json: Vec<u8>,
}

/// Crate-private: callers reach it through `GeminiAgentHome::install`, which
/// requires an `AuthorizedConfinement`. The prompt is delivered on stdin.
pub(crate) fn map(
    harness: &HarnessSettings,
    model: &ModelSettings,
    paths: GeminiPaths<'_>,
    mode: ConfinementMode,
    auth: GeminiAuth<'_>,
) -> Result<GeminiRecipe> {
    ensure!(
        harness.harness.as_str() == "gemini"
            && harness.cli_version.as_str() == GEMINI_CLI_VERSION
            && model.provider.as_str() == "google"
            && model.spending_budget.is_none(),
        "Gemini recipe unsupported settings"
    );
    let declares = |id: &str| {
        harness
            .required_capabilities
            .iter()
            .any(|c| c.id.as_str() == id && c.version == 1)
    };
    ensure!(
        mode == ConfinementMode::Container
            && declares(CONTAINER_CONFINEMENT_CAPABILITY)
            && !declares(NATIVE_CONFINEMENT_CAPABILITY),
        "Gemini runs only under declared container confinement"
    );
    let model_id = model.model.as_str();
    ensure!(
        !model_id.is_empty()
            && model_id
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b"-._".contains(&b)),
        "Gemini model id"
    );
    let GeminiPaths {
        node,
        script,
        workspace,
        private_home,
    } = paths;
    for path in [node, script, workspace, private_home] {
        ensure!(
            path.is_absolute()
                && path
                    .components()
                    .all(|c| matches!(c, Component::RootDir | Component::Normal(_)))
                && !path.as_os_str().as_encoded_bytes().contains(&0),
            "Gemini recipe requires absolute normalized paths"
        );
    }
    ensure!(
        !private_home.starts_with(workspace) && !workspace.starts_with(private_home),
        "Gemini private home must be disjoint from the workspace"
    );
    let home = private_home.to_str().context("Gemini home UTF8")?;
    let system_settings = private_home.join(SYSTEM_SETTINGS_FILE);
    let arguments = vec![
        script.to_str().context("Gemini script UTF8")?.to_owned(),
        // Headless; the prompt text arrives on stdin and is appended.
        "-p".into(),
        String::new(),
        "--output-format".into(),
        "stream-json".into(),
        // Container confinement is the boundary, so tools run unprompted.
        "--approval-mode".into(),
        "yolo".into(),
        // An explicit model skips the routing classifier's extra request.
        "--model".into(),
        model_id.to_owned(),
        "--skip-trust".into(),
    ];
    let mut environment: BTreeMap<String, String> = [
        ("HOME", home),
        ("GEMINI_CLI_HOME", home),
        (
            "GEMINI_CLI_SYSTEM_SETTINGS_PATH",
            system_settings.to_str().context("Gemini settings UTF8")?,
        ),
        ("GEMINI_CLI_TRUST_WORKSPACE", "true"),
        // One process with the given argv; no self-relaunch.
        ("GEMINI_CLI_NO_RELAUNCH", "true"),
        // The keychain is absent; the login stays a plain file (SBX19).
        ("GEMINI_FORCE_FILE_STORAGE", "true"),
        ("PATH", "/usr/local/bin:/usr/bin:/bin"),
    ]
    .into_iter()
    .map(|(k, v)| (k.to_owned(), v.to_owned()))
    .collect();
    let auth_type = match auth {
        GeminiAuth::ApiKeyProxy { origin, capability } => {
            let url = reqwest::Url::parse(origin).context("Gemini proxy origin")?;
            ensure!(
                url.scheme() == "http"
                    && url.host_str() == Some("127.0.0.1")
                    && url.port().is_some()
                    && url.path() == "/"
                    && url.username().is_empty()
                    && url.password().is_none()
                    && url.query().is_none()
                    && origin == format!("http://127.0.0.1:{}", url.port().unwrap_or(0)),
                "Gemini proxy origin must be numeric loopback"
            );
            ensure!(
                is_capability(capability),
                "Gemini home accepts only the proxy's local capability"
            );
            environment.insert("GOOGLE_GEMINI_BASE_URL".into(), origin.to_owned());
            environment.insert("GEMINI_API_KEY".into(), capability.to_owned());
            "gemini-api-key"
        }
        GeminiAuth::Subscription => "oauth-personal",
    };
    Ok(GeminiRecipe {
        executable: node.to_owned(),
        arguments,
        environment,
        working_directory: workspace.to_owned(),
        system_settings_json: serde_json::to_vec_pretty(&system_settings_json(auth_type))?,
    })
}

/// `aq-gemini-` followed by 64 lowercase hex digits.
pub fn is_capability(value: &str) -> bool {
    value.len() == CAPABILITY_PREFIX.len() + 64
        && value.starts_with(CAPABILITY_PREFIX)
        && value[CAPABILITY_PREFIX.len()..]
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

fn system_settings_json(auth_type: &str) -> serde_json::Value {
    serde_json::json!({
        "security": {
            "auth": { "selectedType": auth_type, "enforcedType": auth_type },
            "folderTrust": { "enabled": false },
        },
        "general": {
            "enableAutoUpdate": false,
            "enableAutoUpdateNotification": false,
            "checkpointing": { "enabled": false },
        },
        "privacy": { "usageStatisticsEnabled": false },
        "telemetry": { "enabled": false },
        "hooksConfig": { "enabled": false },
        "skills": { "enabled": false },
        "tools": { "sandbox": false },
        // Admin settings bind only from the system scope, so the workspace
        // cannot re-enable MCP servers, extensions or skills.
        "admin": {
            "mcp": { "enabled": false },
            "extensions": { "enabled": false },
            "skills": { "enabled": false },
        },
    })
}

#[cfg(test)]
mod tests;
