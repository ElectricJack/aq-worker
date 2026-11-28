//! `service.toml` (0600, owner-only).
use crate::enrollment_file::{read_private, write_private};
use anyhow::{Context, Result, ensure};
use aq_protocol::{MAX_MACHINE_SLOTS, valid_machine_key};
use serde::{Deserialize, Serialize};
use std::{
    io::Write,
    os::unix::fs::OpenOptionsExt,
    path::{Path, PathBuf},
};
use uuid::Uuid;

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ServiceConfig {
    pub schema_version: u32,
    pub profile: String,
    pub machine_key: String,
    /// Number of enrolled nodes on this machine; old files called these slots.
    #[serde(alias = "slots")]
    pub nodes: u32,
    pub native_template: PathBuf,
    pub state_root: PathBuf,
    /// Where the slots' Unix sockets live; `layout::default_runtime_root` when unset.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub runtime_root: Option<PathBuf>,
    pub aq_git: AqGitConfig,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub manual: Option<ManualConfig>,
    #[serde(default = "default_shutdown_grace_ms")]
    pub shutdown_grace_ms: u64,
    /// `[retry]`: how long a slot pauses after a failed attempt. Optional, as is each key.
    #[serde(default, skip_serializing_if = "RetryConfig::is_default")]
    pub retry: RetryConfig,
    /// `[observations]`: every limit of the node's observation store. Optional, as is each key.
    #[serde(default, skip_serializing_if = "ObservationConfig::is_default")]
    pub observations: ObservationConfig,
    /// `[environments]` (spec 0017): the node's poll of the admitted environments.
    #[cfg(feature = "project_environments")]
    #[serde(default, skip_serializing_if = "EnvironmentConfig::is_default")]
    pub environments: EnvironmentConfig,
}
/// Spec 0017: how the node reads the server's admitted environments.
#[cfg(feature = "project_environments")]
#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields, default)]
pub struct EnvironmentConfig {
    /// The largest admitted list (`GET /v1/worker/environments`) the node reads. It grows
    /// with projects times kept versions; a larger one is refused and logged as an error.
    pub max_list_bytes: u64,
    /// The largest reply to the node's ready report.
    pub max_reply_bytes: u64,
}
#[cfg(feature = "project_environments")]
impl Default for EnvironmentConfig {
    fn default() -> Self {
        Self {
            max_list_bytes: 4 * 1024 * 1024,
            max_reply_bytes: 1024 * 1024,
        }
    }
}
#[cfg(feature = "project_environments")]
impl EnvironmentConfig {
    fn is_default(&self) -> bool {
        *self == Self::default()
    }
}
/// Limits of the node observation service: one store per node under
/// `<state_root>/observations/<node id>`, which collects the transcripts of the node's
/// running attempts and delivers them to the server.
#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields, default)]
pub struct ObservationConfig {
    /// Hard size of the store file. Fixed when the node's store is first created, as are the
    /// next two.
    pub store_file_bytes: u64,
    /// Retained rows: events not yet acknowledged and the parser state of each source.
    pub store_data_bytes: u64,
    /// The store's page cache, the service's main memory use.
    pub store_cache_bytes: u64,
    /// Transcript items remembered per source for deduplication: the last N. Older ones are
    /// forgotten; a session of any length keeps being collected.
    pub tracked_identities: u32,
    /// Sources (running attempts) the node's store holds at once.
    pub max_sources: u32,
    /// How long after a worker restart a stored source waits for its surviving attempt to
    /// register again before it is fenced and released.
    pub resume_grace_ms: u64,
    /// Directory entries examined when a transcript is found by its session key.
    pub discovery_entries: u32,
    /// Pause between collection rounds when no transcript grew.
    pub poll_ms: u64,
    /// First pause after a failed server call; it doubles up to `retry_max_ms`.
    pub retry_start_ms: u64,
    pub retry_max_ms: u64,
    /// Timeout of one server call.
    pub request_timeout_ms: u64,
    /// How long a finished attempt's transcript may still be drained.
    pub finish_drain_ms: u64,
    /// Disk space one attempt's transcript mount may take, and the directory entries it
    /// may hold, before the node stops the attempt (the mount is outside the sandbox's
    /// disk limit). They apply whether or not the attempt is observed.
    pub transcript_max_bytes: u64,
    pub transcript_max_files: u64,
}
impl Default for ObservationConfig {
    fn default() -> Self {
        Self {
            store_file_bytes: 64 * 1024 * 1024,
            store_data_bytes: 16 * 1024 * 1024,
            store_cache_bytes: 4 * 1024 * 1024,
            tracked_identities: 4096,
            max_sources: 16,
            resume_grace_ms: 120_000,
            discovery_entries: 256,
            poll_ms: 250,
            retry_start_ms: 1_000,
            retry_max_ms: 30_000,
            request_timeout_ms: 10_000,
            finish_drain_ms: 5_000,
            transcript_max_bytes: 1024 * 1024 * 1024,
            transcript_max_files: 10_000,
        }
    }
}
impl ObservationConfig {
    fn is_default(&self) -> bool {
        *self == Self::default()
    }
}
/// The pause of a slot after a failed attempt (`supervisor::FailureKind`). While it pauses the
/// slot still reports to the server; it only asks for no offer.
#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields, default)]
pub struct RetryConfig {
    /// After the server refused the slot, one offer failed before its launch, or the server
    /// was not reached: the pause before the slot asks for another offer.
    pub offer_pause_ms: u64,
    /// Ceiling of that pause, which doubles while the same kind of failure repeats.
    pub offer_pause_max_ms: u64,
    /// After any other failure (this machine's sandbox, disk or credential) or a failed
    /// recovery: the first pause, which doubles until an attempt finishes.
    pub fault_backoff_start_ms: u64,
    /// Ceiling of that back-off.
    pub fault_backoff_max_ms: u64,
}
impl Default for RetryConfig {
    fn default() -> Self {
        Self {
            offer_pause_ms: 3_000,
            offer_pause_max_ms: 30_000,
            fault_backoff_start_ms: crate::supervisor::BACKOFF_START.as_millis() as u64,
            fault_backoff_max_ms: crate::supervisor::BACKOFF_MAX.as_millis() as u64,
        }
    }
}
impl RetryConfig {
    fn is_default(&self) -> bool {
        *self == Self::default()
    }
}
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ManualConfig {
    pub podman_executable: PathBuf,
    /// Accepted for old servers only; slot-aware nodes use their server control count.
    #[serde(default = "default_manual_slots", skip_serializing)]
    pub max_slots: u32,
    /// Owner-only Claude subscription token file (`claude setup-token`) that manual Claude
    /// sessions log in with.
    pub claude_token_file: PathBuf,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub codex: Option<ManualCodexConfig>,
}
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ManualCodexConfig {
    /// Executable inside the fixed sandbox template.
    pub executable: PathBuf,
    /// How often a running session's login is compared with the server's login store
    /// (a refreshed login is written back, a newer stored one installed).
    #[serde(default = "default_login_check_secs")]
    pub login_check_secs: u64,
    /// Optional pinned Codex helpers copied into this manual session only.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub runtime_bundle: Option<PathBuf>,
}
fn default_login_check_secs() -> u64 {
    60
}
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct AqGitConfig {
    pub binary: PathBuf,
    pub git_executable: PathBuf,
}
/// Settings that no longer exist: a worker takes each repository from the server's offer.
/// Refused by name, so an owner is told what to remove instead of reading an
/// "unknown field" error, and nothing is silently ignored.
fn refuse_removed_settings(text: &str) -> Result<()> {
    let Ok(toml::Value::Table(doc)) = text.parse::<toml::Value>() else {
        return Ok(()); // not a document: the real parse reports it
    };
    ensure!(
        !doc.contains_key("repositories"),
        "`repositories` is no longer a setting: a worker takes each repository from the server's \
         offer. Remove `repositories` (and any repositories.json) from service.toml"
    );
    ensure!(
        doc.get("manual")
            .and_then(|m| m.get("source_slugs"))
            .is_none(),
        "`manual.source_slugs` is no longer a setting: a hand-started session's repository \
         comes from the server. Remove `[manual.source_slugs]` from service.toml"
    );
    Ok(())
}
fn default_manual_slots() -> u32 {
    1
}
fn default_shutdown_grace_ms() -> u64 {
    120_000
}
/// `^[a-z0-9][a-z0-9-]{0,31}$`
pub fn valid_profile(p: &str) -> bool {
    (1..=32).contains(&p.len())
        && !p.starts_with('-')
        && p.bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
}

