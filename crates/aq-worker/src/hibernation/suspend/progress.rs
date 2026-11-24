use super::*;

pub(super) struct Suspension<'a> {
    intent: &'a SuspensionIntent,
    owner: &'a mut Suspender,
    journal: Journal,
    state: State,
}
impl<'a> Suspension<'a> {
    pub(super) fn open(owner: &'a mut Suspender, intent: &'a SuspensionIntent) -> Result<Self> {
        let journal = owner.journal()?;
        let digest = canonical_digest(intent)?;
        let (state, created) = journal.open_bound(&digest, || {
            Ok(State {
                intent_digest: digest.clone(),
                intent: intent.clone(),
                checkpoint_message_id: Uuid::new_v4(),
                parking_message_id: Uuid::new_v4(),
                disk_message_id: Uuid::new_v4(),
                stop_observation_id: Uuid::new_v4(),
                checkpoint: None,
                checkpoint_recorded: false,
                input_fence: None,
                stop: None,
                stop_proof: None,
                hold: None,
                source: None,
                credential_fingerprints: Vec::new(),
                scrubbed: false,
                uploads: Vec::new(),
                effects: None,
                compute_released: false,
                parking: None,
                disk: None,
            })
        })?;
        if created {
            owner.boundaries.mark("intent_journaled")?;
        }
        Ok(Self {
            owner,
            journal,
            state,
            intent,
        })
    }
    pub(super) fn run(
        &mut self,
        ports: &mut impl SuspensionPorts,
        aq: &mut impl LifecycleApi,
        retention: &mut impl CentralRetention,
    ) -> Result<Suspended> {
        if let Some(parked) = &self.state.parking {
            let (report, receipt) = self.state.disk.as_ref().unwrap_or(parked);
            return Ok(Suspended {
                report: report.clone(),
                receipt: receipt.clone(),
            });
        }

        self.collect_checkpoint(ports, retention)?;
        let checkpoint = self.record_checkpoint(aq)?;
        let fence = self.close_input(ports)?;
        let stop = self.stop_writers(ports)?;
        if let StopObservation::Unknown { reason } = &stop {
            return self.park(
                Some(format!("writer stop not proven: {}", reason.as_str())),
                aq,
            );
        }
        self.retain_proof(&stop, retention)?;
        let checkpoint_bytes = serde_json::to_vec(&checkpoint)?;
        self.snapshot(&checkpoint_bytes)?;
        self.scrub()?;
        if let Some(hold) = self.state.hold.clone() {
            return self.park(Some(hold), aq);
        }
        self.retain_artifacts(&checkpoint_bytes, retention)?;
        self.reconcile(&checkpoint, &fence, ports)?;
        self.release_compute(&stop, ports)?;
        self.park(None, aq)
    }
    fn collect_checkpoint(
        &mut self,
        ports: &mut impl SuspensionPorts,
        retention: &mut impl CentralRetention,
    ) -> Result<()> {
        if self.state.checkpoint.is_none() {
            let request = CheckpointRequest::for_intent(self.intent)?;
            let retained = ports.request_checkpoint(&request)?;
            let claim = agent_checkpoint_claim(&request, retained.as_ref(), retention)?;
            let claim = bounded_claim(claim, self.intent)?;
            let checkpoint = Checkpoint {
                binding: self.intent.binding.clone(),
                continuation: self.intent.continuation.clone(),
                claim,
            };
            checkpoint.validate()?;
            self.state.checkpoint = Some(checkpoint);
            self.save("checkpoint_journaled")?;
        }
        Ok(())
    }
    fn record_checkpoint(&mut self, aq: &mut impl LifecycleApi) -> Result<Checkpoint> {
        let checkpoint = self
            .state
            .checkpoint
            .clone()
            .context("journaled checkpoint")?;
        if !self.state.checkpoint_recorded {
            let packet = Packet::seal(
                self.state.checkpoint_message_id,
                Message::Checkpoint(checkpoint.clone()),
            )?;
            send_exact(aq, &packet)?;
            self.state.checkpoint_recorded = true;
            self.save("checkpoint_recorded")?;
        }
        Ok(checkpoint)
    }
    fn close_input(&mut self, ports: &mut impl SuspensionPorts) -> Result<InputFence> {
        if self.state.input_fence.is_none() {
            self.state.input_fence = Some(ports.close_input()?);
            self.save("input_closed")?;
        }
        self.state
            .input_fence
            .clone()
            .context("journaled input fence")
    }
    fn stop_writers(&mut self, ports: &mut impl SuspensionPorts) -> Result<StopObservation> {
        if self.state.stop.is_none() {
            let (stop, proof) = match ports.stop_writers()? {
                WriterStop::Unknown { reason } => (
                    StopObservation::Unknown {
                        reason: text(reason)?,
                    },
                    None,
                ),
                WriterStop::Stopped {
                    proof,
                    writer_count,
                    observed_at_ms,
                } => {
                    let c = &self.intent.continuation;
                    let id = format!(
                        "hibernation-{}-{}-g{}",
                        role_name(&ArtifactRole::Result),
                        c.continuation_id,
                        c.generation
                    );
                    let stop = proof.observation(
                        self.state.stop_observation_id,
                        writer_count,
                        observed_at_ms,
                        text(id)?,
                    )?;
                    (stop, Some(proof))
                }
            };
            stop.validate()?;
            self.state.stop = Some(stop);
            self.state.stop_proof = proof;
            self.save("stop_recorded")?;
        }
        self.state.stop.clone().context("journaled stop")
    }
    fn retain_proof(
        &mut self,
        stop: &StopObservation,
        retention: &mut impl CentralRetention,
    ) -> Result<()> {
        let proof = self
            .state
            .stop_proof
            .clone()
            .context("journaled stop proof")?;
        let proof_bytes = proof.to_bytes()?;
        self.upload(ArtifactRole::Result, &proof_bytes, retention)?;
        require(
            aq_lifecycle_protocol::stop_proof_receipt(stop, &self.receipts()).is_some(),
            "retained stop proof does not match the reported observation",
        )?;
        Ok(())
    }
    fn snapshot(&mut self, checkpoint_bytes: &[u8]) -> Result<()> {
        if self.state.source.is_none() && self.state.hold.is_none() {
            let secrets = ScrubbedSecrets::capture(&self.owner.layout)?;
            self.state.credential_fingerprints = secrets.fingerprints.clone();
            match SourceArchive::capture(&self.owner.layout.workspace())? {
                Err(Unsupported(why)) => self.state.hold = Some(why.to_owned()),
                Ok(archive) => {
                    let leak = archive
                        .decoded()?
                        .iter()
                        .find_map(|(path, bytes)| secrets.scan(path, bytes))
                        .or_else(|| secrets.scan("checkpoint", checkpoint_bytes));
                    if let Some(why) = leak {
                        self.state.hold = Some(why.to_owned());
                    } else {
                        let bytes = archive.to_bytes()?;
                        crate::guardian::durable_replace(
                            &self.owner.dir.join(SOURCE_FILE),
                            &bytes,
                        )?;
                        self.state.source = Some((Digest::of(&bytes), bytes.len() as u64));
                    }
                }
            }
            self.save("snapshot_recorded")?;
        }
        Ok(())
    }
    fn scrub(&mut self) -> Result<()> {
        if !self.state.scrubbed {
            credentials::scrub(&self.owner.layout)?;
            self.state.scrubbed = true;
            self.save("credentials_scrubbed")?;
        }
        Ok(())
    }
    fn retain_artifacts(
        &mut self,
        checkpoint_bytes: &[u8],
        retention: &mut impl CentralRetention,
    ) -> Result<()> {
        let source =
            std::fs::read(self.owner.dir.join(SOURCE_FILE)).context("retained snapshot")?;
        let (source_digest, source_len) =
            self.state.source.clone().context("journaled snapshot")?;
        require(
            Digest::of(&source) == source_digest && source.len() as u64 == source_len,
            "local snapshot changed after it was journaled",
        )?;
        for (role, bytes) in [
            (ArtifactRole::Source, source.as_slice()),
            (ArtifactRole::Checkpoint, checkpoint_bytes),
        ] {
            self.upload(role, bytes, retention)?;
        }
        Ok(())
    }
    fn reconcile(
        &mut self,
        checkpoint: &Checkpoint,
        fence: &InputFence,
        ports: &mut impl SuspensionPorts,
    ) -> Result<()> {
        if self.state.effects.is_none() {
            let effects = ports.reconcile_effects(fence, pending_operations(Some(checkpoint)))?;
            effects.validate()?;
            self.state.effects = Some(effects);
            self.save("effects_recorded")?;
        }
        Ok(())
    }
    fn release_compute(
        &mut self,
        stop: &StopObservation,
        ports: &mut impl SuspensionPorts,
    ) -> Result<()> {
        if !self.state.compute_released {
            ports.release_compute(stop)?;
            self.state.compute_released = true;
            self.save("compute_released")?;
        }
        Ok(())
    }
    fn save(&mut self, boundary: &str) -> Result<()> {
        self.owner.save(&self.journal, &self.state, boundary)
    }
    fn upload(
        &mut self,
        role: ArtifactRole,
        bytes: &[u8],
        retention: &mut impl CentralRetention,
    ) -> Result<()> {
        let name = role_name(&role);
        let index = match self.state.uploads.iter().position(|u| u.role == role) {
            Some(i) => i,
            None => {
                let c = &self.state.intent.continuation;
                self.state.uploads.push(Upload {
                    role: role.clone(),
                    operation_id: Uuid::new_v4(),
                    artifact: artifact_ref(
                        format!("hibernation-{name}-{}-g{}", c.continuation_id, c.generation),
                        bytes,
                    )?,
                    total: bytes.len() as u64,
                    offset: 0,
                    receipt: None,
                });
                self.save(&format!("upload_journaled:{name}"))?;
                self.state.uploads.len() - 1
            }
        };
        require(
            Digest::of(bytes) == self.state.uploads[index].artifact.digest,
            "retained bytes differ from the journaled artifact",
        )?;
        while self.state.uploads[index].offset < self.state.uploads[index].total {
            let upload = &self.state.uploads[index];
            let start = upload.offset as usize;
            let end = (start + self.owner.chunk_bytes).min(bytes.len());
            let committed =
                retention.upload_chunk(upload.operation_id, upload.offset, &bytes[start..end])?;
            require(
                committed == end as u64,
                "retention acknowledged an unexpected offset",
            )?;
            self.state.uploads[index].offset = committed;
            self.save(&format!("upload_chunk:{name}"))?;
        }
        if self.state.uploads[index].receipt.is_none() {
            let upload = &self.state.uploads[index];
            let receipt = retention.commit(
                upload.operation_id,
                &upload.artifact,
                role.clone(),
                &self.owner.local,
            )?;
            receipt.validate()?;
            require(
                receipt.artifact == upload.artifact
                    && receipt.role == role
                    && receipt.bytes == upload.total
                    && receipt.binding_digest == canonical_digest(&self.owner.local)?,
                "retention receipt does not match the uploaded artifact",
            )?;
            self.state.uploads[index].receipt = Some(receipt);
            self.save(&format!("receipt_recorded:{name}"))?;
        }
        Ok(())
    }

