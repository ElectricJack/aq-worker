//! Initial-only qualified physical facts; neither inventory nor receipt is authority.
use super::*;
use aq_admission::{scoped_presence as wire, transport::WorkerIdentity};
use aq_session_protocol::{ArtifactRef, Validate};
use serde::Serialize;
use std::sync::atomic::{AtomicU64, Ordering};
fn canonical(value: &impl Serialize) -> Result<Vec<u8>> {
    let mut value = serde_json::to_value(value)?;
    value.sort_all_objects();
    Ok(serde_json::to_vec(&value)?)
}
fn digest(value: &impl Serialize) -> Result<Digest> {
    Ok(Digest::of(&canonical(value)?))
}
fn same(a: &impl Serialize, b: &impl Serialize) -> Result<bool> {
    Ok(canonical(a)? == canonical(b)?)
}
fn lock<T>(value: &Mutex<T>) -> Result<std::sync::MutexGuard<'_, T>> {
    value
        .lock()
        .map_err(|_| anyhow::anyhow!("scoped producer state poisoned"))
}
pub struct QualifiedPresenceProfile {
    reference: ArtifactRef,
    body: wire::Qualification,
}
impl QualifiedPresenceProfile {
    /// Trusted installed qualification input, not an inbound offer/capability.
    pub fn from_definition(
        reference: ArtifactRef,
        bytes: &[u8],
        adapter: &ArtifactRef,
        evidence: &ArtifactRef,
    ) -> Result<Self> {
        ensure!(
            bytes.len() <= 8192 && Digest::of(bytes) == reference.digest,
            "presence qualification definition pin"
        );
        let body: wire::Qualification =
            serde_json::from_slice(bytes).context("invalid presence qualification definition")?;
        body.validate()?;
        ensure!(
            body.adapter == *adapter
                && body.adapter_qualification == *evidence
                && reference
                    .id
                    .as_str()
                    .strip_prefix(wire::QUALIFICATION_PREFIX)
                    .is_some_and(|s| !s.is_empty()),
            "presence qualification identity"
        );
        reference.id.validate()?;
        reference.version.validate()?;
        Ok(Self { reference, body })
    }
}
#[derive(Default)]
struct Tx {
    first_sent_ns: Option<u64>,
    in_flight: bool,
}
pub(crate) struct PresenceSendGuard {
    tx: Arc<Mutex<Tx>>,
}
impl Drop for PresenceSendGuard {
    fn drop(&mut self) {
        if let Ok(mut tx) = self.tx.lock() {
            tx.in_flight = false;
        }
    }
}
fn begin(tx: &Arc<Mutex<Tx>>, now: u64) -> Result<(u64, PresenceSendGuard)> {
    let mut state = lock(tx)?;
    ensure!(!state.in_flight, "scoped publication already in flight");
    state.in_flight = true;
    let sent = *state.first_sent_ns.get_or_insert(now);
    Ok((sent, PresenceSendGuard { tx: tx.clone() }))
}
struct InventoryInner {
    origin: Arc<()>,
    request: wire::InventoryV2,
    tx: Arc<Mutex<Tx>>,
    receipt: Mutex<Option<wire::InventoryReceipt>>,
}
pub struct InventoryOperation {
    inner: Arc<InventoryInner>,
}
#[derive(Clone)]
pub struct AcknowledgedInventory {
    origin: Arc<()>,
    receipt: wire::InventoryReceipt,
    deadline_ns: u64,
}
impl AcknowledgedInventory {
    pub fn receipt(&self) -> &wire::InventoryReceipt {
        &self.receipt
    }
    pub fn deadline_ns(&self) -> u64 {
        self.deadline_ns
    }
}
impl InventoryOperation {
    pub fn request(&self) -> &wire::InventoryV2 {
        &self.inner.request
    }
    pub(crate) fn begin_send(&self, now: u64) -> Result<(wire::InventoryV2, PresenceSendGuard)> {
        let (_, guard) = begin(&self.inner.tx, now)?;
        Ok((self.inner.request.clone(), guard))
    }
    pub(crate) fn finish(
        &self,
        receipt: wire::InventoryReceipt,
        received: u64,
    ) -> Result<AcknowledgedInventory> {
        receipt.validate_for(&self.inner.request)?;
        let first = lock(&self.inner.tx)?
            .first_sent_ns
            .context("inventory was not sent")?;
        let deadline = first
            .checked_add(
                (receipt.expires_at_ms - receipt.recorded_at_ms)
                    .checked_mul(1_000_000)
                    .context("inventory clock overflow")?,
            )
            .context("inventory clock overflow")?;
        ensure!(received >= first, "inventory receive clock regression");
        let mut saved = lock(&self.inner.receipt)?;
        if let Some(old) = saved.as_ref() {
            ensure!(same(old, &receipt)?, "inventory replay receipt changed");
        } else {
            *saved = Some(receipt.clone());
        }
        Ok(AcknowledgedInventory {
            origin: self.inner.origin.clone(),
            receipt,
            deadline_ns: deadline,
        })
    }
    fn acknowledged(&self) -> Result<bool> {
        Ok(lock(&self.inner.receipt)?.is_some())
    }
}
#[derive(Clone)]
struct Scope {
    id: Uuid,
    record: Digest,
    deadline: u64,
    hard: u64,
}
struct ReportInner {
    origin: Arc<()>,
    scope: Scope,
    inventory: AcknowledgedInventory,
    operation: Uuid,
    sequence: u64,
    ttl: u64,
    profile: Arc<QualifiedPresenceProfile>,
    stage: Mutex<Stage>,
    tx: Arc<Mutex<Tx>>,
    receipt: Mutex<Option<wire::PresenceReceipt>>,
    issued_at_ms: AtomicU64,
    hard_ms: AtomicU64,
}
enum Stage {
    Allocated,
    Reading,
    Ready(Box<wire::PresenceReport>),
    Failed,
}
pub struct PresenceOperation {
    inner: Arc<ReportInner>,
}
impl Clone for PresenceOperation {
    fn clone(&self) -> Self {
        Self {
            inner: self.inner.clone(),
        }
    }
}
impl PresenceOperation {
    pub fn operation_id(&self) -> Uuid {
        self.inner.operation
    }
    pub fn sequence(&self) -> u64 {
        self.inner.sequence
    }
    /// Inspection only; a decoded copy cannot construct this producer operation.
    pub fn report(&self) -> Result<Option<wire::PresenceReport>> {
        if lock(&self.inner.tx)?.first_sent_ns.is_none() {
            return Ok(None);
        }
        Ok(match &*lock(&self.inner.stage)? {
            Stage::Ready(report) => Some((**report).clone()),
            _ => None,
        })
    }
    fn acknowledged(&self) -> Result<bool> {
        Ok(lock(&self.inner.receipt)?.is_some())
    }
    fn definitely_unsubmitted_failure(&self) -> Result<bool> {
        Ok(matches!(*lock(&self.inner.stage)?, Stage::Failed)
            && lock(&self.inner.tx)?.first_sent_ns.is_none())
    }
    pub(crate) fn begin_send(&self, now: u64) -> Result<(wire::PresenceReport, PresenceSendGuard)> {
        let mut stage = lock(&self.inner.stage)?;
        let Stage::Ready(report) = &mut *stage else {
            anyhow::bail!("presence operation has no completed read")
        };
        ensure!(
            now >= report.observation.read_completed_ns
                && now - report.observation.read_started_ns <= wire::MAX_READ_MS * 1_000_000
                && now < self.inner.inventory.deadline_ns,
            "presence observation/inventory expired"
        );
        if report.presence.is_present() {
            ensure!(
                now < self.inner.scope.deadline.min(self.inner.scope.hard),
                "positive presence permit expired"
            );
        }
        let (sent, guard) = begin(&self.inner.tx, now)?;
        report.observation.sent_ns = sent;
        report.validate()?;
        Ok(((**report).clone(), guard))
    }
    pub(crate) fn finish(&self, receipt: wire::PresenceReceipt) -> Result<wire::PresenceReceipt> {
        let stage = lock(&self.inner.stage)?;
        let Stage::Ready(report) = &*stage else {
            anyhow::bail!("presence report absent")
        };
        receipt.validate_for(report)?;
        if report.presence.is_present() {
            let maximum = wire::positive_expiry(
                self.inner.issued_at_ms.load(Ordering::SeqCst),
                receipt.recorded_at_ms,
                report.ttl_ms,
                self.inner.profile.body.maximum_age_ms,
                self.inner.hard_ms.load(Ordering::SeqCst),
            )?;
            ensure!(
                receipt.expires_at_ms <= maximum,
                "presence receipt exceeds qualified age/hard bound"
            );
        }
        let mut saved = lock(&self.inner.receipt)?;
        if let Some(old) = saved.as_ref() {
            ensure!(same(old, &receipt)?, "presence replay receipt changed");
        } else {
            *saved = Some(receipt.clone());
        }
        Ok(receipt)
    }
}
struct AttemptOps {
    sequence: u64,
    current: Option<PresenceOperation>,
}
struct ProducerState {
    inventory_sequence: u64,
    inventory: Option<InventoryOperation>,
    attempts: BTreeMap<Uuid, AttemptOps>,
}
fn current_inventory(state: &ProducerState, inventory: &AcknowledgedInventory) -> Result<()> {
    let current = state
        .inventory
        .as_ref()
        .context("current inventory operation absent")?;
    let saved = lock(&current.inner.receipt)?;
    let saved = saved
        .as_ref()
        .context("current inventory acknowledgement unresolved")?;
    ensure!(
        same(&saved.inventory, &inventory.receipt.inventory)?,
        "inventory acknowledgement superseded"
    );
    Ok(())
}
pub struct ScopedPresenceProducer {
    pipeline: Arc<InputPipeline>,
    profile: Arc<QualifiedPresenceProfile>,
    harnesses: Vec<ArtifactRef>,
    identity: WorkerIdentity,
    origin: Arc<()>,
    state: Mutex<ProducerState>,
}
fn scope(owner: &Owned, scope: &Scope, live: bool) -> Result<u64> {
    if live {
        return owner.journal.check_initial_grant(
            scope.id,
            &scope.record,
            scope.deadline,
            scope.hard,
        );
    }
    let _ = owner.journal.available()?;
    let record = owner
        .journal
        .state()
        .attempts
        .get(&scope.id)
        .and_then(|a| a.initial_grant.as_ref())
        .context("presence initial owner absent")?;
    ensure!(
        record.digest()? == scope.record
            && scope.deadline <= record.deadline_ns
            && scope.hard <= record.hard_deadline_ns,
        "presence owner changed"
    );
    boot_ns()
}
struct ProducerClaim<'a> {
    flag: &'a std::sync::atomic::AtomicBool,
    keep: bool,
}
impl Drop for ProducerClaim<'_> {
    fn drop(&mut self) {
        if !self.keep {
            self.flag.store(false, Ordering::SeqCst);
        }
    }
}
impl InputPipeline {
    pub async fn scoped_presence(
        self: &Arc<Self>,
        profile: QualifiedPresenceProfile,
        harnesses: Vec<ArtifactRef>,
        last_inventory_sequence: u64,
    ) -> Result<ScopedPresenceProducer> {
        ensure!(
            self.scoped_presence_owned
                .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
                .is_ok(),
            "scoped producer already owned for this pipeline"
        );
        let mut claim = ProducerClaim {
            flag: &self.scoped_presence_owned,
            keep: false,
        };
        ensure!(
            last_inventory_sequence < i64::MAX as u64,
            "inventory sequence overflow"
        );
        let profile = Arc::new(profile);
        let qualified = profile.clone();
        let (identity, attempts) = self
            .storage(move |owner| {
                let _ = owner.journal.available()?;
                let state = owner.journal.state();
                ensure!(
                    state
                        .attempts
                        .values()
                        .all(|a| a.reservation.runtime.runtime == qualified.body.runtime
                            && a.reservation.runtime.qualification
                                == qualified.body.runtime_qualification),
                    "presence runtime qualification mismatch"
                );
                Ok((
                    state.identity.clone(),
                    state
                        .attempts
                        .keys()
                        .map(|id| {
                            (
                                *id,
                                AttemptOps {
                                    sequence: 0,
                                    current: None,
                                },
                            )
                        })
                        .collect(),
                ))
            })
            .await?;
        let producer = ScopedPresenceProducer {
            pipeline: self.clone(),
            profile,
            harnesses,
            identity,
            origin: Arc::new(()),
            state: Mutex::new(ProducerState {
                inventory_sequence: last_inventory_sequence,
                inventory: None,
                attempts,
            }),
        };
        // Validate trusted inventory inputs without allocating an operation.
        producer
            .inventory_request(Uuid::new_v4(), last_inventory_sequence + 1, 1000)?
            .validate()?;
        claim.keep = true;
        Ok(producer)
    }
}
impl ScopedPresenceProducer {
    fn inventory_request(&self, id: Uuid, sequence: u64, ttl: u64) -> Result<wire::InventoryV2> {
        ensure!(
            (1000..=self.profile.body.maximum_ttl_ms).contains(&ttl),
            "inventory TTL exceeds qualification"
        );
        Ok(wire::InventoryV2 {
            schema_version: 1,
            operation_id: id,
            identity: self.identity.clone(),
            runtime: self.profile.body.runtime.clone(),
            runtime_qualification: self.profile.body.runtime_qualification.clone(),
            negotiation: wire::Negotiation {
                protocol: wire::Protocol::ScopedMaterialPresenceV1,
                qualification: self.profile.reference.clone(),
            },
            sequence,
            ttl_ms: ttl,
            qualifications: self.harnesses.clone(),
            provisionable_only: vec![],
        })
    }
    pub fn begin_inventory(&self, ttl_ms: u64) -> Result<InventoryOperation> {
        let mut state = lock(&self.state)?;
        if let Some(old) = &state.inventory {
            ensure!(
                old.acknowledged()?,
                "unresolved inventory operation retained"
            );
        }
        let sequence = state
            .inventory_sequence
            .checked_add(1)
            .filter(|s| *s <= i64::MAX as u64)
            .context("inventory highwater exhausted")?;
        let request = self.inventory_request(Uuid::new_v4(), sequence, ttl_ms)?;
        request.validate()?;
        let operation = InventoryOperation {
            inner: Arc::new(InventoryInner {
                origin: self.origin.clone(),
                request,
                tx: Arc::new(Mutex::new(Tx::default())),
                receipt: Mutex::new(None),
            }),
        };
        state.inventory_sequence = sequence;
        state.inventory = Some(InventoryOperation {
            inner: operation.inner.clone(),
        });
        Ok(operation)
    }
    pub async fn publish_inventory(
        &self,
        operation: &InventoryOperation,
    ) -> Result<AcknowledgedInventory> {
        ensure!(
            Arc::ptr_eq(&self.origin, &operation.inner.origin),
            "inventory belongs to another producer"
        );
        {
            let state = lock(&self.state)?;
            ensure!(
                state
                    .inventory
                    .as_ref()
                    .is_some_and(|o| Arc::ptr_eq(&o.inner, &operation.inner)),
                "inventory operation superseded"
            );
        }
        self.pipeline
            .storage(|owner| {
                let _ = owner.journal.available()?;
                Ok(())
            })
            .await?;
        self.pipeline.client.scoped_inventory(operation).await
    }
    pub fn begin_presence(
        &self,
        grant: &DurableInitialGrant,
        inventory: &AcknowledgedInventory,
        ttl_ms: u64,
    ) -> Result<PresenceOperation> {
        ensure!(
            Arc::ptr_eq(&self.pipeline.owned, &grant.history.owner)
                && Arc::ptr_eq(&self.pipeline.retained, &grant.history.resources)
                && Arc::ptr_eq(&self.origin, &inventory.origin),
            "presence owner/inventory mismatch"
        );
        ensure!(
            (1000..=self.profile.body.maximum_ttl_ms).contains(&ttl_ms),
            "presence TTL exceeds qualification"
        );
        let mut state = lock(&self.state)?;
        current_inventory(&state, inventory)?;
        let attempt = state
            .attempts
            .get_mut(&grant.history.attempt)
            .context("presence attempt not admitted")?;
        if let Some(old) = &attempt.current {
            ensure!(
                old.acknowledged()? || old.definitely_unsubmitted_failure()?,
                "unresolved presence operation retained"
            );
        }
        let sequence = attempt
            .sequence
            .checked_add(1)
            .filter(|s| *s <= i64::MAX as u64)
            .context("presence highwater exhausted")?;
        let operation = PresenceOperation {
            inner: Arc::new(ReportInner {
                origin: self.origin.clone(),
                scope: Scope {
                    id: grant.history.attempt,
                    record: grant.history.record_digest.clone(),
                    deadline: grant.deadline_ns,
                    hard: grant.hard_deadline_ns,
                },
                inventory: inventory.clone(),
                operation: Uuid::new_v4(),
                sequence,
                ttl: ttl_ms,
                profile: self.profile.clone(),
                stage: Mutex::new(Stage::Allocated),
                tx: Arc::new(Mutex::new(Tx::default())),
                receipt: Mutex::new(None),
                issued_at_ms: AtomicU64::new(0),
                hard_ms: AtomicU64::new(0),
            }),
        };
        attempt.sequence = sequence;
        attempt.current = Some(operation.clone());
        Ok(operation)
    }
    pub fn current_presence(&self, attempt: Uuid) -> Result<Option<PresenceOperation>> {
        Ok(lock(&self.state)?
            .attempts
            .get(&attempt)
            .and_then(|a| a.current.clone()))
    }
    fn current(&self, op: &PresenceOperation) -> Result<()> {
        ensure!(
            Arc::ptr_eq(&self.origin, &op.inner.origin),
            "presence belongs to another producer"
        );
        ensure!(
            lock(&self.state)?
                .attempts
                .get(&op.inner.scope.id)
                .and_then(|a| a.current.as_ref())
                .is_some_and(|p| Arc::ptr_eq(&p.inner, &op.inner)),
            "presence operation superseded"
        );
        Ok(())
    }
    pub async fn observe(&self, operation: &PresenceOperation) -> Result<()> {
        self.current(operation)?;
        {
            let mut stage = lock(&operation.inner.stage)?;
            match &*stage {
                Stage::Ready(_) => return Ok(()),
                Stage::Allocated => *stage = Stage::Reading,
                _ => anyhow::bail!("presence read already running or failed"),
            }
        }
        let op = operation.inner.clone();
        let result = self
            .pipeline
            .storage(move |owner| {
                let result = (|| {
                    let acquired = scope(owner, &op.scope, true)?;
                    ensure!(
                        acquired < op.inventory.deadline_ns,
                        "presence inventory expired"
                    );
                    let a = owner
                        .journal
                        .state()
                        .attempts
                        .get(&op.scope.id)
                        .context("presence attempt absent")?;
                    let original = a
                        .initial_grant
                        .as_ref()
                        .context("presence initial receipt absent")?;
                    let grant = &original.delivery.original;
                    ensure!(
                        grant.materials.len() == 1
                            && a.prepared
                                .as_ref()
                                .is_some_and(|p| p.request.materials.len() == 1),
                        "presence supports one original material only"
                    );
                    ensure!(
                        original.runtime.runtime == op.profile.body.runtime
                            && original.runtime.qualification
                                == op.profile.body.runtime_qualification,
                        "presence runtime mismatch"
                    );
                    let lease = grant.materials[0].clone();
                    let receipt = a.prepared.as_ref().unwrap().request.materials[0].clone();
                    receipt.validate_against(&lease, receipt.received_at_ms)?;
                    op.issued_at_ms
                        .store(grant.clock.server_time_ms, Ordering::SeqCst);
                    op.hard_ms
                        .store(original.delivery.execution_deadline_ms, Ordering::SeqCst);
                    let (identity, kernel) = (
                        owner.journal.state().identity.clone(),
                        owner.journal.state().kernel_boot_id,
                    );
                    let (binding, grant_id, guardian_id, launch_id, reservation_id) = (
                        grant.grant.binding.clone(),
                        grant.grant.grant_id,
                        grant.guardian_id,
                        grant.launch_id,
                        grant.reservation_id,
                    );
                    let authority = wire::AuthorityAnchor {
                        kind: wire::AuthorityKind::InitialGrant,
                        operation_id: original.delivery.operation_id,
                        grant_revision: original.delivery.versions.grant_revision,
                        expires_at_ms: grant.grant.expires_at_ms,
                    };
                    let started = boot_ns()?;
                    ensure!(
                        started < op.scope.deadline.min(op.scope.hard),
                        "presence permit expired before read"
                    );
                    let presence = owner.materials.observe_scoped(&receipt);
                    let completed = boot_ns()?;
                    scope(owner, &op.scope, presence.is_present())?;
                    ensure!(
                        completed >= started
                            && completed - started <= wire::MAX_READ_MS * 1_000_000,
                        "presence read interval exceeded"
                    );
                    let report = wire::PresenceReport {
                        schema_version: 1,
                        operation_id: op.operation,
                        identity,
                        runtime: op.profile.body.runtime.clone(),
                        runtime_qualification: op.profile.body.runtime_qualification.clone(),
                        negotiation: wire::Negotiation {
                            protocol: wire::Protocol::ScopedMaterialPresenceV1,
                            qualification: op.profile.reference.clone(),
                        },
                        binding,
                        grant_id,
                        guardian_id,
                        launch_id,
                        reservation_id,
                        lease_id: lease.lease_id,
                        installation_receipt_digest: digest(&receipt)?,
                        private_binding: receipt.private_binding.clone(),
                        inventory: op.inventory.receipt.inventory.clone(),
                        authority,
                        sequence: op.sequence,
                        ttl_ms: op.ttl,
                        observation: wire::Observation {
                            kernel_boot_id: kernel,
                            clock_source: wire::ClockSource::Boottime,
                            permit_acquired_ns: acquired,
                            permit_deadline_ns: op.scope.deadline.min(op.scope.hard),
                            read_started_ns: started,
                            read_completed_ns: completed,
                            sent_ns: completed,
                        },
                        presence,
                    };
                    report.validate_against(&lease, &receipt)?;
                    *lock(&op.stage)? = Stage::Ready(Box::new(report));
                    Ok(())
                })();
                if result.is_err() {
                    *lock(&op.stage)? = Stage::Failed;
                }
                result
            })
            .await;
        if result.is_err() {
            *lock(&operation.inner.stage)? = Stage::Failed;
        }
        result
    }
    pub async fn publish_presence(
        &self,
        operation: &PresenceOperation,
    ) -> Result<wire::PresenceReceipt> {
        self.current(operation)?;
        if lock(&operation.inner.tx)?.first_sent_ns.is_none()
            && let Err(error) = current_inventory(&*lock(&self.state)?, &operation.inner.inventory)
        {
            *lock(&operation.inner.stage)? = Stage::Failed;
            return Err(error);
        }
        let op = operation.inner.clone();
        self.pipeline.storage(move|owner|{let present=matches!(&*lock(&op.stage)?,Stage::Ready(report) if report.presence.is_present());scope(owner,&op.scope,present)?;Ok(())}).await?;
        self.pipeline.client.scoped_presence(operation).await
    }
}

#[cfg(test)]
impl ScopedPresenceProducer {
    pub(crate) async fn test_pause_read(
        &self,
        pause: &crate::native::journal::WritePause,
    ) -> Result<()> {
        let gate = pause.clone_gate();
        self.pipeline
            .storage(move |owner| {
                owner
                    .materials
                    .test_observation_hook(Box::new(move || gate.wait().expect("test read gate")));
                Ok(())
            })
            .await
    }
}
