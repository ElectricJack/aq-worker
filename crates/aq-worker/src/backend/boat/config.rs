use super::*;
use serde::{Deserialize, Serialize};
use std::{fmt, path::PathBuf};
use zeroize::Zeroizing;

/// Node-local bearer key, never serialized or included in diagnostics.
pub struct BoatKey(Zeroizing<String>);
impl BoatKey {
    pub fn new(value: String) -> Result<Self, BackendError> {
        if value.trim().is_empty() || value.bytes().any(|b| b.is_ascii_control()) {
            return Err(BackendError::InvalidRequest("invalid boat key"));
        }
        Ok(Self(Zeroizing::new(value)))
    }
    pub fn load(path: &std::path::Path) -> Result<Self, BackendError> {
        use std::{
            io::Read,
            os::unix::fs::{MetadataExt, OpenOptionsExt},
        };
        let mut file = std::fs::OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_NOFOLLOW)
            .open(path)
            .map_err(|_| BackendError::InvalidRequest("boat key unavailable"))?;
        let meta = file
            .metadata()
            .map_err(|_| BackendError::InvalidRequest("boat key unreadable"))?;
        if !meta.is_file()
            || meta.mode() & 0o077 != 0
            || meta.uid() != unsafe { libc::geteuid() }
            || meta.len() > 4096
        {
            return Err(BackendError::InvalidRequest(
                "boat key must be a private node-local file",
            ));
        }
        let mut value = Zeroizing::new(String::new());
        file.read_to_string(&mut value)
            .map_err(|_| BackendError::InvalidRequest("boat key unreadable"))?;
        Self::new(value.trim().to_owned())
    }
    pub(super) fn expose(&self) -> &str {
        &self.0
    }
}
impl fmt::Debug for BoatKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("BoatKey([REDACTED])")
    }
}

/// Derived from the C3b allowlist's materializations.boat record. The saved
/// handle is mutable at the provider, so verify its immutable artifact each time.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BoatTemplate {
    pub image: String,
    pub digest: TemplateDigest,
    pub saved_snapshot: String,
    pub snapshot_id: String,
    pub scan_policy: String,
}
#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BoatConfig {
    #[serde(default = "api_url")]
    pub api_url: String,
    pub node_id: Uuid,
    pub attempt_org: String,
    pub template_org: String,
    pub journal_path: PathBuf,
    pub key_file: PathBuf,
    pub templates: Vec<BoatTemplate>,
    /// Trusted node configuration; absent preserves the scanned image command.
    #[serde(default)]
    pub runner_command: Option<Vec<String>>,
    #[serde(default = "api_timeout")]
    pub api_timeout_ms: u64,
    #[serde(default = "poll_timeout")]
    pub poll_timeout_ms: u64,
    #[serde(default = "poll_interval")]
    pub poll_interval_ms: u64,
}
fn api_url() -> String {
    "https://boat.dev/api/v1".into()
}
fn api_timeout() -> u64 {
    30_000
}
fn poll_timeout() -> u64 {
    120_000
}
fn poll_interval() -> u64 {
    500
}
impl BoatConfig {
    pub fn validate(&self) -> Result<(), BackendError> {
        let url = reqwest::Url::parse(&self.api_url)
            .map_err(|_| BackendError::InvalidRequest("invalid boat endpoint"))?;
        // Plain HTTP is exclusively for the loopback conformance double.
        let local = url
            .host_str()
            .is_some_and(|s| s == "127.0.0.1" || s == "[::1]" || s == "::1");
        if !(url.scheme() == "https" || url.scheme() == "http" && local)
            || !url.username().is_empty()
            || url.password().is_some()
            || url.query().is_some()
            || url.fragment().is_some()
        {
            return Err(BackendError::InvalidRequest(
                "boat requires authenticated HTTPS",
            ));
        }
        if self.node_id.is_nil()
            || !safe_id(&self.attempt_org)
            || !safe_id(&self.template_org)
            || self.attempt_org == self.template_org
            || !self.journal_path.is_absolute()
            || !self.key_file.is_absolute()
            || !(1..=120_000).contains(&self.api_timeout_ms)
            || !(1..=300_000).contains(&self.poll_timeout_ms)
            || !(1..=5_000).contains(&self.poll_interval_ms)
            || self.templates.is_empty()
        {
            return Err(BackendError::InvalidRequest("invalid boat scope or bounds"));
        }
        if self.runner_command.as_ref().is_some_and(|args| {
            args.is_empty()
                || args.len() > 64
                || args.iter().any(|s| {
                    s.is_empty() || s.len() > 4096 || s.bytes().any(|b| b.is_ascii_control())
                })
        }) {
            return Err(BackendError::InvalidRequest("invalid fixed runner command"));
        }
        let mut seen = std::collections::BTreeSet::new();
        for t in &self.templates {
            if !t
                .image
                .as_bytes()
                .first()
                .is_some_and(u8::is_ascii_alphanumeric)
                || !t.image.ends_with(&format!("@{}", t.digest.as_str()))
                || !t
                    .image
                    .bytes()
                    .all(|c| c.is_ascii_alphanumeric() || b"./:@_-".contains(&c))
                || !safe_id(&t.saved_snapshot)
                || !safe_id(&t.snapshot_id)
                || t.scan_policy != "aq-credential-scan-v1"
                || !seen.insert(t.digest.as_str())
            {
                return Err(BackendError::InvalidRequest(
                    "boat template lacks scanned pinned materialization",
                ));
            }
        }
        Ok(())
    }
}
pub(super) fn safe_id(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= 128
        && s.bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"_-".contains(&b))
}
