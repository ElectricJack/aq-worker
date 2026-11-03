//! H2 wire contract: aq-server grand-vault-92, da9ea1d.
//! Facts are durable before delivery; an ambiguous acknowledgement is replayed.
use super::*;
use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum VmSize {
    Small,
    Default,
    Large,
    Xlarge,
}
impl VmSize {
    fn for_request(r: &CreateRequest) -> Result<Self, BackendError> {
        match policy::size(
            r.resources
                .ok_or(BackendError::InvalidRequest("missing VM size"))?,
        )? {
            "small" => Ok(Self::Small),
            "default" => Ok(Self::Default),
            "large" => Ok(Self::Large),
            _ => Err(unknown()),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct VmTimeFact {
    pub attempt_id: Uuid,
    pub sandbox_id: String,
    pub size: VmSize,
    pub started_at_ms: u64,
    pub stopped_at_ms: Option<u64>,
}
impl VmTimeFact {
    pub(super) fn is_not_created(&self) -> bool {
        self.sandbox_id == format!("not-created/{}", self.attempt_id)
    }
    pub(super) fn validate(&self) -> Result<(), BackendError> {
        let at = now()?;
        if self.attempt_id.is_nil()
            || !(config::safe_id(&self.sandbox_id) || self.is_not_created())
            || self.started_at_ms == 0
            || self.started_at_ms > at
            || self
                .stopped_at_ms
                .is_some_and(|end| end < self.started_at_ms || end > at)
            || (self.is_not_created() && self.stopped_at_ms != Some(self.started_at_ms))
        {
            return Err(BackendError::InvalidRequest("invalid VM-time fact"));
        }
        Ok(())
    }
    pub(super) fn succeeds(&self, old: &Self) -> bool {
        self.attempt_id == old.attempt_id
            && self.sandbox_id == old.sandbox_id
            && self.size == old.size
            && self.started_at_ms == old.started_at_ms
            && (old.stopped_at_ms.is_none() || old.stopped_at_ms == self.stopped_at_ms)
    }
}

#[derive(Clone, Serialize, Deserialize)]
pub(super) struct Report {
    pub fact: VmTimeFact,
    pub pending: bool,
}

impl BoatBackend {
    /// The production controller always installs its enrolled node client.
    /// Standalone backend users can journal facts without a server connection.
    pub fn with_vm_time_client(mut self, client: crate::client::Client) -> Self {
        self.vm_time_client = Some(client);
        self
    }

    pub fn pending_vm_time_reports(&self) -> usize {
        self.journal.pending_facts().len()
    }

    /// Pending reports include stopped/deleted VMs. HTTP failure never blocks
    /// stopping writers: the fsynced fact remains pending for recovery.
    pub async fn replay_vm_time(&mut self) -> Result<(), BackendError> {
        self.journal.check()?;
        let Some(client) = &self.vm_time_client else {
            return Ok(());
        };
        for fact in self.journal.pending_facts() {
            match client.report_vm_time(&fact).await {
                Ok(()) => self.journal.ack_fact(fact.attempt_id)?,
                Err(error) => {
                    tracing::warn!(attempt=%fact.attempt_id, %error, "VM-time report retained for replay");
                    // A disconnected server should cost one failed call per pass.
                    break;
                }
            }
        }
        Ok(())
    }

    /// Trusted operator import for historical VMs whose old journal predates
    /// lifecycle timestamps. Never infer a historical start from today's clock.
    /// No-create facts require the controller's pre-create proof, not an import.
    pub fn backfill_vm_time(&mut self, fact: VmTimeFact) -> Result<(), BackendError> {
        fact.validate()?;
        if fact.is_not_created() {
            return Err(BackendError::InvalidRequest(
                "no-create fact requires controller proof",
            ));
        }
        if self
            .journal
            .by_id(&fact.sandbox_id)
            .is_some_and(|r| r.request.owner.attempt_id != fact.attempt_id)
            || self
                .journal
                .get(&format!(
                    "{}{}",
                    policy::prefix(self.config.node_id),
                    fact.attempt_id.simple()
                ))
                .is_some_and(|r| {
                    r.backend_id
                        .as_ref()
                        .is_some_and(|id| id != &fact.sandbox_id)
                        || VmSize::for_request(&r.request).is_ok_and(|size| size != fact.size)
                })
            || self
                .journal
                .orphan(&fact.sandbox_id)
                .is_some_and(|o| o.entry.sandbox.owner.attempt_id != fact.attempt_id)
        {
            return Err(BackendError::Conflict);
        }
        self.journal.set_fact(fact)
    }

    pub(super) fn observe_vm_time(
        &mut self,
        vm: &Value,
        e: &InventoryEntry,
    ) -> Result<(), BackendError> {
        let id = e.sandbox.owner.attempt_id;
        let mut fact = if let Some(report) = self.journal.fact(id) {
            if report.fact.sandbox_id != e.sandbox.backend_id {
                return Err(BackendError::Conflict);
            }
            report.fact.clone()
        } else {
            // The immutable request supplies selected size; Boat supplies actual
            // creation time (RFC3339 createdAt). Orphans/missing old metadata
            // need an explicit historical import and remain unaccounted meanwhile.
            let Some(r) = self.journal.get(&e.name) else {
                return Ok(());
            };
            let Some(start) = vm["createdAt"]
                .as_str()
                .and_then(|s| chrono::DateTime::parse_from_rfc3339(s).ok())
                .and_then(|t| u64::try_from(t.timestamp_millis()).ok())
            else {
                tracing::warn!(attempt=%id, "VM creation timestamp unavailable; historical backfill required");
                return Ok(());
            };
            VmTimeFact {
                attempt_id: id,
                sandbox_id: e.sandbox.backend_id.clone(),
                size: VmSize::for_request(&r.request)?,
                started_at_ms: start,
                stopped_at_ms: None,
            }
        };
        if e.writer_proof().is_confirmed() && fact.stopped_at_ms.is_none() {
            // Record the first backend-confirmed end, never intent/202/TTL.
            fact.stopped_at_ms = Some(now()?);
        }
        self.journal.set_fact(fact)
    }

    pub(super) async fn not_created(
        &mut self,
        request: &CreateRequest,
        at_ms: u64,
    ) -> Result<(), BackendError> {
        self.journal.check()?;
        request.validate()?;
        if request.owner.node_id != self.config.node_id || self.journal.get(&request.name).is_some()
        {
            // Any Boat create intent is uncertainty, even an empty inventory.
            return Err(BackendError::Conflict);
        }
        if self.journal.fact(request.owner.attempt_id).is_none() {
            self.journal.set_fact(VmTimeFact {
                attempt_id: request.owner.attempt_id,
                sandbox_id: format!("not-created/{}", request.owner.attempt_id),
                size: VmSize::for_request(request)?,
                started_at_ms: at_ms,
                stopped_at_ms: Some(at_ms),
            })?;
        } else if !self
            .journal
            .fact(request.owner.attempt_id)
            .unwrap()
            .fact
            .is_not_created()
        {
            return Err(BackendError::Conflict);
        }
        self.replay_vm_time().await
    }
}
