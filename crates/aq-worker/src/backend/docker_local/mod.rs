//! `docker-local` placement backend: a dedicated rootless sandbox daemon reached
//! through its Docker Engine API socket. Shared-kernel containers are not a
//! hostile-code isolation boundary; this backend is `container`-confinement only.
//!
//! Every daemon call is bounded by `api_timeout_ms`. A timeout, transport error
//! or unexpected status is uncertainty: it never means "gone" and is never
//! followed by a delete.
pub mod config;
pub mod policy;
pub mod tar;
pub use aq_sandbox_archive::docker_engine as transport;

pub use config::*;
pub use transport::{
    DockerMethod, DockerRequest, DockerResponse, DockerTransport, TransportError,
    TransportErrorKind, UnixSocketTransport,
};

use crate::backend::{
    BackendDiskUsage, BackendError, BackendKind, Capability, ConfinementMode, Confirmation,
    CreateRequest, DestroyAuthorization, DiskLimitMode, DiskLimitStatus, DiskUsage, HostStatvfs,
    InventoryEntry, PlacementBackend, PrivateMaterial, RunnerExit, SandboxIdentity, SandboxState,
    Statvfs, StoppedWriterProof, TemplateDigest, UnconfirmedReason,
};
use policy::{
    container_state, create_body, disk_limit_from_labels, encode, entry_from_labels, labels_match,
    node_filter, volume_body, volume_name,
};
use serde_json::Value;
use std::{
    collections::{BTreeMap, BTreeSet},
    path::Path,
    sync::Arc,
    time::Duration,
};
use transport::CallError;
use uuid::Uuid;
use zeroize::Zeroizing;

/// Docker Engine API version; Podman's compat API and Docker 25+ both serve it.
pub const API_VERSION: &str = "/v1.44";
/// Cap for JSON responses (inspect, lists, info).
const MAX_JSON_BYTES: usize = 8 * 1024 * 1024;
/// Allowance for tar headers and padding around a capped file read.
const READ_ARCHIVE_SLACK: usize = 64 * 1024;
/// Image pulls stream progress and may take minutes on a cold cache.
const PULL_TIMEOUT: Duration = Duration::from_secs(600);

/// Confinement modes this backend can provide (0005 §14.14). Native execution
/// on the host is never offered by docker-local.
pub const CONFINEMENT: &[ConfinementMode] = &[ConfinementMode::Container];

pub fn require_confinement(mode: ConfinementMode) -> Result<(), BackendError> {
    if CONFINEMENT.contains(&mode) {
        Ok(())
    } else {
        Err(BackendError::UnsupportedOperation)
    }
}

impl From<CallError> for BackendError {
    fn from(error: CallError) -> Self {
        BackendError::Uncertain(match error {
            CallError::Timeout => UnconfirmedReason::Timeout,
            CallError::Transport | CallError::TooLarge => UnconfirmedReason::BackendFailure,
        })
    }
}

pub struct DockerLocalBackend<T: DockerTransport = UnixSocketTransport> {
    config: DockerLocalConfig,
    transport: T,
    /// SBX20: admitted by operator override onto a shared root filesystem.
    shared_root_fs: bool,
    statvfs: Arc<dyn Statvfs>,
}

impl DockerLocalBackend<UnixSocketTransport> {
    pub async fn connect_unix(config: DockerLocalConfig) -> Result<Self, BackendError> {
        let transport = UnixSocketTransport::new(config.socket.clone());
        Self::connect(config, transport).await
    }
}

impl<T: DockerTransport> DockerLocalBackend<T> {
    /// Validates the configuration and proves the daemon is a rootless one.
    pub async fn connect(config: DockerLocalConfig, transport: T) -> Result<Self, BackendError> {
        config.validate()?;
        let mut backend = Self {
            config,
            transport,
            shared_root_fs: false,
            statvfs: Arc::new(HostStatvfs),
        };
        let info = backend.get_json("/info").await?;
        let rootless = info
            .get("SecurityOptions")
            .and_then(|options| options.as_array())
            .is_some_and(|options| {
                options
                    .iter()
                    .filter_map(|o| o.as_str())
                    .any(|option| option.split(',').any(|part| part == "name=rootless"))
            });
        if !rootless {
            return Err(BackendError::InvalidRequest(
                "sandbox daemon must be rootless",
            ));
        }
        require_enforced_limits(&info)?;
        backend.shared_root_fs = storage_shares_root(&info);
        if backend.shared_root_fs && !backend.config.shared_root_fs {
            return Err(BackendError::InvalidRequest(
                "sandbox daemon storage shares the root filesystem; set shared_root_fs to override",
            ));
        }
        if backend.config.hard_disk_limit && !transport::backing_filesystem_is_xfs(&info) {
            return Err(BackendError::InvalidRequest(
                "hard disk limit requires XFS (prjquota) sandbox storage",
            ));
        }
        Ok(backend)
    }