/// What systemd allows beyond the shutdown grace before it kills the service: the forced stop
/// fits inside it (asserted in the supervisor).
pub const STOP_MARGIN: std::time::Duration = std::time::Duration::from_secs(30);
impl ObservationConfig {
    fn valid_storage(&self) -> bool {
        const MIB: u64 = 1024 * 1024;
        (MIB..=128 * MIB).contains(&self.store_file_bytes)
            && (2 * MIB..=16 * MIB).contains(&self.store_data_bytes)
            && self.store_data_bytes < self.store_file_bytes
            && (65_536..=8 * MIB).contains(&self.store_cache_bytes)
            && self.tracked_identities >= 16
            && self.max_sources >= 1
            && self.transcript_max_bytes >= MIB
            && (16..=1_000_000).contains(&self.transcript_max_files)
    }

    fn valid_timing(&self) -> bool {
        self.resume_grace_ms <= 3_600_000
            && (10..=60_000).contains(&self.poll_ms)
            && (100..=600_000).contains(&self.retry_start_ms)
            && (self.retry_start_ms..=3_600_000).contains(&self.retry_max_ms)
            && (1_000..=60_000).contains(&self.request_timeout_ms)
            && self.finish_drain_ms <= 600_000
    }
}

impl ServiceConfig {
    /// The unit's TimeoutStopSec: the grace, rounded up to seconds, plus `STOP_MARGIN`.
    pub fn runtime_root(&self) -> PathBuf {
        (self.runtime_root.clone())
            .unwrap_or_else(|| crate::layout::default_runtime_root(&self.profile))
    }
    pub fn stop_timeout_secs(&self) -> u64 {
        self.shutdown_grace_ms.div_ceil(1000) + STOP_MARGIN.as_secs()
    }
    /// Loads the file. One written before workers took repositories from the server's offer
    /// is rewritten once without `repositories` and `manual.source_slugs`
    /// (`retire_repositories`), and one written before qualification measurement was removed
    /// without its `measurement` setting (`retire_measurement`).
    pub fn load(path: &Path) -> Result<Self> {
        let mut text = read_private(path)?;
        if let Some(migrated) = retire_repositories(path, &text)? {
            text = migrated;
        }
        if let Some(migrated) = retire_measurement(path, &text)? {
            text = migrated;
        }
        let config: Self =
            Self::parse(&text).with_context(|| format!("parse {}", path.display()))?;
        config
            .validate()
            .with_context(|| format!("invalid {}", path.display()))?;
        Ok(config)
    }
    /// Parses the document, refusing unknown keys.
    pub fn parse(text: &str) -> Result<Self> {
        refuse_removed_settings(text)?;
        Ok(toml::from_str(text)?)
    }
    pub fn save(&self, path: &Path) -> Result<()> {
        self.validate()?;
        write_private(
            path,
            toml::to_string(self)
                .context("serialize service config")?
                .as_bytes(),
        )
    }
    pub fn validate(&self) -> Result<()> {
        ensure!(
            self.schema_version == 1,
            "unsupported service config schema_version"
        );
        ensure!(valid_profile(&self.profile), "invalid profile name");
        ensure!(valid_machine_key(&self.machine_key), "invalid machine_key");
        ensure!(
            (1..=MAX_MACHINE_SLOTS).contains(&self.nodes),
            "nodes must be 1..={MAX_MACHINE_SLOTS}"
        );
        self.validate_paths()?;
        self.validate_retry()?;
        self.validate_observations()?;
        self.validate_manual()
    }

