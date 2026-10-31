use anyhow::{Result, ensure};
use aq_observation::{
    lifecycle::*,
    spool::{Limits, Node, Spool},
    *,
};
use std::path::Path;
use uuid::Uuid;
pub fn reference(id: &str) -> ArtifactRef {
    ArtifactRef {
        id: Text::new(id).unwrap(),
        version: Text::new("1").unwrap(),
        digest: Digest::of(id.as_bytes()),
    }
}
pub struct FixtureTransportOwner;
impl TransportAuthority for FixtureTransportOwner {
    fn admit_initial(&self, initial: &InitialTransport) -> Result<()> {
        ensure!(
            initial.node.node_id == Uuid::from_u128(5)
                && initial.node.boot_id == Uuid::from_u128(6)
                && initial.transport_owner_id == Uuid::from_u128(400),
            "fixture owner refused transport scope"
        );
        Ok(())
    }
    fn admit_boot(&self, transition: &BootTransition) -> Result<()> {
        ensure!(
            transition.node_id == Uuid::from_u128(5)
                && transition.transport_owner_id == Uuid::from_u128(400),
            "fixture owner refused boot scope"
        );
        Ok(())
    }
}
pub fn initialize(root: &Path, node: Node, limits: Limits) -> Spool {
    let plan = InitialTransport {
        operation_id: Uuid::from_u128(401),
        node,
        transport_owner_id: Uuid::from_u128(400),
        initial_sequence: 1,
        owner_receipt: reference("synthetic-new-node-transport-owner"),
    };
    let verified = VerifiedInitial::verify(plan, &FixtureTransportOwner).unwrap();
    Spool::initialize(root, limits, verified).unwrap()
}
