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
pub(super) fn validate_boot_history(meta: &Meta) -> Result<()> {
    ensure!(meta.boot_history.len() <= 64, "boot history bound");
    let mut boot = meta.limits.boot_id;
    let mut ids = std::collections::BTreeSet::new();
    let mut boots = std::collections::BTreeSet::from([boot]);
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
impl Store {
    pub fn adopt_boot(&self, admission: &VerifiedBootAdmission, observed_at_ms: u64) -> Result<()> {
        ensure!(
            self.managed_optional()?.is_none(),
            "prototype boot callback cannot control managed store"
        );
        self.apply_boot(&admission.plan, observed_at_ms, None)
    }
    pub(super) fn apply_boot(
        &self,
        plan: &BootTransition,
        observed_at_ms: u64,
        owner: Option<&aq_observation_admission::Owner>,
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
                && !known_boot(&meta, plan.new_boot_id)
                && meta.boot_history.len() < 64,
            "boot owner/CAS/retention refusal"
        );
        let monitor = parser();
        let mut sources = tx.open_table(SOURCES)?;
        let mut old = vec![];
        for pair in sources.iter()? {
            let (k, v) = pair?;
            let row: SourceRow = decode(v.value())?;
            if budget::live(&row)
                && matches!(row.source.binding.session,SessionIdentity::Remote{boot_id,..}if boot_id==plan.expected_boot_id)
            {
                old.push((k.value().to_vec(), v.value().to_vec(), row));
            }
        }
        let mut out = tx.open_table(OUTBOX)?;
        let mut ids = tx.open_table(EVENTS)?;
        for (key, prior, mut row) in old {
            let reason = GapReason::ReplayUnavailable;
            let body = Observation {
                binding: row.source.binding.clone(),
                provenance: Provenance {
                    source_id: row.source.source_id,
                    source_generation: row.source.generation,
                    source_kind: if row.source.format
                        == aq_observation::Format::OpenCodeSessionDto6c24ec9
                    {
                        SourceKind::StructuredProviderEvent
                    } else {
                        SourceKind::NativeTranscript
                    },
                    native_session_key: row.source.provenance_session_key().clone(),
                    cursor: SourceCursor::Record {
                        id: Text::new(format!(
                            "boot-fence:{}:{}",
                            plan.operation_id, row.source.source_id
                        ))?,
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
}
