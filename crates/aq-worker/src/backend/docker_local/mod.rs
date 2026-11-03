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
pub mod transport;

pub use config::*;
pub use transport::*;

use crate::backend::{
    BackendError, BackendKind, Capability, ConfinementMode, Confirmation, CreateRequest,
    DestroyAuthorization, InventoryEntry, PlacementBackend, PrivateMaterial, SandboxIdentity,
    SandboxState, StoppedWriterProof, TemplateDigest, UnconfirmedReason,
};
use policy::{
    container_state, create_body, encode, entry_from_labels, labels_match, node_filter, volume_name,
};
use serde_json::{Value, json};
use std::{
    collections::{BTreeMap, BTreeSet},
    time::Duration,
};
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

#[derive(Debug)]
enum CallError {
    Timeout,
    Transport,
    TooLarge,
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
        let backend = Self { config, transport };
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
        Ok(backend)
    }

    pub fn config(&self) -> &DockerLocalConfig {
        &self.config
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
        match tokio::time::timeout(timeout, self.transport.send(request)).await {
            Err(_) => Err(CallError::Timeout),
            Ok(Err(error)) if error.kind == TransportErrorKind::TooLarge => {
                Err(CallError::TooLarge)
            }
            Ok(Err(_)) => Err(CallError::Transport),
            Ok(Ok(response)) => Ok(response),
        }
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
        let entry = match &container {
            Some(container) => {
                let state = &container["State"];
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
        })
    }

    /// Uses the existing work volume when its labels are identical; creates it
    /// otherwise. Any other occupant of the name is a conflict.
    async fn ensure_volume(
        &self,
        name: &str,
        labels: &BTreeMap<String, String>,
    ) -> Result<(), BackendError> {
        let volume = volume_name(name);
        let path = format!("/volumes/{volume}");
        let check = |found: Option<Value>| match found {
            Some(found) if labels_match(&found["Labels"], labels) => Ok(()),
            _ => Err(BackendError::Conflict),
        };
        if let Some(found) = self.get_optional(&path).await? {
            return check(Some(found));
        }
        let body = json!({ "Name": volume, "Driver": "local", "Labels": labels });
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
        match self.get_optional(&format!("/images/{image}/json")).await? {
            Some(_) => Ok(()),
            None => Err(BackendError::Uncertain(UnconfirmedReason::BackendFailure)),
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
        self.ensure_volume(name, &labels).await?;
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
        let observed = self.observe(sandbox).await?;
        if !observed.container || observed.entry.state != SandboxState::Created {
            return Err(BackendError::InvalidState);
        }
        let archive = tar::material_archive(
            &self.config.material_path,
            material.expose_for_control_channel(),
            self.config.material_uid,
            self.config.material_gid,
        )?;
        drop(material);
        let path = format!("/containers/{}/archive?path=%2F", sandbox.backend_id);
        let response = self
            .call(
                DockerMethod::Put,
                &path,
                Some("application/x-tar"),
                archive,
                MAX_JSON_BYTES,
                self.api_timeout(),
            )
            .await?;
        match response.status {
            200 => Ok(()),
            _ => Err(BackendError::Uncertain(UnconfirmedReason::BackendFailure)),
        }
    }

    async fn start_runner(&mut self, sandbox: &SandboxIdentity) -> Result<(), BackendError> {
        let observed = self.observe(sandbox).await?;
        match observed.entry.state {
            SandboxState::Created if observed.container => {}
            SandboxState::Running => return Ok(()),
            _ => return Err(BackendError::InvalidState),
        }
        let path = format!("/containers/{}/start", sandbox.backend_id);
        let response = self.send(DockerMethod::Post, &path, None).await?;
        match response.status {
            204 | 304 => Ok(()),
            404 => Err(BackendError::NotFound),
            _ => Err(BackendError::Uncertain(UnconfirmedReason::BackendFailure)),
        }
    }

    async fn inspect(&mut self, sandbox: &SandboxIdentity) -> Result<InventoryEntry, BackendError> {
        Ok(self.observe(sandbox).await?.entry)
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
}
