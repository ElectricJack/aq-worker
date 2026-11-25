//! Node-local configuration. Offers never reach this type: everything here is
//! operator-owned, and the launch policy built from it is fixed.
use crate::backend::{BackendError, TemplateDigest, valid_alias_name};
use serde::Deserialize;
use std::{
    collections::{BTreeMap, BTreeSet},
    path::{Path, PathBuf},
};

use aq_sandbox_archive::docker_engine::{
    HostStatvfs, PlacementRefusal, UnixSocketTransport, check_placement, container_path_ok,
    exposes_daemon_socket, parent, under, validate_socket,
};
pub use aq_sandbox_archive::docker_engine::{MAIN_DAEMON_SOCKETS, WorkspaceBindPaths};
use std::time::Duration;
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
    /// The runner gets a terminal (the node driver attaches its pane to it).
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub runner_tty: bool,
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
    /// Minimum daemon storage headroom before requesting or accepting work.
    /// Defaults to one sandbox's disk budget.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub minimum_free_disk_bytes: Option<u64>,
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
    /// Set by the launch, per attempt: the node's own 0700 directory and the
    /// harness's transcript directory (`DriverHarness::transcript_dir`, nothing
    /// else) it is mounted at.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub transcript: Option<(PathBuf, String)>,
    /// The attempt's build-cache binds; set by the driver, never read from a file.
    #[serde(skip)]
    pub cache_binds: Vec<String>,
    /// Operator-owned mapping for projects mounted from a containerized worker.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workspace_bind_paths: Option<WorkspaceBindPaths>,
    /// Runtime-only, source-only snapshots of approved read-only projects.
    #[serde(skip)]
    pub(crate) readonly_projects: BTreeMap<uuid::Uuid, PathBuf>,
    /// Spec 0017: the admitted environments this node pulled; set by the service.
    #[cfg(feature = "project_environments")]
    #[serde(skip)]
    pub environments: aq_environment_builder::node::Held,
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

impl DockerLocalConfig {
    pub fn new(socket: PathBuf) -> Self {
        Self {
            socket,
            templates: Vec::new(),
            mount_aliases: BTreeMap::new(),
            runner_command: None,
            runner_tty: false,
            material_path: default_material_path(),
            material_uid: 0,
            material_gid: 0,
            api_timeout_ms: default_api_timeout_ms(),
            stop_grace_secs: default_stop_grace_secs(),
            max_read_bytes: default_max_read_bytes(),
            disk_limit_bytes: default_disk_limit_bytes(),
            minimum_free_disk_bytes: None,
            disk_watch_interval_ms: default_disk_watch_interval_ms(),
            shared_root_fs: false,
            hard_disk_limit: false,
            channel_root: None,
            transcript: None,
            cache_binds: Vec::new(),
            workspace_bind_paths: None,
            readonly_projects: BTreeMap::new(),
            #[cfg(feature = "project_environments")]
            environments: Default::default(),
        }
    }

    pub fn template_image(&self, digest: &TemplateDigest) -> Option<&str> {
        self.templates
            .iter()
            .find(|template| &template.digest == digest)
            .map(|template| template.image.as_str())
    }

    pub async fn check_admission(&self) -> Result<(), PlacementRefusal> {
        validate_socket(&self.socket).map_err(|_| PlacementRefusal::DaemonUnavailable)?;
        check_placement(
            &UnixSocketTransport::new(self.socket.clone()),
            super::API_VERSION,
            Duration::from_millis(self.api_timeout_ms),
            self.minimum_free_disk_bytes
                .unwrap_or(self.disk_limit_bytes)
                .max(1),
            &HostStatvfs,
        )
        .await
    }

    pub(crate) fn project_bind_paths(&self) -> Option<WorkspaceBindPaths> {
        self.workspace_bind_paths
            .clone()
            .or_else(WorkspaceBindPaths::managed)
    }

