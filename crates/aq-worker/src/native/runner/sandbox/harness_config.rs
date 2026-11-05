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
//! - Gemini gets an empty private `GEMINI_CLI_HOME`. Its system-scope
//!   `system-settings.json` (it outranks workspace settings) is written as
//!   uid 0 into a root-owned directory outside the home, because Gemini CLI
//!   0.67.0 ignores system settings unless the file and every ancestor are
//!   root-owned and not group/other writable. With the runner's loopback
//!   Gemini proxy it carries only the proxy capability, in the spawn
//!   environment; on subscription the C6b login installs
//!   `.gemini/oauth_creds.json` after this home exists.
//!
//! These are library APIs; B6a2 owns wiring them into `run_as_agent`.
use super::confinement::{AuthorizedConfinement, ConfinementMode};
use crate::native::adapter::claude::{self, ClaudePaths, ClaudeRecipe};
use crate::native::adapter::gemini::{self, GeminiAuth, GeminiPaths, GeminiRecipe};
use anyhow::{Context, Result, ensure};
use aq_codex_mapper::{LoopbackResponsesEndpoint, Paths, Recipe, Settings};
use aq_session_protocol::{HarnessSettings, ModelSettings};
use std::{
    fs::{self, DirBuilder, OpenOptions},
    io::Write,
    os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt, PermissionsExt},
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
            capability.len() == 77
                && capability.starts_with(b"aq-responses-")
                && capability[13..]
                    .iter()
                    .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(b)),
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

pub struct GeminiAgentHome;
impl GeminiAgentHome {
    /// The launch recipe alone, with nothing written.
    pub fn recipe(
        confinement: &AuthorizedConfinement,
        harness: &HarnessSettings,
        model: &ModelSettings,
        paths: GeminiPaths<'_>,
        auth: GeminiAuth<'_>,
    ) -> Result<GeminiRecipe> {
        gemini::map(harness, model, paths, confinement.mode(), auth)
    }

    /// Creates the empty home and, as uid 0, the system settings directory
    /// (0755) and file (0644). The parent of `system_settings_dir` is checked
    /// before anything is written, and the written file is checked against the
    /// CLI's ownership rule, so a file the CLI would skip is never accepted.
    /// The state directory `.gemini` is left to the CLI or to the C6b
    /// subscription login.
    pub fn install(
        confinement: &AuthorizedConfinement,
        harness: &HarnessSettings,
        model: &ModelSettings,
        paths: GeminiPaths<'_>,
        auth: GeminiAuth<'_>,
    ) -> Result<GeminiRecipe> {
        let recipe = Self::recipe(confinement, harness, model, paths, auth)?;
        let dir = paths.system_settings_dir;
        ensure_root_secure(
            dir.parent()
                .context("Gemini system settings directory has no parent")?,
        )?;
        create_private_home(paths.private_home)?;
        let settings = dir.join(gemini::SYSTEM_SETTINGS_FILE);
        as_root(|| {
            DirBuilder::new()
                .mode(0o755)
                .create(dir)
                .with_context(|| format!("create Gemini system settings {}", dir.display()))?;
            fs::set_permissions(dir, fs::Permissions::from_mode(0o755))?;
            let mut file = OpenOptions::new()
                .write(true)
                .create_new(true)
                .mode(0o644)
                .custom_flags(libc::O_NOFOLLOW)
                .open(&settings)?;
            file.set_permissions(fs::Permissions::from_mode(0o644))?;
            file.write_all(&recipe.system_settings_json)?;
            file.sync_all()?;
            Ok(())
        })?;
        ensure_root_secure(&settings)?;
        Ok(recipe)
    }
}

/// Gemini CLI 0.67.0's rule for system settings, made strict: `path` and
/// every ancestor are owned by uid 0, not group/other writable and not
/// symlinks; directories are traversable and a file is readable by others,
/// so the agent uid can read it rather than see it as absent.
pub(crate) fn ensure_root_secure(path: &Path) -> Result<()> {
    ensure!(
        path.is_absolute(),
        "Gemini system settings path must be absolute"
    );
    for (depth, at) in path.ancestors().enumerate() {
        let meta = fs::symlink_metadata(at)
            .with_context(|| format!("inspect Gemini system settings {}", at.display()))?;
        let mode = meta.mode();
        let reachable = if meta.is_dir() {
            mode & 0o001 != 0
        } else {
            depth == 0 && meta.is_file() && mode & 0o004 != 0
        };
        ensure!(
            meta.uid() == 0 && mode & 0o022 == 0 && reachable,
            "Gemini CLI ignores system settings unless every path element is \
             root-owned and not group/other writable: {} (uid {}, mode {:o})",
            at.display(),
            meta.uid(),
            mode & 0o7777
        );
    }
    Ok(())
}

/// Run `op` with effective uid and gid 0. A root runner runs it inline; the
/// separated non-root runner switches one scoped thread through its retained
/// `CAP_SETUID`/`CAP_SETGID` (raw syscalls change only that thread).
fn as_root<T: Send>(op: impl FnOnce() -> Result<T> + Send) -> Result<T> {
    if unsafe { libc::geteuid() } == 0 && unsafe { libc::getegid() } == 0 {
        return op();
    }
    std::thread::scope(|scope| {
        scope
            .spawn(move || {
                let keep = libc::c_long::from(-1i32);
                ensure!(
                    unsafe { libc::syscall(libc::SYS_setresgid, keep, 0 as libc::c_long, keep) }
                        == 0
                        && unsafe {
                            libc::syscall(libc::SYS_setresuid, keep, 0 as libc::c_long, keep)
                        } == 0
                        && unsafe { libc::geteuid() } == 0
                        && unsafe { libc::getegid() } == 0,
                    "root identity for Gemini system settings refused: {}",
                    std::io::Error::last_os_error()
                );
                op()
            })
            .join()
            .map_err(|_| anyhow::anyhow!("Gemini system settings operation panicked"))?
    })
}

#[cfg(test)]
mod gemini_settings_tests {
    use super::ensure_root_secure;
    use std::path::Path;

    #[test]
    fn root_secure_matches_the_cli_ownership_rule() {
        // A standard root-owned 0644 file under 0755 root directories.
        ensure_root_secure(Path::new("/etc/passwd")).unwrap();
        // World-writable /tmp, or a non-root owner, makes the CLI skip it.
        let tmp = tempfile::tempdir().unwrap();
        let file = tmp.path().join("system-settings.json");
        std::fs::write(&file, b"{}").unwrap();
        assert!(ensure_root_secure(&file).is_err());
        assert!(ensure_root_secure(Path::new("/tmp")).is_err());
        assert!(ensure_root_secure(Path::new("/nonexistent/aq")).is_err());
    }
}
