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
        ensure!(
            manifest.schema_version == 1
                && manifest.installation.schema_version == 1
                && manifest.digest()? == *expected.manifest_digest
                && serde_json::to_vec(&manifest)?.len() <= MAX_MANIFEST
                && (64..=MAX_NATIVE_EXEC_BYTES).contains(&manifest.executable_bytes),
            "native installation profile/size/pin"
        );
        let m = &manifest.installation;
        ensure!(
            m.adapter == *expected.adapter
                && m.runtime == *expected.runtime
                && m.qualification == *expected.qualification
                && m.renderer == *expected.renderer,
            "native installation independent references"
        );
        for r in [&m.adapter, &m.runtime, &m.qualification, &m.renderer] {
            r.id.validate()?;
            r.version.validate()?;
        }
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
#[cfg(test)]
mod tests {
    use super::*;
    use aq_session_protocol::Text;
    use std::os::unix::fs::PermissionsExt;
    fn manifest(root: &std::path::Path) -> NativeExecManifest {
        let mut bytes = vec![0u8; 128];
        bytes[..7].copy_from_slice(b"\x7fELF\x02\x01\x01");
        bytes[16] = 2;
        bytes[18] = if cfg!(target_arch = "aarch64") {
            183
        } else {
            62
        };
        let path = root.join("program");
        if path.exists() {
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o700)).unwrap();
        }
        std::fs::write(&path, &bytes).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o500)).unwrap();
        let reference = ArtifactRef {
            id: Text::new("fixture").unwrap(),
            version: Text::new("1").unwrap(),
            digest: Digest::of(&bytes),
        };
        let pin = FilePin {
            path,
            artifact: reference.clone(),
        };
        NativeExecManifest {
            schema_version: 1,
            profile: NativeExecKind::Codex0166ExecStdinV1,
            executable_bytes: bytes.len() as u64,
            installation: AdapterManifest {
                schema_version: 1,
                adapter: reference.clone(),
                runtime: reference.clone(),
                qualification: reference.clone(),
                renderer: reference,
                handoff: HandoffKind::OpaquePrivateFileV1,
                executable: pin.clone(),
                helper: pin.clone(),
                tmux: pin,
            },
        }
    }
    fn open(m: NativeExecManifest) -> Result<NativeExecInstallation> {
        let r = m.installation.adapter.clone();
        let digest = m.digest().unwrap();
        NativeExecInstallation::open(
            m,
            ExpectedAdapter {
                manifest_digest: &digest,
                adapter: &r,
                runtime: &r,
                qualification: &r,
                renderer: &r,
            },
            &NativeExecPool::new(1024, 1).unwrap(),
        )
    }
    #[test]
    fn explicit_profile_preserves_exact_size_digest_and_independent_identity() {
        let dir = tempfile::tempdir().unwrap();
        let m = manifest(dir.path());
        let path = m.installation.executable.path.clone();
        let digest = m.digest().unwrap();
        let bytes = m.executable_bytes;
        let mut installed = open(m).unwrap();
        assert_eq!(installed.digest(), &digest);
        assert_eq!(installed.executable_bytes(), bytes);
        installed.revalidate().unwrap();
        std::fs::rename(&path, dir.path().join("old")).unwrap();
        std::fs::write(&path, b"offline installation fixture").unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o500)).unwrap();
        assert!(installed.revalidate().is_err());
    }
    #[test]
    fn undeclared_profile_or_actual_size_mismatch_refuse() {
        let dir = tempfile::tempdir().unwrap();
        let mut m = manifest(dir.path());
        m.executable_bytes += 1;
        assert!(open(m).is_err());
        let mut v = serde_json::to_value(manifest(dir.path())).unwrap();
        v["profile"] = serde_json::json!("large-any-executable");
        assert!(NativeExecManifest::decode(&serde_json::to_vec(&v).unwrap()).is_err());
        let mut m = manifest(dir.path());
        m.executable_bytes = MAX_NATIVE_EXEC_BYTES + 1;
        assert!(open(m).is_err());
        let m = manifest(dir.path());
        let r = m.installation.adapter.clone();
        let wrong = Digest::of(b"different independently selected profile");
        assert!(
            NativeExecInstallation::open(
                m,
                ExpectedAdapter {
                    manifest_digest: &wrong,
                    adapter: &r,
                    runtime: &r,
                    qualification: &r,
                    renderer: &r
                },
                &NativeExecPool::new(1024, 1).unwrap()
            )
            .is_err()
        );
    }
}
