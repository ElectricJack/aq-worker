//! Node-local configuration. Offers never reach this type: everything here is
//! operator-owned, and the launch policy built from it is fixed.
use crate::backend::{BackendError, TemplateDigest, valid_alias_name};
use serde::Deserialize;
use std::{
    collections::{BTreeMap, BTreeSet},
    os::unix::fs::{FileTypeExt, MetadataExt},
    path::{Path, PathBuf},
};

/// The host's main Docker daemon sockets (SBX10). Never used, even via symlink.
pub const MAIN_DAEMON_SOCKETS: &[&str] = &["/var/run/docker.sock", "/run/docker.sock"];
/// The per-attempt work volume is always mounted here.
pub const WORK_TARGET: &str = "/aq/work";
/// The attempt's agent-channel directory is always mounted here (0006 §5.1).
pub const CHANNEL_TARGET: &str = aq_agent_channel::CONTAINER_DIR;

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AllowedTemplate {
    /// Pullable reference, which must end in `@{digest}`.
    pub image: String,
    pub digest: TemplateDigest,
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MountAlias {
    pub host_path: PathBuf,
    /// Container path; must be under `/aq/` and outside the work volume.
    pub target: String,
    #[serde(default)]
    pub read_only: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DockerLocalConfig {
    /// The dedicated sandbox daemon's API socket. Never the host's main daemon.
    pub socket: PathBuf,
    #[serde(default)]
    pub templates: Vec<AllowedTemplate>,
    #[serde(default)]
    pub mount_aliases: BTreeMap<String, MountAlias>,
    /// Fixed runner argv. `None` keeps the image's own command. Never secrets.
    #[serde(default)]
    pub runner_command: Option<Vec<String>>,
    #[serde(default = "default_material_path")]
    pub material_path: String,
    #[serde(default)]
    pub material_uid: u32,
    #[serde(default)]
    pub material_gid: u32,
    #[serde(default = "default_api_timeout_ms")]
    pub api_timeout_ms: u64,
    #[serde(default = "default_stop_grace_secs")]
    pub stop_grace_secs: u32,
    #[serde(default = "default_max_read_bytes")]
    pub max_read_bytes: u64,
    /// SBX20 per-attempt disk limit when a reservation carries none.
    #[serde(default = "default_disk_limit_bytes")]
    pub disk_limit_bytes: u64,
    /// SBX20 watchdog sampling interval.
    #[serde(default = "default_disk_watch_interval_ms")]
    pub disk_watch_interval_ms: u64,
    /// SBX20 override: admit a daemon whose storage shares `/`. Reported in
    /// node inventory; one runaway attempt can then fill the host.
    #[serde(default)]
    pub shared_root_fs: bool,
    /// SBX20 optional hard limit: XFS prjquota `StorageOpt.size` and sized
    /// work volumes. The watchdog stays on regardless.
    #[serde(default)]
    pub hard_disk_limit: bool,
    /// Node-owned root of the per-attempt channel directories; a request with
    /// `agent_channel` mounts `<channel_root>/<attempt>` at `/aq/channel`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub channel_root: Option<PathBuf>,
}

fn default_material_path() -> String {
    "/aq/private/material".into()
}
fn default_api_timeout_ms() -> u64 {
    10_000
}
fn default_stop_grace_secs() -> u32 {
    10
}
fn default_max_read_bytes() -> u64 {
    1024 * 1024
}
/// VPS default: 32 GiB per attempt.
pub const DEFAULT_DISK_LIMIT_BYTES: u64 = 32 * 1024 * 1024 * 1024;
fn default_disk_limit_bytes() -> u64 {
    DEFAULT_DISK_LIMIT_BYTES
}
fn default_disk_watch_interval_ms() -> u64 {
    10_000
}

const REFUSE_MAIN: BackendError =
    BackendError::InvalidRequest("refusing the host's main Docker daemon");

impl DockerLocalConfig {
    pub fn new(socket: PathBuf) -> Self {
        Self {
            socket,
            templates: Vec::new(),
            mount_aliases: BTreeMap::new(),
            runner_command: None,
            material_path: default_material_path(),
            material_uid: 0,
            material_gid: 0,
            api_timeout_ms: default_api_timeout_ms(),
            stop_grace_secs: default_stop_grace_secs(),
            max_read_bytes: default_max_read_bytes(),
            disk_limit_bytes: default_disk_limit_bytes(),
            disk_watch_interval_ms: default_disk_watch_interval_ms(),
            shared_root_fs: false,
            hard_disk_limit: false,
            channel_root: None,
        }
    }

    pub fn template_image(&self, digest: &TemplateDigest) -> Option<&str> {
        self.templates
            .iter()
            .find(|template| &template.digest == digest)
            .map(|template| template.image.as_str())
    }

    pub fn validate(&self) -> Result<(), BackendError> {
        self.validate_socket()?;
        let mut digests = BTreeSet::new();
        for template in &self.templates {
            let suffix = format!("@{}", template.digest.as_str());
            if template.image.len() <= suffix.len() || !template.image.ends_with(&suffix) {
                return Err(BackendError::InvalidRequest(
                    "template image must be pinned to its digest",
                ));
            }
            if !digests.insert(template.digest.as_str()) {
                return Err(BackendError::InvalidRequest("duplicate template digest"));
            }
        }
        if let Some(root) = &self.channel_root {
            self.check_channel_root(root)?;
        }
        if !container_path_ok(&self.material_path)
            || under(&self.material_path, WORK_TARGET)
            || under(&self.material_path, CHANNEL_TARGET)
        {
            return Err(BackendError::InvalidRequest(
                "material path must be under /aq/ and outside the work volume",
            ));
        }
        let mut targets = BTreeSet::new();
        for (name, alias) in &self.mount_aliases {
            if !valid_alias_name(name) {
                return Err(BackendError::InvalidRequest("invalid mount alias name"));
            }
            if !alias.host_path.is_absolute() {
                return Err(BackendError::InvalidRequest(
                    "mount alias host path must be absolute",
                ));
            }
            if self.exposes_daemon_socket(&alias.host_path) {
                return Err(BackendError::InvalidRequest(
                    "mount alias exposes a daemon socket",
                ));
            }
            if !container_path_ok(&alias.target)
                || under(&alias.target, WORK_TARGET)
                || under(&alias.target, CHANNEL_TARGET)
                || under(CHANNEL_TARGET, &alias.target)
                || under(&self.material_path, &alias.target)
                || under(&alias.target, parent(&self.material_path))
                || !targets.insert(alias.target.as_str())
            {
                return Err(BackendError::InvalidRequest(
                    "mount alias target must be a distinct path under /aq/",
                ));
            }
        }
        if self
            .runner_command
            .as_ref()
            .is_some_and(|command| command.is_empty())
        {
            return Err(BackendError::InvalidRequest("runner command is empty"));
        }
        if self.api_timeout_ms == 0
            || self.max_read_bytes == 0
            || self.stop_grace_secs > 3600
            || self.disk_limit_bytes == 0
            || !(1..=600_000).contains(&self.disk_watch_interval_ms)
        {
            return Err(BackendError::InvalidRequest(
                "invalid docker-local timeouts or limits",
            ));
        }
        Ok(())
    }

    /// The channel root is absolute, exposes no daemon socket, and no mount
    /// alias's host path equals, contains or sits under it.
    pub fn check_channel_root(&self, root: &Path) -> Result<(), BackendError> {
        let alias = |a: &MountAlias| aq_agent_channel::paths_overlap(&a.host_path, root);
        if !root.is_absolute()
            || self.exposes_daemon_socket(root)
            || self.mount_aliases.values().any(alias)
        {
            return Err(BackendError::InvalidRequest(
                "channel root must be absolute, expose no daemon socket and overlap no mount alias",
            ));
        }
        Ok(())
    }

    fn validate_socket(&self) -> Result<(), BackendError> {
        if !self.socket.is_absolute() {
            return Err(BackendError::InvalidRequest(
                "sandbox daemon socket must be an absolute path",
            ));
        }
        if is_main_daemon_path(&self.socket) {
            return Err(REFUSE_MAIN);
        }
        let canonical = std::fs::canonicalize(&self.socket)
            .map_err(|_| BackendError::InvalidRequest("sandbox daemon socket unavailable"))?;
        let metadata = std::fs::metadata(&canonical)
            .map_err(|_| BackendError::InvalidRequest("sandbox daemon socket unavailable"))?;
        if is_main_daemon_path(&canonical) || same_file_as_main_daemon(&metadata) {
            return Err(REFUSE_MAIN);
        }
        if !metadata.file_type().is_socket() {
            return Err(BackendError::InvalidRequest(
                "sandbox daemon socket is not a Unix socket",
            ));
        }
        // SAFETY: geteuid has no preconditions and cannot fail.
        if metadata.uid() != unsafe { libc::geteuid() } {
            return Err(BackendError::InvalidRequest(
                "sandbox daemon socket is not owned by the worker user",
            ));
        }
        Ok(())
    }

    fn exposes_daemon_socket(&self, host_path: &Path) -> bool {
        let canonical = std::fs::canonicalize(host_path).unwrap_or_else(|_| host_path.into());
        let socket = std::fs::canonicalize(&self.socket).unwrap_or_else(|_| self.socket.clone());
        is_main_daemon_path(host_path)
            || is_main_daemon_path(&canonical)
            || socket.starts_with(&canonical)
            || std::fs::metadata(&canonical)
                .is_ok_and(|m| m.file_type().is_socket() || same_file_as_main_daemon(&m))
    }
}

fn is_main_daemon_path(path: &Path) -> bool {
    MAIN_DAEMON_SOCKETS
        .iter()
        .any(|main| path == Path::new(main))
}

fn same_file_as_main_daemon(metadata: &std::fs::Metadata) -> bool {
    MAIN_DAEMON_SOCKETS.iter().any(|main| {
        std::fs::metadata(main)
            .is_ok_and(|m| m.dev() == metadata.dev() && m.ino() == metadata.ino())
    })
}

/// Absolute, normalized and under `/aq/`.
fn container_path_ok(path: &str) -> bool {
    path.strip_prefix("/aq/").is_some_and(|rest| {
        !rest.is_empty()
            && rest
                .split('/')
                .all(|part| !part.is_empty() && part != "." && part != "..")
    })
}

/// True when `path` is `base` or below it.
fn under(path: &str, base: &str) -> bool {
    path == base
        || path
            .strip_prefix(base)
            .is_some_and(|rest| rest.starts_with('/'))
}

fn parent(path: &str) -> &str {
    path.rsplit_once('/').map_or("/", |(parent, _)| parent)
}
