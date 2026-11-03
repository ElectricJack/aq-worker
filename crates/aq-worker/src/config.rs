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

/// The node's runner Access service token (0005 §7), stored next to
/// `worker.toml` as the JSON file the runner API proxy reads.
#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RunnerAccess {
    pub client_id: String,
    pub client_secret: String,
}

impl fmt::Debug for RunnerAccess {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("RunnerAccess")
            .field("client_id", &self.client_id)
            .field("client_secret", &"<redacted>")
            .finish()
    }
}

/// `runner-access.json` in the same directory as `worker.toml`.
pub fn runner_access_path(worker_toml: &Path) -> PathBuf {
    worker_toml.with_file_name("runner-access.json")
}

impl RunnerAccess {
    /// Same private write as `worker.toml`: 0600 file, 0700 directory, atomic rename.
    pub fn save(&self, path: &Path) -> Result<()> {
        let body = serde_json::to_vec(self).context("serialize runner access token")?;
        write_private(path, &body)
    }
}

/// Writes a 0600 temp file in the same directory, then renames it into place.
fn write_private(path: &Path, body: &[u8]) -> Result<()> {
    let dir = match path.parent() {
        Some(d) if !d.as_os_str().is_empty() => d,
        _ => Path::new("."),
    };
    std::fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(dir)
        .with_context(|| format!("create {}", dir.display()))?;
    let tmp = dir.join(format!(".aq-config.{}.tmp", Uuid::new_v4().simple()));
    let write = || -> Result<()> {
        let mut f = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&tmp)?;
        f.write_all(body)?;
        f.sync_all()?;
        std::fs::rename(&tmp, path)?;
        Ok(())
    };
    write().map_err(|e| {
        let _ = std::fs::remove_file(&tmp);
        e.context(format!("write {}", path.display()))
    })
}

impl WorkerConfig {
    /// Writes a 0600 temp file in the same directory, then renames it into place.
    pub fn save(&self, path: &Path) -> Result<()> {
        let body = toml::to_string(self).context("serialize worker config")?;
        write_private(path, body.as_bytes())
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::MetadataExt;

    #[test]
    fn runner_access_path_sits_next_to_worker_toml() {
        assert_eq!(
            runner_access_path(Path::new("/config/agent-q/worker.toml")),
            PathBuf::from("/config/agent-q/runner-access.json")
        );
        assert_eq!(
            runner_access_path(Path::new("worker.toml")),
            PathBuf::from("runner-access.json")
        );
    }

    #[test]
    fn runner_access_save_is_private_json() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("agent-q").join("runner-access.json");
        let token = RunnerAccess {
            client_id: "id.access".into(),
            client_secret: "s3cret-value".into(),
        };
        assert!(!format!("{token:?}").contains("s3cret-value"));
        token.save(&path).unwrap();
        let meta = std::fs::metadata(&path).unwrap();
        assert_eq!(meta.mode() & 0o777, 0o600);
        assert_eq!(meta.nlink(), 1);
        assert_eq!(
            std::fs::metadata(path.parent().unwrap()).unwrap().mode() & 0o777,
            0o700
        );
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct Proxy {
            client_id: String,
            client_secret: String,
        }
        let read: Proxy = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        assert_eq!(read.client_id, "id.access");
        assert_eq!(read.client_secret, "s3cret-value");
        RunnerAccess {
            client_id: "id2".into(),
            client_secret: "other".into(),
        }
        .save(&path)
        .unwrap();
        let read: Proxy = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        assert_eq!(read.client_id, "id2");
        assert_eq!(
            std::fs::read_dir(path.parent().unwrap()).unwrap().count(),
            1
        );
    }
}
