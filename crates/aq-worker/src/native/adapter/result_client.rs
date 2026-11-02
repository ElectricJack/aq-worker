//! Independently pinned readonly local-result client. Metadata is not a grant,
//! result-channel owner, process launcher or source of remote authority.
use super::*;
use aq_session_host::observed::{
    NamedReadonlyEnvironment, NamedReadonlyExecPool, NamedReadonlyFile,
};
use std::path::Path;

pub const MAX_RESULT_CLIENT_INSTALLATION_BYTES: u64 = 64 * 1024 * 1024;
#[derive(Clone, Debug, Serialize, Deserialize)]
pub enum ResultClientKind {
    #[serde(rename = "aq-agent-result-client-named-readonly-v1")]
    NamedReadonlyLocalV1,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ResultClientManifest {
    pub schema_version: u32,
    pub profile: ResultClientKind,
    pub contract: ArtifactRef,
    pub environment: ArtifactRef,
    pub executable: FilePin,
    pub executable_bytes: u64,
}
/// Operator-selected references, separate from the manifest's own assertions.
pub struct ExpectedResultClient<'a> {
    pub manifest_digest: &'a Digest,
    pub contract: &'a ArtifactRef,
    pub environment: &'a ArtifactRef,
    pub executable: &'a ArtifactRef,
}
impl ResultClientManifest {
    pub fn decode(bytes: &[u8]) -> Result<Self> {
        ensure!(
            !bytes.is_empty() && bytes.len() <= MAX_MANIFEST,
            "result client manifest bound"
        );
        Ok(serde_json::from_slice(bytes)?)
    }
    pub fn digest(&self) -> Result<Digest> {
        super::super::journal::canonical_digest(self)
    }
    pub fn artifact(&self) -> Result<ArtifactRef> {
        Ok(manifest_reference(self.digest()?))
    }
    fn validate_selection(
        &self,
        expected: &ExpectedResultClient<'_>,
        environment: &ArtifactRef,
    ) -> Result<()> {
        ensure!(
            self.schema_version == 1
                && self.digest()? == *expected.manifest_digest
                && (64..=MAX_RESULT_CLIENT_INSTALLATION_BYTES).contains(&self.executable_bytes)
                && self.contract == *expected.contract
                && self.contract == aq_agent_result_local::artifact()
                && self.environment == *expected.environment
                && self.environment == *environment
                && self.executable.artifact == *expected.executable
                && self.executable.path.is_absolute()
                && self.executable.path.as_os_str().len() <= 2048
                && !self
                    .executable
                    .path
                    .as_os_str()
                    .as_encoded_bytes()
                    .contains(&0),
            "result client independent contract/environment/image/byte pins"
        );
        for reference in [&self.contract, &self.environment, &self.executable.artifact] {
            reference.id.validate()?;
            reference.version.validate()?;
        }
        Ok(())
    }
}
/// No Serialize, Debug, constructor from history, or process/FD accessor.
/// A dedicated, explicitly charged <=64MiB pool holds exactly one client file;
/// this does not borrow the Codex installation's image budget or entry capacity.
pub struct InstalledResultClient {
    manifest: ResultClientManifest,
    digest: Digest,
    image: NamedReadonlyFile,
}
impl InstalledResultClient {
    pub fn open(
        manifest: ResultClientManifest,
        expected: ExpectedResultClient<'_>,
        environment: &NamedReadonlyEnvironment,
    ) -> Result<Self> {
        manifest.validate_selection(&expected, environment.qualification())?;
        let pool = NamedReadonlyExecPool::new(
            MAX_RESULT_CLIENT_INSTALLATION_BYTES,
            1,
            environment.clone(),
        )?;
        let image =
            pool.load_auxiliary_verified(&manifest.executable.path, &manifest.executable.artifact)?;
        ensure!(
            image.bytes() == manifest.executable_bytes,
            "result client actual byte pin"
        );
        ensure!(
            pool.charged_bytes()? == manifest.executable_bytes,
            "result client installation charge differs"
        );
        Ok(Self {
            manifest,
            digest: expected.manifest_digest.clone(),
            image,
        })
    }
    pub fn revalidate(&self) -> Result<()> {
        self.image.revalidate()
    }
    pub fn path(&self) -> &Path {
        self.image.source()
    }
    pub fn artifact(&self) -> &ArtifactRef {
        self.image.artifact()
    }
    pub fn contract(&self) -> &ArtifactRef {
        &self.manifest.contract
    }
    pub fn environment(&self) -> &ArtifactRef {
        &self.manifest.environment
    }
    pub fn manifest_digest(&self) -> &Digest {
        &self.digest
    }
    pub fn manifest_artifact(&self) -> ArtifactRef {
        manifest_reference(self.digest.clone())
    }
    pub fn executable_bytes(&self) -> u64 {
        self.image.bytes()
    }
    /// Explicit additional image reservation; callers must account it separately
    /// from Codex's own preflight pool and per-attempt workspace/resource charge.
    pub fn reserved_bytes(&self) -> u64 {
        MAX_RESULT_CLIENT_INSTALLATION_BYTES
    }
}

fn manifest_reference(digest: Digest) -> ArtifactRef {
    ArtifactRef {
        id: aq_session_protocol::Text::new("aq.agent-result-client-installation")
            .expect("constant installation id"),
        version: aq_session_protocol::Text::new("1").expect("constant installation version"),
        digest,
    }
}

#[cfg(test)]
mod tests;
