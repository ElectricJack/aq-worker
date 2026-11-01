//! Native no-launch reservations and exact acceptance recovery. No grants or
//! process permits are reconstructed from disk. Run fsync on a bounded lane.
mod granted;
#[cfg(any(test, feature = "test-support"))]
mod pause;
#[cfg(any(test, feature = "test-support"))]
pub use pause::WritePause;
mod input_origin;
mod inputs;
pub use input_origin::InputOrigin;
mod phase_prepared;
pub mod preparation_v2;
mod prepared;
pub use phase_prepared::PhasePreparedIntent;
mod source;
mod storage;
use super::http::{boot_ns, control::Preparation};
use anyhow::{Context, Result, ensure};
use aq_admission::{
    control::PreparationDelivery,
    startup::NativeOffer,
    transport::{Accept, NoLaunchReservation, TransportDocument, WorkerIdentity},
};
use aq_session_protocol::{
    ArtifactRef, Capability, Digest, ProfiledAcceptance, SessionIdentity, Validate,
};
pub use granted::ReceivedInitialGrant;
pub use inputs::{ArtifactDownload, MaterialDownload};
pub use prepared::PreparedIntent;
use serde::{Deserialize, Serialize};
pub use source::{SourceDownload, SourceRange};
use std::{
    collections::{BTreeMap, BTreeSet},
    path::Path,
};
use storage::Storage;
use uuid::Uuid;

pub const FORMAT: u32 = 7;
pub const MAX_BYTES: usize = 8 * 1024 * 1024;
const MAX_ATTEMPTS: usize = 64;
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Limits {
    pub slots: u32,
    pub memory_bytes: u64,
    pub cpu_millis: u64,
    pub disk_bytes: u64,
}
impl Limits {
    fn validate(&self) -> Result<()> {
        ensure!(
            (1..=64).contains(&self.slots)
                && self.memory_bytes > 0
                && self.cpu_millis > 0
                && self.disk_bytes > 0,
            "invalid native capacity"
        );
        Ok(())
    }
}
/// Both original installed runtime identity and its operator qualification must
/// survive configuration changes; neither is inferred from the offered harness.
#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RuntimeIdentity {
    pub runtime: ArtifactRef,
    pub qualification: ArtifactRef,
}
#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Reservation {
    pub offer: NativeOffer,
    pub runtime: RuntimeIdentity,
    pub reserved_at_ns: u64,
    pub deadline_ns: u64,
    pub disk_bytes: u64,
    pub reservation_id: Uuid,
    pub reservation_operation_id: Uuid,
    pub accept_operation_id: Uuid,
    pub guardian_id: Uuid,
    pub launch_id: Uuid,
}
#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RecordedPreparation {
    pub delivery: PreparationDelivery,
    pub sent_ns: u64,
    pub received_ns: u64,
    pub deadline_ns: u64,
}
#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Attempt {
    pub reservation: Reservation,
    pub acceptance: Option<Accept>,
    pub acceptance_digest: Option<Digest>,
    pub preparation: Option<RecordedPreparation>,
    pub downloads: BTreeMap<String, ArtifactDownload>,
    pub material_downloads: BTreeMap<Uuid, MaterialDownload>,
    pub source: Option<SourceDownload>,
    pub prepared: Option<PreparedIntent>,
    pub initial_grant: Option<ReceivedInitialGrant>,
    pub phase_acceptance: Option<preparation_v2::PhaseAcceptance>,
    pub phase_preparation: Option<preparation_v2::ReceivedPreparation>,
    pub phase_prepared: Option<PhasePreparedIntent>,
}
#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct State {
    pub format: u32,
    pub identity: WorkerIdentity,
    pub kernel_boot_id: Uuid,
    pub revision: u64,
    pub attempts: BTreeMap<Uuid, Attempt>,
}
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Snapshot {
    state: State,
    digest: Digest,
}
#[cfg(any(test, feature = "test-support"))]
#[derive(Clone, Copy)]
pub enum WriteFault {
    BeforeWrite,
    AfterFileSync,
    AfterRename,
    AfterDirectorySync,
}

