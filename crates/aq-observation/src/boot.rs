//! Host/central-owner admission for transport identity. These references do not
//! confer an execution grant. Lost spools have no local reseed shortcut.
use crate::{lifecycle::*, spool::Node};
use anyhow::{Result, ensure};
use serde::{Deserialize, Serialize};
use uuid::Uuid;
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct InitialTransport {
    pub operation_id: Uuid,
    pub node: Node,
    pub transport_owner_id: Uuid,
    pub initial_sequence: u64,
    pub owner_receipt: ArtifactRef,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BootTransition {
    pub operation_id: Uuid,
    pub node_id: Uuid,
    pub transport_owner_id: Uuid,
    pub expected_boot_id: Uuid,
    pub new_boot_id: Uuid,
    pub expected_next_sequence: u64,
    pub owner_receipt: ArtifactRef,
}
impl InitialTransport {
    pub(crate) fn validate(&self) -> Result<()> {
        ensure!(
            !self.operation_id.is_nil()
                && !self.node.node_id.is_nil()
                && !self.node.boot_id.is_nil()
                && !self.transport_owner_id.is_nil()
                && self.initial_sequence == 1,
            "initial transport admission refused"
        );
        artifact(&self.owner_receipt)?;
        Ok(())
    }
}
impl BootTransition {
    pub(crate) fn validate(&self) -> Result<()> {
        ensure!(
            !self.operation_id.is_nil()
                && !self.node_id.is_nil()
                && !self.transport_owner_id.is_nil()
                && !self.expected_boot_id.is_nil()
                && !self.new_boot_id.is_nil()
                && self.expected_boot_id != self.new_boot_id
                && self.expected_next_sequence > 0,
            "boot transition admission refused"
        );
        artifact(&self.owner_receipt)?;
        Ok(())
    }
}
/// Implemented by a host adapter that verifies authenticated central transport
/// ownership. Initial admission must refuse nodes with prior transport history;
/// missing state needs central recovery/gap admission and old-owner retirement.
pub trait TransportAuthority {
    fn admit_initial(&self, initial: &InitialTransport) -> Result<()>;
    fn admit_boot(&self, transition: &BootTransition) -> Result<()>;
}
pub struct VerifiedInitial {
    pub(crate) plan: InitialTransport,
}
impl VerifiedInitial {
    pub fn verify(plan: InitialTransport, authority: &impl TransportAuthority) -> Result<Self> {
        plan.validate()?;
        authority.admit_initial(&plan)?;
        Ok(Self { plan })
    }
}
pub struct VerifiedBootTransition {
    pub(crate) plan: BootTransition,
}
impl VerifiedBootTransition {
    pub fn verify(plan: BootTransition, authority: &impl TransportAuthority) -> Result<Self> {
        plan.validate()?;
        authority.admit_boot(&plan)?;
        Ok(Self { plan })
    }
}
