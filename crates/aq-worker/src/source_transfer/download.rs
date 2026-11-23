use super::*;
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DownloadState {
    pub manifest: TransferManifest,
    pub manifest_offset: u64,
    pub bundle_offset: u64,
    pub verified: bool,
}
impl DownloadState {
    fn complete(&self) -> bool {
        self.manifest_offset == self.manifest.manifest_bytes
            && self.bundle_offset == self.manifest.bundle_bytes
    }
    pub fn validate(&self, offer: &Offer, source: &SourceBinding) -> Result<()> {
        self.manifest.validate()?;
        ensure!(
            self.manifest.role == SourceRole::Base
                && &self.manifest.source == source
                && self.manifest.attempt == offer.attempt
                && self.manifest.resolved_spec_digest == offer.spec.resolved_spec_digest,
            "download assignment mismatch"
        );
        ensure!(
            self.manifest_offset <= self.manifest.manifest_bytes
                && self.bundle_offset <= self.manifest.bundle_bytes
                && (self.bundle_offset == 0
                    || self.manifest_offset == self.manifest.manifest_bytes),
            "invalid download offsets"
        );
        ensure!(
            !self.verified || self.complete(),
            "incomplete verified download"
        );
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
#[cfg(feature = "synthetic-runtime")]
pub struct DownloadRange {
    pub attempt_id: Uuid,
    pub transfer_id: Uuid,
    pub artifact_id: Uuid,
    pub file: TransferFile,
    pub offset: u64,
    pub length: usize,
}
#[cfg(feature = "synthetic-runtime")]
impl DownloadRange {
    pub async fn execute(&self, client: &TransferClient) -> Result<Vec<u8>> {
        client
            .download(self.attempt_id, self.file.clone(), self.offset, self.length)
            .await
    }
}
#[cfg(feature = "synthetic-runtime")]
pub enum DownloadAction {
    Range(DownloadRange),
    Verify(Box<DownloadVerification>),
}
#[cfg(feature = "synthetic-runtime")]
mod verification;
#[cfg(feature = "synthetic-runtime")]
pub use verification::{DownloadVerification, VerifiedDownload};

#[cfg(feature = "synthetic-runtime")]
mod admission;

#[cfg(feature = "synthetic-runtime")]
impl<C: Clock> Runtime<C> {
    /// Replaying an existing bounded transport does not grant import or launch
    /// authority. Those operations independently require the current incarnation.
    pub fn next_base_download(&self, id: Uuid) -> Result<Option<DownloadAction>> {
        let r = self.source_attempt(id)?;
        let source = r.source.as_ref().context("source absent")?;
        let d = source
            .download
            .as_ref()
            .context("download intent missing")?;
        d.validate(&r.offer, &source.binding)?;
        if d.verified {
            return Ok(None);
        }
        ensure!(
            r.result.is_none() && !source.retired,
            "download no longer retained"
        );
        if let Some((file, offset, length)) =
            next_chunk(&d.manifest, d.manifest_offset, d.bundle_offset)
        {
            return Ok(Some(DownloadAction::Range(DownloadRange {
                attempt_id: id,
                transfer_id: d.manifest.transfer_id,
                artifact_id: d.manifest.artifact_id,
                file,
                offset,
                length,
            })));
        }
        Ok(Some(DownloadAction::Verify(Box::new(
            DownloadVerification {
                root: source.managed_root.clone().context("source root absent")?,
                manifest: d.manifest.clone(),
            },
        ))))
    }

    pub fn store_base_download_chunk(
        &mut self,
        id: Uuid,
        range: &DownloadRange,
        bytes: &[u8],
    ) -> Result<()> {
        let expected = match self.next_base_download(id)? {
            Some(DownloadAction::Range(r)) => r,
            _ => anyhow::bail!("download is not awaiting bytes"),
        };
        ensure!(
            &expected == range && bytes.len() == range.length,
            "download range association mismatch"
        );
        let r = &self.journal.state().attempts[&id];
        let source = r.source.as_ref().unwrap();
        let root = source
            .managed_root
            .as_ref()
            .context("source root missing")?;
        ensure!(
            fs2::available_space(root)?
                .saturating_sub(self.journal.limits().minimum_free_disk_bytes)
                >= bytes.len() as u64,
            "download physical headroom unavailable"
        );
        let name = match range.file {
            TransferFile::Manifest => "manifest.json",
            TransferFile::Bundle => "objects.bundle",
        };
        let mut file = artifact_file(root, range.artifact_id, name, ArtifactAccess::WriteIncoming)?;
        let current = file.metadata()?.len();
        ensure!(
            current >= range.offset && current <= range.offset + range.length as u64,
            "download disk progress inconsistent"
        );
        file.set_len(range.offset)?;
        file.seek(SeekFrom::Start(range.offset))?;
        file.write_all(bytes)?;
        file.sync_all()?;
        // If this journal commit fails, reopen truncates only this owned
        // unacknowledged tail and downloads the same range again.
        self.journal.transaction(|s| {
            let d = s.attempt_source_mut(id).download.as_mut().unwrap();
            match range.file {
                TransferFile::Manifest => d.manifest_offset += bytes.len() as u64,
                TransferFile::Bundle => d.bundle_offset += bytes.len() as u64,
            }
            Ok(())
        })
    }

    pub fn acknowledge_base_verification(
        &mut self,
        id: Uuid,
        verified: VerifiedDownload,
    ) -> Result<()> {
        let r = self.source_attempt(id)?;
        let source = r.source.as_ref().context("source missing")?;
        let d = source.download.as_ref().context("download missing")?;
        d.validate(&r.offer, &source.binding)?;
        ensure!(
            r.result.is_none()
                && !source.retired
                && source.managed_root.as_ref() == Some(&verified.root)
                && d.manifest == verified.manifest
                && d.complete(),
            "verified download association mismatch"
        );
        self.journal.transaction(|s| {
            s.attempt_source_mut(id).download.as_mut().unwrap().verified = true;
            Ok(())
        })
    }
}
