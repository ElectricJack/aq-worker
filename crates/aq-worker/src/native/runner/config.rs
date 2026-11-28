//! Operator-installed configuration, never received from a task.
use super::*;
use crate::native::adapter::{AdapterManifest, ExpectedAdapter, InstalledAdapter};
use aq_admission::{scoped_presence, transport::Feature};
use aq_machine_load::AdmissionDeferral;
use aq_session_protocol::{ArtifactRef, Capability, Digest, Validate};
use serde::Deserialize;
use std::collections::BTreeSet;
use std::{
    fs::{Metadata, OpenOptions},
    io::Read,
    os::unix::fs::{MetadataExt, OpenOptionsExt},
    path::{Path, PathBuf},
};

/// Exact configuration selection. This value is not runtime eligibility.
#[derive(Clone, Debug)]
pub enum ConfiguredPreparation {
    Frozen(aq_admission::preparation_v2::Negotiation),
    Refresh(aq_admission::preparation_refresh_v1::Negotiation),
}
impl From<aq_admission::preparation_v2::Negotiation> for ConfiguredPreparation {
    fn from(value: aq_admission::preparation_v2::Negotiation) -> Self {
        Self::Frozen(value)
    }
}
impl From<aq_admission::preparation_refresh_v1::Negotiation> for ConfiguredPreparation {
    fn from(value: aq_admission::preparation_refresh_v1::Negotiation) -> Self {
        Self::Refresh(value)
    }
}
impl<'de> Deserialize<'de> for ConfiguredPreparation {
    fn deserialize<D: serde::Deserializer<'de>>(
        deserializer: D,
    ) -> std::result::Result<Self, D::Error> {
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct Envelope {
            contract: ArtifactRef,
            features: Vec<String>,
        }
        let wire = Envelope::deserialize(deserializer)?;
        let old =
            aq_admission::preparation_v2::contract_artifact().map_err(serde::de::Error::custom)?;
        let old_long = aq_admission::preparation_v2::long_contract_artifact()
            .map_err(serde::de::Error::custom)?;
        let refresh = aq_admission::preparation_refresh_v1::contract_artifact()
            .map_err(serde::de::Error::custom)?;
        let refresh_long = aq_admission::preparation_refresh_v1::long_contract_artifact()
            .map_err(serde::de::Error::custom)?;
        let features = serde_json::to_value(wire.features).map_err(serde::de::Error::custom)?;
        let selected = if wire.contract == old || wire.contract == old_long {
            Self::Frozen(aq_admission::preparation_v2::Negotiation {
                contract: wire.contract,
                features: serde_json::from_value(features).map_err(serde::de::Error::custom)?,
            })
        } else if wire.contract == refresh || wire.contract == refresh_long {
            Self::Refresh(aq_admission::preparation_refresh_v1::Negotiation {
                contract: wire.contract,
                features: serde_json::from_value(features).map_err(serde::de::Error::custom)?,
            })
        } else {
            return Err(serde::de::Error::custom(
                "unsupported exact native preparation contract",
            ));
        };
        selected.validate().map_err(serde::de::Error::custom)?;
        Ok(selected)
    }
}
impl serde::Serialize for ConfiguredPreparation {
    fn serialize<S: serde::Serializer>(
        &self,
        serializer: S,
    ) -> std::result::Result<S::Ok, S::Error> {
        match self {
            Self::Frozen(n) => serde::Serialize::serialize(n, serializer),
            Self::Refresh(n) => serde::Serialize::serialize(n, serializer),
        }
    }
}
impl ConfiguredPreparation {
    pub fn validate(&self) -> Result<()> {
        match self {
            Self::Frozen(n) => n.validate()?,
            Self::Refresh(n) => n.validate()?,
        };
        Ok(())
    }
    pub(super) fn profile(&self) -> super::profile::TerminalProfile {
        match self {
            Self::Frozen(_) => super::profile::TerminalProfile::Frozen,
            Self::Refresh(_) => super::profile::TerminalProfile::Refresh,
        }
    }
    fn maximum_preparation_ms(&self) -> u64 {
        match self {
            Self::Frozen(_) => aq_admission::preparation_v2::MAX_PREPARATION_MS,
            Self::Refresh(_) => aq_admission::preparation_refresh_v1::MAX_PREPARATION_MS,
        }
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NativeConfig {
    pub schema_version: u32,
    pub state_dir: PathBuf,
    pub git_socket: PathBuf,
    pub git_root: PathBuf,
    pub capacity: Limits,
    pub runtime: RuntimeIdentity,
    pub adapter: ArtifactRef,
    pub renderer: ArtifactRef,
    pub adapter_manifest: PathBuf,
    pub adapter_manifest_digest: Digest,
    pub capabilities: Vec<Capability>,
    pub features: Vec<Feature>,
    pub negotiation: ConfiguredPreparation,
    pub source_profile: ArtifactRef,
    pub maximum_preparation_ms: u64,
    pub inventory_definition: PathBuf,
    pub inventory_reference: ArtifactRef,
    pub inventory_adapter: ArtifactRef,
    pub inventory_evidence: ArtifactRef,
    pub harness_qualifications: Vec<ArtifactRef>,
    pub inventory_ttl_ms: u64,
    #[serde(default)]
    pub executor: Option<super::codex::ExecutorConfig>,
    /// Spec 0016 §5.3: the node's per-project repositories and its git.
    #[serde(default)]
    pub node_git: Option<aq_node_git::Config>,
}
fn file_version(file: &Metadata) -> (u64, i64, i64, i64, i64) {
    (
        file.len(),
        file.mtime(),
        file.mtime_nsec(),
        file.ctime(),
        file.ctime_nsec(),
    )
}
fn validate_private_metadata(path: &Path, before: &Metadata, maximum: usize) -> Result<()> {
    let why = match (before.mode() & 0o7777, before.len()) {
        _ if !before.is_file() => "not a regular file".into(),
        _ if before.uid() != unsafe { libc::geteuid() } => "not owned by you".into(),
        (mode, _) if mode != 0o600 => format!("mode {mode:04o}, 0600 is required"),
        (_, len) if !(1..=maximum as u64).contains(&len) => format!("size {len}, max {maximum}"),
        _ => String::new(),
    };
    ensure!(why.is_empty(), "native config {}: {why}", path.display());
    Ok(())
}
pub(super) fn private_file(path: &Path, maximum: usize) -> Result<Vec<u8>> {
    ensure!(
        path.is_absolute() && path.canonicalize()? == path,
        "native configuration must be canonical"
    );
    let file = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK | libc::O_CLOEXEC)
        .open(path)?;
    let before = file.metadata()?;
    validate_private_metadata(path, &before, maximum)?;
    let mut bytes = Vec::new();
    (&file).take(maximum as u64 + 1).read_to_end(&mut bytes)?;
    let after = file.metadata()?;
    let named = std::fs::symlink_metadata(path)?;
    ensure!(
        bytes.len() <= maximum
            && before.len() == bytes.len() as u64
            && file_version(&before) == file_version(&after)
            && named.is_file()
            && (named.dev(), named.ino()) == (before.dev(), before.ino()),
        "native configuration changed"
    );
    Ok(bytes)
}
impl NativeConfig {
    /// Why the node cannot admit a sandbox now, if it cannot. A disk shortfall first
    /// reclaims stale build-cache keys and checks again.
    pub(super) async fn check_backend_admission(&self) -> Result<(), AdmissionDeferral> {
        // A node that cannot complete requests to the server cannot launch what it is offered.
        aq_node_service::transport_health::deferral().map_or(Ok(()), Err)?;
        use aq_sandbox_archive::{build_cache::BuildCacheConfig, docker_engine::admit_reclaiming};
        let Some(super::codex::ExecutorConfig::ClaudeDockerLocalResultLocal { settings, .. }) =
            &self.executor
        else {
            return Ok(());
        };
        let journal = fs2::available_space(self.state_dir.join("journal"));
        let minimum = crate::config::MINIMUM_FREE_JOURNAL_BYTES;
        AdmissionDeferral::check_free(journal, minimum, "native_journal")?;
        let reclaim = || BuildCacheConfig::reclaim_off_runtime(settings.host.build_cache.clone());
        let docker = &settings.host.docker_local;
        Ok(admit_reclaiming(|| docker.check_admission(), reclaim).await?)
    }