    pub fn config(&self) -> &DockerLocalConfig {
        &self.config
    }

    /// Replaces the host statvfs used to measure the daemon's GraphRoot.
    pub fn with_statvfs(mut self, statvfs: impl Statvfs + 'static) -> Self {
        self.statvfs = Arc::new(statvfs);
        self
    }

    fn api_timeout(&self) -> Duration {
        Duration::from_millis(self.config.api_timeout_ms)
    }

    async fn call(
        &self,
        method: DockerMethod,
        path: &str,
        content_type: Option<&'static str>,
        body: Zeroizing<Vec<u8>>,
        max_response_bytes: usize,
        timeout: Duration,
    ) -> Result<DockerResponse, CallError> {
        let request = DockerRequest {
            method,
            path: format!("{API_VERSION}{path}"),
            content_type,
            body,
            max_response_bytes,
        };
        transport::call(&self.transport, request, timeout).await
    }

    /// GET returning parsed JSON; anything but 200 is uncertainty.
    async fn get_json(&self, path: &str) -> Result<serde_json::Value, BackendError> {
        let response = self
            .call(
                DockerMethod::Get,
                path,
                None,
                Zeroizing::default(),
                MAX_JSON_BYTES,
                self.api_timeout(),
            )
            .await?;
        if response.status != 200 {
            return Err(BackendError::Uncertain(UnconfirmedReason::BackendFailure));
        }
        parse_json(&response.body)
    }
}

pub use transport::storage_shares_root;

pub fn require_enforced_limits(info: &Value) -> Result<(), BackendError> {
    transport::require_enforced_limits(info).map_err(BackendError::InvalidRequest)
}

fn parse_json(body: &[u8]) -> Result<serde_json::Value, BackendError> {
    serde_json::from_slice(body)
        .map_err(|_| BackendError::Uncertain(UnconfirmedReason::UnknownState))
}

/// Maps an observation failure to the reason a confirmation was withheld.
fn unconfirmed(error: BackendError) -> UnconfirmedReason {
    match error {
        BackendError::Uncertain(reason) => reason,
        BackendError::NotFound => UnconfirmedReason::NotFound,
        _ => UnconfirmedReason::UnknownState,
    }
}

/// What a by-name inspect saw: the entry, and whether a container exists.
struct Observed {
    entry: InventoryEntry,
    container: bool,
    exit: Option<RunnerExit>,
}

/// `/images/<reference>`: a reference's `/`, `:` and `@` stay as the daemon's
/// route expects; anything else outside the unreserved set is percent-encoded.
fn image_path(image: &str) -> String {
    let keep = |c: u8| c.is_ascii_alphanumeric() || b"-._~/:@".contains(&c);
    let encoded = image.bytes().map(|c| match keep(c) {
        true => (c as char).to_string(),
        false => format!("%{c:02X}"),
    });
    format!("/images/{}", encoded.collect::<String>())
}

impl<T: DockerTransport> DockerLocalBackend<T> {
    /// Sends a JSON (or empty) request under the API timeout.
    async fn send(
        &self,
        method: DockerMethod,
        path: &str,
        body: Option<&Value>,
    ) -> Result<DockerResponse, BackendError> {
        self.send_with(method, path, body, self.api_timeout()).await
    }

    async fn send_with(
        &self,
        method: DockerMethod,
        path: &str,
        body: Option<&Value>,
        timeout: Duration,
    ) -> Result<DockerResponse, BackendError> {
        let (content_type, bytes) = match body {
            Some(value) => (
                Some("application/json"),
                Zeroizing::new(serde_json::to_vec(value).expect("JSON value serializes")),
            ),
            None => (None, Zeroizing::default()),
        };
        Ok(self
            .call(method, path, content_type, bytes, MAX_JSON_BYTES, timeout)
            .await?)
    }

    /// GET returning `None` only on an authoritative 404.
    async fn get_optional(&self, path: &str) -> Result<Option<Value>, BackendError> {
        let response = self.send(DockerMethod::Get, path, None).await?;
        match response.status {
            200 => parse_json(&response.body).map(Some),
            404 => Ok(None),
            _ => Err(BackendError::Uncertain(UnconfirmedReason::BackendFailure)),
        }
    }

