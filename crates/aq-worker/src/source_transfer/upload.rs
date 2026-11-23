use super::*;
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct UploadState {
    pub identity: NodeIdentity,
    pub begin_operation: Uuid,
    pub finalize_operation: Uuid,
    pub begun: bool,
    pub manifest_offset: u64,
    pub bundle_offset: u64,
    pub pending: Option<TransferChunkRequest>,
}
impl UploadState {
    pub fn validate(&self, manifest: &TransferManifest, node: Uuid) -> Result<()> {
        self.identity.validate()?;
        manifest.validate()?;
        ensure!(
            self.identity.node_id == node
                && ![self.begin_operation, self.finalize_operation].contains(&Uuid::nil())
                && self.begin_operation != self.finalize_operation,
            "invalid upload identities"
        );
        ensure!(
            self.manifest_offset <= manifest.manifest_bytes
                && self.bundle_offset <= manifest.bundle_bytes,
            "upload offsets exceed manifest"
        );
        ensure!(
            self.begun
                || ((self.manifest_offset, self.bundle_offset) == (0, 0) && self.pending.is_none()),
            "unbegun upload has progress"
        );
        if let Some(p) = &self.pending {
            self.validate_pending(p, manifest)?;
        }
        Ok(())
    }
    fn validate_pending(
        &self,
        p: &TransferChunkRequest,
        manifest: &TransferManifest,
    ) -> Result<()> {
        p.validate()?;
        let (offset, size) = match p.file {
            TransferFile::Manifest => (self.manifest_offset, manifest.manifest_bytes),
            TransferFile::Bundle => (self.bundle_offset, manifest.bundle_bytes),
        };
        ensure!(
            p.identity == self.identity
                && p.transfer_id == manifest.transfer_id
                && p.offset == offset
                && p.offset + u64::from(p.length) <= size
                && (p.file != TransferFile::Bundle
                    || self.manifest_offset == manifest.manifest_bytes),
            "invalid pending upload association"
        );
        Ok(())
    }
}

/// Bytes are deliberately not Debug/Serialize: the journal stores their digest,
/// and reconstructs the same immutable chunk from the retained artifact.
pub enum UploadAction {
    Begin {
        identity: NodeIdentity,
        operation_id: Uuid,
        manifest: TransferManifest,
    },
    Chunk {
        request: TransferChunkRequest,
        bytes: Vec<u8>,
    },
    Finalize(TransferStatusRequest),
}
impl UploadAction {
    pub async fn execute(&self, client: &TransferClient) -> Result<TransferStatus> {
        match self {
            Self::Begin {
                identity,
                operation_id,
                manifest,
            } => client.begin(identity, *operation_id, manifest).await,
            Self::Chunk { request, bytes } => client.upload(request, bytes.clone()).await,
            Self::Finalize(request) => client.finalize(request).await,
        }
    }
}

#[cfg(feature = "synthetic-runtime")]
mod manifest;

#[cfg(feature = "synthetic-runtime")]
impl<C: Clock> Runtime<C> {
    /// Establish exactly one transport identity for a stopped exported source.
    /// Existing manually managed transfers remain under their original owner.
    pub fn prepare_source_upload(&mut self, id: Uuid, git: &GitClient) -> Result<()> {
        let record = self.source_attempt(id)?;
        let source = record.source.as_ref().context("source missing")?;
        ensure!(
            source.managed_root.as_deref() == Some(git.root()),
            "source root changed"
        );
        if source.receipt.is_some() {
            return Ok(());
        }
        ensure!(
            record.result.is_none()
                && record.stop_certainty == StopCertainty::ConfirmedStopped
                && record
                    .stopped_proof
                    .as_ref()
                    .is_some_and(|p| p.kernel_wait_echild
                        && p.phase == crate::guardian::GuardianPhase::Stopped),
            "upload requires stopped proof and unacknowledged report"
        );
        if let Some(upload) = &source.upload {
            return upload.validate(
                source.transfer.as_ref().context("upload manifest absent")?,
                self.journal.state().identity.node_id,
            );
        }
        ensure!(
            source.transfer.is_none(),
            "existing externally managed source transfer requires original recovery"
        );
        let manifest = manifest::retained_manifest(record, source, git)?;
        let upload = UploadState {
            identity: self.journal.state().identity.clone(),
            begin_operation: Uuid::new_v4(),
            finalize_operation: Uuid::new_v4(),
            begun: false,
            manifest_offset: 0,
            bundle_offset: 0,
            pending: None,
        };
        upload.validate(&manifest, self.journal.state().identity.node_id)?;
        self.journal.transaction(|s| {
            let source = s.attempt_source_mut(id);
            source.transfer = Some(manifest);
            source.upload = Some(upload);
            Ok(())
        })
    }

    pub fn next_source_upload(&mut self, id: Uuid) -> Result<Option<UploadAction>> {
        let record = self.source_attempt(id)?;
        let source = record.source.as_ref().context("source missing")?;
        if source.receipt.is_some() {
            return Ok(None);
        }
        ensure!(record.result.is_none(), "source already acknowledged");
        let m = source
            .transfer
            .as_ref()
            .context("transport manifest missing")?
            .clone();
        let upload = source
            .upload
            .as_ref()
            .context("upload intent missing")?
            .clone();
        upload.validate(&m, self.journal.state().identity.node_id)?;
        if upload.begun {
            source
                .managed_root
                .as_ref()
                .context("source root missing")?;
        }
        let proposed = super::steps::propose_upload(
            &upload,
            &m,
            source.managed_root.as_deref(),
            self.journal.state().identity.node_id,
        )?;
        if proposed.state.pending != upload.pending {
            self.journal.transaction(|s| {
                s.attempt_source_mut(id).upload = Some(proposed.state);
                Ok(())
            })?;
        }
        Ok(Some(proposed.action))
    }

    pub fn acknowledge_source_upload(
        &mut self,
        id: Uuid,
        action: &UploadAction,
        status: TransferStatus,
    ) -> Result<()> {
        let source = self
            .journal
            .state()
            .attempts
            .get(&id)
            .and_then(|r| r.source.as_ref())
            .context("source missing")?;
        let manifest = source
            .transfer
            .as_ref()
            .context("transport manifest missing")?;
        let upload = source.upload.as_ref().context("upload missing")?;
        let next = match super::steps::acknowledge_upload(
            upload,
            manifest,
            (action, status),
            self.journal.state().identity.node_id,
        )? {
            super::steps::UploadAcknowledgment::Progress(next) => next,
            super::steps::UploadAcknowledgment::RetentionClaim(receipt) => {
                return self.acknowledge_source(id, receipt);
            }
        };
        self.journal.transaction(|s| {
            s.attempt_source_mut(id).upload = Some(next);
            Ok(())
        })
    }
}
