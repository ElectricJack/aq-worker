//! Actual process-local observations, never deserializable input permissions.
use super::*;
use crate::native::http::preparation_v2::AcceptedPreparation;
#[derive(Clone)]
pub(crate) enum InputContext {
    Legacy(Arc<Preparation>),
    Phase(Arc<AcceptedPreparation>),
    Refresh(Arc<crate::native::http::preparation_refresh_v1::AcceptedPreparation>),
}
impl InputContext {
    pub(crate) fn deadline_ns(&self) -> u64 {
        match self {
            Self::Legacy(p) => p.deadline_ns(),
            Self::Phase(p) => p.deadline_ns(),
            Self::Refresh(p) => p.deadline_ns(),
        }
    }
    pub(crate) fn binding(&self) -> &aq_session_protocol::LaunchBinding {
        match self {
            Self::Legacy(p) => &p.plan().binding,
            Self::Phase(p) => &p.delivery().receipt.lease.binding,
            Self::Refresh(p) => &p.delivery().receipt.lease.binding,
        }
    }
    pub(crate) fn materials(&self) -> &[aq_admission::MaterialLease] {
        match self {
            Self::Legacy(p) => &p.plan().materials,
            Self::Phase(p) => &p.delivery().receipt.lease.materials,
            Self::Refresh(p) => &p.delivery().receipt.lease.materials,
        }
    }
    pub(crate) fn digest(&self) -> Result<aq_session_protocol::Digest> {
        match self {
            Self::Legacy(p) => crate::native::journal::canonical_digest(p.plan()),
            Self::Phase(p) => Ok(p.delivery().receipt.digest()?),
            Self::Refresh(p) => Ok(p.delivery().receipt.digest()?),
        }
    }
    pub(crate) fn check(&self, j: &Journal, id: Uuid) -> Result<()> {
        match self {
            Self::Legacy(p) => j.check_preparation(id, p),
            Self::Phase(p) => j.check_phase_preparation(id, p).map(|_| ()),
            Self::Refresh(p) => j.check_refresh_preparation(id, p).map(|_| ()),
        }
    }
}