    fn validate_paths(&self) -> Result<()> {
        for (name, path) in [
            ("native_template", &self.native_template),
            ("state_root", &self.state_root),
            ("aq_git.binary", &self.aq_git.binary),
            ("aq_git.git_executable", &self.aq_git.git_executable),
        ] {
            ensure!(path.is_absolute(), "{name} must be an absolute path");
        }
        if let Some(root) = &self.runtime_root {
            ensure!(root.is_absolute(), "runtime_root must be an absolute path");
        }
        crate::layout::check_socket_paths(&self.runtime_root(), self.nodes)
    }

    fn validate_retry(&self) -> Result<()> {
        ensure!(
            (10_000..=600_000).contains(&self.shutdown_grace_ms),
            "shutdown_grace_ms must be 10000..=600000"
        );
        let r = &self.retry;
        ensure!(
            (100..=60_000).contains(&r.offer_pause_ms)
                && (r.offer_pause_ms..=600_000).contains(&r.offer_pause_max_ms)
                && (1_000..=3_600_000).contains(&r.fault_backoff_start_ms)
                && (r.fault_backoff_start_ms..=86_400_000).contains(&r.fault_backoff_max_ms),
            "invalid [retry]: offer_pause_ms 100..=60000 <= offer_pause_max_ms <= 600000, fault_backoff_start_ms 1000..=3600000 <= fault_backoff_max_ms <= 86400000"
        );
        Ok(())
    }