    /// Inspects the container and volume by deterministic name and verifies
    /// that every label present names exactly this sandbox.
    async fn observe(&self, sandbox: &SandboxIdentity) -> Result<Observed, BackendError> {
        if sandbox.kind != BackendKind::DockerLocal
            || sandbox.backend_id != sandbox.owner.deterministic_name()
        {
            return Err(BackendError::InvalidRequest("not a docker-local sandbox"));
        }
        let name = &sandbox.backend_id;
        let unknown = BackendError::Uncertain(UnconfirmedReason::UnknownState);
        let container = self
            .get_optional(&format!("/containers/{name}/json"))
            .await?;
        let volume = self
            .get_optional(&format!("/volumes/{}", volume_name(name)))
            .await?;
        let volume_entry = match &volume {
            Some(volume) => Some(entry_from_labels(
                name,
                &volume["Labels"],
                SandboxState::Stopped,
                Some(0),
                true,
            )?),
            None => None,
        };
        if volume_entry
            .as_ref()
            .is_some_and(|entry| entry.sandbox != *sandbox)
        {
            return Err(unknown);
        }
        let mut exit = None;
        let entry = match &container {
            Some(container) => {
                let state = &container["State"];
                if state["Status"] == "exited" && state["Running"] == false {
                    exit = if state["OOMKilled"] == true {
                        Some(RunnerExit::Signalled)
                    } else {
                        state["ExitCode"]
                            .as_i64()
                            .and_then(|code| i32::try_from(code).ok())
                            .map(RunnerExit::Exited)
                    };
                }
                let status = state["Status"].as_str().unwrap_or_default();
                let running = state["Running"].as_bool().unwrap_or(true);
                let (state, processes) = container_state(status, running);
                entry_from_labels(
                    name,
                    &container["Config"]["Labels"],
                    state,
                    processes,
                    volume.is_some(),
                )?
            }
            None => volume_entry.ok_or(BackendError::NotFound)?,
        };
        if entry.sandbox != *sandbox {
            return Err(unknown);
        }
        Ok(Observed {
            entry,
            container: container.is_some(),
            exit,
        })
    }

    /// Uses the existing work volume when its labels are identical; creates it
    /// otherwise. Any other occupant of the name is a conflict.
    async fn ensure_volume(
        &self,
        request: &CreateRequest,
        labels: &BTreeMap<String, String>,
    ) -> Result<(), BackendError> {
        let volume = volume_name(&request.name);
        let path = format!("/volumes/{volume}");
        let check = |found: Option<Value>| match found {
            Some(found) if labels_match(&found["Labels"], labels) => Ok(()),
            _ => Err(BackendError::Conflict),
        };
        if let Some(found) = self.get_optional(&path).await? {
            return check(Some(found));
        }
        let body = volume_body(&self.config, request, labels);
        let response = self
            .send(DockerMethod::Post, "/volumes/create", Some(&body))
            .await?;
        match response.status {
            200 | 201 | 409 => check(self.get_optional(&path).await?),
            _ => Err(BackendError::Uncertain(UnconfirmedReason::BackendFailure)),
        }
    }

    /// Pulls an allowlisted template by digest and proves it is present.
    /// The pull stream reports errors in-band, so only the inspect counts.
    pub async fn pull_template(&mut self, digest: &TemplateDigest) -> Result<(), BackendError> {
        let image = self
            .config
            .template_image(digest)
            .ok_or(BackendError::InvalidRequest(
                "template digest not allowlisted",
            ))?
            .to_string();
        let path = format!("/images/create?fromImage={}", encode(&image));
        self.send_with(DockerMethod::Post, &path, None, PULL_TIMEOUT)
            .await?;
        match self
            .get_optional(&format!("{}/json", image_path(&image)))
            .await?
        {
            Some(_) => Ok(()),
            None => Err(BackendError::Uncertain(UnconfirmedReason::BackendFailure)),
        }
    }

