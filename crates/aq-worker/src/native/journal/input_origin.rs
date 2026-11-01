//! Explicit physical-input provenance. Serialized history is never live authority.
use super::*;
use aq_admission::MaterialLease;
use aq_session_protocol::LaunchBinding;
#[derive(Clone, Serialize, Deserialize)]
#[serde(
    tag = "protocol",
    content = "record",
    rename_all = "snake_case",
    deny_unknown_fields
)]
pub enum InputOrigin {
    Legacy(Box<RecordedPreparation>),
    Phase(Box<preparation_v2::ReceivedPreparation>),
}
impl InputOrigin {
    pub fn deadline_ns(&self) -> u64 {
        match self {
            Self::Legacy(p) => p.deadline_ns,
            Self::Phase(p) => p.deadline_ns,
        }
    }
    pub fn binding(&self) -> &LaunchBinding {
        match self {
            Self::Legacy(p) => &p.delivery.original.binding,
            Self::Phase(p) => &p.delivery.receipt.lease.binding,
        }
    }
    pub fn materials(&self) -> &[MaterialLease] {
        match self {
            Self::Legacy(p) => &p.delivery.original.materials,
            Self::Phase(p) => &p.delivery.receipt.lease.materials,
        }
    }
    pub fn digest(&self) -> Result<Digest> {
        match self {
            Self::Legacy(p) => canonical_digest(&p.delivery.original),
            Self::Phase(p) => Ok(p.receipt_digest.clone()),
        }
    }
    pub fn observed_at_ms(&self) -> u64 {
        match self {
            Self::Legacy(p) => p.delivery.original.clock.server_time_ms,
            Self::Phase(p) => p.delivery.receipt.lease.issued_at_ms,
        }
    }
    pub(super) fn capture(a: &Attempt) -> Result<Self> {
        if a.phase_acceptance.is_some() {
            Ok(Self::Phase(Box::new(
                a.phase_preparation
                    .clone()
                    .context("phase receipt absent")?,
            )))
        } else {
            Ok(Self::Legacy(Box::new(
                a.preparation.clone().context("legacy preparation absent")?,
            )))
        }
    }
    pub(super) fn validate(
        &self,
        a: &Attempt,
        created_revision: u64,
        created_ns: u64,
        revision: u64,
    ) -> Result<()> {
        match self {
            Self::Legacy(p) => {
                ensure!(
                    a.phase_acceptance.is_none(),
                    "legacy input on phase attempt"
                );
                inputs::validate_capture(a, p, created_revision, created_ns, revision)
            }
            Self::Phase(p) => {
                ensure!(a.phase_acceptance.is_some(), "phase input intent absent");
                let saved = a
                    .phase_preparation
                    .as_ref()
                    .context("phase input receipt absent")?;
                // State::validate already validates this exact saved phase
                // receipt against the original proposal once per attempt.
                // Equality below makes every input depend on that checked owner
                // without revalidating all offer descriptors once per input.
                ensure!(
                    same(p, saved)?
                        && p.receipt_digest == p.delivery.receipt.digest()?
                        && created_revision >= p.record_revision
                        && created_revision < revision
                        && created_ns >= p.observed_before_write_ns
                        && created_ns < p.deadline_ns,
                    "phase input provenance"
                );
                Ok(())
            }
        }
    }
}
