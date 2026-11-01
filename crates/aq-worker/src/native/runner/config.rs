//! Operator-installed configuration, never received from a task.
use super::*;
use crate::native::adapter::{AdapterManifest, ExpectedAdapter, InstalledAdapter};
use aq_admission::{scoped_presence, transport::Feature};
use aq_session_protocol::{ArtifactRef, Capability, Digest, Validate};
use serde::Deserialize;
use std::collections::BTreeSet;
use std::{
    fs::OpenOptions,
    io::Read,
    os::unix::fs::{MetadataExt, OpenOptionsExt},
    path::{Path, PathBuf},
};

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
    pub negotiation: aq_admission::preparation_v2::Negotiation,
    pub source_profile: ArtifactRef,
    pub maximum_preparation_ms: u64,
    pub inventory_definition: PathBuf,
    pub inventory_reference: ArtifactRef,
    pub inventory_adapter: ArtifactRef,
    pub inventory_evidence: ArtifactRef,
    pub harness_qualifications: Vec<ArtifactRef>,
    pub inventory_ttl_ms: u64,
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
    ensure!(
        before.is_file()
            && before.uid() == unsafe { libc::geteuid() }
            && before.mode() & 0o7777 == 0o600
            && before.len() > 0
            && before.len() <= maximum as u64,
        "native configuration owner/mode/size"
    );
    let mut bytes = Vec::new();
    (&file).take(maximum as u64 + 1).read_to_end(&mut bytes)?;
    let after = file.metadata()?;
    let named = std::fs::symlink_metadata(path)?;
    ensure!(
        bytes.len() <= maximum
            && before.len() == bytes.len() as u64
            && after.len() == before.len()
            && before.mtime() == after.mtime()
            && before.mtime_nsec() == after.mtime_nsec()
            && before.ctime() == after.ctime()
            && before.ctime_nsec() == after.ctime_nsec()
            && named.is_file()
            && named.dev() == before.dev()
            && named.ino() == before.ino(),
        "native configuration changed"
    );
    Ok(bytes)
}
impl NativeConfig {
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
                && self.capacity.slots == 1
                && self.capacity.memory_bytes > 0
                && self.capacity.cpu_millis > 0
                && self.capacity.disk_bytes > 0,
            "native single-attempt capacity/version"
        );
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
        self.negotiation.validate()?;
        ensure!(
            (1..=aq_admission::preparation_v2::MAX_PREPARATION_MS)
                .contains(&self.maximum_preparation_ms),
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