    /// Spec 0017 §6.7: removes a template image no container (running, stopped
    /// or preserved) uses. A template this configuration allowlists is never removed.
    #[cfg(feature = "project_environments")]
    pub async fn remove_template(
        &mut self,
        digest: &TemplateDigest,
        image: &str,
    ) -> Result<aq_environment_builder::node::Removal, BackendError> {
        use aq_environment_builder::node::Removal;
        if self.config.template_image(digest).is_some() {
            return Ok(Removal::Kept);
        }
        // The image comes from the server: checked before any daemon call.
        if !config::pinned_reference(image, digest) {
            return Err(BackendError::InvalidRequest(
                "template image must be a reference pinned to its digest",
            ));
        }
        let path = image_path(image);
        let Some(found) = self.get_optional(&format!("{path}/json")).await? else {
            return Ok(Removal::Removed);
        };
        let containers = self.get_json("/containers/json?all=true").await?;
        let unknown = BackendError::Uncertain(UnconfirmedReason::UnknownState);
        let uses = |c: &Value| {
            [&c["Image"], &c["ImageID"]]
                .iter()
                .any(|v| **v == image || **v == found["Id"])
        };
        if containers.as_array().ok_or(unknown)?.iter().any(uses) {
            return Ok(Removal::InUse);
        }
        match self.send(DockerMethod::Delete, &path, None).await?.status {
            200 | 404 => Ok(Removal::Removed),
            409 => Ok(Removal::InUse),
            _ => Err(BackendError::Uncertain(UnconfirmedReason::BackendFailure)),
        }
    }

    /// Reads one regular file from a sandbox container, bounded by
    /// `max_read_bytes`. A volume-only sandbox has no container to read from.
    pub async fn read_file(
        &mut self,
        sandbox: &SandboxIdentity,
        path: &str,
    ) -> Result<Vec<u8>, BackendError> {
        if !self.observe(sandbox).await?.container {
            return Err(BackendError::NotFound);
        }
        let too_large = BackendError::InvalidRequest("archive exceeds read cap");
        let limit = usize::try_from(self.config.max_read_bytes)
            .unwrap_or(usize::MAX)
            .saturating_add(READ_ARCHIVE_SLACK);
        let request = format!(
            "/containers/{}/archive?path={}",
            sandbox.backend_id,
            encode(path)
        );
        let response = match self
            .call(
                DockerMethod::Get,
                &request,
                None,
                Zeroizing::default(),
                limit,
                self.api_timeout(),
            )
            .await
        {
            Err(CallError::TooLarge) => return Err(too_large),
            other => other?,
        };
        match response.status {
            200 => tar::single_file(&response.body, self.config.max_read_bytes),
            404 => Err(BackendError::NotFound),
            _ => Err(BackendError::Uncertain(UnconfirmedReason::BackendFailure)),
        }
    }

    /// Uploads a non-secret ustar archive, extracted at `/`, into a created
    /// (not yet started) sandbox container. Secrets use
    /// `deliver_private_material` instead.
    pub async fn put_archive(
        &mut self,
        sandbox: &SandboxIdentity,
        archive: Vec<u8>,
    ) -> Result<(), BackendError> {
        self.put_preparation_archive(
            sandbox,
            Zeroizing::new(archive),
            self.api_timeout().max(Duration::from_secs(60)),
        )
        .await
    }

    async fn observe_preparation(
        &self,
        sandbox: &SandboxIdentity,
    ) -> Result<Observed, BackendError> {
        match self.observe(sandbox).await {
            Err(BackendError::Uncertain(UnconfirmedReason::Timeout)) => self.observe(sandbox).await,
            result => result,
        }
    }

    async fn put_preparation_archive(
        &self,
        sandbox: &SandboxIdentity,
        archive: Zeroizing<Vec<u8>>,
        timeout: Duration,
    ) -> Result<(), BackendError> {
        let observed = self.observe_preparation(sandbox).await?;
        if !observed.container || observed.entry.state != SandboxState::Created {
            return Err(BackendError::InvalidState);
        }
        let path = format!("/containers/{}/archive?path=%2F", sandbox.backend_id);
        let response = self
            .call(
                DockerMethod::Put,
                &path,
                Some("application/x-tar"),
                archive,
                MAX_JSON_BYTES,
                timeout,
            )
            .await?;
        match response.status {
            200 => Ok(()),
            _ => Err(BackendError::Uncertain(UnconfirmedReason::BackendFailure)),
        }
    }

    /// Reads `path` (a file or directory) from a sandbox container as a raw
    /// ustar archive, bounded by `max_bytes`. Works on stopped containers.
    pub async fn get_archive(
        &mut self,
        sandbox: &SandboxIdentity,
        path: &str,
        max_bytes: usize,
    ) -> Result<Vec<u8>, BackendError> {
        if !self.observe(sandbox).await?.container {
            return Err(BackendError::NotFound);
        }
        let path = format!(
            "{API_VERSION}/containers/{}/archive?path={}",
            sandbox.backend_id,
            encode(path)
        );
        self.download_archive(path, max_bytes)
            .await?
            .ok_or(BackendError::NotFound)
    }

