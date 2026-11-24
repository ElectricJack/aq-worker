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
pub(super) struct Creation {
    pub revision: u64,
    pub at_ns: u64,
}
impl Creation {
    pub fn new(revision: u64, at_ns: u64) -> Self {
        Self { revision, at_ns }
    }
}
pub(super) fn validate_capture(
    attempt: &Attempt,
    p: &RecordedPreparation,
    created: Creation,
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
        created.revision > 0
            && created.revision < current_revision
            && p.received_ns >= p.sent_ns
            && p.received_ns <= created.at_ns
            && created.at_ns < p.deadline_ns
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
pub(super) fn validate(attempt: &Attempt, state: &State, ids: &mut BTreeSet<Uuid>) -> Result<()> {
    ensure!(
        attempt.downloads.len() <= 126 && attempt.material_downloads.len() <= 32,
        "attempt input operation bound"
    );
    validate_artifacts(attempt, state, ids)?;
    validate_materials(attempt, state, ids)
}

impl ArtifactDownload {
    fn validate_completion(&self) -> Result<()> {
        ensure!(
            self.receipt.is_some() == self.completed_ns.is_some(),
            "orphan artifact receipt"
        );
        if let Some(receipt) = &self.receipt {
            ensure!(
                receipt.artifact == self.descriptor.artifact
                    && receipt.bytes == self.descriptor.bytes
                    && receipt.digest == self.descriptor.artifact.digest
                    && self
                        .completed_ns
                        .is_some_and(|t| t >= self.created_ns && t < self.preparation.deadline_ns()),
                "artifact completion differs"
            );
        }
        Ok(())
    }

    fn validate_requests(&self, identity: &WorkerIdentity, ids: &mut BTreeSet<Uuid>) -> Result<()> {
        let chunk = crate::native::MAX_CHUNK_BYTES as u64;
        let count = self.descriptor.bytes.div_ceil(chunk) as usize;
        ensure!(
            self.requests.len() == count && self.request_digests.len() == count,
            "artifact request plan coverage"
        );
        for (i, request) in self.requests.iter().enumerate() {
            TransportDocument::ArtifactFetch(request.clone()).validate()?;
            let offset = i as u64 * chunk;
            let expected_range = (offset, (self.descriptor.bytes - offset).min(chunk));
            ensure!(
                request.identity == *identity
                    && request.binding == *self.preparation.binding()
                    && request.artifact == self.descriptor.artifact
                    && (request.offset, u64::from(request.length)) == expected_range
                    && ids.insert(request.operation_id)
                    && self.request_digests[i] == canonical_digest(request)?,
                "artifact immutable request identity"
            );
        }
        Ok(())
    }
}

impl MaterialDownload {
    fn validate_installation(
        &self,
        state: &State,
        lease: &aq_admission::MaterialLease,
    ) -> Result<()> {
        ensure!(
            self.installation.is_some() == self.recorded_ns.is_some(),
            "orphan material pin"
        );
        if let Some(record) = &self.installation {
            record.validate()?;
            let receipt = record.receipt()?;
            receipt.validate_against(lease, receipt.received_at_ms)?;
            let installation_window = record.material_synced_ns
                ..record
                    .retrieval_deadline_ns
                    .min(self.preparation.deadline_ns());
            ensure!(
                (record.operation_id, &record.worker, record.kernel_boot_id)
                    == (
                        self.request.operation_id,
                        &state.identity,
                        state.kernel_boot_id
                    )
                    && record.preparation_digest == self.preparation.digest()?
                    && same(&record.delivery.lease, lease)?
                    && record.material_synced_ns >= self.created_ns
                    && self
                        .recorded_ns
                        .is_some_and(|time| installation_window.contains(&time)),
                "material independent installation pin"
            );
        }
        Ok(())
    }
}

fn validate_artifacts(attempt: &Attempt, state: &State, ids: &mut BTreeSet<Uuid>) -> Result<()> {
    for (k, op) in &attempt.downloads {
        op.preparation.validate(
            attempt,
            Creation::new(op.created_revision, op.created_ns),
            state.revision,
        )?;
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
        op.validate_requests(&state.identity, ids)?;
        op.validate_completion()?;
    }
    Ok(())
}

fn validate_materials(attempt: &Attempt, state: &State, ids: &mut BTreeSet<Uuid>) -> Result<()> {
    for (lease_id, op) in &attempt.material_downloads {
        op.preparation.validate(
            attempt,
            Creation::new(op.created_revision, op.created_ns),
            state.revision,
        )?;
        TransportDocument::MaterialFetch(op.request.clone()).validate()?;
        let lease = op
            .preparation
            .materials()
            .iter()
            .find(|l| l.lease_id == *lease_id)
            .context("material lease not original")?;
        ensure!(
            op.request.identity == state.identity
                && op.request.binding == lease.binding
                && op.request.lease_id == *lease_id
                && ids.insert(op.request.operation_id)
                && op.request_digest == canonical_digest(&op.request)?,
            "material immutable request identity"
        );
        op.validate_installation(state, lease)?;
    }
    Ok(())
}

fn artifact_plan(
    attempt: &Attempt,
    state: &State,
    now: u64,
    reference: &ArtifactRef,
) -> Result<ArtifactDownload> {
    let preparation = InputOrigin::capture(attempt)?;
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
            identity: state.identity.clone(),
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
        created_revision: state.revision,
        created_ns: now,
        receipt: None,
        completed_ns: None,
    })
}
impl Journal {
    fn legacy_input_attempt(&self, id: Uuid, message: &'static str) -> Result<&Attempt> {
        self.state
            .attempts
            .get(&id)
            .filter(|attempt| {
                attempt.phase_acceptance.is_none() && attempt.refresh_acceptance.is_none()
            })
            .context(message)
    }

