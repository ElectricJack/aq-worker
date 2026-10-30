//! The worker's saved enrollment (`worker.toml`).

use std::fmt;
use std::io::Write;
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkerConfig {
    pub server_url: String,
    pub node_id: Uuid,
    pub credential: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cf_client_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cf_client_secret: Option<String>,
}

impl fmt::Debug for WorkerConfig {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("WorkerConfig")
            .field("server_url", &self.server_url)
            .field("node_id", &self.node_id)
            .field("credential", &"<redacted>")
            .field("cf_client_id", &self.cf_client_id)
            .field(
                "cf_client_secret",
                &self.cf_client_secret.as_ref().map(|_| "<redacted>"),
            )
            .finish()
    }
}

/// `$XDG_CONFIG_HOME/agent-q/worker.toml`, falling back to `~/.config`.
pub fn default_path() -> PathBuf {
    let base = std::env::var_os("XDG_CONFIG_HOME")
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
        .or_else(|| {
            std::env::var_os("HOME")
                .filter(|v| !v.is_empty())
                .map(|h| PathBuf::from(h).join(".config"))
        })
        .unwrap_or_else(|| PathBuf::from(".config"));
    base.join("agent-q").join("worker.toml")
}

impl WorkerConfig {
    /// Writes a 0600 temp file in the same directory, then renames it into place.
    pub fn save(&self, path: &Path) -> Result<()> {
        let dir = match path.parent() {
            Some(d) if !d.as_os_str().is_empty() => d,
            _ => Path::new("."),
        };
        std::fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(dir)
            .with_context(|| format!("create {}", dir.display()))?;
        let body = toml::to_string(self).context("serialize worker config")?;
        let tmp = dir.join(format!(".worker.toml.{}.tmp", Uuid::new_v4().simple()));
        let write = || -> Result<()> {
            let mut f = std::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .mode(0o600)
                .open(&tmp)?;
            f.write_all(body.as_bytes())?;
            f.sync_all()?;
            std::fs::rename(&tmp, path)?;
            Ok(())
        };
        write().map_err(|e| {
            let _ = std::fs::remove_file(&tmp);
            e.context(format!("write {}", path.display()))
        })
    }

    pub fn load(path: &Path) -> Result<WorkerConfig> {
        let meta = std::fs::metadata(path).with_context(|| format!("read {}", path.display()))?;
        if meta.permissions().mode() & 0o077 != 0 {
            bail!(
                "{p} is readable by other users; run chmod 600 {p}",
                p = path.display()
            );
        }
        let text =
            std::fs::read_to_string(path).with_context(|| format!("read {}", path.display()))?;
        toml::from_str(&text).with_context(|| format!("parse {}", path.display()))
    }
}
