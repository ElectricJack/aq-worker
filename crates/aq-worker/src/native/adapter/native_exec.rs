//! Explicit native installation profile. Legacy manifest/pin limits stay intact.
use super::*;
use aq_session_host::observed::{MAX_NATIVE_EXEC_BYTES, NativeExecImage, NativeExecPool};

#[derive(Clone, Debug, Serialize, Deserialize)]
pub enum NativeExecKind {
    #[serde(rename = "codex-0.166-exec-stdin-v1")]
    Codex0166ExecStdinV1,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NativeExecManifest {
    pub schema_version: u32,
    pub profile: NativeExecKind,
    pub executable_bytes: u64,
    pub installation: AdapterManifest,
}
impl NativeExecManifest {
    pub fn decode(bytes: &[u8]) -> Result<Self> {
        ensure!(
            !bytes.is_empty() && bytes.len() <= MAX_MANIFEST,
            "native installation manifest bound"
        );
        Ok(serde_json::from_slice(bytes)?)
    }
    fn validate_profile(&self, expected: &ExpectedAdapter<'_>) -> Result<()> {
        ensure!(
            self.schema_version == 1
                && self.installation.schema_version == 1
                && self.digest()? == *expected.manifest_digest
                && serde_json::to_vec(self)?.len() <= MAX_MANIFEST
                && (64..=MAX_NATIVE_EXEC_BYTES).contains(&self.executable_bytes),
            "native installation profile/size/pin"
        );
        Ok(())
    }
    pub fn digest(&self) -> Result<Digest> {
        super::super::journal::canonical_digest(self)
    }
}
/// Input-only installation with an explicit named immutable image policy.
/// Construct before admission; the shared pool charges actual bytes once.
/// No Serialize/Debug, execution authority, eviction or implicit renewal.
pub struct NativeExecInstallation {
    manifest: NativeExecManifest,
    digest: Digest,
    pool: NativeExecPool,
    _image: NativeExecImage,
    local: [PinnedFile; 2],
}
impl NativeExecInstallation {
    pub fn open(
        manifest: NativeExecManifest,
        expected: ExpectedAdapter<'_>,
        pool: &NativeExecPool,
    ) -> Result<Self> {
        manifest.validate_profile(&expected)?;
        let m = &manifest.installation;
        ensure!(
            m.matches_references(&expected),
            "native installation independent references"
        );
        m.validate_references()?;
        let meta = std::fs::symlink_metadata(&m.executable.path)?;
        ensure!(
            meta.is_file()
                && meta.len() == manifest.executable_bytes
                && meta.mode() & 0o111 != 0
                && meta.mode() & 0o6022 == 0
                && (meta.uid() == 0 || meta.uid() == unsafe { libc::geteuid() }),
            "native installation actual executable size/mode"
        );
        // The pool performs the bounded full hash/copy before sealing. Do not
        // hash 319MiB again through the generic mutable-file input primitive.
        let image = pool.load_verified(&m.executable.path, &m.executable.artifact)?;
        ensure!(
            image.bytes() == manifest.executable_bytes,
            "native sealed image byte charge"
        );
        let local = [
            PinnedFile::open_bounded(m.helper.clone(), 64 * 1024 * 1024)?,
            PinnedFile::open_bounded(m.tmux.clone(), 64 * 1024 * 1024)?,
        ];
        Ok(Self {
            manifest,
            digest: expected.manifest_digest.clone(),
            pool: pool.clone(),
            _image: image,
            local,
        })
    }
    pub fn digest(&self) -> &Digest {
        &self.digest
    }
    pub fn executable_bytes(&self) -> u64 {
        self.manifest.executable_bytes
    }
    pub fn image(&self) -> Result<NativeExecImage> {
        self.pool.load_verified(
            &self.manifest.installation.executable.path,
            &self.manifest.installation.executable.artifact,
        )
    }
    pub fn revalidate(&mut self) -> Result<()> {
        ensure!(
            self.image()?.bytes() == self.manifest.executable_bytes,
            "native installation byte charge changed"
        );
        for file in &mut self.local {
            file.verify()?;
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
    pub(crate) fn installation(&self) -> &AdapterManifest {
        &self.manifest.installation
    }
}