pub struct Journal {
    storage: Storage,
    state: State,
    limits: Limits,
    invocation: Uuid,
    kernel_boot_id: Uuid,
    poisoned: bool,
    #[cfg(any(test, feature = "test-support"))]
    fault: Option<WriteFault>,
    #[cfg(any(test, feature = "test-support"))]
    pause: Option<std::sync::Arc<pause::Gate>>,
}
fn canonical_bytes(value: &impl Serialize) -> Result<Vec<u8>> {
    let mut value = serde_json::to_value(value)?;
    value.sort_all_objects();
    Ok(serde_json::to_vec(&value)?)
}
pub(crate) fn canonical_digest(value: &impl Serialize) -> Result<Digest> {
    Ok(Digest::of(&canonical_bytes(value)?))
}
fn kernel_boot_id() -> Result<Uuid> {
    Ok(Uuid::parse_str(
        std::fs::read_to_string("/proc/sys/kernel/random/boot_id")?.trim(),
    )?)
}
fn same(a: &impl Serialize, b: &impl Serialize) -> Result<bool> {
    Ok(canonical_bytes(a)? == canonical_bytes(b)?)
}
impl State {
    fn validate(&self, limits: &Limits) -> Result<()> {
        limits.validate()?;
        self.identity.validate()?;
        ensure!(
            self.format == FORMAT
                && self.revision > 0
                && !self.kernel_boot_id.is_nil()
                && self.attempts.len() <= MAX_ATTEMPTS,
            "native journal version/shape"
        );
        let mut used = (0u32, 0u64, 0u64, 0u64);
        let mut identities = BTreeSet::new();
        let mut download_records = 0usize;
        let mut download_requests = 0usize;
        for (id, attempt) in &self.attempts {
            let r = &attempt.reservation;
            r.offer.validate()?;
            ensure!(
                r.offer.offered.artifacts.len() <= 126
                    && r.offer
                        .offered
                        .artifacts
                        .iter()
                        .map(|a| a.artifact.digest.as_str())
                        .collect::<BTreeSet<_>>()
                        .len()
                        == r.offer.offered.artifacts.len(),
                "native inline receipt set cannot satisfy Prepare"
            );
            for reference in [&r.runtime.runtime, &r.runtime.qualification] {
                reference.id.validate()?;
                reference.version.validate()?;
            }
            let binding = &r.offer.offered.offer.binding;
            ensure!(
                *id == binding.attempt_id
                    && matches!(binding.session, SessionIdentity::Remote { node_id, boot_id, .. } if node_id == self.identity.node_id && boot_id == self.identity.boot_id),
                "native reservation owner"
            );
            ensure!(
                r.reserved_at_ns < r.deadline_ns
                    && r.deadline_ns - r.reserved_at_ns
                        <= r.offer
                            .offered
                            .reservation_ttl_ms
                            .checked_mul(1_000_000)
                            .context("reservation duration overflow")?
                    && r.disk_bytes > 0,
                "reservation deadline/disk"
            );
            ensure!(
                [
                    r.reservation_id,
                    r.reservation_operation_id,
                    r.accept_operation_id,
                    r.guardian_id,
                    r.launch_id
                ]
                .iter()
                .all(|id| !id.is_nil() && identities.insert(*id)),
                "reservation identifiers"
            );
            let resource = &r.offer.offered.offer.manifest.resources;
            used.0 = used.0.checked_add(1).context("slot overflow")?;
            used.1 = used
                .1
                .checked_add(resource.memory_bytes)
                .context("memory overflow")?;
            used.2 = used
                .2
                .checked_add(resource.cpu_millis)
                .context("cpu overflow")?;
            used.3 = used.3.checked_add(r.disk_bytes).context("disk overflow")?;
            if let Some(request) = &attempt.acceptance {
                ensure!(
                    attempt.acceptance_digest.as_ref() == Some(&canonical_digest(request)?),
                    "acceptance request digest"
                );
                TransportDocument::Accept(request.clone()).validate()?;
                request
                    .acceptance
                    .validate_against(&r.offer.offered.offer)?;
                ensure!(
                    request.identity == self.identity
                        && request.operation_id == r.accept_operation_id
                        && request.guardian_id == r.guardian_id
                        && request.launch_id == r.launch_id
                        && request.reservation.reservation_id == r.reservation_id
                        && request.reservation.reservation_operation_id
                            == r.reservation_operation_id
                        && request.reservation.reservation_journal_digest == canonical_digest(r)?
                        && request.journal_revision < self.revision,
                    "acceptance journal association"
                );
                ensure!(
                    canonical_bytes(request)?.len() <= 65_536,
                    "acceptance bytes bound"
                );
            }
            ensure!(
                attempt.acceptance.is_some() == attempt.acceptance_digest.is_some(),
                "orphan acceptance digest"
            );
            // Validate the one saved phase owner before checking any physical
            // input copies. InputOrigin equality relies on this shared check;
            // repeated proposal validation per input would be quadratic.
            preparation_v2::validate(attempt, &self.identity, self.revision)?;
            if let Some(record) = &attempt.preparation {
                let request = attempt
                    .acceptance
                    .as_ref()
                    .context("preparation without acceptance")?;
                record
                    .delivery
                    .validate_for(request, record.delivery.delivery.nonce)?;
                let maximum = record.delivery.delivery.client_deadline_ns(
                    record.sent_ns,
                    record.received_ns,
                    Some(r.deadline_ns),
                )?;
                ensure!(
                    record.received_ns >= record.sent_ns
                        && record.received_ns < record.deadline_ns
                        && record.deadline_ns <= maximum,
                    "preparation delivery clocks"
                );
            }
            source::validate(attempt, self.revision, &mut identities)?;
            prepared::validate(attempt, &self.identity, self.revision, &mut identities)?;
            phase_prepared::validate(attempt, &self.identity, self.revision, &mut identities)?;
            granted::validate(attempt, self.revision)?;
            ensure!(
                attempt
                    .initial_grant
                    .as_ref()
                    .is_none_or(|g| g.kernel_boot_id == self.kernel_boot_id),
                "received grant kernel clock namespace"
            );
            inputs::validate(
                attempt,
                &self.identity,
                self.kernel_boot_id,
                self.revision,
                &mut identities,
            )?;
            download_records += attempt.downloads.len() + attempt.material_downloads.len();
            download_requests += attempt
                .downloads
                .values()
                .map(|p| p.requests.len())
                .sum::<usize>()
                + attempt.material_downloads.len();
        }
        ensure!(
            download_records <= inputs::MAX_RECORDS && download_requests <= inputs::MAX_REQUESTS,
            "native input operation capacity"
        );
        ensure!(
            used.0 <= limits.slots
                && used.1 <= limits.memory_bytes
                && used.2 <= limits.cpu_millis
                && used.3 <= limits.disk_bytes,
            "native capacity exhausted"
        );
        Ok(())
    }
}
impl Journal {
    /// Explicit initialization requires a brand new root; an existing empty or
    /// damaged root is never silently initialized as a node with free capacity.
    pub fn create(root: &Path, node_id: Uuid, limits: Limits) -> Result<Self> {
        Self::create_identity(
            root,
            WorkerIdentity {
                node_id,
                boot_id: Uuid::new_v4(),
            },
            limits,
        )
    }
    fn create_identity(root: &Path, identity: WorkerIdentity, limits: Limits) -> Result<Self> {
        identity.validate()?;
        limits.validate()?;
        let kernel_boot_id = kernel_boot_id()?;
        let state = State {
            format: FORMAT,
            identity: identity.clone(),
            kernel_boot_id,
            revision: 1,
            attempts: BTreeMap::new(),
        };
        let storage = Storage::open(root, true)?;
        let mut result = Self {
            storage,
            state,
            limits,
            invocation: identity.boot_id,
            kernel_boot_id,
            poisoned: false,
            #[cfg(any(test, feature = "test-support"))]
            fault: None,
            #[cfg(any(test, feature = "test-support"))]
            pause: None,
        };
        result.persist(result.state.clone())?;
        Ok(result)
    }
    /// Restart is inspection/stop-and-preserve only. Stored boot IDs and local
    /// clock values are history, never renewable authority in this invocation.
    pub fn open(root: &Path, node_id: Uuid, limits: Limits) -> Result<Self> {
        limits.validate()?;
        let storage = Storage::open(root, false)?;
        let names = storage.members()?;
        ensure!(
            names.iter().any(|n| n == "journal.json"),
            "native journal missing; capacity unknown"
        );
        let raw = storage.read(c"journal.json", MAX_BYTES)?;
        let header: serde_json::Value = serde_json::from_slice(&raw)?;
        ensure!(
            header["state"]["format"].as_u64() == Some(FORMAT as u64),
            "unsupported native journal format; preserve original evidence"
        );
        let snapshot: Snapshot = serde_json::from_slice(&raw)?;
        ensure!(
            snapshot.digest == canonical_digest(&snapshot.state)?,
            "native journal checksum"
        );
        snapshot.state.validate(&limits)?;
        ensure!(
            snapshot.state.identity.node_id == node_id,
            "native journal belongs to another node"
        );
        if names.iter().any(|n| n == "journal.pending") {
            // An unacknowledged pre-rename temp cannot precede a physical effect.
            // Validate the file safety, then keep the last published state. A
            // post-rename interruption already has the newer published state.
            storage.read(c"journal.pending", MAX_BYTES)?;
            storage.remove_pending()?;
        }
        Ok(Self {
            storage,
            state: snapshot.state,
            limits,
            invocation: Uuid::new_v4(),
            kernel_boot_id: kernel_boot_id()?,
            poisoned: false,
            #[cfg(any(test, feature = "test-support"))]
            fault: None,
            #[cfg(any(test, feature = "test-support"))]
            pause: None,
        })
    }
    pub fn state(&self) -> &State {
        &self.state
    }
    /// Capacity for new polling is available only to this original live owner.
    /// Every retained record holds resources, including expired/unknown requests.
    pub fn available(&self) -> Result<Limits> {
        self.live()?;
        let mut free = self.limits.clone();
        for attempt in self.state.attempts.values() {
            let resource = &attempt.reservation.offer.offered.offer.manifest.resources;
            free.slots -= 1;
            free.memory_bytes -= resource.memory_bytes;
            free.cpu_millis -= resource.cpu_millis;
            free.disk_bytes -= attempt.reservation.disk_bytes;
        }
        Ok(free)
    }
    pub fn recovery_required(&self) -> bool {
        self.invocation != self.state.identity.boot_id
            || self.kernel_boot_id != self.state.kernel_boot_id
            || self.poisoned
    }
    fn live(&self) -> Result<u64> {
        self.storage.check_owner()?;
        ensure!(
            !self.recovery_required(),
            "native journal requires stop-and-preserve recovery"
        );
        boot_ns()
    }
    #[cfg(any(test, feature = "test-support"))]
    pub fn inject_fault(&mut self, fault: WriteFault) {
        self.fault = Some(fault);
    }
    #[cfg(any(test, feature = "test-support"))]
    pub fn pause_after_file_sync(&mut self, pause: &WritePause) {
        self.pause = Some(pause.gate.clone());
    }
    #[cfg(test)]
    pub(crate) fn set_pause_gate(&mut self, gate: std::sync::Arc<pause::Gate>) {
        self.pause = Some(gate);
    }
    fn persist(&mut self, state: State) -> Result<()> {
        ensure!(!self.poisoned, "native journal writer poisoned");
        state.validate(&self.limits)?;
        let bytes = canonical_bytes(&Snapshot {
            digest: canonical_digest(&state)?,
            state: state.clone(),
        })?;
        ensure!(bytes.len() <= MAX_BYTES, "native journal byte capacity");
        // From the first possible write onward, any error poisons this owner.
        self.poisoned = true;
        #[cfg(any(test, feature = "test-support"))]
        let fault = self.fault.take();
        #[cfg(any(test, feature = "test-support"))]
        if matches!(fault, Some(WriteFault::BeforeWrite)) {
            anyhow::bail!("injected before write");
        }
        self.storage.write_new(c"journal.pending", &bytes)?;
        #[cfg(any(test, feature = "test-support"))]
        if let Some(pause) = self.pause.take() {
            pause.wait()?;
        }
        #[cfg(any(test, feature = "test-support"))]
        if matches!(fault, Some(WriteFault::AfterFileSync)) {
            anyhow::bail!("injected after file fsync");
        }
        self.storage.publish()?;
        #[cfg(any(test, feature = "test-support"))]
        if matches!(fault, Some(WriteFault::AfterRename)) {
            anyhow::bail!("injected after rename");
        }
        self.storage.sync()?;
        #[cfg(any(test, feature = "test-support"))]
        if matches!(fault, Some(WriteFault::AfterDirectorySync)) {
            anyhow::bail!("injected after directory fsync");
        }
        self.state = state;
        self.poisoned = false;
        Ok(())
    }
    fn next(&self) -> Result<State> {
        let mut state = self.state.clone();
        state.revision = state
            .revision
            .checked_add(1)
            .context("native journal revision exhausted")?;
        Ok(state)
    }
    pub fn reserve(
        &mut self,
        offer: NativeOffer,
        runtime: RuntimeIdentity,
        deadline_ns: u64,
        disk_bytes: u64,
    ) -> Result<Uuid> {
        let now = self.live()?;
        offer.validate()?;
        ensure!(
            offer.offered.artifacts.len() <= 126
                && offer
                    .offered
                    .artifacts
                    .iter()
                    .map(|a| a.artifact.digest.as_str())
                    .collect::<BTreeSet<_>>()
                    .len()
                    == offer.offered.artifacts.len(),
            "native inline receipt set cannot satisfy Prepare"
        );
        let id = offer.offered.offer.binding.attempt_id;
        ensure!(
            !self.state.attempts.contains_key(&id),
            "attempt already reserved"
        );
        let maximum = now
            .checked_add(
                offer
                    .offered
                    .reservation_ttl_ms
                    .checked_mul(1_000_000)
                    .context("reservation duration overflow")?,
            )
            .context("reservation deadline overflow")?;
        ensure!(
            now < deadline_ns && deadline_ns <= maximum,
            "expired/extended reservation"
        );
        let reservation = Reservation {
            offer,
            runtime,
            reserved_at_ns: now,
            deadline_ns,
            disk_bytes,
            reservation_id: Uuid::new_v4(),
            reservation_operation_id: Uuid::new_v4(),
            accept_operation_id: Uuid::new_v4(),
            guardian_id: Uuid::new_v4(),
            launch_id: Uuid::new_v4(),
        };
        let mut state = self.next()?;
        state.attempts.insert(
            id,
            Attempt {
                reservation,
                acceptance: None,
                acceptance_digest: None,
                preparation: None,
                downloads: BTreeMap::new(),
                material_downloads: BTreeMap::new(),
                source: None,
                prepared: None,
                initial_grant: None,
                phase_acceptance: None,
                phase_preparation: None,
                phase_prepared: None,
            },
        );
        self.persist(state)?;
        Ok(id)
    }
    /// Persist before sending. Retries return the original typed bytes/IDs and
    /// original remaining-time claim; physical sends use separate fresh nonces.
    pub fn begin_accept(
        &mut self,
        id: Uuid,
        advertised_capabilities: Vec<Capability>,
    ) -> Result<Accept> {
        let now = self.live()?;
        let attempt = self
            .state
            .attempts
            .get(&id)
            .context("unknown native attempt")?;
        let r = &attempt.reservation;
        ensure!(
            attempt.phase_acceptance.is_none(),
            "phase attempt cannot use legacy Accept"
        );
        ensure!(now < r.deadline_ns, "native reservation expired");
        let acceptance = ProfiledAcceptance {
            binding: r.offer.offered.offer.binding.clone(),
            advertised_capabilities,
        };
        acceptance.validate_against(&r.offer.offered.offer)?;
        if let Some(request) = &attempt.acceptance {
            ensure!(
                same(&request.acceptance, &acceptance)?,
                "changed acceptance retry"
            );
            return Ok(request.clone());
        }
        let request = Accept {
            schema_version: 1,
            operation_id: r.accept_operation_id,
            identity: self.state.identity.clone(),
            acceptance,
            reservation: NoLaunchReservation {
                reservation_id: r.reservation_id,
                reservation_operation_id: r.reservation_operation_id,
                reservation_journal_digest: canonical_digest(r)?,
            },
            guardian_id: r.guardian_id,
            launch_id: r.launch_id,
            journal_revision: self.state.revision,
            journal_digest: canonical_digest(&self.state)?,
            reservation_remaining_ms: (r.deadline_ns - now) / 1_000_000,
        };
        TransportDocument::Accept(request.clone()).validate()?;
        let mut state = self.next()?;
        let attempt = state.attempts.get_mut(&id).unwrap();
        attempt.acceptance_digest = Some(canonical_digest(&request)?);
        attempt.acceptance = Some(request.clone());
        self.persist(state)?;
        Ok(request)
    }
    pub fn record_preparation(&mut self, id: Uuid, observed: &Preparation) -> Result<()> {
        let now = self.live()?;
        let attempt = self
            .state
            .attempts
            .get(&id)
            .context("unknown native attempt")?;
        let request = attempt
            .acceptance
            .as_ref()
            .context("acceptance not durable")?;
        ensure!(
            attempt.phase_acceptance.is_none(),
            "phase attempt cannot use a legacy preparation plan"
        );
        observed
            .message()
            .validate_for(request, observed.message().delivery.nonce)?;
        let cap = attempt
            .preparation
            .as_ref()
            .map_or(attempt.reservation.deadline_ns, |p| p.deadline_ns);
        ensure!(
            now < observed.deadline_ns() && observed.deadline_ns() <= cap,
            "expired/extended preparation"
        );
        if let Some(previous) = &attempt.preparation {
            ensure!(
                same(&previous.delivery.original, &observed.message().original)?,
                "preparation replay changed"
            );
            observed
                .message()
                .versions
                .validate_after(&previous.delivery.versions)?;
        }
        let mut state = self.next()?;
        state.attempts.get_mut(&id).unwrap().preparation = Some(RecordedPreparation {
            delivery: observed.message().clone(),
            sent_ns: observed.sent_ns(),
            received_ns: observed.received_ns(),
            deadline_ns: observed.deadline_ns(),
        });
        self.persist(state)
    }
}
#[cfg(test)]
mod tests;