    /// The digest of the template image this node launches, as its sandbox daemon resolves it:
    /// `None` without a docker-local template, or while that image is not present there.
    pub(super) async fn resolved_launch_template(&self) -> Option<crate::backend::TemplateDigest> {
        use crate::backend::docker_local::DockerLocalBackend as Docker;
        let Some(super::codex::ExecutorConfig::ClaudeDockerLocalResultLocal { settings, .. }) =
            &self.executor
        else {
            return None;
        };
        let (docker, template) = (&settings.host.docker_local, &settings.host.template);
        let backend = Docker::connect_unix(docker.clone());
        let resolved = async { backend.await?.resolve_template(template).await }.await;
        if !matches!(resolved, Ok(Some(_))) {
            let template = template.as_str();
            tracing::warn!(
                template,
                ?resolved,
                "launch template image not resolved: none reported"
            );
        }
        resolved.ok().flatten()
    }

    /// The slot's aq-git and, when `node_git` is configured, the node's git with its remote.
    pub fn git_client(&self, worker: &crate::config::WorkerConfig) -> Result<GitClient> {
        let mut git = GitClient::new(&self.git_socket, &self.git_root)?;
        if let Some(node) = &self.node_git {
            let cf = worker
                .cf_client_id
                .as_deref()
                .zip(worker.cf_client_secret.as_deref());
            let remote = aq_node_git::Remote::new(&worker.server_url, &worker.credential, cf);
            git.node = Some(node.node(remote).into());
        }
        Ok(git)
    }
    pub fn load(path: &Path) -> Result<Self> {
        let bytes = private_file(path, 65536)?;
        let config: Self =
            serde_json::from_slice(&bytes).context("invalid native configuration")?;
        config.validate()?;
        Ok(config)
    }
    pub fn validate(&self) -> Result<()> {
        ensure!(
            self.schema_version == 1
                && (1..=aq_protocol::MAX_NODE_SLOTS).contains(&self.capacity.slots)
                && self.capacity.memory_bytes > 0
                && self.capacity.cpu_millis > 0
                && self.capacity.disk_bytes > 0,
            "native capacity/version"
        );
        self.validate_storage()?;
        self.validate_preparation()?;
        self.validate_references()
    }
    fn validate_storage(&self) -> Result<()> {
        ensure!(
            self.state_dir.is_absolute()
                && self.state_dir.canonicalize()? == self.state_dir
                && self.git_root.starts_with(&self.state_dir)
                && self.git_root != self.state_dir
                && self.git_socket.is_absolute(),
            "native storage scope"
        );
        let m = std::fs::symlink_metadata(&self.state_dir)?;
        ensure!(
            m.is_dir() && m.uid() == unsafe { libc::geteuid() } && m.mode() & 0o7777 == 0o700,
            "native private state owner"
        );
        Ok(())
    }
    fn validate_preparation(&self) -> Result<()> {
        self.negotiation.validate()?;
        ensure!(
            (1..=self.negotiation.maximum_preparation_ms()).contains(&self.maximum_preparation_ms),
            "native preparation maximum"
        );
        ensure!(
            self.capabilities.len() <= 64
                && self.features.len() <= 4
                && !self.harness_qualifications.is_empty()
                && self.harness_qualifications.len() <= 32,
            "native advertised bounds"
        );
        ensure!(
            self.features.iter().copied().collect::<BTreeSet<_>>().len() == self.features.len(),
            "duplicate native features"
        );
        ensure!(
            self.harness_qualifications
                .iter()
                .map(|r| r.digest.as_str())
                .collect::<BTreeSet<_>>()
                .len()
                == self.harness_qualifications.len(),
            "duplicate native harness qualifications"
        );
        Ok(())
    }
    fn validate_references(&self) -> Result<()> {
        aq_session_protocol::require_capabilities(&[], &self.capabilities)?;
        for c in &self.capabilities {
            c.validate()?;
        }
        for r in [
            &self.runtime.runtime,
            &self.runtime.qualification,
            &self.adapter,
            &self.renderer,
            &self.source_profile,
            &self.inventory_reference,
            &self.inventory_adapter,
            &self.inventory_evidence,
        ]
        .into_iter()
        .chain(self.harness_qualifications.iter())
        {
            r.id.validate()?;
            r.version.validate()?;
        }
        Ok(())
    }
    pub fn installed_adapter(&self) -> Result<InstalledAdapter> {
        self.validate()?;
        let manifest = AdapterManifest::decode(&private_file(&self.adapter_manifest, 16384)?)?;
        InstalledAdapter::open(
            manifest,
            ExpectedAdapter {
                manifest_digest: &self.adapter_manifest_digest,
                adapter: &self.adapter,
                runtime: &self.runtime.runtime,
                qualification: &self.runtime.qualification,
                renderer: &self.renderer,
            },
        )
    }
    pub(super) fn presence_profile(&self) -> Result<QualifiedPresenceProfile> {
        let bytes = private_file(&self.inventory_definition, 8192)?;
        let definition: scoped_presence::Qualification = serde_json::from_slice(&bytes)?;
        ensure!(
            definition.runtime == self.runtime.runtime
                && definition.runtime_qualification == self.runtime.qualification
                && (1000..=definition.maximum_ttl_ms).contains(&self.inventory_ttl_ms),
            "native inventory runtime/TTL"
        );
        QualifiedPresenceProfile::from_definition(
            self.inventory_reference.clone(),
            &bytes,
            &self.inventory_adapter,
            &self.inventory_evidence,
        )
    }
}
