//! Installed input boundary only: no harness implementation or launch authority.
use super::materials::InstalledMaterial;
use anyhow::{Result, ensure};
use aq_admission::MaterialReceipt;
use aq_session_protocol::{ArtifactRef, Digest, LaunchBinding, Validate};
use serde::{Deserialize, Serialize};
use std::{
    fs::{File, OpenOptions},
    io::{Read, Seek},
    os::unix::fs::{MetadataExt, OpenOptionsExt},
    path::PathBuf,
};
use zeroize::Zeroizing;

pub mod codex;
pub mod named_exec;
pub mod native_exec;
pub mod responses_transport;
pub mod result_client;
pub mod result_profile;

const MAX_MANIFEST: usize = 16_384;
const MAX_EXECUTABLE: u64 = 256 * 1024 * 1024;
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HandoffKind {
    OpaquePrivateFileV1,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FilePin {
    pub path: PathBuf,
    pub artifact: ArtifactRef,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AdapterManifest {
    pub schema_version: u32,
    pub adapter: ArtifactRef,
    pub runtime: ArtifactRef,
    pub qualification: ArtifactRef,
    pub renderer: ArtifactRef,
    pub handoff: HandoffKind,
    pub executable: FilePin,
    pub helper: FilePin,
    pub tmux: FilePin,
}
/// Independently owner-selected pins, not values inferred from this manifest.
pub struct ExpectedAdapter<'a> {
    pub manifest_digest: &'a Digest,
    pub adapter: &'a ArtifactRef,
    pub runtime: &'a ArtifactRef,
    pub qualification: &'a ArtifactRef,
    pub renderer: &'a ArtifactRef,
}
struct PinnedFile {
    pin: FilePin,
    file: File,
    device: u64,
    inode: u64,
    max_bytes: u64,
}
impl PinnedFile {
    fn open(pin: FilePin) -> Result<Self> {
        Self::open_bounded(pin, MAX_EXECUTABLE)
    }
    fn open_bounded(pin: FilePin, max_bytes: u64) -> Result<Self> {
        pin.artifact.id.validate()?;
        pin.artifact.version.validate()?;
        ensure!(
            pin.path.is_absolute()
                && pin.path.as_os_str().len() <= 2048
                && std::fs::canonicalize(&pin.path)? == pin.path,
            "installed adapter path"
        );
        let file = OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK | libc::O_CLOEXEC)
            .open(&pin.path)?;
        let meta = file.metadata()?;
        let mut value = Self {
            pin,
            file,
            device: meta.dev(),
            inode: meta.ino(),
            max_bytes,
        };
        value.verify()?;
        Ok(value)
    }
    fn verify(&mut self) -> Result<()> {
        let before = self.file.metadata()?;
        ensure!(
            before.is_file()
                && before.len() > 0
                && before.len() <= self.max_bytes
                && before.mode() & 0o111 != 0
                && before.mode() & 0o6022 == 0
                && (before.uid() == 0 || before.uid() == unsafe { libc::geteuid() }),
            "installed adapter file permissions/size"
        );
        self.file.rewind()?;
        use sha2::Digest as _;
        let mut hash = sha2::Sha256::new();
        let mut remaining = before.len();
        let mut scratch = [0u8; 65536];
        while remaining > 0 {
            let limit = usize::try_from(remaining.min(scratch.len() as u64))?;
            let n = self.file.read(&mut scratch[..limit])?;
            ensure!(n > 0, "installed adapter truncated");
            hash.update(&scratch[..n]);
            remaining -= n as u64;
        }
        ensure!(
            self.file.read(&mut scratch[..1])? == 0,
            "installed adapter grew"
        );
        let after = self.file.metadata()?;
        let named = std::fs::symlink_metadata(&self.pin.path)?;
        ensure!(
            named.is_file()
                && named.dev() == self.device
                && named.ino() == self.inode
                && after.dev() == self.device
                && after.ino() == self.inode
                && before.len() == after.len()
                && before.mtime() == after.mtime()
                && before.mtime_nsec() == after.mtime_nsec()
                && before.ctime() == after.ctime()
                && before.ctime_nsec() == after.ctime_nsec()
                && format!("{:x}", hash.finalize()) == self.pin.artifact.digest.as_str(),
            "installed adapter file changed"
        );
        Ok(())
    }
}
/// No Serialize/Debug, launch method, command or credential-bearing environment.
pub struct InstalledAdapter {
    digest: Digest,
    adapter: ArtifactRef,
    renderer: ArtifactRef,
    files: [PinnedFile; 3],
}
impl AdapterManifest {
    /// Check byte bounds before deserializing untrusted configuration.
    pub fn decode(bytes: &[u8]) -> Result<Self> {
        ensure!(
            !bytes.is_empty() && bytes.len() <= MAX_MANIFEST,
            "adapter manifest byte bound"
        );
        Ok(serde_json::from_slice(bytes)?)
    }
    pub fn digest(&self) -> Result<Digest> {
        super::journal::canonical_digest(self)
    }
}
impl InstalledAdapter {
    pub fn open(manifest: AdapterManifest, expected: ExpectedAdapter<'_>) -> Result<Self> {
        ensure!(
            manifest.schema_version == 1 && serde_json::to_vec(&manifest)?.len() <= MAX_MANIFEST,
            "adapter manifest version/size"
        );
        for item in [
            &manifest.adapter,
            &manifest.runtime,
            &manifest.qualification,
            &manifest.renderer,
        ] {
            item.id.validate()?;
            item.version.validate()?;
        }
        ensure!(
            manifest.adapter == *expected.adapter
                && manifest.runtime == *expected.runtime
                && manifest.qualification == *expected.qualification
                && manifest.renderer == *expected.renderer,
            "adapter independent pins differ"
        );
        let digest = manifest.digest()?;
        ensure!(
            digest == *expected.manifest_digest,
            "adapter manifest independent digest"
        );
        Ok(Self {
            digest,
            adapter: manifest.adapter,
            renderer: manifest.renderer,
            files: [
                PinnedFile::open(manifest.executable)?,
                PinnedFile::open(manifest.helper)?,
                PinnedFile::open(manifest.tmux)?,
            ],
        })
    }
    pub fn digest(&self) -> &Digest {
        &self.digest
    }
    /// Detects change now; this is not an immutable-filesystem or exec claim.
    pub fn revalidate(&mut self) -> Result<()> {
        for file in &mut self.files {
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
        material_handoff(installed, expected, binding)
    }
}
fn material_handoff<'a>(
    installed: &'a InstalledMaterial,
    expected: &MaterialReceipt,
    binding: &LaunchBinding,
) -> Result<MaterialHandoff<'a>> {
    binding.validate()?;
    ensure!(
        expected.binding == *binding
            && super::journal::canonical_digest(&installed.receipt()?)?
                == super::journal::canonical_digest(expected)?,
        "adapter material independent pin"
    );
    drop(installed.read_private()?);
    Ok(MaterialHandoff {
        installed,
        receipt: expected.clone(),
    })
}
/// Borrowed installed owner only. No new durable state, expiry or live authority.
pub struct MaterialHandoff<'a> {
    installed: &'a InstalledMaterial,
    receipt: MaterialReceipt,
}
impl MaterialHandoff<'_> {
    pub fn receipt(&self) -> &MaterialReceipt {
        &self.receipt
    }
    /// Explicit sensitive access for future closed adapter code; never metadata.
    pub fn read_private(&self) -> Result<Zeroizing<Vec<u8>>> {
        ensure!(
            super::journal::canonical_digest(&self.installed.receipt()?)?
                == super::journal::canonical_digest(&self.receipt)?,
            "handoff material pin changed"
        );
        self.installed.read_private()
    }
}
#[cfg(test)]
mod tests;
