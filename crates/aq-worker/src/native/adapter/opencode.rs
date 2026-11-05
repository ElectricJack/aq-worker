//! Pure OpenCode recipe for unattended sandbox runs (I3; 0005 §8.3).
//!
//! The recipe is metadata only: argv, environment, working directory and the
//! `opencode.json` bytes the private home must hold. It grants no execution and
//! carries no credential.
//!
//! - Headless: `opencode run --format json --pure`, prompt on stdin (read when
//!   stdin is not a TTY), events as JSON lines on stdout.
//! - Config: only `OPENCODE_CONFIG` in the private home; project config, Claude
//!   Code compatibility files, external skills, external plugins (`--pure`),
//!   autoupdate, sharing and the models.dev fetch are all disabled. OpenCode's
//!   built-in plugins are also disabled on the proxy route; the subscription
//!   route keeps them because its ChatGPT login is one. OpenCode's background
//!   `npm install` of its plugin SDK runs with `npm_config_offline`, so it
//!   fails locally (a logged warning) instead of reaching the npm registry. `HOME` and every `XDG_*` directory
//!   sit inside the private home, so the credential file
//!   (`$XDG_DATA_HOME/opencode/auth.json`) and session store stay private.
//! - Provider: one per route, and the only one enabled.
//!   [`OpenCodeRoute::ResponsesProxy`] points the `openai` provider's `baseURL`
//!   at the runner's loopback Responses proxy and the credential file holds
//!   only the proxy's attempt-local capability (SBX7).
//!   [`OpenCodeRoute::Subscription`] keeps the stock ChatGPT endpoint and the
//!   credential file is the leased ChatGPT login (C6b).
//!   [`OpenCodeRoute::Ollama`] is a local Ollama server's OpenAI-compatible
//!   chat endpoint (`ollama` provider, OpenCode's bundled
//!   `@ai-sdk/openai-compatible`), with no credential file at all.
//! - Requests: the output cap (`OPENCODE_EXPERIMENTAL_OUTPUT_TOKEN_MAX`) and,
//!   on the `openai` routes, the model's `reasoningEffort` come from
//!   [`ModelSettings`] (Ollama keeps the model's own thinking default), and
//!   title generation (a second, temperature-bearing request) and automatic
//!   compaction are off, so every request is the selected turn the B6b
//!   policy admits.
//! - Permissions: every tool is allowed without prompting, except leaving the
//!   workspace (`external_directory`), asking the operator (`question`) and
//!   the web tools, whose egress would bypass the proxy.
//! - Confinement: OpenCode has no OS sandbox of its own, so only `container`
//!   mode is qualified; `native` is refused.
use crate::native::runner::sandbox::confinement::ConfinementMode;
use anyhow::{Context, Result, ensure};
use aq_session_protocol::{
    CONTAINER_CONFINEMENT_CAPABILITY, HarnessSettings, ModelSettings, NATIVE_CONFINEMENT_CAPABILITY,
};
use std::{
    collections::BTreeMap,
    net::Ipv4Addr,
    path::{Component, Path, PathBuf},
};

/// The OpenCode CLI version this recipe was qualified against.
pub const OPENCODE_CLI_VERSION: &str = "1.18.35";
/// Config file inside the private home.
pub const CONFIG_FILE: &str = "opencode.json";
/// Credential file, relative to the private home (`$XDG_DATA_HOME/opencode`).
pub const AUTH_FILE: &str = "data/opencode/auth.json";
/// Variables that would bypass the private config or credential file, or add
/// another credential. None may appear in an OpenCode recipe's environment.
pub const FORBIDDEN_ENVIRONMENT: &[&str] = &[
    "OPENCODE_AUTH_CONTENT",
    "OPENCODE_CONFIG_CONTENT",
    "OPENCODE_CONFIG_DIR",
    "OPENCODE_PERMISSION",
    "OPENCODE_API_KEY",
    "OPENCODE_CONSOLE_TOKEN",
    "OPENCODE_SERVER_PASSWORD",
    "OPENAI_API_KEY",
    "OPENAI_BASE_URL",
    "ANTHROPIC_API_KEY",
    "CODEX_API_KEY",
];

/// How the `openai` provider reaches the model.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum OpenCodeRoute {
    /// The runner's loopback Responses proxy on `127.0.0.1:<port>`.
    ResponsesProxy { port: u16 },
    /// A leased ChatGPT subscription login at the stock endpoint.
    Subscription,
    /// A local Ollama server at `http://<host>:<port>/v1`, with no credential.
    /// `host` is loopback or link-local (a container's host gateway, such as
    /// pasta's `169.254.1.2`), never a name or a routable address.
    Ollama { host: Ipv4Addr, port: u16 },
}

