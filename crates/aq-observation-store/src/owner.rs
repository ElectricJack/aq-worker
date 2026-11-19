//! Explicit admission for new node observation stores. There is no migration or
//! missing-store recovery shortcut. Real authenticated authority lives outside.
//! PROTOTYPE ONLY: Prepared/Activated handshake and source-active gating are
//! not implemented by these old fixture callbacks; use LocalIntent instead.
//! A replayed initial receipt is not a production create
//! grant; these callback fixtures must not be wired into the worker runtime.
use super::*;
use aq_observation::{InitialTransport, TransportAuthority};
pub struct VerifiedNewStore {
    pub(super) initial: InitialTransport,
    pub(super) limits: Limits,
}
impl VerifiedNewStore {
    pub fn verify(
        initial: InitialTransport,
        limits: Limits,
        owner: &impl TransportAuthority,
    ) -> Result<Self> {
        limits.validate()?;
        ensure!(
            !initial.operation_id.is_nil()
                && !initial.transport_owner_id.is_nil()
                && initial.initial_sequence == 1
                && initial.node.node_id == limits.node_id
                && initial.node.boot_id == limits.boot_id,
            "new store owner scope/sequence"
        );
        artifact(&initial.owner_receipt)?;
        owner.admit_initial(&initial)?;
        Ok(Self { initial, limits })
    }
}
impl Store {
    pub fn create_admitted(root: &Path, admission: VerifiedNewStore) -> Result<Self> {
        Self::open_inner(
            root,
            admission.limits,
            true,
            Some(admission.initial),
            None,
            false,
        )
    }
    pub fn open_admitted(root: &Path, limits: Limits, owner_id: Uuid) -> Result<Self> {
        ensure!(!owner_id.is_nil(), "transport owner required");
        let store = Self::open(root, limits)?;
        let tx = store.db.begin_read()?;
        let meta: Meta = decode(
            tx.open_table(META)?
                .get("state")?
                .context("metadata absent")?
                .value(),
        )?;
        let initial = meta
            .transport
            .context("unadmitted prototype format; refuse runtime ownership")?;
        ensure!(
            initial.transport_owner_id == owner_id,
            "transport owner mismatch"
        );
        drop(tx);
        Ok(store)
    }
}
