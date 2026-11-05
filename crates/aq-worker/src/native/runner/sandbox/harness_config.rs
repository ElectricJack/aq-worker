//! Per-attempt private harness homes inside the sandbox (C6a; 0005 §8.3).
//!
//! Each builder creates a fresh 0700 home, writes owner-only files and returns the
//! launch recipe for an [`AuthorizedConfinement`]. Neither home ever receives a
//! provider key:
//! - Codex gets a private `CODEX_HOME` with `cli_auth_credentials_store="file"`
//!   and a custom `model_providers.aq-runner-proxy` (wire_api `responses`) at
//!   the runner's loopback Responses proxy. Its `auth.json` holds only the
//!   proxy's attempt-local capability; the provider key stays in the runner's
//!   private file.
//! - Claude gets a private `HOME`/`CLAUDE_CONFIG_DIR` holding `settings.json`.
//!   It logs in by subscription only, so the home holds no key or token here.
//!
//! - Gemini gets a private `GEMINI_CLI_HOME` holding the system-scope
//!   `system-settings.json` (it outranks workspace settings). With the
//!   runner's loopback Gemini proxy it carries only the proxy capability, in
//!   the spawn environment; on subscription the C6b login installs
//!   `.gemini/oauth_creds.json` after this home exists.
//!
//! These are library APIs; B6a2 owns wiring them into `run_as_agent`.
use super::confinement::{AuthorizedConfinement, ConfinementMode};
use crate::native::adapter::claude::{self, ClaudePaths, ClaudeRecipe};
use crate::native::adapter::gemini::{self, GeminiAuth, GeminiPaths, GeminiRecipe};
use crate::native::adapter::opencode::{self, OpenCodePaths, OpenCodeRecipe, OpenCodeRoute};
use anyhow::{Context, Result, ensure};
use aq_codex_mapper::{LoopbackResponsesEndpoint, Paths, Recipe, Settings};
use aq_session_protocol::{HarnessSettings, ModelSettings};
use std::{
    fs::{self, DirBuilder, OpenOptions},
    io::Write,
    os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt},
    path::{Path, PathBuf},
};
use zeroize::Zeroizing;

/// Create a fresh owner-only directory; an existing path is refused.
pub(super) fn create_private_home(path: &Path) -> Result<PathBuf> {
    DirBuilder::new()
        .mode(0o700)
        .create(path)
        .with_context(|| format!("create private harness home {}", path.display()))?;
    let meta = fs::symlink_metadata(path)?;
    ensure!(
        meta.is_dir() && meta.mode() & 0o777 == 0o700 && meta.uid() == unsafe { libc::geteuid() },
        "private harness home is not an owner-only directory"
    );
    Ok(path.to_owned())
}

pub(super) fn write_private(home: &Path, name: &str, bytes: &[u8]) -> Result<()> {
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW)
        .open(home.join(name))?;
    file.write_all(bytes)?;
    file.sync_all()?;
    Ok(())
}

fn proxy_capability(capability: &[u8]) -> bool {
    capability.len() == 77
        && capability.starts_with(b"aq-responses-")
        && capability[13..]
            .iter()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(b))
}

pub struct CodexAgentHome;
impl CodexAgentHome {
    /// `capability` is the proxy's attempt-local bearer
    /// (`RunnerResponsesProxy::local_capability`), never the provider key.
    pub fn install(
        confinement: &AuthorizedConfinement,
        settings: Settings<'_>,
        paths: Paths<'_>,
        proxy: LoopbackResponsesEndpoint,
        capability: &[u8],
    ) -> Result<Recipe> {
        ensure!(
            proxy_capability(capability),
            "Codex home accepts only the proxy's local capability"
        );
        let home = paths.private_home;
        // Native workspace-write would let the agent rewrite its own CODEX_HOME.
        ensure!(
            !home.starts_with(paths.workspace) && !paths.workspace.starts_with(home),
            "Codex private home must be disjoint from the workspace"
        );
        let mut recipe = match confinement.mode() {
            ConfinementMode::Native => aq_codex_mapper::map(settings, paths)?,
            ConfinementMode::Container => aq_codex_mapper::map_container(settings, paths)?,
        };
        // Insert before the trailing stdin prompt marker `-`.
        ensure!(
            recipe.arguments.last().map(String::as_str) == Some("-"),
            "Codex exec recipe must end with the stdin prompt marker"
        );
        let position = recipe.arguments.len() - 1;
        let overrides: Vec<String> = aq_codex_mapper::proxy_provider_overrides(proxy)
            .into_iter()
            .flat_map(|setting| ["--config".to_owned(), setting])
            .collect();
        recipe.arguments.splice(position..position, overrides);
        let home = create_private_home(home)?;
        let mut auth = Zeroizing::new(Vec::with_capacity(capability.len() + 32));
        auth.extend_from_slice(b"{\"OPENAI_API_KEY\":");
        serde_json::to_writer(&mut *auth, std::str::from_utf8(capability)?)?;
        auth.push(b'}');
        write_private(&home, "auth.json", &auth)?;
        Ok(recipe)
    }
}

