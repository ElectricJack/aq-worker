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
        let private = meta.mode() & 0o077 == 0 && meta.uid() == unsafe { libc::geteuid() };
        let bounded_file = meta.is_file() && meta.len() <= 4096;
        if !private || !bounded_file {
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
/// Omitting both snapshot fields selects the stock boat image: each VM then
/// pulls the C3b-scanned public image by digest and verifies its RepoDigest.
/// That is the path until the owner materializes a template (on demand).
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BoatTemplate {
    pub image: String,
    pub digest: TemplateDigest,
    #[serde(default)]
    pub saved_snapshot: Option<String>,
    #[serde(default)]
    pub snapshot_id: Option<String>,
    pub scan_policy: String,
}
impl BoatTemplate {
    /// Stock-image template: no saved snapshot to verify or create from.
    pub fn is_stock(&self) -> bool {
        self.saved_snapshot.is_none()
    }
}
/// Billing scope of a personal boat account (no organization). Ownership is
/// then the node name prefix plus `access == "owner"` on an unbilled-to-org VM.
pub const PERSONAL_SCOPE: &str = "personal";
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
    /// Personal account: sweeps scope by name prefix plus delete-own.
    pub fn personal(&self) -> bool {
        self.attempt_org == PERSONAL_SCOPE
    }
    pub fn validate(&self) -> Result<(), BackendError> {
        validate_endpoint(&self.api_url)?;
        if !self.valid_scope() || !self.valid_bounds() {
            return Err(BackendError::InvalidRequest("invalid boat scope or bounds"));
        }
        if !self.runner_command.as_deref().is_none_or(valid_command) {
            return Err(BackendError::InvalidRequest("invalid fixed runner command"));
        }
        let mut seen = std::collections::BTreeSet::new();
        for template in &self.templates {
            if !template.is_scanned() || !seen.insert(template.digest.as_str()) {
                return Err(BackendError::InvalidRequest(
                    "boat template lacks scanned pinned materialization",
                ));
            }
        }
        Ok(())
    }

    fn valid_scope(&self) -> bool {
        let scopes = if self.personal() {
            self.template_org == PERSONAL_SCOPE
        } else {
            self.template_org != PERSONAL_SCOPE && self.attempt_org != self.template_org
        };
        !self.node_id.is_nil()
            && safe_id(&self.attempt_org)
            && safe_id(&self.template_org)
            && scopes
    }

    fn valid_bounds(&self) -> bool {
        self.journal_path.is_absolute()
            && self.key_file.is_absolute()
            && !self.templates.is_empty()
            && (1..=120_000).contains(&self.api_timeout_ms)
            && (1..=300_000).contains(&self.poll_timeout_ms)
            && (1..=5_000).contains(&self.poll_interval_ms)
    }
}

fn validate_endpoint(endpoint: &str) -> Result<(), BackendError> {
    let url = reqwest::Url::parse(endpoint)
        .map_err(|_| BackendError::InvalidRequest("invalid boat endpoint"))?;
    // Plain HTTP is exclusively for the loopback conformance double.
    let local = matches!(url.host_str(), Some("127.0.0.1" | "[::1]" | "::1"));
    let transport = url.scheme() == "https" || (url.scheme() == "http" && local);
    let bare = url.username().is_empty()
        && url.password().is_none()
        && url.query().is_none()
        && url.fragment().is_none();
    if transport && bare {
        Ok(())
    } else {
        Err(BackendError::InvalidRequest(
            "boat requires authenticated HTTPS",
        ))
    }
}

fn valid_command(args: &[String]) -> bool {
    (1..=64).contains(&args.len())
        && args.iter().all(|arg| {
            (1..=4096).contains(&arg.len()) && !arg.bytes().any(|b| b.is_ascii_control())
        })
}

impl BoatTemplate {
    fn is_scanned(&self) -> bool {
        let pinned = self
            .image
            .as_bytes()
            .first()
            .is_some_and(u8::is_ascii_alphanumeric)
            && self.image.ends_with(&format!("@{}", self.digest.as_str()))
            && self
                .image
                .bytes()
                .all(|c| c.is_ascii_alphanumeric() || b"./:@_-".contains(&c));
        let snapshots = match (&self.saved_snapshot, &self.snapshot_id) {
            (Some(name), Some(id)) => safe_id(name) && safe_id(id),
            (None, None) => true,
            _ => false,
        };
        pinned && snapshots && self.scan_policy == "aq-credential-scan-v1"
    }
}
pub(super) fn safe_id(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= 128
        && s.bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"_-".contains(&b))
}