    fn validate_observations(&self) -> Result<()> {
        let o = &self.observations;
        let storage = o.valid_storage();
        let timing = o.valid_timing();
        ensure!(
            storage && timing && (1..=4096).contains(&o.discovery_entries),
            "invalid [observations]: transcript_max_bytes >= 1 MiB, transcript_max_files 16..=1000000, store_file_bytes 1 MiB..=128 MiB > store_data_bytes 2 MiB..=16 MiB, store_cache_bytes 64 KiB..=8 MiB, tracked_identities >= 16, max_sources >= 1, resume_grace_ms <= 3600000, discovery_entries 1..=4096, poll_ms 10..=60000, retry_start_ms 100..=600000 <= retry_max_ms <= 3600000, request_timeout_ms 1000..=60000, finish_drain_ms <= 600000"
        );
        #[cfg(feature = "project_environments")]
        ensure!(
            [
                self.environments.max_list_bytes,
                self.environments.max_reply_bytes
            ]
            .iter()
            .all(|n| (64 * 1024..=256 * 1024 * 1024).contains(n)),
            "invalid [environments]: max_list_bytes and max_reply_bytes 64 KiB..=256 MiB"
        );
        Ok(())
    }

    fn validate_manual(&self) -> Result<()> {
        if let Some(m) = &self.manual {
            ensure!(
                m.podman_executable.is_absolute() && (1..=8).contains(&m.max_slots),
                "invalid manual worker configuration"
            );
            if let Some(c) = &m.codex {
                ensure!(
                    c.executable.is_absolute()
                        && c.runtime_bundle.as_ref().is_none_or(|p| p.is_absolute()),
                    "manual Codex paths must be absolute"
                );
                ensure!(
                    (1..=86_400).contains(&c.login_check_secs),
                    "manual.codex.login_check_secs must be 1..=86400"
                );
            }
            ensure!(
                m.claude_token_file.is_absolute(),
                "manual.claude_token_file must be an absolute path"
            );
        }
        Ok(())
    }
}

/// Suffix of the one-time copy of a `service.toml` as it was before `retire_measurement`.
pub const BEFORE_MEASUREMENT_REMOVAL: &str = "before-measurement-removal";

/// What `without_measurement` did about the Claude token file of manual sessions.
#[derive(Debug, PartialEq, Eq)]
enum CarriedToken {
    /// No `[manual]` section: nothing needs the token file.
    NoManual,
    /// `[manual]` already named its `claude_token_file`.
    Kept,
    /// `measurement.token_file` became `[manual] claude_token_file`.
    Moved,
    /// `[manual]` names no token file and the old setting had none either.
    Missing,
}

/// The document without its `measurement` setting (a `[measurement]` section, an inline table
/// or dotted keys), or `None` when it has none or is not TOML (the caller's parse reports
/// that). The old `token_file` is the Claude subscription token manual sessions log in with:
/// it becomes `[manual] claude_token_file` when `[manual]` exists and names none. Every other
/// key, its value, the comments and the order are kept.
fn without_measurement(text: &str) -> Option<(String, CarriedToken)> {
    let mut document: toml_edit::DocumentMut = text.parse().ok()?;
    let old = document.as_table_mut().remove("measurement")?;
    let token = (old.get("token_file"))
        .and_then(|v| v.as_str())
        .filter(|v| !v.is_empty());
    let manual = (document.get_mut("manual")).and_then(|m| m.as_table_like_mut());
    let carried = match (manual, token) {
        (None, _) => CarriedToken::NoManual,
        (Some(manual), _) if manual.contains_key("claude_token_file") => CarriedToken::Kept,
        (Some(manual), Some(token)) => {
            manual.insert("claude_token_file", toml_edit::value(token));
            CarriedToken::Moved
        }
        (Some(_), None) => CarriedToken::Missing,
    };
    Some((document.to_string(), carried))
}

/// Rewrites, once, a `service.toml` written before qualification measurement was removed, so
/// the worker accepts it: see `without_measurement`. Returns the new text, or `None` when the
/// file needs no change (it is then not touched).
///
/// This exists for machines updated remotely: the receiver such a machine already has replaces
/// the worker binary and restarts the service, and nothing else rewrites the configuration, so
/// a worker that refused the old file would leave the machine offline with no remote way back.
/// It can be deleted once every machine has started a worker that has it.
///
/// Several programs load the same file (the service, the terminal relays) and may start
/// together. Each derives the same text from the same old file and replaces the file by
/// rename, so a reader sees the old file or the new one, never a part; one that finds the new
/// file does nothing. The file as it was is kept beside it (0600) by whichever came first.
fn retire_measurement(path: &Path, text: &str) -> Result<Option<String>> {
    let Some((migrated, carried)) = without_measurement(text) else {
        return Ok(None);
    };
    let backup = replace_keeping_copy(path, text, &migrated, BEFORE_MEASUREMENT_REMOVAL)
        .with_context(|| {
            format!(
                "{} still has the measurement setting, which was removed, and could not be rewritten without it",
                path.display()
            )
        })?;
    let token = match carried {
        CarriedToken::NoManual => "it has no [manual] section, so no token file was needed",
        CarriedToken::Kept => "[manual] claude_token_file was already set and is unchanged",
        CarriedToken::Moved => "its token_file is now [manual] claude_token_file",
        CarriedToken::Missing => {
            "it named no token_file, so [manual] still needs claude_token_file"
        }
    };
    tracing::warn!(
        "rewrote {} once: the measurement setting was removed (qualification measurement no longer exists); {token}. The file as it was is kept at {}",
        path.display(),
        backup.display()
    );
    Ok(Some(migrated))
}

