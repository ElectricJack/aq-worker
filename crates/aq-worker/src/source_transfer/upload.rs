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
                && !self.begin_operation.is_nil()
                && !self.finalize_operation.is_nil()
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
                || (self.manifest_offset == 0 && self.bundle_offset == 0 && self.pending.is_none()),
            "unbegun upload has progress"
        );
        if let Some(p) = &self.pending {
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
        }
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

impl<C: Clock> Runtime<C> {
    /// Establish exactly one transport identity for a stopped exported source.
    /// Existing manually managed transfers remain under their original owner.
    pub fn prepare_source_upload(&mut self, id: Uuid, git: &GitClient) -> Result<()> {
        let record = self
            .journal
            .state()
            .attempts
            .get(&id)
            .context("unknown attempt")?;
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
        let exported = source.exported.as_ref().context("export required")?;
        let artifact: Uuid = serde_json::from_value(exported["artifact_handle"].clone())?;
        let mut file = artifact_file(git.root(), artifact, false, "manifest.json", false)?;
        ensure!(
            file.metadata()?.len() <= 1024 * 1024,
            "artifact manifest bound"
        );
        let mut bytes = Vec::new();
        Read::by_ref(&mut file)
            .take(1024 * 1024 + 1)
            .read_to_end(&mut bytes)?;
        ensure!(bytes.len() <= 1024 * 1024, "artifact manifest bound");
        let m: GitManifest = serde_json::from_slice(&bytes)?;
        ensure!(
            serde_json::to_value(&m)? == exported["manifest"]
                && m.format_version == 1
                && m.transfer_id == artifact
                && m.repository_id == source.binding.repository_id
                && m.repository_identity == source.binding.logical_identity
                && m.binding_generation == source.binding.binding_generation
                && m.base == source.binding.base
                && m.source
                    == source
                        .checkpoint
                        .as_ref()
                        .context("checkpoint required")?
                        .source
                && m.object_format == "sha1"
                && m.reference_only == (m.bundle_size == 0),
            "retained artifact manifest changed"
        );
        let manifest = TransferManifest {
            transfer_id: Uuid::new_v4(),
            role: SourceRole::Checkpoint,
            checkpoint: m.source,
            attempt: record.offer.attempt.clone(),
            resolved_spec_digest: record.offer.spec.resolved_spec_digest.clone(),
            source: source.binding.clone(),
            artifact_id: artifact,
            manifest_digest: format!("{:x}", Sha256::digest(&bytes)),
            bundle_digest: m.bundle_sha256,
            manifest_bytes: bytes.len() as u64,
            bundle_bytes: m.bundle_size,
        };
        manifest.validate()?;
        ensure!(
            manifest
                .manifest_bytes
                .checked_add(manifest.bundle_bytes)
                .is_some_and(|n| n <= record.pending_bytes),
            "upload exceeds reserved artifact bytes"
        );
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
            let source = s.attempts.get_mut(&id).unwrap().source.as_mut().unwrap();
            source.transfer = Some(manifest);
            source.upload = Some(upload);
            Ok(())
        })
    }

    pub fn next_source_upload(&mut self, id: Uuid) -> Result<Option<UploadAction>> {
        let record = self
            .journal
            .state()
            .attempts
            .get(&id)
            .context("unknown attempt")?;
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
        if !upload.begun {
            return Ok(Some(UploadAction::Begin {
                identity: upload.identity,
                operation_id: upload.begin_operation,
                manifest: m,
            }));
        }
        let root = source
            .managed_root
            .as_ref()
            .context("source root missing")?;
        if let Some(request) = upload.pending {
            let size = if request.file == TransferFile::Manifest {
                m.manifest_bytes
            } else {
                m.bundle_bytes
            };
            let bytes = read_range(
                root,
                m.artifact_id,
                &request.file,
                request.offset,
                request.length as usize,
                size,
            )?;
            ensure!(
                format!("{:x}", Sha256::digest(&bytes)) == request.digest,
                "pending artifact bytes changed; preserved without replay"
            );
            return Ok(Some(UploadAction::Chunk { request, bytes }));
        }
        let remaining = if upload.manifest_offset < m.manifest_bytes {
            Some((
                TransferFile::Manifest,
                upload.manifest_offset,
                m.manifest_bytes,
            ))
        } else if upload.bundle_offset < m.bundle_bytes {
            Some((TransferFile::Bundle, upload.bundle_offset, m.bundle_bytes))
        } else {
            None
        };
        if let Some((file, offset, size)) = remaining {
            let length = (size - offset).min(MAX_CHUNK_BYTES as u64) as usize;
            let bytes = read_range(root, m.artifact_id, &file, offset, length, size)?;
            let request = TransferChunkRequest {
                identity: upload.identity,
                operation_id: Uuid::new_v4(),
                transfer_id: m.transfer_id,
                file,
                offset,
                length: length as u32,
                digest: format!("{:x}", Sha256::digest(&bytes)),
            };
            self.journal.transaction(|s| {
                s.attempts
                    .get_mut(&id)
                    .unwrap()
                    .source
                    .as_mut()
                    .unwrap()
                    .upload
                    .as_mut()
                    .unwrap()
                    .pending = Some(request.clone());
                Ok(())
            })?;
            return Ok(Some(UploadAction::Chunk { request, bytes }));
        }
        Ok(Some(UploadAction::Finalize(TransferStatusRequest {
            identity: upload.identity,
            operation_id: upload.finalize_operation,
            transfer_id: m.transfer_id,
        })))
    }

    pub fn acknowledge_source_upload(
        &mut self,
        id: Uuid,
        action: &UploadAction,
        status: TransferStatus,
    ) -> Result<()> {
        status.validate()?;
        let source = self
            .journal
            .state()
            .attempts
            .get(&id)
            .and_then(|r| r.source.as_ref())
            .context("source missing")?;
        let m = source
            .transfer
            .as_ref()
            .context("transport manifest missing")?;
        let upload = source.upload.as_ref().context("upload missing")?;
        upload.validate(m, self.journal.state().identity.node_id)?;
        ensure!(
            status.transfer_id == m.transfer_id,
            "upload response transfer mismatch"
        );
        let mut next = upload.clone();
        match action {
            UploadAction::Begin {
                identity,
                operation_id,
                manifest,
            } => {
                ensure!(
                    !upload.begun
                        && identity == &upload.identity
                        && operation_id == &upload.begin_operation
                        && manifest == m
                        && status.manifest_offset == 0
                        && status.bundle_offset == 0
                        && status.receipt.is_none(),
                    "unexpected begin receipt"
                );
                next.begun = true;
            }
            UploadAction::Chunk { request, bytes } => {
                ensure!(
                    upload.pending.as_ref() == Some(request)
                        && bytes.len() == request.length as usize
                        && format!("{:x}", Sha256::digest(bytes)) == request.digest
                        && status.receipt.is_none(),
                    "unexpected chunk receipt"
                );
                match request.file {
                    TransferFile::Manifest => next.manifest_offset += u64::from(request.length),
                    TransferFile::Bundle => next.bundle_offset += u64::from(request.length),
                }
                ensure!(
                    status.manifest_offset == next.manifest_offset
                        && status.bundle_offset == next.bundle_offset,
                    "unexpected chunk progress"
                );
                next.pending = None;
            }
            UploadAction::Finalize(request) => {
                ensure!(
                    request.identity == upload.identity
                        && request.operation_id == upload.finalize_operation
                        && request.transfer_id == m.transfer_id
                        && upload.begun
                        && upload.pending.is_none()
                        && upload.manifest_offset == m.manifest_bytes
                        && upload.bundle_offset == m.bundle_bytes
                        && status.manifest_offset == m.manifest_bytes
                        && status.bundle_offset == m.bundle_bytes,
                    "unexpected finalize receipt"
                );
                return self.acknowledge_source(
                    id,
                    status.receipt.context("central retention unconfirmed")?,
                );
            }
        }
        next.validate(m, self.journal.state().identity.node_id)?;
        self.journal.transaction(|s| {
            s.attempts
                .get_mut(&id)
                .unwrap()
                .source
                .as_mut()
                .unwrap()
                .upload = Some(next);
            Ok(())
        })
    }
}
