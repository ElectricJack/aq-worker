//! Initial-only qualified physical facts; neither inventory nor receipt is authority.
use super::*;
mod inventory;
mod observation;
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
    fn inventory_attempts(
        &self,
        journal: &Journal,
    ) -> Result<(WorkerIdentity, BTreeMap<Uuid, AttemptOps>)> {
        journal.available()?;
        let state = journal.state();
        ensure!(
            state
                .attempts
                .values()
                .all(|a| a.reservation.runtime.runtime == self.body.runtime
                    && a.reservation.runtime.qualification == self.body.runtime_qualification),
            "presence runtime qualification mismatch"
        );
        let attempts = state
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
            .collect();
        Ok((state.identity.clone(), attempts))
    }
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
            (scope.deadline, scope.hard),
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
            .storage(move |owner| qualified.inventory_attempts(&owner.journal))
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
        self.pipeline.storage(move |owner| {
            let present = matches!(&*lock(&op.stage)?, Stage::Ready(report) if report.presence.is_present());
            scope(owner, &op.scope, present)?;
            Ok(())
        }).await?;
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
