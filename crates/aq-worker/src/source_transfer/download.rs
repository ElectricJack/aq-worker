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
            !self.verified
                || (self.manifest_offset == self.manifest.manifest_bytes
                    && self.bundle_offset == self.manifest.bundle_bytes),
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
/// This read/hash job can move to a bounded blocking pool while the journal
/// owner continues servicing renewal/cancellation. It never mutates the journal.
#[cfg(feature = "synthetic-runtime")]
pub struct DownloadVerification {
    root: PathBuf,
    manifest: TransferManifest,
}
#[cfg(feature = "synthetic-runtime")]
pub struct VerifiedDownload {
    root: PathBuf,
    manifest: TransferManifest,
}
#[cfg(feature = "synthetic-runtime")]
impl DownloadVerification {
    pub fn verify(self) -> Result<VerifiedDownload> {
        let m = &self.manifest;
        let f = artifact_file(&self.root, m.artifact_id, true, "manifest.json", false)?;
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
                && native.repository_id == m.source.repository_id
                && native.repository_identity == m.source.logical_identity
                && native.binding_generation == m.source.binding_generation
                && native.transfer_id == m.artifact_id
                && native.object_format == "sha1"
                && native.base == m.source.base
                && native.source == m.checkpoint
                && native.bundle_size == m.bundle_bytes
                && native.bundle_sha256 == m.bundle_digest
                && native.reference_only == (m.bundle_bytes == 0),
            "download native manifest association mismatch"
        );
        if let Some(expected) = &m.bundle_digest {
            let mut f = artifact_file(&self.root, m.artifact_id, true, "objects.bundle", false)?;
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
        Ok(VerifiedDownload {
            root: self.root,
            manifest: self.manifest,
        })
    }
}

#[cfg(feature = "synthetic-runtime")]
impl<C: Clock> Runtime<C> {
    pub fn prepare_base_download(
        &mut self,
        id: Uuid,
        manifest: TransferManifest,
        git: &GitClient,
    ) -> Result<()> {
        let r = self
            .journal
            .state()
            .attempts
            .get(&id)
            .context("unknown attempt")?;
        let source = r.source.as_ref().context("source missing")?;
        ensure!(
            source.managed_root.as_deref() == Some(git.root()),
            "source root changed"
        );
        if let Some(prior) = &source.download {
            ensure!(
                prior.manifest == manifest,
                "base transfer changed during download"
            );
            return prior.validate(&r.offer, &source.binding);
        }
        ensure!(
            !self.journal.state().reconciliation_required
                && r.offer.identity == self.journal.state().identity
                && matches!(r.phase, Phase::Authorized | Phase::Preparing)
                && r.launch_id.is_none()
                && !r.start_intent
                && source.workspace.is_none()
                && !source.operations.contains_key("create")
                && self.clock.now_ms()
                    < r.grant_deadline_ms
                        .context("source download grant absent")?,
            "new base download requires current unstarted authority"
        );
        let state = DownloadState {
            manifest,
            manifest_offset: 0,
            bundle_offset: 0,
            verified: false,
        };
        state.validate(&r.offer, &source.binding)?;
        let bytes = state
            .manifest
            .manifest_bytes
            .checked_add(state.manifest.bundle_bytes)
            .context("download size overflow")?;
        ensure!(
            bytes <= git.transfer_byte_limit(),
            "download exceeds configured source profile"
        );
        let pending = r
            .pending_bytes
            .checked_add(bytes)
            .context("download reservation overflow")?;
        ensure!(
            pending <= r.offer.spec.resources.disk_bytes,
            "download exceeds attempt storage reservation"
        );
        let total = self
            .journal
            .state()
            .attempts
            .iter()
            .filter(|(key, r)| **key != id && r.phase != Phase::Retired)
            .try_fold(pending, |n, (_, r)| n.checked_add(r.pending_bytes))
            .context("download pending accounting overflow")?;
        ensure!(
            total <= self.journal.limits().max_pending_bytes,
            "download pending capacity exhausted"
        );
        ensure!(
            fs2::available_space(git.root())?
                .saturating_sub(self.journal.limits().minimum_free_disk_bytes)
                >= bytes,
            "download physical headroom unavailable"
        );
        let path = git
            .root()
            .join("artifacts/incoming")
            .join(state.manifest.artifact_id.to_string());
        match std::fs::symlink_metadata(path) {
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(e.into()),
            Ok(_) => anyhow::bail!("unowned incoming artifact already exists"),
        }
        ensure!(
            self.journal
                .state()
                .attempts
                .values()
                .all(|r| r.source.as_ref().is_none_or(|s| s
                    .download
                    .as_ref()
                    .is_none_or(|d| d.manifest.artifact_id != state.manifest.artifact_id)
                    && !s.imported_bases.contains(&state.manifest.artifact_id))),
            "base artifact already owned"
        );
        self.journal.transaction(|s| {
            let r = s.attempts.get_mut(&id).unwrap();
            r.pending_bytes = pending;
            r.source.as_mut().unwrap().download = Some(state);
            r.phase = Phase::Preparing;
            Ok(())
        })
    }

    /// Replaying an existing bounded transport does not grant import or launch
    /// authority. Those operations independently require the current incarnation.
    pub fn next_base_download(&self, id: Uuid) -> Result<Option<DownloadAction>> {
        let r = self
            .journal
            .state()
            .attempts
            .get(&id)
            .context("unknown attempt")?;
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
        let remaining = if d.manifest_offset < d.manifest.manifest_bytes {
            Some((
                TransferFile::Manifest,
                d.manifest_offset,
                d.manifest.manifest_bytes,
            ))
        } else if d.bundle_offset < d.manifest.bundle_bytes {
            Some((
                TransferFile::Bundle,
                d.bundle_offset,
                d.manifest.bundle_bytes,
            ))
        } else {
            None
        };
        if let Some((file, offset, size)) = remaining {
            return Ok(Some(DownloadAction::Range(DownloadRange {
                attempt_id: id,
                transfer_id: d.manifest.transfer_id,
                artifact_id: d.manifest.artifact_id,
                file,
                offset,
                length: (size - offset).min(MAX_CHUNK_BYTES as u64) as usize,
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
        let mut file = artifact_file(root, range.artifact_id, true, name, true)?;
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
            let d = s
                .attempts
                .get_mut(&id)
                .unwrap()
                .source
                .as_mut()
                .unwrap()
                .download
                .as_mut()
                .unwrap();
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
        let r = self
            .journal
            .state()
            .attempts
            .get(&id)
            .context("unknown attempt")?;
        let source = r.source.as_ref().context("source missing")?;
        let d = source.download.as_ref().context("download missing")?;
        d.validate(&r.offer, &source.binding)?;
        ensure!(
            r.result.is_none()
                && !source.retired
                && source.managed_root.as_ref() == Some(&verified.root)
                && d.manifest == verified.manifest
                && d.manifest_offset == d.manifest.manifest_bytes
                && d.bundle_offset == d.manifest.bundle_bytes,
            "verified download association mismatch"
        );
        self.journal.transaction(|s| {
            s.attempts
                .get_mut(&id)
                .unwrap()
                .source
                .as_mut()
                .unwrap()
                .download
                .as_mut()
                .unwrap()
                .verified = true;
            Ok(())
        })
    }
}