impl OpenCodeRoute {
    /// The OpenCode provider id this route enables.
    pub fn provider(self) -> &'static str {
        match self {
            Self::ResponsesProxy { .. } | Self::Subscription => "openai",
            Self::Ollama { .. } => "ollama",
        }
    }
}

/// Lexical recipe paths, never evidence of installation or confinement.
#[derive(Clone, Copy)]
pub struct OpenCodePaths<'a> {
    pub executable: &'a Path,
    pub workspace: &'a Path,
    /// Private agent home: `HOME`, with every `XDG_*` directory beneath it.
    pub private_home: &'a Path,
}

pub struct OpenCodeRecipe {
    pub executable: PathBuf,
    pub arguments: Vec<String>,
    pub environment: BTreeMap<String, String>,
    pub working_directory: PathBuf,
    /// Bytes for `<private_home>/opencode.json`.
    pub config_json: Vec<u8>,
}

/// Crate-private: callers reach it only through `OpenCodeAgentHome::install`,
/// which requires an `AuthorizedConfinement`.
pub(crate) fn map(
    harness: &HarnessSettings,
    model: &ModelSettings,
    paths: OpenCodePaths<'_>,
    mode: ConfinementMode,
    route: OpenCodeRoute,
) -> Result<OpenCodeRecipe> {
    ensure!(
        model.max_output_tokens > 0,
        "OpenCode recipe requires an output cap"
    );
    ensure!(
        harness.harness.as_str() == "opencode"
            && harness.cli_version.as_str() == OPENCODE_CLI_VERSION
            && model.provider.as_str() == route.provider()
            && model.spending_budget.is_none(),
        "OpenCode recipe unsupported settings"
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
        "OpenCode is qualified only for declared container confinement"
    );
    match route {
        OpenCodeRoute::ResponsesProxy { port } => {
            ensure!(port > 0, "OpenCode proxy route requires a nonzero port")
        }
        OpenCodeRoute::Ollama { host, port } => ensure!(
            port > 0 && (host.is_loopback() || host.is_link_local()),
            "OpenCode Ollama route requires a loopback or link-local origin"
        ),
        OpenCodeRoute::Subscription => {}
    }
    let OpenCodePaths {
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
            "OpenCode recipe requires absolute normalized paths"
        );
    }
    ensure!(
        !private_home.starts_with(workspace) && !workspace.starts_with(private_home),
        "OpenCode private home must be disjoint from the workspace"
    );
    let home = private_home.to_str().context("OpenCode home UTF8")?;
    let model_ref = format!("{}/{}", route.provider(), model.model.as_str());
    let arguments = vec![
        "run".to_owned(),
        "--format".to_owned(),
        "json".to_owned(),
        "--pure".to_owned(),
        "--model".to_owned(),
        model_ref.clone(),
    ];
    let sub = |name: &str| format!("{home}/{name}");
    let mut environment: BTreeMap<String, String> = [
        ("HOME", home.to_owned()),
        ("XDG_CONFIG_HOME", sub("config")),
        ("XDG_DATA_HOME", sub("data")),
        ("XDG_CACHE_HOME", sub("cache")),
        ("XDG_STATE_HOME", sub("state")),
        ("OPENCODE_CONFIG", sub(CONFIG_FILE)),
        ("PATH", "/usr/local/bin:/usr/bin:/bin".to_owned()),
        ("OPENCODE_DISABLE_PROJECT_CONFIG", "1".to_owned()),
        ("OPENCODE_DISABLE_AUTOUPDATE", "1".to_owned()),
        ("OPENCODE_DISABLE_MODELS_FETCH", "1".to_owned()),
        ("OPENCODE_DISABLE_LSP_DOWNLOAD", "1".to_owned()),
        ("OPENCODE_DISABLE_SHARE", "1".to_owned()),
        ("OPENCODE_DISABLE_CLAUDE_CODE", "1".to_owned()),
        ("OPENCODE_DISABLE_EXTERNAL_SKILLS", "1".to_owned()),
        ("npm_config_offline", "true".to_owned()),
        (
            "OPENCODE_EXPERIMENTAL_OUTPUT_TOKEN_MAX",
            model.max_output_tokens.to_string(),
        ),
    ]
    .into_iter()
    .map(|(k, v)| (k.to_owned(), v))
    .collect();
    if route != OpenCodeRoute::Subscription {
        // The subscription route needs the internal ChatGPT auth plugin; the
        // other routes need no plugin at all.
        environment.insert(
            "OPENCODE_DISABLE_DEFAULT_PLUGINS".to_owned(),
            "1".to_owned(),
        );
    }
    Ok(OpenCodeRecipe {
        executable: executable.to_owned(),
        arguments,
        environment,
        working_directory: workspace.to_owned(),
        config_json: serde_json::to_vec_pretty(&config(model, &model_ref, route))?,
    })
}

