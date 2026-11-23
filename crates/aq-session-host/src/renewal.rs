//! Publish exact permit renewals under the session lock, with replay after interrupted writes.
use super::*;
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct RenewalLedger {
    latest: Option<BoundRequest>,
}

impl SessionHandle {
    pub fn renew(&self, permit: &ExecutionPermit) -> Result<()> {
        let _lock = RenewalLock::acquire(&self.directory.join("renew.lock"))?;
        let state = self.observe()?;
        ensure!(
            state.phase == HostPhase::Running,
            "renewal requires live running session"
        );
        let now = ownership::raw_boot_ms();
        let config: SealedLaunch = read_json(&self.directory.join("launch.json"))?;
        config.validate_permit_at(
            permit,
            &self.identity.intent.session,
            &self.identity.intent.manifest_digest,
            now,
        )?;
        ensure!(
            now < state.permit_deadline_boot_ms,
            "expired current authority cannot renew"
        );
        let wall = config.wall_deadline()?;
        ensure!(
            permit.expires_boot_ms <= wall && !self.directory.join("stop.request").exists(),
            "renewal exceeds wall limit or cancelled session"
        );
        let renewal = BoundRequest {
            guardian: self.identity.clone(),
            permit: permit.clone(),
        };
        let path = self.directory.join("renewals.json");
        let mut ledger = RenewalLedger::load(&path, &self.identity)?;
        if !ledger.is_replay(permit)? {
            if matches!(config.manifest, LaunchDocument::Plan(_)) {
                let prior = ledger.latest.as_ref().map_or(&config.permit, |r| &r.permit);
                advance_plan(&state, prior, permit, now)?;
            }
            ledger.record(&path, &state, &renewal)?;
        }
        // A crash after ledger fsync but before publication is retriable. Older
        // sequences refuse as stale and cannot overwrite newer authority.
        if permit.sequence > state.permit_sequence && ledger.has_sequence(permit.sequence) {
            publish_pending(&self.directory.join("renew.request"), &renewal)?;
        }
        Ok(())
    }
}

impl RenewalLedger {
    fn load(path: &Path, identity: &GuardianIdentity) -> Result<Self> {
        let ledger: Self = if path.exists() {
            read_json(path)?
        } else {
            Self { latest: None }
        };
        if let Some(old) = &ledger.latest {
            ensure!(
                &old.guardian == identity,
                "renewal ledger guardian mismatch"
            );
        }
        Ok(ledger)
    }

    fn has_sequence(&self, sequence: u64) -> bool {
        self.latest
            .as_ref()
            .is_some_and(|r| r.permit.sequence == sequence)
    }

    fn is_replay(&self, permit: &ExecutionPermit) -> Result<bool> {
        let Some(old) = self
            .latest
            .as_ref()
            .filter(|r| r.permit.sequence == permit.sequence)
        else {
            return Ok(false);
        };
        ensure!(old.permit == *permit, "changed renewal sequence payload");
        Ok(true)
    }

    fn record(
        &mut self,
        path: &Path,
        state: &HostObservation,
        renewal: &BoundRequest,
    ) -> Result<()> {
        let sequence = renewal.permit.sequence;
        ensure!(
            sequence > state.permit_sequence
                && self
                    .latest
                    .as_ref()
                    .is_none_or(|r| sequence > r.permit.sequence),
            "stale renewal sequence"
        );
        self.latest = Some(renewal.clone());
        durable_replace(path, &serde_json::to_vec(self)?)
    }
}

fn advance_plan(
    state: &HostObservation,
    prior: &ExecutionPermit,
    next: &ExecutionPermit,
    now: u64,
) -> Result<()> {
    let current = now >= state.observed_boot_ms && prior.sequence == state.permit_sequence;
    let successor = prior.sequence.checked_add(1) == Some(next.sequence);
    let extends =
        next.issued_boot_ms >= prior.issued_boot_ms && next.expires_boot_ms > prior.expires_boot_ms;
    ensure!(
        current && successor && extends,
        "host renewal must advance exact live prior permit"
    );
    Ok(())
}

fn publish_pending(path: &Path, renewal: &BoundRequest) -> Result<()> {
    if path.exists() {
        let old: BoundRequest = read_json(path)?;
        ensure!(
            old.guardian == renewal.guardian,
            "pending renewal guardian mismatch"
        );
        if old.permit.sequence == renewal.permit.sequence {
            ensure!(
                old.permit == renewal.permit,
                "changed pending renewal payload"
            );
            return Ok(());
        }
        ensure!(
            old.permit.sequence < renewal.permit.sequence,
            "pending renewal ahead of ledger"
        );
    }
    durable_replace(path, &serde_json::to_vec(renewal)?)
}