    pub(crate) fn check_preparation(&self, id: Uuid, observed: &Preparation) -> Result<()> {
        let now = self.live()?;
        let saved = self
            .legacy_input_attempt(id, "legacy input on phase attempt")?
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
        self.legacy_input_attempt(id, "legacy artifact on phase attempt")?;
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
        let attempt = self.attempt(id)?;
        let op = artifact_plan(attempt, &self.state, now, reference)?;
        let key = key(reference)?;
        if attempt.downloads.contains_key(&key) {
            return Ok(op);
        }
        let mut state = self.next()?;
        state.attempt_mut(id)?.downloads.insert(key, op.clone());
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
        let a = self.attempt(id)?;
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
            let op = artifact_plan(a, &self.state, now, &d.artifact)?;
            let key = key(&d.artifact)?;
            if !a.downloads.contains_key(&key) {
                next.attempt_mut(id)?.downloads.insert(key, op.clone());
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
        let a = self.attempt(id)?;
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
                let op = next.attempt_mut(id)?.downloads.get_mut(&key).unwrap();
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
        self.legacy_input_attempt(id, "legacy artifact completion on phase attempt")?;
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
        let attempt = self.attempt(id)?;
        let op = attempt
            .downloads
            .get(&key)
            .context("artifact intent not durable")?;
        ensure!(
            now < InputOrigin::capture(attempt)?
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
        let op = state.attempt_mut(id)?.downloads.get_mut(&key).unwrap();
        op.receipt = Some(receipt);
        op.completed_ns = Some(now);
        self.persist(state)
    }
    pub fn begin_material_download(&mut self, id: Uuid, lease: Uuid) -> Result<MaterialDownload> {
        self.legacy_input_attempt(id, "legacy material on phase attempt")?;
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
        let attempt = self.attempt(id)?;
        let preparation = InputOrigin::capture(attempt)?;
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
            .attempt_mut(id)?
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
        let attempt = self.attempt(id)?;
        let op = attempt
            .material_downloads
            .get(&receipt.lease_id)
            .context("material intent not durable")?;
        ensure!(
            now < InputOrigin::capture(attempt)?
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
            .attempt_mut(id)?
            .material_downloads
            .get_mut(&receipt.lease_id)
            .unwrap();
        op.installation = Some(installed.metadata().clone());
        op.recorded_ns = Some(now);
        self.persist(state)
    }
}