    pub async fn stopped_work_tree(
        &self,
        sandbox: &SandboxIdentity,
        max: u64,
    ) -> Result<Vec<tar::TreeEntry>, BackendError> {
        let observed = self.observe(sandbox).await?;
        if observed.entry.state != SandboxState::Stopped {
            return Err(BackendError::InvalidState);
        }
        let id = &sandbox.backend_id;
        let path = transport::workspace_archive_path(
            API_VERSION,
            id,
            &volume_name(id),
            observed.container,
        );
        let raw = self
            .download_archive(path, max.saturating_mul(2).try_into().unwrap_or(usize::MAX))
            .await?
            .ok_or(BackendError::NotFound)?;
        let after = self.observe(sandbox).await?;
        if after.entry.state != SandboxState::Stopped || after.container != observed.container {
            return Err(BackendError::InvalidState);
        }
        aq_sandbox_archive::tar::read_workspace_tree(&raw, max, observed.container)
            .map_err(|error| BackendError::InvalidRequest(error.0))
    }

    async fn download_archive(
        &self,
        path: String,
        max: usize,
    ) -> Result<Option<Vec<u8>>, BackendError> {
        transport::archive(&self.transport, path, max, self.api_timeout())
            .await
            .map_err(|error| match error {
                CallError::TooLarge => BackendError::InvalidRequest("archive exceeds read cap"),
                other => other.into(),
            })
    }

    /// Sets the size of a running sandbox's terminal (`runner_tty`).
    pub async fn resize(
        &mut self,
        sandbox: &SandboxIdentity,
        (columns, rows): (u16, u16),
    ) -> Result<(), BackendError> {
        let id = &sandbox.backend_id;
        let path = format!("/containers/{id}/resize?h={rows}&w={columns}");
        match self.send(DockerMethod::Post, &path, None).await?.status {
            200 | 201 => Ok(()),
            _ => Err(BackendError::Uncertain(UnconfirmedReason::BackendFailure)),
        }
    }

    /// statvfs of the daemon's storage root (`DockerRootDir`; Podman's
    /// libpod `store.graphRoot`). It must be an absolute host path.
    async fn graph_root_stats(&self) -> Option<crate::backend::FsStats> {
        let info = self.get_json("/info").await.ok()?;
        let root = info
            .get("DockerRootDir")
            .or_else(|| info.pointer("/store/graphRoot"))
            .and_then(Value::as_str)
            .map(Path::new)
            .filter(|root| root.is_absolute())?;
        self.statvfs.statvfs(root).ok()
    }

    /// Preserved containers' writable layers (`SizeRw`) plus their work
    /// volumes' `UsageData.Size`. Any unknown size fails the whole sum.
    async fn preserved_bytes(&self, preserved: &[SandboxIdentity]) -> Result<u64, BackendError> {
        if preserved.is_empty() {
            return Ok(0);
        }
        let unknown = BackendError::Uncertain(UnconfirmedReason::UnknownState);
        let df = self.get_json("/system/df?type=volume").await?;
        let mut volumes = BTreeMap::new();
        for volume in df["Volumes"].as_array().ok_or(unknown.clone())? {
            let name = volume["Name"].as_str().ok_or(unknown.clone())?;
            volumes.insert(name.to_owned(), volume["UsageData"]["Size"].as_i64());
        }
        let mut total = 0u64;
        for sandbox in preserved {
            if sandbox.kind != BackendKind::DockerLocal
                || sandbox.backend_id != sandbox.owner.deterministic_name()
            {
                return Err(BackendError::InvalidRequest("not a docker-local sandbox"));
            }
            let name = &sandbox.backend_id;
            if let Some(container) = self
                .get_optional(&format!("/containers/{name}/json?size=true"))
                .await?
            {
                let size = container["SizeRw"].as_u64().ok_or(unknown.clone())?;
                total = total.saturating_add(size);
            }
            match volumes.get(&volume_name(name)) {
                Some(Some(size)) if *size >= 0 => total = total.saturating_add(*size as u64),
                _ => return Err(unknown),
            }
        }
        Ok(total)
    }

    async fn confirm_absent(&self, path: &str) -> bool {
        matches!(self.get_optional(path).await, Ok(None))
    }
}