fn config(settings: &ModelSettings, model: &str, route: OpenCodeRoute) -> serde_json::Value {
    let mut provider = serde_json::json!({ "openai": { "models": {
        settings.model.as_str(): {
            "options": { "reasoningEffort": settings.reasoning.as_str() }
        }
    } } });
    match route {
        OpenCodeRoute::ResponsesProxy { port } => {
            provider["openai"]["options"] =
                serde_json::json!({ "baseURL": format!("http://127.0.0.1:{port}/v1") });
        }
        OpenCodeRoute::Ollama { host, port } => {
            provider = serde_json::json!({ "ollama": {
                "npm": "@ai-sdk/openai-compatible",
                "name": "Ollama",
                "options": { "baseURL": format!("http://{host}:{port}/v1") },
                "models": { settings.model.as_str(): {} },
            } });
        }
        OpenCodeRoute::Subscription => {}
    }
    serde_json::json!({
        "$schema": "https://opencode.ai/config.json",
        "model": model,
        "small_model": model,
        "enabled_providers": [route.provider()],
        "provider": provider,
        "autoupdate": false,
        "share": "disabled",
        "snapshot": false,
        "agent": { "title": { "disable": true } },
        "compaction": { "auto": false, "prune": false },
        "permission": {
            "*": "allow",
            "external_directory": "deny",
            "question": "deny",
            "webfetch": "deny",
            "websearch": "deny",
        },
    })
}

/// Largest leased OpenCode login accepted (C5b's material bound).
pub const MAX_AUTH_BYTES: usize = 64 * 1024;

/// Validate an OpenCode ChatGPT subscription `auth.json`: exactly one
/// `openai` entry of type `oauth` with non-empty `refresh` and `access` tokens
/// and a positive `expires` (epoch milliseconds), which is returned. Any other
/// provider entry, an API key or an unknown field is refused, so a leased login
/// can never carry a second credential into the sandbox.
pub fn validate_chatgpt_auth_json(bytes: &[u8]) -> Result<i64> {
    use serde::Deserialize;
    use zeroize::Zeroizing;
    fn secret<'de, D: serde::Deserializer<'de>>(
        d: D,
    ) -> std::result::Result<Zeroizing<String>, D::Error> {
        String::deserialize(d).map(Zeroizing::new)
    }
    #[derive(Deserialize)]
    #[serde(deny_unknown_fields)]
    struct OAuth {
        r#type: String,
        #[serde(deserialize_with = "secret")]
        refresh: Zeroizing<String>,
        #[serde(deserialize_with = "secret")]
        access: Zeroizing<String>,
        expires: i64,
        #[serde(rename = "accountId")]
        account_id: Option<String>,
        #[serde(rename = "enterpriseUrl")]
        enterprise_url: Option<()>,
    }
    #[derive(Deserialize)]
    #[serde(deny_unknown_fields)]
    struct Auth {
        openai: OAuth,
    }
    ensure!(
        !bytes.is_empty() && bytes.len() <= MAX_AUTH_BYTES,
        "OpenCode login JSON byte bound"
    );
    let auth: Auth = serde_json::from_slice(bytes)
        .map_err(|_| anyhow::anyhow!("unsupported OpenCode login JSON"))?;
    let o = auth.openai;
    let token =
        |t: &str| !t.is_empty() && t.len() <= 16 * 1024 && t.bytes().all(|b| b.is_ascii_graphic());
    ensure!(
        o.r#type == "oauth"
            && token(&o.refresh)
            && token(&o.access)
            && o.expires > 0
            && o.enterprise_url.is_none()
            && o.account_id.as_deref().is_none_or(token),
        "OpenCode ChatGPT subscription login required"
    );
    Ok(o.expires)
}

#[cfg(test)]
mod tests;
