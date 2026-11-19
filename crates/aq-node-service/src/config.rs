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
    #[serde(default)]
    pub repositories: Vec<RepositoryBinding>,
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
    /// Distinct transcript items remembered per source for deduplication.
    pub tracked_identities: u32,
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
}
impl Default for ObservationConfig {
    fn default() -> Self {
        Self {
            store_file_bytes: 64 * 1024 * 1024,
            store_data_bytes: 16 * 1024 * 1024,
            store_cache_bytes: 4 * 1024 * 1024,
            tracked_identities: 4096,
            discovery_entries: 256,
            poll_ms: 250,
            retry_start_ms: 1_000,
            retry_max_ms: 30_000,
            request_timeout_ms: 10_000,
            finish_drain_ms: 5_000,
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
    #[serde(default)]
    pub source_slugs: std::collections::BTreeMap<uuid::Uuid, String>,
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
#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct RepositoryBinding {
    pub repository_id: Uuid,
    pub identity: String,
    pub binding_generation: u64,
    pub object_format: String,
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
impl ServiceConfig {
    /// The unit's TimeoutStopSec: the grace, rounded up to seconds, plus `STOP_MARGIN`.
    pub fn runtime_root(&self) -> PathBuf {
        (self.runtime_root.clone())
            .unwrap_or_else(|| crate::layout::default_runtime_root(&self.profile))
    }
    pub fn stop_timeout_secs(&self) -> u64 {
        self.shutdown_grace_ms.div_ceil(1000) + STOP_MARGIN.as_secs()
    }
    /// Loads the file. One written before qualification measurement was removed is rewritten
    /// once without its `measurement` setting first (`retire_measurement`).
    pub fn load(path: &Path) -> Result<Self> {
        let mut text = read_private(path)?;
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
        crate::layout::check_socket_paths(&self.runtime_root(), self.nodes)?;
        ensure!(self.repositories.len() <= 16, "at most 16 repositories");
        for r in &self.repositories {
            ensure!(
                !r.identity.is_empty() && r.identity.len() <= 128,
                "invalid repository identity"
            );
            ensure!(r.object_format == "sha1", "unsupported object_format");
        }
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
        let o = &self.observations;
        const MIB: u64 = 1024 * 1024;
        ensure!(
            (MIB..=128 * MIB).contains(&o.store_file_bytes)
                && (2 * MIB..=16 * MIB).contains(&o.store_data_bytes)
                && o.store_data_bytes < o.store_file_bytes
                && (65_536..=8 * MIB).contains(&o.store_cache_bytes)
                && (16..=4096).contains(&o.tracked_identities)
                && (1..=4096).contains(&o.discovery_entries)
                && (10..=60_000).contains(&o.poll_ms)
                && (100..=600_000).contains(&o.retry_start_ms)
                && (o.retry_start_ms..=3_600_000).contains(&o.retry_max_ms)
                && (1_000..=60_000).contains(&o.request_timeout_ms)
                && o.finish_drain_ms <= 600_000,
            "invalid [observations]: store_file_bytes 1 MiB..=128 MiB > store_data_bytes 2 MiB..=16 MiB, store_cache_bytes 64 KiB..=8 MiB, tracked_identities 16..=4096, discovery_entries 1..=4096, poll_ms 10..=60000, retry_start_ms 100..=600000 <= retry_max_ms <= 3600000, request_timeout_ms 1000..=60000, finish_drain_ms <= 600000"
        );
        if let Some(m) = &self.manual {
            ensure!(
                m.podman_executable.is_absolute() && (1..=8).contains(&m.max_slots),
                "invalid manual worker configuration"
            );
            ensure!(
                m.source_slugs.len() <= 16
                    && m.source_slugs.values().all(|v| v.split('/').count() == 2
                        && v.bytes()
                            .all(|b| b.is_ascii_alphanumeric() || b"/-_.".contains(&b))),
                "invalid manual repository source"
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
    let mut backup = path.as_os_str().to_owned();
    backup.push(format!(".{BEFORE_MEASUREMENT_REMOVAL}"));
    let backup = PathBuf::from(backup);
    keep_first_copy(&backup, text.as_bytes())
        .and_then(|()| write_private(path, migrated.as_bytes()))
        .with_context(|| {
            format!(
                "{} still has the measurement setting, which was removed, and could not be rewritten without it",
                path.display()
            )
        })?;
    // Best effort: the rename is what readers see; this only makes it outlast a power cut.
    if let Some(dir) = path.parent().filter(|d| !d.as_os_str().is_empty()) {
        let _ = std::fs::File::open(dir).and_then(|d| d.sync_all());
    }
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
mod tests {
    use super::*;
    #[test]
    fn old_slots_key_is_node_count_and_new_files_name_nodes() {
        let config = valid();
        let current = toml::to_string(&config).unwrap();
        assert!(current.contains("nodes = 2"));
        let legacy = current.replace("nodes = 2", "slots = 2");
        assert_eq!(ServiceConfig::parse(&legacy).unwrap().nodes, 2);
        assert!(ServiceConfig::parse(&format!("nodes = 1\n{legacy}")).is_err());
    }
    fn valid() -> ServiceConfig {
        ServiceConfig {
            schema_version: 1,
            profile: "local".into(),
            machine_key: "desk".into(),
            nodes: 2,
            native_template: "/etc/aq/native.json".into(),
            state_root: "/var/aq".into(),
            runtime_root: Some("/run/user/1000/aq/local".into()),
            aq_git: AqGitConfig {
                binary: "/opt/aq-git".into(),
                git_executable: "/usr/bin/git".into(),
            },
            repositories: vec![],
            manual: None,
            shutdown_grace_ms: 120_000,
            retry: Default::default(),
            observations: Default::default(),
        }
    }
    fn manual(token: &str) -> ManualConfig {
        ManualConfig {
            podman_executable: "/usr/bin/podman".into(),
            max_slots: 1,
            claude_token_file: token.into(),
            source_slugs: Default::default(),
            codex: None,
        }
    }
    const MINIMAL: &str = r#"schema_version = 1
profile = "local"
machine_key = "desk"
slots = 1
native_template = "/n.json"
state_root = "/s"
runtime_root = "/run/user/1000/aq/local"
[aq_git]
binary = "/g"
git_executable = "/usr/bin/git"
"#;
    fn load_text(text: &str) -> Result<ServiceConfig> {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("service.toml");
        write_private(&path, text.as_bytes()).unwrap();
        ServiceConfig::load(&path)
    }
    const MANUAL: &str = "[manual]\npodman_executable = \"/usr/bin/podman\"\nmax_slots = 1\n";
    fn mode(path: &Path) -> u32 {
        use std::os::unix::fs::PermissionsExt;
        std::fs::metadata(path).unwrap().permissions().mode() & 0o777
    }
    fn backup_of(path: &Path) -> PathBuf {
        path.with_file_name(format!("service.toml.{BEFORE_MEASUREMENT_REMOVAL}"))
    }
    fn names(dir: &Path) -> Vec<String> {
        let mut names: Vec<String> = (std::fs::read_dir(dir).unwrap())
            .map(|e| e.unwrap().file_name().into_string().unwrap())
            .collect();
        names.sort();
        names
    }
    /// Loads `text` from a file and returns the result, the directory and the file's path.
    fn migrate(text: &str) -> (Result<ServiceConfig>, tempfile::TempDir, PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("service.toml");
        write_private(&path, text.as_bytes()).unwrap();
        (ServiceConfig::load(&path), dir, path)
    }
    /// A machine updated remotely still has the configuration its installer wrote. The worker
    /// rewrites it once: the measurement setting goes, its token file becomes the manual
    /// sessions' token file, every other key keeps its value, the comments and the order
    /// stay, the file as it was is kept (0600), and the result loads.
    #[test]
    fn an_old_measurement_setting_is_removed_once_and_its_token_file_carried() {
        let slugs = "\n[manual.source_slugs]\n\"e0000000-0000-4000-8000-000000000002\" = \"o/r\"\n\n[manual.codex]\nexecutable = \"/opt/aq/bin/codex\"\n";
        let section = "\n# set by Push logins\n[measurement]\nenabled = true\ntoken_file = \"/c/provider-auth/claude-token\"\nmodel = \"m\"\nrenew_before_ms = 14400000\n";
        let inline = "measurement = { enabled = true, token_file = \"/c/provider-auth/claude-token\", model = \"m\" }\n";
        let dotted = "measurement.token_file = \"/c/provider-auth/claude-token\"\n";
        for old in [
            // As the hosted installer leaves it: the section, then the manual settings.
            format!(
                "# the owner's note\nshutdown_grace_ms = 30000 # short\n{MINIMAL}{section}\n{MANUAL}{slugs}"
            ),
            format!("{MINIMAL}{MANUAL}{slugs}{section}"),
            // As a hand-written file has it: one line among the top-level keys.
            format!("{inline}{MINIMAL}\n{MANUAL}{slugs}"),
            format!("{dotted}{MINIMAL}\n{MANUAL}{slugs}"),
        ] {
            let (loaded, _dir, path) = migrate(&old);
            let loaded = loaded.unwrap();
            let manual = loaded.manual.as_ref().unwrap();
            assert_eq!(
                manual.claude_token_file,
                Path::new("/c/provider-auth/claude-token")
            );
            assert_eq!(manual.source_slugs.len(), 1);
            let new = std::fs::read_to_string(&path).unwrap();
            assert_eq!(mode(&path), 0o600);
            // Apart from the two intended changes every key has the value it had.
            let mut expected: toml::Table = toml::from_str(&old).unwrap();
            expected.remove("measurement").unwrap();
            expected["manual"].as_table_mut().unwrap().insert(
                "claude_token_file".into(),
                "/c/provider-auth/claude-token".into(),
            );
            assert_eq!(toml::from_str::<toml::Table>(&new).unwrap(), expected);
            assert!(!new.contains("measurement"), "{new}");
            assert_eq!(
                new.contains("# the owner's note"),
                old.contains("# the owner's note")
            );
            assert_eq!(new.contains("30000 # short"), old.contains("30000 # short"));
            let backup = backup_of(&path);
            assert_eq!(std::fs::read_to_string(&backup).unwrap(), old);
            assert_eq!(mode(&backup), 0o600);
            // A second start finds the new file and leaves it and the copy alone.
            ServiceConfig::load(&path).unwrap();
            assert_eq!(std::fs::read_to_string(&path).unwrap(), new);
            assert_eq!(std::fs::read_to_string(&backup).unwrap(), old);
            assert_eq!(
                names(path.parent().unwrap()),
                ["service.toml", "service.toml.before-measurement-removal"]
            );
        }
        // The exact text of the common case: the key lands in `[manual]`, before its tables.
        let (text, carried) =
            without_measurement(&format!("{MINIMAL}{section}\n{MANUAL}{slugs}")).unwrap();
        assert_eq!(carried, CarriedToken::Moved);
        assert_eq!(
            text,
            format!(
                "{MINIMAL}\n{MANUAL}claude_token_file = \"/c/provider-auth/claude-token\"\n{slugs}"
            )
        );
    }
    /// Without manual sessions nothing needs the token file; a token file `[manual]` already
    /// names is kept, whatever the old setting said.
    #[test]
    fn the_measurement_setting_is_removed_without_manual_sessions_or_with_their_token_named() {
        for old in [
            "[measurement]\nenabled = false\nrenew_before_ms = 14400000\n",
            "[measurement]\nenabled = true\ntoken_file = \"/t\"\nmodel = \"m\"\n",
            "[measurement]\n",
        ] {
            let (loaded, _dir, path) = migrate(&format!("{MINIMAL}{old}"));
            assert!(loaded.unwrap().manual.is_none());
            assert_eq!(std::fs::read_to_string(&path).unwrap(), MINIMAL);
            assert!(backup_of(&path).exists());
        }
        let named = format!(
            "{MINIMAL}{MANUAL}claude_token_file = \"/mine\"\n[measurement]\ntoken_file = \"/t\"\n"
        );
        let (loaded, _dir, path) = migrate(&named);
        assert_eq!(
            loaded.unwrap().manual.unwrap().claude_token_file,
            Path::new("/mine")
        );
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            format!("{MINIMAL}{MANUAL}claude_token_file = \"/mine\"\n")
        );
    }
    /// Manual sessions with no token file anywhere: no path is invented. The setting is
    /// removed, the file is whole, and the load fails naming the key that is needed.
    #[test]
    fn manual_sessions_without_any_token_file_fail_naming_the_key() {
        for old in [
            "[measurement]\nenabled = false\n",
            "[measurement]\ntoken_file = \"\"\n",
        ] {
            let (loaded, dir, path) = migrate(&format!("{MINIMAL}{MANUAL}{old}"));
            let e = format!("{:#}", loaded.unwrap_err());
            assert!(e.contains("missing field `claude_token_file`"), "{e}");
            assert!(e.contains(path.to_str().unwrap()), "{e}");
            assert_eq!(
                std::fs::read_to_string(&path).unwrap(),
                format!("{MINIMAL}{MANUAL}")
            );
            assert_eq!(
                names(dir.path()),
                ["service.toml", "service.toml.before-measurement-removal"]
            );
        }
    }
    /// A file without the setting is not touched: same bytes, same file, no copy. Any other
    /// unknown key is still refused, and so is a document that is not TOML.
    #[test]
    fn a_file_without_the_measurement_setting_is_not_rewritten() {
        use std::os::unix::fs::MetadataExt;
        let text = format!("# kept\n{MINIMAL}{MANUAL}claude_token_file = \"/t\"\n");
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("service.toml");
        write_private(&path, text.as_bytes()).unwrap();
        let before = std::fs::metadata(&path).unwrap();
        ServiceConfig::load(&path).unwrap();
        let after = std::fs::metadata(&path).unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), text);
        assert_eq!(
            (before.ino(), before.mtime(), before.mtime_nsec()),
            (after.ino(), after.mtime(), after.mtime_nsec())
        );
        assert_eq!(names(dir.path()), ["service.toml"]);
        let e = format!(
            "{:#}",
            load_text(&format!("{MINIMAL}[other]\n")).unwrap_err()
        );
        assert!(e.contains("unknown field `other`"), "{e}");
        let e = format!(
            "{:#}",
            load_text(&format!("{MINIMAL}{MANUAL}token_file = \"/t\"\n")).unwrap_err()
        );
        assert!(e.contains("unknown field `token_file`"), "{e}");
        let (broken, dir, _path) = migrate("[measurement\nslots = ");
        assert!(broken.is_err());
        assert_eq!(names(dir.path()), ["service.toml"]);
    }
    /// The service and the terminal relays load the same file and may start together: every
    /// one of them loads, and what is left is one whole file and one copy of the old one.
    #[test]
    fn programs_starting_together_leave_one_whole_file() {
        let old = format!("{MINIMAL}[measurement]\ntoken_file = \"/t\"\n{MANUAL}");
        for _ in 0..20 {
            let dir = tempfile::tempdir().unwrap();
            let path = dir.path().join("service.toml");
            write_private(&path, old.as_bytes()).unwrap();
            let start = std::sync::Barrier::new(8);
            std::thread::scope(|scope| {
                for _ in 0..8 {
                    scope.spawn(|| {
                        start.wait();
                        let loaded = ServiceConfig::load(&path).unwrap();
                        assert_eq!(loaded.manual.unwrap().claude_token_file, Path::new("/t"));
                    });
                }
            });
            assert_eq!(
                std::fs::read_to_string(&path).unwrap(),
                format!("{MINIMAL}{MANUAL}claude_token_file = \"/t\"\n")
            );
            assert_eq!(std::fs::read_to_string(backup_of(&path)).unwrap(), old);
            assert_eq!(
                names(dir.path()),
                ["service.toml", "service.toml.before-measurement-removal"]
            );
        }
    }
    /// A directory the worker cannot write to: the error names the file, which is unchanged.
    #[test]
    fn a_file_that_cannot_be_rewritten_is_refused_naming_it() {
        use std::os::unix::fs::PermissionsExt;
        if unsafe { libc::geteuid() } == 0 {
            return; // root writes anywhere
        }
        let old = format!("{MINIMAL}[measurement]\nenabled = false\n");
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("service.toml");
        write_private(&path, old.as_bytes()).unwrap();
        std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o500)).unwrap();
        let e = format!("{:#}", ServiceConfig::load(&path).unwrap_err());
        std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
        assert!(
            e.contains(&format!(
                "{} still has the measurement setting, which was removed, and could not be rewritten without it",
                path.display()
            )),
            "{e}"
        );
        assert!(e.contains("Permission denied"), "{e}");
        assert_eq!(std::fs::read_to_string(&path).unwrap(), old);
        assert_eq!(names(dir.path()), ["service.toml"]);
    }
    /// Manual sessions name their Claude subscription token file in `[manual]`; it is
    /// required there and must be absolute.
    #[test]
    fn manual_sessions_name_their_claude_token_file() {
        let manual = MANUAL;
        let c = load_text(&format!(
            "{MINIMAL}{manual}claude_token_file = \"/etc/aq/token\"\n"
        ))
        .unwrap();
        assert_eq!(
            c.manual.as_ref().unwrap().claude_token_file,
            Path::new("/etc/aq/token")
        );
        let saved = toml::to_string(&c).unwrap();
        assert!(
            saved.contains("claude_token_file = \"/etc/aq/token\""),
            "{saved}"
        );
        assert!(!saved.contains("measurement"), "{saved}");
        let e = format!(
            "{:#}",
            load_text(&format!("{MINIMAL}{manual}")).unwrap_err()
        );
        assert!(e.contains("missing field `claude_token_file`"), "{e}");
        let relative = format!("{MINIMAL}{manual}claude_token_file = \"token\"\n");
        let e = format!("{:#}", load_text(&relative).unwrap_err());
        assert!(
            e.contains("manual.claude_token_file must be an absolute path"),
            "{e}"
        );
    }
    #[test]
    fn loads_valid_and_refuses_bounds() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("service.toml");
        valid().save(&path).unwrap();
        assert_eq!(ServiceConfig::load(&path).unwrap().nodes, 2);
        let bad: [fn(&mut ServiceConfig); 9] = [
            |c| c.nodes = 0,
            |c| c.nodes = 9,
            |c| c.native_template = "native.json".into(),
            |c| c.profile = "Bad_Name".into(),
            |c| c.machine_key = "Desk".into(),
            |c| c.manual = Some(manual("token")),
            |c| {
                c.manual = Some(ManualConfig {
                    max_slots: 9,
                    ..manual("/t")
                })
            },
            |c| c.runtime_root = Some("run/aq".into()),
            |c| {
                c.repositories = vec![RepositoryBinding {
                    repository_id: Uuid::nil(),
                    identity: "x".into(),
                    binding_generation: 1,
                    object_format: "sha256".into(),
                }]
            },
        ];
        for edit in bad {
            let mut c = valid();
            edit(&mut c);
            assert!(c.validate().is_err());
        }
    }
    #[test]
    fn defaults_fill_the_grace() {
        let text = r#"schema_version = 1
profile = "local"
machine_key = "desk"
slots = 1
native_template = "/n.json"
state_root = "/s"
[aq_git]
binary = "/g"
git_executable = "/usr/bin/git"
"#;
        let c: ServiceConfig = toml::from_str(text).unwrap();
        c.validate().unwrap();
        assert_eq!(c.shutdown_grace_ms, 120_000);
        // No repositories list: every repository is registered from its offer. No [retry]:
        // the defaults, which are not written back.
        assert!(c.repositories.is_empty());
        let r = &c.retry;
        assert_eq!(
            (r.offer_pause_ms, r.offer_pause_max_ms),
            (3_000, 30_000),
            "the short pause and its ceiling"
        );
        assert_eq!(
            (r.fault_backoff_start_ms, r.fault_backoff_max_ms),
            (30_000, 300_000)
        );
        assert!(!toml::to_string(&c).unwrap().contains("retry"));
        // No [observations]: the defaults, and one key overrides only itself.
        assert_eq!(c.observations, ObservationConfig::default());
        assert!(!toml::to_string(&c).unwrap().contains("observations"));
        let tuned = format!("{text}[observations]\npoll_ms = 1000\n");
        let tuned: ServiceConfig = toml::from_str(&tuned).unwrap();
        tuned.validate().unwrap();
        assert_eq!(tuned.observations.poll_ms, 1_000);
        assert_eq!(tuned.observations.store_file_bytes, 64 * 1024 * 1024);
        let mut small = tuned.clone();
        small.observations.store_data_bytes = small.observations.store_file_bytes;
        assert!(small.validate().is_err());
        let set = format!("{text}[retry]\noffer_pause_max_ms = 10000\n");
        let set = ServiceConfig::parse(&set).unwrap();
        set.validate().unwrap();
        assert_eq!(set.retry.offer_pause_max_ms, 10_000);
        assert_eq!(set.retry.offer_pause_ms, 3_000, "each key is optional");
        let again = ServiceConfig::parse(&toml::to_string(&set).unwrap()).unwrap();
        assert_eq!(again.retry, set.retry);
        let mut bad = set.clone();
        bad.retry.offer_pause_max_ms = 1_000;
        assert!(bad.validate().is_err(), "a ceiling below the pause");
        assert_eq!(c.stop_timeout_secs(), 150, "the template's default");
        let longest = ServiceConfig {
            shutdown_grace_ms: 600_000,
            ..c.clone()
        };
        assert_eq!(longest.stop_timeout_secs(), 630);
        assert!(c.repositories.is_empty() && c.manual.is_none());
        assert!(c.runtime_root.is_none());
        assert!(!toml::to_string(&c).unwrap().contains("runtime_root"));
    }
    /// D1: with a long home, the default state root holds no socket; the runtime root keeps
    /// every socket path under the limit, and a runtime root whose paths cannot fit is refused
    /// when the configuration is loaded, naming the path and its length.
    #[test]
    fn socket_paths_fit_or_the_config_is_refused() {
        let home = format!("/home/{}", "u".repeat(60));
        let mut c = valid();
        c.nodes = 8;
        c.state_root = format!("{home}/.local/state/agent-q/service/{}", c.profile).into();
        c.runtime_root = None;
        let layout = crate::Layout::for_service(Path::new("/c"), &c);
        for slot in 1..=8 {
            for socket in [
                layout.git_socket(slot),
                layout
                    .host_root(slot)
                    .join(format!("{}/tmux.sock", Uuid::nil())),
            ] {
                assert!(!socket.starts_with(&c.state_root), "{}", socket.display());
            }
        }
        c.validate().unwrap();
        // The longest uid and the longest profile each fallback root allows.
        c.runtime_root = Some(format!("/run/user/4294967294/aq/{}", "a".repeat(14)).into());
        c.validate().unwrap();
        c.runtime_root = Some(format!("/run/user/4294967294/aq/{}", "a".repeat(15)).into());
        assert!(c.validate().is_err());
        let fallback = PathBuf::from("/tmp/aq-1000").join("a".repeat(25));
        crate::layout::check_socket_paths(&fallback, 8).unwrap();
        // The live failure: the state root of a normal home cannot hold the sockets.
        let live = Path::new("/home/jkern/.local/state/agent-q/service/ms1/slots/1/current");
        assert!(crate::layout::check_socket_paths(live, 1).is_err());
        c.runtime_root = Some(format!("{home}/run").into());
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("service.toml");
        let text = toml::to_string(&c).unwrap();
        crate::enrollment_file::write_private(&path, text.as_bytes()).unwrap();
        let e = format!("{:#}", ServiceConfig::load(&path).unwrap_err());
        let long = format!("{home}/run/8/host/{}/terminal-", Uuid::nil());
        assert!(e.contains(&long), "{e}");
        assert!(
            e.contains("would be 135 bytes, over the 103-byte limit"),
            "{e}"
        );
    }
}