impl<T: DockerTransport> PlacementBackend for DockerLocalBackend<T> {
    fn kind(&self) -> BackendKind {
        BackendKind::DockerLocal
    }

    fn capabilities(&self) -> Vec<Capability> {
        vec![Capability::PersistentDiskDuringAttempt]
    }

    async fn create(&mut self, request: &CreateRequest) -> Result<SandboxIdentity, BackendError> {
        request.validate()?;
        let resources = request.resources.ok_or(BackendError::InvalidRequest(
            "docker-local requires reservation limits",
        ))?;
        let image = self
            .config
            .template_image(&request.template_digest)
            .ok_or(BackendError::InvalidRequest(
                "template digest not allowlisted",
            ))?
            .to_string();
        let mut seen = BTreeSet::new();
        for alias in &request.mount_aliases {
            if !self.config.mount_aliases.contains_key(alias) {
                return Err(BackendError::InvalidRequest("undeclared mount alias"));
            }
            if !seen.insert(alias) {
                return Err(BackendError::InvalidRequest("duplicate mount alias"));
            }
        }
        if request.agent_channel {
            let source = self.config.channel_root.as_ref().map(|root| {
                std::fs::symlink_metadata(root.join(request.owner.attempt_id.to_string()))
            });
            if !matches!(source, Some(Ok(meta)) if meta.is_dir()) {
                return Err(BackendError::InvalidRequest(
                    "agent channel directory absent (agent_channel_unavailable)",
                ));
            }
        }
        if let Some((host, _)) = &self.config.transcript {
            use std::os::unix::fs::MetadataExt;
            // SAFETY: geteuid has no preconditions and cannot fail.
            let own = |m: std::fs::Metadata| {
                m.is_dir() && m.uid() == unsafe { libc::geteuid() } && m.mode() & 0o777 == 0o700
            };
            if !std::fs::symlink_metadata(host).is_ok_and(own) {
                return Err(BackendError::InvalidRequest(
                    "transcript directory must be the node's own 0700 directory",
                ));
            }
        }
        let supported = self.capabilities();
        if let Some(capability) = request
            .required_capabilities
            .iter()
            .find(|capability| !supported.contains(capability))
        {
            return Err(BackendError::UnsupportedCapability(*capability));
        }
        let identity = SandboxIdentity {
            kind: BackendKind::DockerLocal,
            backend_id: request.name.clone(),
            owner: request.owner.clone(),
        };
        let labels = policy::labels(request);
        let name = &request.name;
        let inspect = format!("/containers/{name}/json");
        let same = |found: Option<Value>| match found {
            Some(found) if labels_match(&found["Config"]["Labels"], &labels) => {
                Ok(identity.clone())
            }
            _ => Err(BackendError::Conflict),
        };
        if let Some(found) = self.get_optional(&inspect).await? {
            return same(Some(found));
        }
        self.ensure_volume(request, &labels).await?;
        let body = create_body(&self.config, request, &image, resources);
        let path = format!("/containers/create?name={}", encode(name));
        let response = self.send(DockerMethod::Post, &path, Some(&body)).await?;
        match response.status {
            201 => Ok(identity),
            409 => same(self.get_optional(&inspect).await?),
            404 => Err(BackendError::InvalidRequest(
                "template image not present on sandbox daemon",
            )),
            _ => Err(BackendError::Uncertain(UnconfirmedReason::BackendFailure)),
        }
    }

    async fn deliver_private_material(
        &mut self,
        sandbox: &SandboxIdentity,
        material: PrivateMaterial,
    ) -> Result<(), BackendError> {
        let archive = tar::material_archive(
            &self.config.material_path,
            material.expose_for_control_channel(),
            self.config.material_uid,
            self.config.material_gid,
        )?;
        drop(material);
        self.put_preparation_archive(sandbox, archive, self.api_timeout())
            .await
    }

    async fn start_runner(&mut self, sandbox: &SandboxIdentity) -> Result<(), BackendError> {
        let observed = self.observe_preparation(sandbox).await?;
        match observed.entry.state {
            SandboxState::Created if observed.container => {}
            SandboxState::Running => return Ok(()),
            _ => return Err(BackendError::InvalidState),
        }
        let path = format!("/containers/{}/start", sandbox.backend_id);
        let response = match self.send(DockerMethod::Post, &path, None).await {
            // The start may have applied. A read can confirm it without risking
            // starting a second runner after a first runner exits.
            Err(BackendError::Uncertain(_))
                if self
                    .observe_preparation(sandbox)
                    .await
                    .is_ok_and(|seen| seen.entry.state == SandboxState::Running) =>
            {
                return Ok(());
            }
            result => result?,
        };
        match response.status {
            204 | 304 => Ok(()),
            404 => Err(BackendError::NotFound),
            _ => Err(BackendError::Uncertain(UnconfirmedReason::BackendFailure)),
        }
    }