/// Suffix of the one-time copy of a `service.toml` as it was before `retire_repositories`.
pub const BEFORE_REPOSITORY_REMOVAL: &str = "before-repository-removal";

/// The document without `repositories` and `manual.source_slugs`, or `None` when it has
/// neither or is not TOML (the caller's parse reports that). Every other key, its value, the
/// comments and the order are kept.
fn without_repositories(text: &str) -> Option<String> {
    let mut document: toml_edit::DocumentMut = text.parse().ok()?;
    let mut found = document.as_table_mut().remove("repositories").is_some();
    if let Some(manual) = (document.get_mut("manual")).and_then(|m| m.as_table_like_mut()) {
        found |= manual.remove("source_slugs").is_some();
    }
    found.then(|| document.to_string())
}

/// Rewrites, once, a `service.toml` that still lists `repositories` or `manual.source_slugs`,
/// which a worker no longer reads (it takes each repository from the server's offer). Machines
/// are updated remotely and nobody edits their configuration, so a worker that refused such a
/// file would leave the machine offline. Returns the new text, or `None` when the file has
/// neither (it is then not touched). Concurrent loaders and the kept copy are as for
/// `retire_measurement`. `ServiceConfig::parse` of a document still holding them is refused.
fn retire_repositories(path: &Path, text: &str) -> Result<Option<String>> {
    let Some(migrated) = without_repositories(text) else {
        return Ok(None);
    };
    let backup = replace_keeping_copy(path, text, &migrated, BEFORE_REPOSITORY_REMOVAL)
        .with_context(|| {
            format!(
                "{} still has `repositories` or `manual.source_slugs`, which were removed, and could not be rewritten without them",
                path.display()
            )
        })?;
    tracing::warn!(
        "rewrote {} once: `repositories` and `manual.source_slugs` were removed (a worker takes each repository from the server's offer). The file as it was is kept at {}",
        path.display(),
        backup.display()
    );
    Ok(Some(migrated))
}

/// Keeps the first copy of `old` beside `path` (`<path>.<suffix>`, 0600), then replaces `path`
/// by rename with `new`. Returns the copy's path.
fn replace_keeping_copy(path: &Path, old: &str, new: &str, suffix: &str) -> Result<PathBuf> {
    let mut backup = path.as_os_str().to_owned();
    backup.push(format!(".{suffix}"));
    let backup = PathBuf::from(backup);
    keep_first_copy(&backup, old.as_bytes())?;
    write_private(path, new.as_bytes())?;
    // Best effort: the rename is what readers see; this only makes it outlast a power cut.
    if let Some(dir) = path.parent().filter(|d| !d.as_os_str().is_empty()) {
        let _ = std::fs::File::open(dir).and_then(|d| d.sync_all());
    }
    Ok(backup)
}

/// Writes `body` to `path` (0600) unless the file already exists; the first writer's complete
/// copy stays.
fn keep_first_copy(path: &Path, body: &[u8]) -> Result<()> {
    let dir = match path.parent() {
        Some(d) if !d.as_os_str().is_empty() => d,
        _ => Path::new("."),
    };
    let tmp = dir.join(format!(".aq-config.{}.tmp", Uuid::new_v4().simple()));
    let write = || -> std::io::Result<()> {
        let mut f = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&tmp)?;
        f.write_all(body)?;
        f.sync_all()?;
        // A link, unlike a rename, never replaces an existing file.
        match std::fs::hard_link(&tmp, path) {
            Err(e) if e.kind() != std::io::ErrorKind::AlreadyExists => Err(e),
            _ => Ok(()),
        }
    };
    let result = write();
    let _ = std::fs::remove_file(&tmp);
    result.with_context(|| format!("write {}", path.display()))
}

#[cfg(test)]
mod tests;
