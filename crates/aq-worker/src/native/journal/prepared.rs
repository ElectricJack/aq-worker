//! Immutable outbound Prepare intent; retained metadata is never live input proof.
use super::*;
use aq_admission::{
    MaterialReceipt,
    transport::{ArtifactReceipt, Prepare},
};
#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PreparedIntent {
    pub request: Prepare,
    pub request_digest: Digest,
    pub prepared_at_ns: u64,
    pub preparation: RecordedPreparation,
}
pub(super) fn validate(
    a: &Attempt,
    identity: &WorkerIdentity,
    revision: u64,
    ids: &mut BTreeSet<Uuid>,
) -> Result<()> {
    let Some(p) = &a.prepared else { return Ok(()) };
    let r = &p.request;
    TransportDocument::Prepare(r.clone()).validate()?;
    ensure!(
        canonical_bytes(r)?.len() <= 65_536
            && p.request_digest == canonical_digest(r)?
            && ids.insert(r.operation_id),
        "prepared request digest/identity/bound"
    );
    inputs::validate_capture(
        a,
        &p.preparation,
        r.journal_revision,
        p.prepared_at_ns,
        revision,
    )?;
    let plan = &p.preparation.delivery.original;
    ensure!(
        r.identity == *identity
            && r.binding == a.reservation.offer.offered.offer.binding
            && r.guardian_id == plan.guardian_id
            && r.launch_id == plan.launch_id
            && same(&r.reservation, &plan.reservation)?
            && r.journal_revision
                > a.acceptance
                    .as_ref()
                    .context("prepare acceptance absent")?
                    .journal_revision,
        "prepared owner association"
    );
    let source = a.source.as_ref().context("prepare source absent")?;
    ensure!(
        source.verified.is_some()
            && source.complete()
            && p.prepared_at_ns < source.preparation.deadline_ns(),
        "prepared source history absent/expired"
    );
    let expected = receipts(a)?;
    ensure!(
        same(&r.artifacts, &expected)?,
        "prepared artifact set changed"
    );
    ensure!(
        r.materials.len() == plan.materials.len(),
        "prepared material count"
    );
    for (receipt, lease) in r.materials.iter().zip(&plan.materials) {
        receipt.validate_against(lease, receipt.received_at_ms)?;
        let retained = a
            .material_downloads
            .get(&lease.lease_id)
            .context("prepared material intent absent")?
            .receipt()?
            .context("prepared material pin absent")?;
        ensure!(same(receipt, &retained)?, "prepared material pin changed");
    }
    Ok(())
}
pub(super) fn receipts(a: &Attempt) -> Result<Vec<ArtifactReceipt>> {
    let mut receipts = Vec::new();
    for descriptor in &a.reservation.offer.offered.artifacts {
        let d = a
            .downloads
            .values()
            .find(|d| d.descriptor.artifact == descriptor.artifact)
            .context("prepare download absent")?;
        ensure!(
            same(&d.descriptor, descriptor)?,
            "prepare original descriptor changed"
        );
        receipts.push(
            d.receipt
                .clone()
                .context("prepare verified receipt absent")?,
        );
    }
    let m = a
        .source
        .as_ref()
        .and_then(|s| s.manifest.as_ref())
        .context("prepare transfer absent")?;
    for (name, hash, bytes) in [
        ("manifest", m.manifest_digest.as_str(), m.manifest_bytes),
        (
            "bundle",
            m.bundle_digest
                .as_deref()
                .context("prepare bundle digest absent")?,
            m.bundle_bytes,
        ),
    ] {
        let digest: Digest = serde_json::from_value(serde_json::json!(hash))?;
        receipts.push(ArtifactReceipt {
            artifact: ArtifactRef {
                id: aq_session_protocol::Text::new(format!("aq.source-{name}/{}", m.transfer_id))?,
                version: aq_session_protocol::Text::new("1")?,
                digest: digest.clone(),
            },
            bytes,
            digest,
        });
    }
    Ok(receipts)
}
impl Journal {
    /// Called only after the input owner checks actual live bytes and local owners.
    pub(crate) fn record_prepare(
        &mut self,
        id: Uuid,
        observed: &Preparation,
        materials: Vec<MaterialReceipt>,
    ) -> Result<Prepare> {
        self.check_preparation(id, observed)?;
        let a = self
            .state
            .attempts
            .get(&id)
            .context("prepare attempt absent")?;
        let artifacts = receipts(a)?;
        if let Some(p) = &a.prepared {
            ensure!(
                same(&p.request.artifacts, &artifacts)? && same(&p.request.materials, &materials)?,
                "changed Prepare retry"
            );
            return Ok(p.request.clone());
        }
        let plan = observed.plan();
        let request = Prepare {
            schema_version: 1,
            operation_id: Uuid::new_v4(),
            identity: self.state.identity.clone(),
            binding: plan.binding.clone(),
            reservation: plan.reservation.clone(),
            guardian_id: plan.guardian_id,
            launch_id: plan.launch_id,
            journal_revision: self.state.revision,
            journal_digest: canonical_digest(&self.state)?,
            artifacts,
            materials,
        };
        let intent = PreparedIntent {
            request_digest: canonical_digest(&request)?,
            request: request.clone(),
            prepared_at_ns: self.live()?,
            preparation: a
                .preparation
                .clone()
                .context("prepare observation absent")?,
        };
        let mut next = self.next()?;
        next.attempts.get_mut(&id).unwrap().prepared = Some(intent);
        self.persist(next)?;
        self.check_preparation(id, observed)?;
        Ok(request)
    }
}