    pub fn validate(&self) -> Result<(), BackendError> {
        validate_socket(&self.socket).map_err(BackendError::InvalidRequest)?;
        check_templates(&self.templates)?;
        if let Some(root) = &self.channel_root {
            self.check_channel_root(root)?;
        }
        if let Some((host, target)) = &self.transcript {
            use aq_handoff_note::harness::DriverHarness::{Claude, Codex};
            let clear = |path: &str| !under(path, target) && !under(target, path);
            if self.check_channel_root(host).is_err()
                || ![Claude, Codex].iter().any(|h| h.transcript_dir() == target)
                || !self.mount_aliases.values().all(|a| clear(&a.target))
                || !clear(&self.material_path)
            {
                return Err(BackendError::InvalidRequest(
                    "transcript mount must be a harness transcript directory clear of every alias",
                ));
            }
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
            if exposes_daemon_socket(&self.socket, &alias.host_path) {
                return Err(BackendError::InvalidRequest(
                    "mount alias exposes a daemon socket",
                ));
            }
            let project_overlap = self.project_bind_paths().is_some()
                && (under(&alias.target, "/aq/projects") || under("/aq/projects", &alias.target));
            if project_overlap
                || !container_path_ok(&alias.target)
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
            || exposes_daemon_socket(&self.socket, root)
            || self.mount_aliases.values().any(alias)
        {
            return Err(BackendError::InvalidRequest(
                "channel root must be absolute, expose no daemon socket and overlap no mount alias",
            ));
        }
        Ok(())
    }
}

/// An OCI reference `[host[:port]/]path[:tag]@<digest>` and nothing else, so it
/// is safe in a daemon API path: no `?`, `&`, `#`, space or control character.
pub fn pinned_reference(image: &str, digest: &TemplateDigest) -> bool {
    let alnum = |c: u8| c.is_ascii_lowercase() || c.is_ascii_digit();
    let bounded = |s: &str, ok: &dyn Fn(u8) -> bool| {
        let b = s.as_bytes();
        !b.is_empty() && b.iter().all(|c| ok(*c)) && alnum(b[0]) && alnum(b[b.len() - 1])
    };
    let component = |s: &str| bounded(s, &|c| alnum(c) || b"._-".contains(&c));
    let host = |s: &str| {
        let (host, port) = s.split_once(':').unwrap_or((s, "0"));
        let label = |l: &str| bounded(&l.to_ascii_lowercase(), &|c| alnum(c) || c == b'-');
        host.split('.').all(label) && !port.is_empty() && port.bytes().all(|c| c.is_ascii_digit())
    };
    let Some(name) = (image.strip_suffix(digest.as_str())).and_then(|n| n.strip_suffix('@')) else {
        return false;
    };
    let name = match name.rsplit_once(':') {
        Some((name, tag)) if !tag.contains('/') => {
            let tag_ok = |c: u8| c.is_ascii_alphanumeric() || b"._-".contains(&c);
            let ok = tag.len() <= 128 && tag.bytes().all(tag_ok) && !tag.starts_with(['.', '-']);
            if !ok || tag.is_empty() {
                return false;
            }
            name
        }
        _ => name,
    };
    let mut parts: Vec<&str> = name.split('/').collect();
    let first = parts[0];
    if parts.len() > 1 && (first.contains(['.', ':']) || first == "localhost") {
        if !host(first) {
            return false;
        }
        parts.remove(0);
    }
    name.len() <= 255 && parts.into_iter().all(component)
}

fn check_templates(templates: &[AllowedTemplate]) -> Result<(), BackendError> {
    let mut digests = BTreeSet::new();
    for template in templates {
        if !pinned_reference(&template.image, &template.digest) {
            return Err(BackendError::InvalidRequest(
                "template image must be a reference pinned to its digest",
            ));
        }
        if !digests.insert(template.digest.as_str()) {
            return Err(BackendError::InvalidRequest("duplicate template digest"));
        }
    }
    Ok(())
}

#[cfg(feature = "project_environments")]
impl DockerLocalConfig {
    /// Merges admitted templates into `templates`, checked like the static ones.
    pub fn admit(
        &mut self,
        admitted: &[aq_environment_builder::node::AdmittedEnvironment],
    ) -> Result<(), BackendError> {
        let mut templates = self.templates.clone();
        for environment in admitted {
            let digest = TemplateDigest::parse(environment.digest.clone())?;
            if self.template_image(&digest).is_none() {
                let image = environment.image.clone();
                templates.push(AllowedTemplate { image, digest });
            }
        }
        check_templates(&templates)?;
        self.templates = templates;
        Ok(())
    }

    /// The admitted template of a held environment, once admitted here.
    pub fn template_for(
        &self,
        environment: &aq_session_protocol::ArtifactRef,
    ) -> Option<&AllowedTemplate> {
        let held = self.environments.get(environment)?;
        (self.templates.iter()).find(|t| t.digest.as_str() == held.digest)
    }

    /// The template an offer in `environment` runs in: a held environment's,
    /// admitted into this copy, else `boot`. Only the boot environment reaches the
    /// fallback: the executor's `Held::select` refuses an unheld one before launch.
    pub fn offered_template(
        &mut self,
        environment: &aq_session_protocol::ArtifactRef,
        boot: &TemplateDigest,
    ) -> Result<TemplateDigest, BackendError> {
        let Some(held) = self.environments.get(environment) else {
            return Ok(boot.clone());
        };
        self.admit(&[held])?;
        Ok(self
            .template_for(environment)
            .map_or(boot, |t| &t.digest)
            .clone())
    }
}
