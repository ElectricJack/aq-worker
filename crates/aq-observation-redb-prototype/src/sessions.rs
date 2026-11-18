//! Retained node transport boot ownership. Watch evidence never renews a grant.
use super::*;
use aq_observation::{BootTransition, TransportAuthority};
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum HoldReason {
    BootFenced,
    SpoolLoss,
    SourceChange,
}
pub struct VerifiedBootAdmission {
    plan: BootTransition,
}
impl VerifiedBootAdmission {
    pub fn verify(plan: BootTransition, owner: &impl TransportAuthority) -> Result<Self> {
        ensure!(
            !plan.operation_id.is_nil()
                && !plan.node_id.is_nil()
                && !plan.transport_owner_id.is_nil()
                && !plan.expected_boot_id.is_nil()
                && !plan.new_boot_id.is_nil()
                && plan.expected_boot_id != plan.new_boot_id
                && plan.expected_next_sequence > 0,
            "boot admission fields"
        );
        artifact(&plan.owner_receipt)?;
        owner.admit_boot(&plan)?;
        Ok(Self { plan })
    }
}
pub(super) fn known_boot(meta: &Meta, boot: Uuid) -> bool {
    boot == meta.limits.boot_id || meta.boot_history.iter().any(|p| p.new_boot_id == boot)
}
/// Boot transitions the store keeps. A node restarts without limit: older
/// transitions are dropped and counted in `Meta::boots_trimmed`. The newest
/// one replays a repeated boot request; the rest refuse a recent boot's reuse.
pub(super) const BOOT_HISTORY_KEPT: usize = 16;
pub(super) fn validate_boot_history(meta: &Meta) -> Result<()> {
    ensure!(
        meta.boot_history.len() <= BOOT_HISTORY_KEPT
            && (meta.boots_trimmed == 0 || meta.boot_history.len() == BOOT_HISTORY_KEPT),
        "boot history retention"
    );
    let mut boot = match meta.boot_history.first() {
        Some(oldest) if meta.boots_trimmed > 0 => oldest.expected_boot_id,
        _ => meta.limits.boot_id,
    };
    let mut ids = std::collections::BTreeSet::new();
    let mut boots = std::collections::BTreeSet::from([boot, meta.limits.boot_id]);
    for transition in &meta.boot_history {
        ensure!(
            transition.expected_boot_id == boot
                && transition.node_id == meta.limits.node_id
                && transition.expected_next_sequence <= meta.next
                && ids.insert(transition.operation_id)
                && boots.insert(transition.new_boot_id)
                && meta
                    .transport
                    .as_ref()
                    .is_some_and(|i| i.transport_owner_id == transition.transport_owner_id
                        && i.operation_id != transition.operation_id),
            "boot history owner/identity/sequence"
        );
        artifact(&transition.owner_receipt)?;
        boot = transition.new_boot_id;
    }
    ensure!(boot == meta.active_boot, "active boot history mismatch");
    Ok(())
}
pub(super) fn event_boot(event: &ObservationRecord) -> Result<Uuid> {
    let SessionIdentity::Remote { boot_id, .. } = event.body.binding.session else {
        anyhow::bail!("remote transport only")
    };
    Ok(boot_id)
}
pub(super) fn parser() -> ArtifactRef {
    ArtifactRef {
        id: Text::new("row-store-source-fence").unwrap(),
        version: Text::new("0.0.5").unwrap(),
        digest: Digest::of(include_bytes!("sessions.rs")),
    }
}
/// Queues one store-fence gap record for `row` and holds the source. The
/// record's cursor says why (`aq_observation_admission::FENCED_BY_RESTART`, ...).
#[allow(clippy::too_many_arguments)]
fn fence_row(
    meta: &mut Meta,
    sources: &mut redb::Table<&[u8], &[u8]>,
    out: &mut redb::Table<u64, &[u8]>,
    ids: &mut redb::Table<&[u8], &[u8]>,
    key: Vec<u8>,
    prior: Vec<u8>,
    mut row: SourceRow,
    cursor: String,
    observed_at_ms: u64,
) -> Result<()> {
    let monitor = parser();
    let reason = GapReason::ReplayUnavailable;
    let body = Observation {
        binding: row.source.binding.clone(),
        provenance: Provenance {
            source_id: row.source.source_id,
            source_generation: row.source.generation,
            source_kind: if row.source.format == aq_observation::Format::OpenCodeSessionDto6c24ec9 {
                SourceKind::StructuredProviderEvent
            } else {
                SourceKind::NativeTranscript
            },
            native_session_key: row.source.provenance_session_key().clone(),
            cursor: SourceCursor::Record {
                id: Text::new(cursor)?,
            },
            parser: monitor.clone(),
            raw_record: RawRecordEvidence::Unavailable {
                reason: reason.clone(),
            },
        },
        sequence: meta.next,
        observed_at_ms,
        kind: ObservationKind::Gap {
            reason,
            affected: None,
        },
    };
    let event = ObservationRecord::seal(stable(&body)?, body)?;
    let bytes = encode(&event)?;
    let digest = encode(&event.body_digest)?;
    ensure!(
        ids.get(event.event_id.as_bytes().as_slice())?.is_none(),
        "boot fact ID conflict"
    );
    out.insert(meta.next, bytes.as_slice())?;
    ids.insert(event.event_id.as_bytes().as_slice(), digest.as_slice())?;
    meta.next = meta.next.checked_add(1).context("sequence exhausted")?;
    meta.gaps += 1;
    meta.gap_bytes += bytes.len() as u64;
    meta.logical_bytes += (bytes.len() + digest.len()) as u64;
    row.hold = Some(HoldReason::BootFenced);
    row.last_control_sequence = Some(meta.next - 1);
    if !row.parsers.contains(&monitor) {
        ensure!(row.parsers.len() < 16, "source parser history bound");
        row.parsers.push(monitor.clone());
    }
    if let Some(engine) = &mut row.engine {
        engine.revision = engine
            .revision
            .checked_add(1)
            .context("source revision exhausted")?;
    }
    budget::source_growth(&prior, &row)?;
    let bytes = encode(&row)?;
    sources.insert(key.as_slice(), bytes.as_slice())?;
    meta.logical_bytes = meta.logical_bytes - prior.len() as u64 + bytes.len() as u64;
    Ok(())
}
impl Store {
    pub fn adopt_boot(&self, admission: &VerifiedBootAdmission, observed_at_ms: u64) -> Result<()> {
        ensure!(
            self.managed_optional()?.is_none(),
            "prototype boot callback cannot control managed store"
        );
        self.apply_boot(&admission.plan, observed_at_ms, None, true)
    }
    /// With `fence`, every live source of the ended boot is held behind a gap
    /// record at once. Without it they stay collectable, so an attempt that
    /// survived the restart resumes from its stored cursor; the caller fences
    /// what nobody resumes with [`Store::fence_source`].
    pub(super) fn apply_boot(
        &self,
        plan: &BootTransition,
        observed_at_ms: u64,
        owner: Option<&aq_observation_admission::Owner>,
        fence: bool,
    ) -> Result<()> {
        let tx = self.begin_owner()?;
        let mut meta = read_meta(&tx)?;
        if let Some(old) = meta
            .boot_history
            .iter()
            .find(|p| p.operation_id == plan.operation_id)
        {
            ensure!(old == plan, "boot ID payload conflict");
            return self.abort(tx);
        }
        let initial = meta
            .transport
            .as_ref()
            .context("explicit owner-admitted store required")?;
        ensure!(
            plan.operation_id != initial.operation_id
                && plan.transport_owner_id == initial.transport_owner_id
                && plan.node_id == self.limits.node_id
                && plan.expected_boot_id == meta.active_boot
                && plan.expected_next_sequence == meta.next
                && !known_boot(&meta, plan.new_boot_id),
            "boot owner/CAS/retention refusal"
        );
        let mut sources = tx.open_table(SOURCES)?;
        let mut old = vec![];
        for pair in sources.iter()? {
            let (k, v) = pair?;
            let row: SourceRow = decode(v.value())?;
            if fence
                && budget::live(&row)
                && matches!(row.source.binding.session,SessionIdentity::Remote{boot_id,..}if boot_id==plan.expected_boot_id)
            {
                old.push((k.value().to_vec(), v.value().to_vec(), row));
            }
        }
        let mut out = tx.open_table(OUTBOX)?;
        let mut ids = tx.open_table(EVENTS)?;
        for (key, prior, row) in old {
            let cursor = format!(
                "{}{}:{}",
                aq_observation_admission::FENCED_BY_RESTART,
                plan.operation_id,
                row.source.source_id
            );
            fence_row(
                &mut meta,
                &mut sources,
                &mut out,
                &mut ids,
                key,
                prior,
                row,
                cursor,
                observed_at_ms,
            )?;
        }
        drop(ids);
        drop(out);
        drop(sources);
        ensure!(
            meta.gaps <= 16
                && meta.gap_bytes <= 65536
                && meta.logical_bytes <= self.limits.data_limit(),
            "reserved boot gap quota; retain old ownership"
        );
        meta.active_boot = plan.new_boot_id;
        meta.boot_history.push(plan.clone());
        if meta.boot_history.len() > BOOT_HISTORY_KEPT {
            meta.boot_history.remove(0);
            meta.boots_trimmed += 1;
        }
        if let Some(owner) = owner {
            meta.managed = Some(super::local_intent::Managed {
                owner: owner.clone(),
                blocked: false,
            });
            super::local_intent::validate_managed(&meta)?;
        }
        write_meta(&tx, &meta)?;
        self.commit(tx)
    }
    /// Holds one source behind an explicit gap record: a source of an ended
    /// boot that nobody resumed (`prefix` is `FENCED_BY_RESTART`), or one the
    /// node gave up reading (`INCOMPLETE_UNREADABLE`). `Ok(false)` when the
    /// source is absent or already held. Fails, changing nothing, while the
    /// reserved gap quota is full; the caller tries again after a receipt.
    pub fn fence_source(&self, id: Uuid, prefix: &str, observed_at_ms: u64) -> Result<bool> {
        self.require_collectible()?;
        let tx = self.begin()?;
        let mut meta = read_meta(&tx)?;
        let mut sources = tx.open_table(SOURCES)?;
        let prior = sources
            .get(id.as_bytes().as_slice())?
            .map(|v| v.value().to_vec());
        let row = prior.as_deref().map(decode::<SourceRow>).transpose()?;
        let Some((prior, row)) = prior.zip(row).filter(|(_, row)| budget::live(row)) else {
            drop(sources);
            self.abort(tx)?;
            return Ok(false);
        };
        let cursor = format!("{prefix}{}:{id}", meta.active_boot);
        {
            let mut out = tx.open_table(OUTBOX)?;
            let mut ids = tx.open_table(EVENTS)?;
            fence_row(
                &mut meta,
                &mut sources,
                &mut out,
                &mut ids,
                id.as_bytes().to_vec(),
                prior,
                row,
                cursor,
                observed_at_ms,
            )?;
        }
        drop(sources);
        ensure!(
            meta.gaps <= 16
                && meta.gap_bytes <= 65536
                && meta.logical_bytes <= self.limits.routine_control_limit(),
            "reserved source fence quota"
        );
        write_meta(&tx, &meta)?;
        self.commit(tx)?;
        Ok(true)
    }
}
