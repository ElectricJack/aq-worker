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
/// Shared cache location, outside the per-attempt work volume.
pub const SCCACHE_TARGET: &str = "/var/cache/sccache";
pub const SCCACHE_CACHE_SIZE: &str = "10G";

#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SandboxConfig {
    /// Opt in to a shared compiler cache with sccache's 10G eviction cap.
    #[serde(default)]
    pub shared_sccache: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AllowedTemplate {
    /// Pullable reference, which must end in `@{digest}`.
    pub image: String,
    pub digest: TemplateDigest,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MountAlias {
    pub host_path: PathBuf,
    /// Container path; must be under `/aq/` and outside the work volume.
    pub target: String,
    #[serde(default)]
    pub read_only: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DockerLocalConfig {
    /// The dedicated sandbox daemon's API socket. Never the host's main daemon.
    pub socket: PathBuf,
    /// Populated from the node's `sandbox.shared_sccache` configuration.
    #[serde(skip)]
    pub shared_sccache: bool,
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

const REFUSE_MAIN: BackendError =
    BackendError::InvalidRequest("refusing the host's main Docker daemon");

impl DockerLocalConfig {
    pub fn new(socket: PathBuf) -> Self {
        Self {
            socket,
            shared_sccache: false,
            templates: Vec::new(),
            mount_aliases: BTreeMap::new(),
            runner_command: None,
            material_path: default_material_path(),
            material_uid: 0,
            material_gid: 0,
            api_timeout_ms: default_api_timeout_ms(),
            stop_grace_secs: default_stop_grace_secs(),
            max_read_bytes: default_max_read_bytes(),
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
        if !container_path_ok(&self.material_path) || under(&self.material_path, WORK_TARGET) {
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
        if self.api_timeout_ms == 0 || self.max_read_bytes == 0 || self.stop_grace_secs > 3600 {
            return Err(BackendError::InvalidRequest(
                "invalid docker-local timeouts or limits",
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