pub struct ClaudeAgentHome;
impl ClaudeAgentHome {
    pub fn install(
        confinement: &AuthorizedConfinement,
        harness: &HarnessSettings,
        model: &ModelSettings,
        paths: ClaudePaths<'_>,
    ) -> Result<ClaudeRecipe> {
        let recipe = claude::map(harness, model, paths, confinement.mode())?;
        let home = create_private_home(paths.private_home)?;
        write_private(&home, claude::SETTINGS_FILE, &recipe.settings_json)?;
        Ok(recipe)
    }
}

pub struct OpenCodeAgentHome;
impl OpenCodeAgentHome {
    /// Proxy route: `capability` is the proxy's attempt-local bearer, never the
    /// provider key. Subscription route: `capability` must be `None`; the
    /// caller then passes [`Self::login_dir`] to `SubscriptionLogin::start`.
    /// Ollama route: `capability` must be `None` and no credential file exists.
    pub fn install(
        confinement: &AuthorizedConfinement,
        harness: &HarnessSettings,
        model: &ModelSettings,
        paths: OpenCodePaths<'_>,
        route: OpenCodeRoute,
        capability: Option<&[u8]>,
    ) -> Result<OpenCodeRecipe> {
        match (route, capability) {
            (OpenCodeRoute::ResponsesProxy { .. }, Some(c)) => ensure!(
                proxy_capability(c),
                "OpenCode home accepts only the proxy's local capability"
            ),
            (OpenCodeRoute::Subscription | OpenCodeRoute::Ollama { .. }, None) => {}
            _ => anyhow::bail!("OpenCode route and credential disagree"),
        }
        let recipe = opencode::map(harness, model, paths, confinement.mode(), route)?;
        let home = create_private_home(paths.private_home)?;
        write_private(&home, opencode::CONFIG_FILE, &recipe.config_json)?;
        let data = create_private_home(&home.join("data"))?;
        if let Some(capability) = capability {
            let dir = create_private_home(&data.join("opencode"))?;
            let mut auth = Zeroizing::new(Vec::with_capacity(capability.len() + 40));
            auth.extend_from_slice(b"{\"openai\":{\"type\":\"api\",\"key\":");
            serde_json::to_writer(&mut *auth, std::str::from_utf8(capability)?)?;
            auth.extend_from_slice(b"}}");
            write_private(&dir, "auth.json", &auth)?;
        }
        Ok(recipe)
    }
    /// The `$XDG_DATA_HOME/opencode` directory a subscription login installs
    /// into (it must not exist yet).
    pub fn login_dir(private_home: &Path) -> PathBuf {
        private_home.join("data/opencode")
    }
}

pub struct GeminiAgentHome;
impl GeminiAgentHome {
    /// Creates the home and its system settings; the state directory
    /// `.gemini` is left to the CLI or to the C6b subscription login.
    pub fn install(
        confinement: &AuthorizedConfinement,
        harness: &HarnessSettings,
        model: &ModelSettings,
        paths: GeminiPaths<'_>,
        auth: GeminiAuth<'_>,
    ) -> Result<GeminiRecipe> {
        let recipe = gemini::map(harness, model, paths, confinement.mode(), auth)?;
        let home = create_private_home(paths.private_home)?;
        write_private(
            &home,
            gemini::SYSTEM_SETTINGS_FILE,
            &recipe.system_settings_json,
        )?;
        Ok(recipe)
    }
}
