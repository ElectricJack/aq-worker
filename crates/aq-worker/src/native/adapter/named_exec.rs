//! Separately tagged named-readonly installation. No legacy mutable fallback.
use super::*;
use aq_session_host::observed::{
    MAX_NAMED_EXEC_BYTES, NamedReadonlyExecImage, NamedReadonlyExecPool, NamedReadonlyFile,
};
#[derive(Clone, Debug, Serialize, Deserialize)]
pub enum NamedExecKind {
    #[serde(rename = "codex-0.166-named-readonly-exec-stdin-v1")]
    Codex0166NamedReadonlyExecStdinV1,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NamedExecManifest {
    pub schema_version: u32,
    pub profile: NamedExecKind,
    pub executable_bytes: u64,
    pub environment: ArtifactRef,
    pub installation: AdapterManifest,
}
impl NamedExecManifest {
    pub fn decode(bytes: &[u8]) -> Result<Self> {
        ensure!(
            !bytes.is_empty() && bytes.len() <= MAX_MANIFEST,
            "named installation manifest bound"
        );
        Ok(serde_json::from_slice(bytes)?)
    }
    pub fn digest(&self) -> Result<Digest> {
        super::super::journal::canonical_digest(self)
    }
}
/// Opaque retained preflight installation; environment reference is independently
/// selected, and the pool has inspected actual kernel premises. Neither this
/// owner nor its artifact references grant launch/model/secret-isolation authority.
pub struct NamedExecInstallation {
    manifest: NamedExecManifest,
    digest: Digest,
    pool: NamedReadonlyExecPool,
    program: NamedReadonlyExecImage,
    local: [NamedReadonlyFile; 2],
}
impl NamedExecInstallation {
    pub fn open(
        manifest: NamedExecManifest,
        expected: ExpectedAdapter<'_>,
        expected_environment: &ArtifactRef,
        pool: &NamedReadonlyExecPool,
    ) -> Result<Self> {
        ensure!(
            manifest.schema_version == 1
                && manifest.installation.schema_version == 1
                && manifest.digest()? == *expected.manifest_digest
                && serde_json::to_vec(&manifest)?.len() <= MAX_MANIFEST
                && (64..=MAX_NAMED_EXEC_BYTES).contains(&manifest.executable_bytes)
                && manifest.environment == *expected_environment
                && manifest.environment == *pool.qualification(),
            "named installation profile/size/independent environment pin"
        );
        let m = &manifest.installation;
        ensure!(
            m.adapter == *expected.adapter
                && m.runtime == *expected.runtime
                && m.qualification == *expected.qualification
                && m.renderer == *expected.renderer,
            "named installation independent references"
        );
        for reference in [&m.adapter, &m.runtime, &m.qualification, &m.renderer] {
            reference.id.validate()?;
            reference.version.validate()?;
        }
        let program = pool.load_verified(&m.executable.path, &m.executable.artifact)?;
        ensure!(
            program.bytes() == manifest.executable_bytes,
            "named actual executable byte pin"
        );
        let local = [
            pool.load_auxiliary_verified(&m.helper.path, &m.helper.artifact)?,
            pool.load_auxiliary_verified(&m.tmux.path, &m.tmux.artifact)?,
        ];
        Ok(Self {
            manifest,
            digest: expected.manifest_digest.clone(),
            pool: pool.clone(),
            program,
            local,
        })
    }
    pub fn digest(&self) -> &Digest {
        &self.digest
    }
    pub fn executable_bytes(&self) -> u64 {
        self.manifest.executable_bytes
    }
    pub fn image(&self) -> Result<NamedReadonlyExecImage> {
        self.program.revalidate()?;
        self.pool.load_verified(
            &self.manifest.installation.executable.path,
            &self.manifest.installation.executable.artifact,
        )
    }
    pub fn revalidate(&mut self) -> Result<()> {
        self.program.revalidate()?;
        for file in &self.local {
            file.revalidate()?;
        }
        Ok(())
    }
    pub fn material<'a>(
        &self,
        installed: &'a InstalledMaterial,
        expected: &MaterialReceipt,
        binding: &LaunchBinding,
    ) -> Result<MaterialHandoff<'a>> {
        super::material_handoff(installed, expected, binding)
    }
    pub fn installation(&self) -> &AdapterManifest {
        &self.manifest.installation
    }
}
