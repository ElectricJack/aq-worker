//! Finite original input operations. No Prepare/launch/grant gate exists here.
use super::*;
use crate::native::{
    VerifiedArtifact,
    materials::{InstalledMaterial, Record},
};
use aq_admission::{
    RetainedArtifact,
    transport::{ArtifactFetch, ArtifactReceipt, MaterialFetch},
};
pub(super) const MAX_RECORDS: usize = 1024;
pub(super) const MAX_REQUESTS: usize = 4096;
#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ArtifactDownload {
    pub descriptor: RetainedArtifact,
    pub requests: Vec<ArtifactFetch>,
    pub request_digests: Vec<Digest>,
    pub preparation: InputOrigin,
    pub created_revision: u64,
    pub created_ns: u64,
    pub receipt: Option<ArtifactReceipt>,
    pub completed_ns: Option<u64>,
}
#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MaterialDownload {
    pub request: MaterialFetch,
    pub request_digest: Digest,
    pub preparation: InputOrigin,
    pub created_revision: u64,
    pub created_ns: u64,
    installation: Option<Record>,
    recorded_ns: Option<u64>,
}
impl MaterialDownload {
    pub fn receipt(&self) -> Result<Option<aq_admission::MaterialReceipt>> {
        self.installation.as_ref().map(Record::receipt).transpose()
    }
}
fn key(reference: &ArtifactRef) -> Result<String> {
    Ok(canonical_digest(reference)?.as_str().into())
}
fn capture(attempt: &Attempt) -> Result<InputOrigin> {
    InputOrigin::capture(attempt)
}
pub(super) fn validate_capture(
    attempt: &Attempt,
    p: &RecordedPreparation,
    created_revision: u64,
    created_ns: u64,
    current_revision: u64,
) -> Result<()> {
    let accept = attempt
        .acceptance
        .as_ref()
        .context("input without durable acceptance")?;
    p.delivery.validate_for(accept, p.delivery.delivery.nonce)?;
    let maximum = p.delivery.delivery.client_deadline_ns(
        p.sent_ns,
        p.received_ns,
        Some(attempt.reservation.deadline_ns),
    )?;
    ensure!(
        created_revision > 0
            && created_revision < current_revision
            && p.received_ns >= p.sent_ns
            && p.received_ns <= created_ns
            && created_ns < p.deadline_ns
            && p.deadline_ns <= maximum
            && same(
                &p.delivery.original,
                &attempt
                    .preparation
                    .as_ref()
                    .context("legacy preparation absent")?
                    .delivery
                    .original
            )?,
        "input preparation provenance"
    );
    Ok(())
}
pub(super) fn validate(
    attempt: &Attempt,
    identity: &WorkerIdentity,
    kernel: Uuid,
    revision: u64,
    ids: &mut BTreeSet<Uuid>,
) -> Result<()> {
    ensure!(
        attempt.downloads.len() <= 126 && attempt.material_downloads.len() <= 32,
        "attempt input operation bound"
    );
    for (k, op) in &attempt.downloads {
        op.preparation
            .validate(attempt, op.created_revision, op.created_ns, revision)?;
        op.descriptor.validate()?;
        ensure!(
            k == &key(&op.descriptor.artifact)?
                && op.descriptor.bytes <= crate::native::MAX_INLINE_BYTES
                && attempt
                    .reservation
                    .offer
                    .offered
                    .artifacts
                    .iter()
                    .any(|d| same(d, &op.descriptor).unwrap_or(false)),
            "artifact original descriptor"
        );
        let count = op
            .descriptor
            .bytes
            .div_ceil(crate::native::MAX_CHUNK_BYTES as u64) as usize;
        ensure!(
            op.requests.len() == count && op.request_digests.len() == count,
            "artifact request plan coverage"
        );
        for (i, request) in op.requests.iter().enumerate() {
            TransportDocument::ArtifactFetch(request.clone()).validate()?;
            let offset = i as u64 * crate::native::MAX_CHUNK_BYTES as u64;
            ensure!(
                request.identity == *identity
                    && request.binding == *op.preparation.binding()
                    && request.artifact == op.descriptor.artifact
                    && request.offset == offset
                    && request.length as u64
                        == (op.descriptor.bytes - offset)
                            .min(crate::native::MAX_CHUNK_BYTES as u64)
                    && ids.insert(request.operation_id)
                    && op.request_digests[i] == canonical_digest(request)?,
                "artifact immutable request identity"
            );
        }
        ensure!(
            op.receipt.is_some() == op.completed_ns.is_some(),
            "orphan artifact receipt"
        );
        if let Some(receipt) = &op.receipt {
            ensure!(
                receipt.artifact == op.descriptor.artifact
                    && receipt.bytes == op.descriptor.bytes
                    && receipt.digest == op.descriptor.artifact.digest
                    && op
                        .completed_ns
                        .is_some_and(|t| t >= op.created_ns && t < op.preparation.deadline_ns()),
                "artifact completion differs"
            );
        }
    }
    for (lease_id, op) in &attempt.material_downloads {
        op.preparation
            .validate(attempt, op.created_revision, op.created_ns, revision)?;
        TransportDocument::MaterialFetch(op.request.clone()).validate()?;
        let lease = op
            .preparation
            .materials()
            .iter()
            .find(|l| l.lease_id == *lease_id)
            .context("material lease not original")?;
        ensure!(
            op.request.identity == *identity
                && op.request.binding == lease.binding
                && op.request.lease_id == *lease_id
                && ids.insert(op.request.operation_id)
                && op.request_digest == canonical_digest(&op.request)?,
            "material immutable request identity"
        );
        ensure!(
            op.installation.is_some() == op.recorded_ns.is_some(),
            "orphan material pin"
        );
        if let Some(record) = &op.installation {
            record.validate()?;
            let receipt = record.receipt()?;
            receipt.validate_against(lease, receipt.received_at_ms)?;
            ensure!(
                record.operation_id == op.request.operation_id
                    && record.worker == *identity
                    && record.kernel_boot_id == kernel
                    && record.preparation_digest == op.preparation.digest()?
                    && same(&record.delivery.lease, lease)?
                    && record.material_synced_ns >= op.created_ns
                    && op
                        .recorded_ns
                        .is_some_and(|t| t >= record.material_synced_ns
                            && t < record
                                .retrieval_deadline_ns
                                .min(op.preparation.deadline_ns())),
                "material independent installation pin"
            );
        }
    }
    Ok(())
}
fn artifact_plan(
    attempt: &Attempt,
    identity: &WorkerIdentity,
    revision: u64,
    now: u64,
    reference: &ArtifactRef,
) -> Result<ArtifactDownload> {
    let preparation = capture(attempt)?;
    ensure!(now < preparation.deadline_ns(), "input preparation expired");
    let key = key(reference)?;
    if let Some(old) = attempt.downloads.get(&key) {
        ensure!(
            now < old.preparation.deadline_ns(),
            "original download expired"
        );
        return Ok(old.clone());
    }
    let descriptor = attempt
        .reservation
        .offer
        .offered
        .artifacts
        .iter()
        .find(|d| d.artifact == *reference)
        .context("artifact not in original offer")?
        .clone();
    ensure!(
        descriptor.bytes <= crate::native::MAX_INLINE_BYTES,
        "inline artifact bound"
    );
    let mut requests = vec![];
    let mut request_digests = vec![];
    for offset in (0..descriptor.bytes).step_by(crate::native::MAX_CHUNK_BYTES) {
        let request = ArtifactFetch {
            schema_version: 1,
            operation_id: Uuid::new_v4(),
            identity: identity.clone(),
            binding: preparation.binding().clone(),
            artifact: reference.clone(),
            offset,
            length: (descriptor.bytes - offset).min(crate::native::MAX_CHUNK_BYTES as u64) as u32,
        };
        request_digests.push(canonical_digest(&request)?);
        requests.push(request);
    }
    Ok(ArtifactDownload {
        descriptor,
        requests,
        request_digests,
        preparation: preparation.clone(),
        created_revision: revision,
        created_ns: now,
        receipt: None,
        completed_ns: None,
    })
}
impl Journal {
    pub(crate) fn check_preparation(&self, id: Uuid, observed: &Preparation) -> Result<()> {
        let now = self.live()?;
        ensure!(
            self.state
                .attempts
                .get(&id)
                .is_some_and(|a| a.phase_acceptance.is_none() && a.refresh_acceptance.is_none()),
            "legacy input on phase attempt"
        );
        let saved = self
            .state
            .attempts
            .get(&id)
            .context("unknown native attempt")?
            .preparation
            .as_ref()
            .context("legacy preparation absent")?;
        ensure!(
            now < saved.deadline_ns
                && saved.sent_ns == observed.sent_ns()
                && saved.received_ns == observed.received_ns()
                && saved.deadline_ns == observed.deadline_ns()
                && same(&saved.delivery, observed.message())?,
            "preparation observation not durable/current"
        );
        Ok(())
    }
    /// Plans one exact descriptor before I/O; use the batch owner for full sets.
    pub fn begin_artifact_download(
        &mut self,
        id: Uuid,
        reference: &ArtifactRef,
    ) -> Result<ArtifactDownload> {
        ensure!(
            self.state
                .attempts
                .get(&id)
                .is_some_and(|a| a.phase_acceptance.is_none() && a.refresh_acceptance.is_none()),
            "legacy artifact on phase attempt"
        );
        self.artifact_download(id, reference)
    }
    pub(crate) fn begin_artifact_context(
        &mut self,
        id: Uuid,
        reference: &ArtifactRef,
        context: &crate::native::preparation::InputContext,
    ) -> Result<ArtifactDownload> {
        context.check(self, id)?;
        self.artifact_download(id, reference)
    }
    fn artifact_download(&mut self, id: Uuid, reference: &ArtifactRef) -> Result<ArtifactDownload> {
        let now = self.live()?;
        let attempt = self
            .state
            .attempts
            .get(&id)
            .context("unknown native attempt")?;
        let op = artifact_plan(
            attempt,
            &self.state.identity,
            self.state.revision,
            now,
            reference,
        )?;
        let key = key(reference)?;
        if attempt.downloads.contains_key(&key) {
            return Ok(op);
        }
        let mut state = self.next()?;
        state
            .attempts
            .get_mut(&id)
            .unwrap()
            .downloads
            .insert(key, op.clone());
        self.persist(state)?;
        Ok(op)
    }
    /// Whole original inline set, exact IDs durably planned in one transaction.
    pub(crate) fn begin_artifact_batch(
        &mut self,
        id: Uuid,
        observed: &crate::native::preparation::InputContext,
    ) -> Result<Vec<ArtifactDownload>> {
        observed.check(self, id)?;
        let now = self.live()?;
        let a = self
            .state
            .attempts
            .get(&id)
            .context("unknown native attempt")?;
        let total = a
            .reservation
            .offer
            .offered
            .artifacts
            .iter()
            .try_fold(0u64, |sum, d| sum.checked_add(d.bytes))
            .context("inline set overflow")?;
        ensure!(
            total <= crate::native::MAX_INLINE_SET_BYTES,
            "inline set exceeds bounded batch"
        );
        let mut plans = Vec::new();
        let mut next = self.next()?;
        let mut changed = false;
        for d in &a.reservation.offer.offered.artifacts {
            let op = artifact_plan(
                a,
                &self.state.identity,
                self.state.revision,
                now,
                &d.artifact,
            )?;
            let key = key(&d.artifact)?;
            if !a.downloads.contains_key(&key) {
                next.attempts
                    .get_mut(&id)
                    .unwrap()
                    .downloads
                    .insert(key, op.clone());
                changed = true;
            }
            plans.push(op);
        }
        if changed {
            self.persist(next)?;
        }
        observed.check(self, id)?;
        Ok(plans)
    }
    /// Verified bytes remain in memory. Commit the complete exact receipt set
    /// once; a failed download never fabricates a partially complete batch.
    pub(crate) fn complete_artifact_batch(
        &mut self,
        id: Uuid,
        observed: &crate::native::preparation::InputContext,
        verified: &[VerifiedArtifact],
    ) -> Result<()> {
        observed.check(self, id)?;
        let now = self.live()?;
        let a = self
            .state
            .attempts
            .get(&id)
            .context("unknown native attempt")?;
        ensure!(
            verified.len() == a.reservation.offer.offered.artifacts.len(),
            "incomplete inline batch"
        );
        let mut keys = BTreeSet::new();
        let mut next = self.next()?;
        let mut changed = false;
        for v in verified {
            let key = key(&v.receipt().artifact)?;
            ensure!(keys.insert(key.clone()), "duplicate inline batch input");
            let op = a.downloads.get(&key).context("inline batch plan absent")?;
            ensure!(
                now < op.preparation.deadline_ns() && same(&op.descriptor, &v.descriptor)?,
                "inline batch scope/expiry"
            );
            let receipt = v.receipt();
            if let Some(old) = &op.receipt {
                ensure!(same(old, &receipt)?, "changed inline batch completion");
            } else {
                let op = next
                    .attempts
                    .get_mut(&id)
                    .unwrap()
                    .downloads
                    .get_mut(&key)
                    .unwrap();
                op.receipt = Some(receipt);
                op.completed_ns = Some(now);
                changed = true;
            }
        }
        if changed {
            self.persist(next)?;
        }
        observed.check(self, id)?;
        Ok(())
    }
    pub fn complete_artifact_download(
        &mut self,
        id: Uuid,
        verified: &VerifiedArtifact,
    ) -> Result<()> {
        ensure!(
            self.state
                .attempts
                .get(&id)
                .is_some_and(|a| a.phase_acceptance.is_none() && a.refresh_acceptance.is_none()),
            "legacy artifact completion on phase attempt"
        );
        self.finish_artifact(id, verified)
    }
    pub(crate) fn complete_artifact_context(
        &mut self,
        id: Uuid,
        verified: &VerifiedArtifact,
        context: &crate::native::preparation::InputContext,
    ) -> Result<()> {
        context.check(self, id)?;
        self.finish_artifact(id, verified)?;
        context.check(self, id)
    }
    fn finish_artifact(&mut self, id: Uuid, verified: &VerifiedArtifact) -> Result<()> {
        let now = self.live()?;
        let key = key(&verified.receipt().artifact)?;
        let attempt = self
            .state
            .attempts
            .get(&id)
            .context("unknown native attempt")?;
        let op = attempt
            .downloads
            .get(&key)
            .context("artifact intent not durable")?;
        ensure!(
            now < capture(attempt)?
                .deadline_ns()
                .min(op.preparation.deadline_ns())
                && same(&op.descriptor, &verified.descriptor)?,
            "artifact completion scope/expiry"
        );
        let receipt = verified.receipt();
        if let Some(old) = &op.receipt {
            ensure!(same(old, &receipt)?, "changed artifact completion");
            return Ok(());
        }
        let mut state = self.next()?;
        let op = state
            .attempts
            .get_mut(&id)
            .unwrap()
            .downloads
            .get_mut(&key)
            .unwrap();
        op.receipt = Some(receipt);
        op.completed_ns = Some(now);
        self.persist(state)
    }
    pub fn begin_material_download(&mut self, id: Uuid, lease: Uuid) -> Result<MaterialDownload> {
        ensure!(
            self.state
                .attempts
                .get(&id)
                .is_some_and(|a| a.phase_acceptance.is_none() && a.refresh_acceptance.is_none()),
            "legacy material on phase attempt"
        );
        self.material_download(id, lease)
    }
    pub(crate) fn begin_material_context(
        &mut self,
        id: Uuid,
        lease: Uuid,
        context: &crate::native::preparation::InputContext,
    ) -> Result<MaterialDownload> {
        context.check(self, id)?;
        self.material_download(id, lease)
    }
    fn material_download(&mut self, id: Uuid, lease: Uuid) -> Result<MaterialDownload> {
        let now = self.live()?;
        let attempt = self
            .state
            .attempts
            .get(&id)
            .context("unknown native attempt")?;
        let preparation = capture(attempt)?;
        ensure!(now < preparation.deadline_ns(), "input preparation expired");
        if let Some(old) = attempt.material_downloads.get(&lease) {
            ensure!(
                now < old.preparation.deadline_ns(),
                "original material download expired"
            );
            return Ok(old.clone());
        }
        ensure!(
            preparation.materials().iter().any(|l| l.lease_id == lease),
            "material lease not original"
        );
        let request = MaterialFetch {
            schema_version: 1,
            operation_id: Uuid::new_v4(),
            identity: self.state.identity.clone(),
            binding: preparation.binding().clone(),
            lease_id: lease,
        };
        let request_digest = canonical_digest(&request)?;
        let op = MaterialDownload {
            request,
            request_digest,
            preparation: preparation.clone(),
            created_revision: self.state.revision,
            created_ns: now,
            installation: None,
            recorded_ns: None,
        };
        let mut state = self.next()?;
        state
            .attempts
            .get_mut(&id)
            .unwrap()
            .material_downloads
            .insert(lease, op.clone());
        self.persist(state)?;
        Ok(op)
    }
    pub fn pin_installed_material(
        &mut self,
        id: Uuid,
        installed: &InstalledMaterial,
    ) -> Result<()> {
        let now = self.live()?;
        let receipt = installed.receipt()?;
        let attempt = self
            .state
            .attempts
            .get(&id)
            .context("unknown native attempt")?;
        let op = attempt
            .material_downloads
            .get(&receipt.lease_id)
            .context("material intent not durable")?;
        ensure!(
            now < capture(attempt)?
                .deadline_ns()
                .min(op.preparation.deadline_ns())
                .min(installed.metadata().retrieval_deadline_ns),
            "material pin expired"
        );
        installed.read_private()?;
        if let Some(old) = op.receipt()? {
            ensure!(same(&old, &receipt)?, "material pin changed");
            return Ok(());
        }
        let mut state = self.next()?;
        let op = state
            .attempts
            .get_mut(&id)
            .unwrap()
            .material_downloads
            .get_mut(&receipt.lease_id)
            .unwrap();
        op.installation = Some(installed.metadata().clone());
        op.recorded_ns = Some(now);
        self.persist(state)
    }
}
