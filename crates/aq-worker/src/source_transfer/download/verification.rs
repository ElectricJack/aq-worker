use super::*;

/// This read/hash job can move to a bounded blocking pool while the journal
/// owner continues servicing renewal/cancellation. It never mutates the journal.
pub struct DownloadVerification {
    pub(super) root: PathBuf,
    pub(super) manifest: TransferManifest,
}
pub struct VerifiedDownload {
    pub(super) root: PathBuf,
    pub(super) manifest: TransferManifest,
}
impl DownloadVerification {
    pub fn verify(self) -> Result<VerifiedDownload> {
        self.verify_manifest()?;
        self.verify_bundle()?;
        Ok(VerifiedDownload {
            root: self.root,
            manifest: self.manifest,
        })
    }
    fn verify_manifest(&self) -> Result<()> {
        let m = &self.manifest;
        let f = artifact_file(
            &self.root,
            m.artifact_id,
            "manifest.json",
            ArtifactAccess::ReadIncoming,
        )?;
        ensure!(
            f.metadata()?.len() == m.manifest_bytes,
            "download manifest length mismatch"
        );
        let mut bytes = Vec::new();
        f.take(1024 * 1024 + 1).read_to_end(&mut bytes)?;
        ensure!(
            bytes.len() as u64 == m.manifest_bytes
                && format!("{:x}", Sha256::digest(&bytes)) == m.manifest_digest,
            "download manifest digest mismatch"
        );
        let native: GitManifest = serde_json::from_slice(&bytes)?;
        ensure!(
            native.format_version == 1
                && matches_source_binding(&native, &m.source)
                && native.transfer_id == m.artifact_id
                && native.object_format == "sha1"
                && native.source == m.checkpoint
                && (
                    native.bundle_size,
                    &native.bundle_sha256,
                    native.reference_only
                ) == (m.bundle_bytes, &m.bundle_digest, m.bundle_bytes == 0),
            "download native manifest association mismatch"
        );
        Ok(())
    }
    fn verify_bundle(&self) -> Result<()> {
        let m = &self.manifest;
        if let Some(expected) = &m.bundle_digest {
            let mut f = artifact_file(
                &self.root,
                m.artifact_id,
                "objects.bundle",
                ArtifactAccess::ReadIncoming,
            )?;
            ensure!(
                f.metadata()?.len() == m.bundle_bytes,
                "download bundle length mismatch"
            );
            let mut hasher = Sha256::new();
            let mut buf = [0u8; 64 * 1024];
            let mut left = m.bundle_bytes;
            while left > 0 {
                let n = f.read(&mut buf[..(left as usize).min(64 * 1024)])?;
                ensure!(n > 0, "download bundle truncated");
                hasher.update(&buf[..n]);
                left -= n as u64;
            }
            ensure!(
                format!("{:x}", hasher.finalize()) == *expected,
                "download bundle digest mismatch"
            );
        }
        Ok(())
    }
}