    async fn inspect(&mut self, sandbox: &SandboxIdentity) -> Result<InventoryEntry, BackendError> {
        Ok(self.observe(sandbox).await?.entry)
    }

    async fn runner_exit(&mut self, sandbox: &SandboxIdentity) -> Result<RunnerExit, BackendError> {
        let observed = self.observe(sandbox).await?;
        match (observed.entry.state, observed.exit) {
            (SandboxState::Stopped, Some(exit)) => Ok(exit),
            (SandboxState::Stopped, None) => {
                Err(BackendError::Uncertain(UnconfirmedReason::UnknownState))
            }
            _ => Err(BackendError::InvalidState),
        }
    }

    async fn list_owned(&mut self, node_id: Uuid) -> Result<Vec<InventoryEntry>, BackendError> {
        let unknown = BackendError::Uncertain(UnconfirmedReason::UnknownState);
        let filter = node_filter(node_id);
        let containers = self
            .get_json(&format!("/containers/json?all=true&filters={filter}"))
            .await?;
        let volumes = self.get_json(&format!("/volumes?filters={filter}")).await?;
        let containers = containers.as_array().ok_or(unknown.clone())?;
        let volumes = match &volumes["Volumes"] {
            Value::Null => &Vec::new(),
            Value::Array(volumes) => volumes,
            _ => return Err(unknown),
        };
        let mut volume_labels = BTreeMap::new();
        for volume in volumes {
            let name = volume["Name"].as_str().ok_or(unknown.clone())?;
            volume_labels.insert(name.to_string(), &volume["Labels"]);
        }
        let mut entries = BTreeMap::new();
        for container in containers {
            let name = container["Names"][0]
                .as_str()
                .map(|name| name.trim_start_matches('/'))
                .ok_or(unknown.clone())?;
            let status = container["State"].as_str().ok_or(unknown.clone())?;
            let (state, processes) = container_state(status, status == "running");
            let disk_present = volume_labels.contains_key(&volume_name(name));
            let entry =
                entry_from_labels(name, &container["Labels"], state, processes, disk_present)?;
            entries.insert(name.to_string(), entry);
        }
        for (volume, labels) in volume_labels {
            let name = volume.strip_suffix("-work").ok_or(unknown.clone())?;
            if !entries.contains_key(name) {
                let entry = entry_from_labels(name, labels, SandboxState::Stopped, Some(0), true)?;
                entries.insert(name.to_string(), entry);
            }
        }
        if entries
            .values()
            .any(|entry| entry.sandbox.owner.node_id != node_id)
        {
            return Err(unknown);
        }
        Ok(entries.into_values().collect())
    }

    fn disk_limit(&self) -> DiskLimitStatus {
        DiskLimitStatus {
            mode: Some(if self.config.hard_disk_limit {
                DiskLimitMode::Hard
            } else {
                DiskLimitMode::Watchdog
            }),
            shared_root_fs: self.shared_root_fs,
            watch_interval_ms: self.config.disk_watch_interval_ms,
        }
    }

