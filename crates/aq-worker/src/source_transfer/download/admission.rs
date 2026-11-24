use super::*;

impl<C: Clock> Runtime<C> {
    fn reserve_download_bytes(
        &self,
        id: Uuid,
        state: &DownloadState,
        git: &GitClient,
    ) -> Result<u64> {
        let r = &self.journal.state().attempts[&id];
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
        Ok(pending)
    }
    fn ensure_incoming_unowned(&self, state: &DownloadState, git: &GitClient) -> Result<()> {
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
        Ok(())
    }
    fn ensure_download_authority(
        &self,
        r: &crate::journal::AttemptRecord,
        source: &crate::source::SourceState,
    ) -> Result<()> {
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
        Ok(())
    }
    pub fn prepare_base_download(
        &mut self,
        id: Uuid,
        manifest: TransferManifest,
        git: &GitClient,
    ) -> Result<()> {
        let r = self.source_attempt(id)?;
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
        self.ensure_download_authority(r, source)?;
        let state = DownloadState {
            manifest,
            manifest_offset: 0,
            bundle_offset: 0,
            verified: false,
        };
        state.validate(&r.offer, &source.binding)?;
        let pending = self.reserve_download_bytes(id, &state, git)?;
        self.ensure_incoming_unowned(&state, git)?;
        self.journal.transaction(|s| {
            let r = s.attempts.get_mut(&id).unwrap();
            r.pending_bytes = pending;
            r.source.as_mut().unwrap().download = Some(state);
            r.phase = Phase::Preparing;
            Ok(())
        })
    }
}