    fn receipts(&self) -> Vec<RetentionReceipt> {
        self.state
            .uploads
            .iter()
            .filter_map(|u| u.receipt.clone())
            .collect()
    }

    fn report(&self, hold: Option<String>) -> Result<ParkingReport> {
        let state = &self.state;
        let stop = state
            .stop
            .clone()
            .context("parking needs a stop observation")?;
        let effects = state
            .effects
            .clone()
            .unwrap_or_else(|| EffectsObservation::Pending {
                operation_ids: pending_operations(state.checkpoint.as_ref()).to_vec(),
            });
        let disposition = match hold {
            Some(reason) => SuspensionDisposition::Held {
                reason: text(reason)?,
            },
            None if matches!(effects, EffectsObservation::Reconciled { .. }) => {
                SuspensionDisposition::RetainedSuspended
            }
            None => SuspensionDisposition::StoppedRetaining,
        };
        let local = std::fs::metadata(self.owner.dir.join(SOURCE_FILE)).map_or(0, |m| m.len());
        let report = ParkingReport {
            binding: self.state.intent.binding.clone(),
            continuation: self.state.intent.continuation.clone(),
            disposition,
            stop,
            local_retained_bytes: local,
            central_receipts: self.receipts(),
            effects,
            compute_released: self.state.compute_released,
            protected_disk_released: false,
            disk_release_directive: None,
        };
        report.validate()?;
        Ok(report)
    }

    fn park(&mut self, hold: Option<String>, aq: &mut impl LifecycleApi) -> Result<Suspended> {
        let report = self.report(hold)?;
        let packet = Packet::seal(
            self.state.parking_message_id,
            Message::Parking(report.clone()),
        )?;
        let receipt = send_exact(aq, &packet)?;
        self.state.parking = Some((report.clone(), receipt.clone()));
        self.save("parking_reported")?;
        Ok(Suspended { report, receipt })
    }
}

fn pending_operations(checkpoint: Option<&Checkpoint>) -> &[Uuid] {
    match checkpoint.map(|c| &c.claim) {
        Some(CheckpointClaim::Supplied { summary, .. }) => &summary.pending_operation_ids,
        _ => &[],
    }
}
