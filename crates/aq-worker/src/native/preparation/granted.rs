//! Fsync-gated initial authority over retained actual inputs, with no host hooks.
use super::*;
use crate::native::http::control::Granted;
use aq_session_protocol::Digest;
use std::collections::{BTreeMap, BTreeSet};

pub(super) struct RetainedOwners {
    admitted: BTreeSet<Uuid>,
    bundles: BTreeMap<Uuid, Arc<PreparedInputs>>,
}
impl RetainedOwners {
    pub(super) fn new(state: &State) -> Self {
        Self {
            admitted: state.attempts.keys().copied().collect(),
            bundles: BTreeMap::new(),
        }
    }
    fn stats(&self) -> Result<RetainedInputStats> {
        let mut stats = RetainedInputStats {
            attempts: self.bundles.len(),
            inline_bytes: 0,
            startup_bytes: 0,
        };
        for bundle in self.bundles.values() {
            stats.inline_bytes = stats
                .inline_bytes
                .checked_add(bundle.retained_inline_bytes())
                .context("retained inline byte overflow")?;
            stats.startup_bytes = stats
                .startup_bytes
                .checked_add(bundle.startup_bytes())
                .context("retained startup byte overflow")?;
        }
        ensure!(
            self.admitted.len() <= 64
                && self.bundles.len() <= self.admitted.len()
                && stats.inline_bytes
                    <= self.admitted.len() as u64 * crate::native::MAX_INLINE_SET_BYTES
                && stats.startup_bytes <= stats.inline_bytes,
            "retained resource bound"
        );
        Ok(stats)
    }
    fn retain(&mut self, inputs: PreparedInputs) -> Result<Arc<PreparedInputs>> {
        let id = inputs.request().binding.attempt_id;
        ensure!(
            self.admitted.contains(&id),
            "input owner belongs to another pipeline"
        );
        ensure!(
            !self.bundles.contains_key(&id),
            "input owner already retained; use initial grant retry"
        );
        ensure!(
            inputs.retained_inline_bytes() <= crate::native::MAX_INLINE_SET_BYTES
                && inputs.startup_bytes() <= inputs.retained_inline_bytes(),
            "retained input byte shape"
        );
        let bundle = Arc::new(inputs);
        self.bundles.insert(id, bundle.clone());
        self.stats()?;
        Ok(bundle)
    }
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RetainedInputStats {
    pub attempts: usize,
    pub inline_bytes: u64,
    pub startup_bytes: u64,
}
/// Pure inspection/ownership handle. Never a serialized-to-live conversion.
pub struct RetainedGrantHistory {
    attempt: Uuid,
    record_digest: Digest,
    observed_after_sync_ns: u64,
    owner: Arc<Mutex<Owned>>,
    resources: Arc<Mutex<RetainedOwners>>,
    inputs: Arc<PreparedInputs>,
}
impl RetainedGrantHistory {
    pub fn attempt_id(&self) -> Uuid {
        self.attempt
    }
    pub fn record_digest(&self) -> &Digest {
        &self.record_digest
    }
    /// Process-local sample taken after successful fsync, not persisted metadata.
    pub fn observed_after_sync_ns(&self) -> u64 {
        self.observed_after_sync_ns
    }
}
/// This wrapper has no host-launch interface. Future effects need their own
/// durable launch intent and a fresh current-owner/deadline check.
pub struct DurableInitialGrant {
    history: RetainedGrantHistory,
    deadline_ns: u64,
    hard_deadline_ns: u64,
}
impl DurableInitialGrant {
    pub fn history(&self) -> &RetainedGrantHistory {
        &self.history
    }
    pub fn deadline_ns(&self) -> u64 {
        self.deadline_ns
    }
    pub fn hard_deadline_ns(&self) -> u64 {
        self.hard_deadline_ns
    }
}
pub enum GrantCommit {
    Usable(DurableInitialGrant),
    HistoryOnly(RetainedGrantHistory),
}
impl InputPipeline {
    pub fn retained_input_stats(&self) -> Result<RetainedInputStats> {
        self.retained
            .lock()
            .map_err(|_| anyhow::anyhow!("retained input owner poisoned"))?
            .stats()
    }
    /// Ownership transfer occurs synchronously, before the returned future can
    /// be abandoned or poll a busy storage lane. The map is never held over I/O.
    pub fn record_initial_grant(
        &self,
        inputs: PreparedInputs,
        observed: Granted,
    ) -> impl std::future::Future<Output = Result<GrantCommit>> + Send + '_ {
        let retained = self
            .retained
            .lock()
            .map_err(|_| anyhow::anyhow!("retained input owner poisoned"))
            .and_then(|mut map| map.retain(inputs));
        async move { self.commit_grant(retained?, observed).await }
    }
    /// Reuse the exclusive original bundle after an error or cancelled waiter.
    /// Fresh opaque network evidence is mandatory; serialized history cannot call this.
    pub fn retry_initial_grant(
        &self,
        id: Uuid,
        observed: Granted,
    ) -> impl std::future::Future<Output = Result<GrantCommit>> + Send + '_ {
        let retained = self
            .retained
            .lock()
            .map_err(|_| anyhow::anyhow!("retained input owner poisoned"))
            .and_then(|map| {
                map.bundles
                    .get(&id)
                    .cloned()
                    .context("initial grant retained owner absent")
            });
        async move { self.commit_grant(retained?, observed).await }
    }
    async fn commit_grant(
        &self,
        inputs: Arc<PreparedInputs>,
        observed: Granted,
    ) -> Result<GrantCommit> {
        let owned = self.owned.clone();
        let resources = self.retained.clone();
        self.storage(move |owner| {
            let id = inputs.request().binding.attempt_id;
            let record = owner
                .journal
                .record_initial_grant(id, inputs.request(), &observed)?;
            let deadline_ns = record.deadline_ns.min(observed.deadline_ns());
            let hard_deadline_ns = record.hard_deadline_ns.min(observed.hard_deadline_ns());
            let observed_after_sync_ns = boot_ns()?;
            let history = RetainedGrantHistory {
                attempt: id,
                record_digest: record.digest()?,
                observed_after_sync_ns,
                owner: owned,
                resources,
                inputs,
            };
            // History is retained even if verification/expiry fails after commit.
            if owner
                .journal
                .check_initial_grant(id, &history.record_digest, deadline_ns, hard_deadline_ns)
                .is_err()
                || history.inputs.verify_current(&owner.journal).is_err()
                || owner
                    .journal
                    .check_initial_grant(id, &history.record_digest, deadline_ns, hard_deadline_ns)
                    .is_err()
            {
                return Ok(GrantCommit::HistoryOnly(history));
            }
            Ok(GrantCommit::Usable(DurableInitialGrant {
                history,
                deadline_ns,
                hard_deadline_ns,
            }))
        })
        .await
    }
    pub async fn check_initial_grant(&self, grant: &DurableInitialGrant) -> Result<()> {
        ensure!(
            Arc::ptr_eq(&self.owned, &grant.history.owner)
                && Arc::ptr_eq(&self.retained, &grant.history.resources),
            "initial grant belongs to another owner"
        );
        let id = grant.history.attempt;
        let digest = grant.history.record_digest.clone();
        let deadline = grant.deadline_ns;
        let hard = grant.hard_deadline_ns;
        let inputs = grant.history.inputs.clone();
        self.storage(move |owner| {
            owner
                .journal
                .check_initial_grant(id, &digest, deadline, hard)?;
            inputs.verify_current(&owner.journal)?;
            owner
                .journal
                .check_initial_grant(id, &digest, deadline, hard)?;
            Ok(())
        })
        .await
    }
}
