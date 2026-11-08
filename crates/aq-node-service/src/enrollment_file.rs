//! `worker.toml` for one slot; byte-compatible with `aq_worker::config::WorkerConfig`.
use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use std::{
    io::Write,
    os::unix::fs::{DirBuilderExt, OpenOptionsExt, PermissionsExt},
    path::Path,
};
use uuid::Uuid;

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EnrollmentFile {
    pub server_url: String,
    pub node_id: Uuid,
    pub credential: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cf_client_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cf_client_secret: Option<String>,
}
impl std::fmt::Debug for EnrollmentFile {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("EnrollmentFile")
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
impl EnrollmentFile {
    pub fn load(path: &Path) -> Result<Self> {
        let text = read_private(path)?;
        toml::from_str(&text).with_context(|| format!("parse {}", path.display()))
    }
    pub fn save(&self, path: &Path) -> Result<()> {
        write_private(
            path,
            toml::to_string(self)
                .context("serialize enrollment")?
                .as_bytes(),
        )
    }
}

/// Reads a file only the owner can access.
pub(crate) fn read_private(path: &Path) -> Result<String> {
    let meta = std::fs::metadata(path).with_context(|| format!("read {}", path.display()))?;
    if meta.permissions().mode() & 0o077 != 0 {
        bail!(
            "{p} is readable by other users; run chmod 600 {p}",
            p = path.display()
        );
    }
    std::fs::read_to_string(path).with_context(|| format!("read {}", path.display()))
}

/// Directory 0700, file 0600: a create-new temp file in the same directory, then a rename.
pub fn write_private(path: &Path, body: &[u8]) -> Result<()> {
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

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn debug_redacts_secrets_and_file_is_private() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("slots/1/worker.toml");
        let file = EnrollmentFile {
            server_url: "https://aq.example.com".into(),
            node_id: Uuid::from_u128(7),
            credential: "aqn_secret".into(),
            cf_client_id: Some("id".into()),
            cf_client_secret: Some("cfsecret".into()),
        };
        file.save(&path).unwrap();
        let shown = format!("{:?}", EnrollmentFile::load(&path).unwrap());
        assert!(!shown.contains("aqn_secret") && !shown.contains("cfsecret"));
        assert_eq!(
            std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
        assert!(EnrollmentFile::load(&path).is_err());
    }
}
