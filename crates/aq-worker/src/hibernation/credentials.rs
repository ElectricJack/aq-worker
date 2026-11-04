//! Credential material that must never reach retained hibernation state: the
//! runner permit/provider/access files and the agent subscription logins.
//! `capture` reads them into zeroizing memory for the leak scan, `scrub`
//! deletes them, and `scan` refuses retained bytes or paths that carry them.
use anyhow::{Context, Result};
use sha2::{Digest as _, Sha256};
use std::path::{Path, PathBuf};
use zeroize::Zeroizing;

/// Runner-root files (see `native::runner::sandbox::private`).
pub const RUNNER_CREDENTIALS: [&str; 3] = ["permit.json", "provider.key", "access.json"];
/// Subscription logins under the sandbox work root (`/aq/work`).
pub const LOGIN_CREDENTIALS: [&str; 2] = [
    "agent-home/claude/.credentials.json",
    "agent-home/codex/auth.json",
];
/// Basenames that are credential material wherever they appear in a snapshot.
const CREDENTIAL_BASENAMES: [&str; 3] = [".credentials.json", "provider.key", "permit.json"];
/// Secrets shorter than this are not scanned as substrings (false positives).
const MIN_SCANNED_SECRET: usize = 8;

#[derive(Clone, Debug)]
pub struct CredentialLayout {
    pub runner_root: PathBuf,
    pub work_root: PathBuf,
}
impl CredentialLayout {
    pub fn new(runner_root: impl Into<PathBuf>, work_root: impl Into<PathBuf>) -> Self {
        Self {
            runner_root: runner_root.into(),
            work_root: work_root.into(),
        }
    }
    /// The agent workspace inside the sandbox (`/aq/work/repo`).
    pub fn workspace(&self) -> PathBuf {
        self.work_root.join("repo")
    }
    pub fn paths(&self) -> Vec<PathBuf> {
        RUNNER_CREDENTIALS
            .iter()
            .map(|n| self.runner_root.join(n))
            .chain(LOGIN_CREDENTIALS.iter().map(|n| self.work_root.join(n)))
            .collect()
    }
}

/// Secret bytes held only in memory between capture and scan; never journaled.
pub struct ScrubbedSecrets {
    secrets: Vec<Zeroizing<Vec<u8>>>,
    /// SHA-256 fingerprints of the captured files (safe to journal and report).
    pub fingerprints: Vec<String>,
}
impl ScrubbedSecrets {
    /// Read every present credential file. Missing files are fine.
    pub fn capture(layout: &CredentialLayout) -> Result<Self> {
        let mut secrets = Vec::new();
        let mut fingerprints = Vec::new();
        for path in layout.paths() {
            match std::fs::read(&path) {
                Ok(bytes) => {
                    let bytes = Zeroizing::new(bytes);
                    fingerprints.push(format!("{:x}", Sha256::digest(&*bytes)));
                    secrets.push(bytes);
                }
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                Err(e) => return Err(e).with_context(|| format!("read {}", path.display())),
            }
        }
        Ok(Self {
            secrets,
            fingerprints,
        })
    }
    /// First leak found in a retained entry: a credential path or secret bytes.
    pub fn scan(&self, path: &str, bytes: &[u8]) -> Option<&'static str> {
        let normalized = path.trim_start_matches("./");
        let basename = normalized.rsplit('/').next().unwrap_or(normalized);
        if CREDENTIAL_BASENAMES.contains(&basename)
            || normalized.split('/').any(|c| c == "agent-home")
            || LOGIN_CREDENTIALS.iter().any(|p| normalized.ends_with(p))
        {
            return Some("retained state contains a credential path");
        }
        for secret in &self.secrets {
            let needle = trim_ascii(secret);
            if needle.len() >= MIN_SCANNED_SECRET
                && bytes.windows(needle.len()).any(|w| w == needle)
            {
                return Some("retained state contains credential bytes");
            }
        }
        None
    }
}

fn trim_ascii(bytes: &[u8]) -> &[u8] {
    let start = bytes
        .iter()
        .position(|b| !b.is_ascii_whitespace())
        .unwrap_or(bytes.len());
    let end = bytes
        .iter()
        .rposition(|b| !b.is_ascii_whitespace())
        .map_or(start, |i| i + 1);
    &bytes[start..end]
}

/// Delete every credential file and fsync the parent directories.
pub fn scrub(layout: &CredentialLayout) -> Result<()> {
    for path in layout.paths() {
        match std::fs::remove_file(&path) {
            Ok(()) => sync_parent(&path)?,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(e).with_context(|| format!("scrub {}", path.display())),
        }
    }
    Ok(())
}

fn sync_parent(path: &Path) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::File::open(parent)
            .and_then(|d| d.sync_all())
            .with_context(|| format!("sync {}", parent.display()))?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn plant(layout: &CredentialLayout) {
        for (i, p) in layout.paths().iter().enumerate() {
            std::fs::create_dir_all(p.parent().unwrap()).unwrap();
            std::fs::write(p, format!("secret-material-{i}-0123456789")).unwrap();
        }
    }

    #[test]
    fn scrub_removes_every_known_credential_file() {
        let dir = tempfile::tempdir().unwrap();
        let layout = CredentialLayout::new(dir.path().join("runner"), dir.path().join("work"));
        plant(&layout);
        let captured = ScrubbedSecrets::capture(&layout).unwrap();
        assert_eq!(captured.fingerprints.len(), 5);
        scrub(&layout).unwrap();
        for p in layout.paths() {
            assert!(!p.exists(), "{} survived scrub", p.display());
        }
        scrub(&layout).unwrap();
    }

    #[test]
    fn scan_flags_credential_paths_and_embedded_secret_bytes() {
        let dir = tempfile::tempdir().unwrap();
        let layout = CredentialLayout::new(dir.path().join("runner"), dir.path().join("work"));
        plant(&layout);
        let captured = ScrubbedSecrets::capture(&layout).unwrap();
        assert!(captured.scan("src/main.rs", b"fn main() {}").is_none());
        assert!(captured.scan("notes/permit.json", b"{}").is_some());
        assert!(
            captured
                .scan("x/agent-home/codex/auth.json", b"{}")
                .is_some()
        );
        assert!(
            captured
                .scan(
                    "src/leak.txt",
                    b"prefix secret-material-1-0123456789 suffix"
                )
                .is_some()
        );
    }
}