    /// SBX20 sample: container writable layer (`SizeRw`) plus the work
    /// volume's `UsageData.Size`. A volume size the daemon has not computed
    /// (-1 or absent) counts as zero; a running container without `SizeRw`
    /// is an incomplete sample and therefore Err.
    async fn disk_usage(&mut self, node_id: Uuid) -> Result<Vec<DiskUsage>, BackendError> {
        let unknown = BackendError::Uncertain(UnconfirmedReason::UnknownState);
        let filter = node_filter(node_id);
        let containers = self
            .get_json(&format!(
                "/containers/json?all=true&size=true&filters={filter}"
            ))
            .await?;
        let containers = containers.as_array().ok_or(unknown.clone())?;
        if !containers.iter().any(|c| c["State"] == "running") {
            return Ok(Vec::new());
        }
        let df = self.get_json("/system/df?type=volume").await?;
        let mut volumes = BTreeMap::new();
        match &df["Volumes"] {
            Value::Null => {}
            Value::Array(list) => {
                for volume in list {
                    let name = volume["Name"].as_str().ok_or(unknown.clone())?;
                    if let Some(size) = volume["UsageData"]["Size"].as_u64() {
                        volumes.insert(name.to_string(), size);
                    }
                }
            }
            _ => return Err(unknown),
        }
        let mut usage = Vec::new();
        for container in containers {
            if container["State"].as_str() != Some("running") {
                continue;
            }
            let name = container["Names"][0]
                .as_str()
                .map(|name| name.trim_start_matches('/'))
                .ok_or(unknown.clone())?;
            let labels = &container["Labels"];
            let entry = entry_from_labels(name, labels, SandboxState::Running, None, true)?;
            if entry.sandbox.owner.node_id != node_id {
                return Err(unknown);
            }
            let limit_bytes =
                disk_limit_from_labels(&self.config, labels).ok_or(unknown.clone())?;
            let layer = container["SizeRw"].as_u64().ok_or(unknown.clone())?;
            let volume = volumes.get(&volume_name(name)).copied().unwrap_or(0);
            usage.push(DiskUsage {
                sandbox: entry.sandbox,
                used_bytes: layer.saturating_add(volume),
                limit_bytes,
            });
        }
        Ok(usage)
    }

    async fn stop(&mut self, sandbox: &SandboxIdentity) -> Confirmation<StoppedWriterProof> {
        let observed = match self.observe(sandbox).await {
            Ok(observed) => observed,
            Err(error) => return Confirmation::Unconfirmed(unconfirmed(error)),
        };
        let name = &sandbox.backend_id;
        if observed.container {
            let response = if observed.entry.state == SandboxState::Created {
                // Never started: remove the container without starting it.
                // The work volume is a separate object and stays.
                self.send(DockerMethod::Delete, &format!("/containers/{name}"), None)
                    .await
            } else {
                let grace = self.config.stop_grace_secs;
                let timeout = self.api_timeout() + Duration::from_secs(grace.into());
                let path = format!("/containers/{name}/stop?t={grace}");
                self.send_with(DockerMethod::Post, &path, None, timeout)
                    .await
            };
            match response {
                Err(error) => return Confirmation::Unconfirmed(unconfirmed(error)),
                Ok(response) if matches!(response.status, 204 | 304 | 404) => {}
                Ok(response) if response.status == 409 => {
                    return Confirmation::Unconfirmed(UnconfirmedReason::StillRunning);
                }
                Ok(_) => return Confirmation::Unconfirmed(UnconfirmedReason::BackendFailure),
            }
        }
        match self.observe(sandbox).await {
            Err(error) => Confirmation::Unconfirmed(unconfirmed(error)),
            Ok(observed) if observed.entry.state == SandboxState::Running => {
                Confirmation::Unconfirmed(UnconfirmedReason::StillRunning)
            }
            Ok(observed) => observed.entry.writer_proof(),
        }
    }

    async fn destroy(
        &mut self,
        sandbox: &SandboxIdentity,
        authorization: &DestroyAuthorization,
    ) -> Confirmation<StoppedWriterProof> {
        if !authorization.authorizes(sandbox) {
            return Confirmation::Unconfirmed(UnconfirmedReason::Unauthorized);
        }
        if let Confirmation::Unconfirmed(reason) = self.stop(sandbox).await {
            return Confirmation::Unconfirmed(reason);
        }
        let container = format!("/containers/{}", sandbox.backend_id);
        let volume = format!("/volumes/{}", volume_name(&sandbox.backend_id));
        for path in [&container, &volume] {
            match self.send(DockerMethod::Delete, path, None).await {
                Err(error) => return Confirmation::Unconfirmed(unconfirmed(error)),
                Ok(response) if matches!(response.status, 204 | 404) => {}
                Ok(_) => return Confirmation::Unconfirmed(UnconfirmedReason::BackendFailure),
            }
        }
        let container_gone = self.confirm_absent(&format!("{container}/json")).await;
        if container_gone && self.confirm_absent(&volume).await {
            Confirmation::Confirmed(StoppedWriterProof::destroyed(sandbox.clone()))
        } else {
            Confirmation::Unconfirmed(UnconfirmedReason::UnknownState)
        }
    }
    async fn measured_disk_usage(&mut self, preserved: &[SandboxIdentity]) -> BackendDiskUsage {
        BackendDiskUsage {
            filesystem: self.graph_root_stats().await,
            preserved_bytes: self.preserved_bytes(preserved).await.ok(),
            ..BackendDiskUsage::default()
        }
    }
}
