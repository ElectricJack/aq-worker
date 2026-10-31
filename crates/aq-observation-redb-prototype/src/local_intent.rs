//! Durable local owner handshake. Only calls through a trusted authenticated peer
//! produce live replies; deserialized receipts are never creation authority.
use super::*;
use aq_observation_admission::{
    self as admission, Marker, Operation, Owner, Phase, Reply, Request, Status,
};
use intent_files::IntentFiles;
/// Adapter must authenticate the current node, enforce response bounds, and
/// perform a fresh request. No cached response may implement this interface.
/// Credentials and network configuration remain exclusively in the adapter.
pub trait AuthenticatedOwnerPeer {
    fn mutate(&mut self, request: &Request) -> Result<Reply>;
    fn status(&mut self, node_id: Uuid) -> Result<Status>;
}
struct LiveReply(Reply);
struct LiveStatus(Status);
fn reply(peer: &mut impl AuthenticatedOwnerPeer, request: &Request) -> Result<LiveReply> {
    request.validate()?;
    let r = peer.mutate(request)?;
    ensure!(
        encode(&r)?.len() <= admission::MAX_BYTES,
        "owner reply bound"
    );
    r.validate_against(request)?;
    Ok(LiveReply(r))
}
fn status(peer: &mut impl AuthenticatedOwnerPeer, node: Uuid) -> Result<LiveStatus> {
    let s = peer.status(node)?;
    s.validate()?;
    ensure!(
        s.node_id == node && encode(&s)?.len() <= admission::MAX_BYTES,
        "owner status scope/bound"
    );
    Ok(LiveStatus(s))
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Managed {
    pub owner: Owner,
    pub blocked: bool,
}
pub(super) fn validate_managed(meta: &Meta) -> Result<()> {
    if let Some(m) = &meta.managed {
        m.owner.validate()?;
        let initial = meta
            .transport
            .as_ref()
            .context("managed transport missing")?;
        ensure!(
            m.owner.marker.node_id == meta.limits.node_id
                && m.owner.marker.initial_boot_id == meta.limits.boot_id
                && m.owner.marker.owner_id == initial.transport_owner_id
                && m.owner.current_boot_id == meta.active_boot
                && m.owner.boot_transitions as usize == meta.boot_history.len(),
            "managed marker/boot history mismatch"
        );
        if m.owner.phase == Phase::Prepared {
            ensure!(
                meta.next == 1 && meta.acked == 0 && meta.sources == 0 && meta.seen == 0,
                "prepared store has observations"
            );
        }
    }
    Ok(())
}
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LocalStage {
    PreparePending,
    Prepared,
    CreationStarted,
    StoreCreated,
    ActivationPending,
    Active,
    BootPending,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct State {
    version: u32,
    limits: Limits,
    prepare: Request,
    prepare_receipt: Option<admission::Receipt>,
    marker: Option<Marker>,
    activation: Option<Request>,
    owner: Option<Owner>,
    boot: Option<Request>,
    stage: LocalStage,
}
impl State {
    fn validate(&self) -> Result<()> {
        ensure!(self.version == 1, "local intent version");
        self.limits.validate()?;
        self.prepare.validate()?;
        ensure!(
            matches!(self.prepare.operation,Operation::Prepare{boot_id,..}if boot_id==self.limits.boot_id)
                && self.prepare.node_id == self.limits.node_id,
            "prepare binding"
        );
        if self.stage == LocalStage::PreparePending {
            ensure!(
                self.prepare_receipt.is_none()
                    && self.marker.is_none()
                    && self.activation.is_none()
                    && self.owner.is_none()
                    && self.boot.is_none(),
                "initial intent shape"
            );
            return Ok(());
        }
        let receipt = self
            .prepare_receipt
            .as_ref()
            .context("prepare receipt absent")?;
        receipt.validate_against(&self.prepare)?;
        let marker = self.marker.as_ref().context("local marker absent")?;
        marker.validate()?;
        ensure!(
            &receipt.resulting_owner.marker == marker,
            "local marker changed"
        );
        let owner = self.owner.as_ref().context("local owner absent")?;
        owner.validate()?;
        ensure!(&owner.marker == marker, "owner marker changed");
        let activation_needed = matches!(
            self.stage,
            LocalStage::ActivationPending | LocalStage::Active | LocalStage::BootPending
        );
        ensure!(
            self.activation.is_some() == activation_needed,
            "activation stage"
        );
        if let Some(a) = &self.activation {
            a.validate()?;
            ensure!(
                a.node_id == self.limits.node_id
                    && a.operation_id != self.prepare.operation_id
                    && matches!(&a.operation,Operation::Activate{marker:m}if m==marker),
                "activation binding"
            );
        }
        ensure!(
            self.boot.is_some() == (self.stage == LocalStage::BootPending),
            "boot stage"
        );
        if let Some(b) = &self.boot {
            b.validate()?;
            ensure!(
                b.node_id == self.limits.node_id
                    && b.operation_id != self.prepare.operation_id
                    && self
                        .activation
                        .as_ref()
                        .is_some_and(|a| a.operation_id != b.operation_id)
                    && matches!(&b.operation,Operation::AdvanceBoot{marker:m,expected_boot_id,expected_revision,..}if m==marker&&*expected_boot_id==owner.current_boot_id&&*expected_revision==owner.revision),
                "boot intent binding"
            );
        }
        ensure!(
            owner.phase
                == if matches!(self.stage, LocalStage::Active | LocalStage::BootPending) {
                    Phase::Active
                } else {
                    Phase::Prepared
                },
            "local owner phase"
        );
        Ok(())
    }
}
/// Owns one private node transport directory. It never exposes its managed Store
/// through the anonymous fixture open path. No transcript data enters the intent.
pub struct LocalIntent {
    files: IntentFiles,
    state: State,
    store: Option<Store>,
    host_boot: Uuid,
    reconciled: bool,
}
impl LocalIntent {
    pub fn create(root: &Path, limits: Limits, operation_id: Uuid, store_id: Uuid) -> Result<Self> {
        limits.validate()?;
        let prepare = Request {
            version: admission::VERSION,
            operation_id,
            node_id: limits.node_id,
            operation: Operation::Prepare {
                boot_id: limits.boot_id,
                store_id,
            },
        };
        let state = State {
            version: 1,
            limits: limits.clone(),
            prepare,
            prepare_receipt: None,
            marker: None,
            activation: None,
            owner: None,
            boot: None,
            stage: LocalStage::PreparePending,
        };
        state.validate()?;
        let mut files = IntentFiles::acquire(root, true)?;
        files.persist(&state)?;
        Ok(Self {
            files,
            state,
            store: None,
            host_boot: limits.boot_id,
            reconciled: false,
        })
    }
    pub fn open(root: &Path, host_boot: Uuid) -> Result<Self> {
        ensure!(!host_boot.is_nil(), "host boot absent");
        let files = IntentFiles::acquire(root, false)?;
        let state: State = files.read()?;
        state.validate()?;
        let path = files.store_path();
        let has = std::fs::symlink_metadata(&path).is_ok();
        let needs = matches!(
            state.stage,
            LocalStage::CreationStarted
                | LocalStage::StoreCreated
                | LocalStage::ActivationPending
                | LocalStage::Active
                | LocalStage::BootPending
        );
        ensure!(needs || !has, "unexpected store before creation intent");
        let store = if needs {
            let s = Store::open_inner(&path, state.limits.clone(), false, None, None, true)?;
            let m = s.managed()?;
            ensure!(
                Some(&m.owner.marker) == state.marker.as_ref(),
                "existing store marker mismatch"
            );
            Some(s)
        } else {
            None
        };
        let mut this = Self {
            files,
            state,
            store,
            host_boot,
            reconciled: false,
        };
        if this.state.stage == LocalStage::CreationStarted {
            this.save(|s| s.stage = LocalStage::StoreCreated)?;
        }
        Ok(this)
    }
    pub fn stage(&self) -> LocalStage {
        self.state.stage
    }
    pub fn intent_monitor(&self) -> Monitor {
        self.files.monitor.clone()
    }
    pub fn store_monitor(&self) -> Result<Monitor> {
        Ok(self.store.as_ref().context("store not created")?.monitor())
    }
    fn save(&mut self, change: impl FnOnce(&mut State)) -> Result<()> {
        let mut next = self.state.clone();
        change(&mut next);
        next.validate()?;
        self.files.persist(&next)?;
        self.state = next;
        Ok(())
    }
    pub fn prepare(&mut self, peer: &mut impl AuthenticatedOwnerPeer) -> Result<()> {
        self.files.check()?;
        ensure!(
            matches!(
                self.state.stage,
                LocalStage::PreparePending | LocalStage::Prepared
            ),
            "prepare stage"
        );
        ensure!(
            self.host_boot == self.state.limits.boot_id,
            "initial host boot changed"
        );
        let LiveReply(r) = self.files.step(|| reply(peer, &self.state.prepare))?;
        ensure!(
            r.may_create(&self.state.prepare)? && r.admitted_node_boot_id == self.host_boot,
            "central recovery required; cannot create"
        );
        self.save(|s| {
            s.prepare_receipt = Some(r.receipt);
            s.marker = Some(r.current_owner.marker.clone());
            s.owner = Some(r.current_owner);
            s.stage = LocalStage::Prepared
        })
    }
    pub fn create_store(&mut self, peer: &mut impl AuthenticatedOwnerPeer) -> Result<()> {
        ensure!(
            self.state.stage == LocalStage::Prepared,
            "store creation stage"
        );
        // A durable old receipt is insufficient: refresh the exact prepare through
        // the live authenticated adapter immediately before creation admission.
        self.prepare(peer)?;
        self.save(|s| s.stage = LocalStage::CreationStarted)?;
        let marker = self.state.marker.clone().context("marker")?;
        let owner = self.state.owner.clone().context("owner")?;
        let receipt = self
            .state
            .prepare_receipt
            .as_ref()
            .context("prepare receipt")?;
        let initial = aq_observation::InitialTransport {
            operation_id: self.state.prepare.operation_id,
            node: aq_observation::spool::Node {
                node_id: marker.node_id,
                boot_id: marker.initial_boot_id,
            },
            transport_owner_id: marker.owner_id,
            initial_sequence: 1,
            owner_receipt: receipt_artifact(receipt)?,
        };
        let store = self.files.step(|| {
            Store::open_inner(
                &self.files.store_path(),
                self.state.limits.clone(),
                true,
                Some(initial),
                Some(Managed {
                    owner,
                    blocked: true,
                }),
                true,
            )
        })?;
        self.store = Some(store);
        self.save(|s| s.stage = LocalStage::StoreCreated)
    }
    pub fn activate(
        &mut self,
        peer: &mut impl AuthenticatedOwnerPeer,
        operation_id: Uuid,
    ) -> Result<()> {
        self.files.check()?;
        ensure!(
            matches!(
                self.state.stage,
                LocalStage::StoreCreated | LocalStage::ActivationPending
            ),
            "activation stage"
        );
        if self.state.stage == LocalStage::StoreCreated {
            let marker = self.state.marker.clone().context("marker")?;
            let request = Request {
                version: admission::VERSION,
                operation_id,
                node_id: self.state.limits.node_id,
                operation: Operation::Activate { marker },
            };
            self.save(|s| {
                s.activation = Some(request);
                s.stage = LocalStage::ActivationPending
            })?;
        } else {
            ensure!(
                self.state
                    .activation
                    .as_ref()
                    .is_some_and(|a| a.operation_id == operation_id),
                "activation immutable operation"
            );
        }
        let request = self.state.activation.as_ref().context("activation")?;
        let LiveReply(r) = self.files.step(|| reply(peer, request))?;
        ensure!(
            r.current_owner == r.receipt.resulting_owner
                && r.current_owner.current_boot_id == self.host_boot
                && r.admitted_node_boot_id == self.host_boot
                && r.accepted_through == 0,
            "activation owner advanced; recovery required"
        );
        self.install_active(r.current_owner)
    }
    fn install_active(&mut self, owner: Owner) -> Result<()> {
        ensure!(
            owner.phase == Phase::Active && Some(&owner.marker) == self.state.marker.as_ref(),
            "active marker"
        );
        let store = self.store.as_ref().context("existing store required")?;
        self.files.step(|| store.install_owner(&owner, false))?;
        self.save(|s| {
            s.owner = Some(owner);
            s.stage = LocalStage::Active;
            s.boot = None
        })?;
        self.reconciled = true;
        Ok(())
    }
    pub fn resume(&mut self, peer: &mut impl AuthenticatedOwnerPeer) -> Result<()> {
        self.files.check()?;
        ensure!(
            matches!(
                self.state.stage,
                LocalStage::ActivationPending | LocalStage::Active
            ),
            "status cannot recover missing or uncreated store"
        );
        let store = self.store.as_ref().context("existing store required")?;
        let stored = store.managed()?;
        let LiveStatus(s) = self
            .files
            .step(|| status(peer, self.state.limits.node_id))?;
        let owner = s.owner.context("central owner missing")?;
        ensure!(
            Some(&owner.marker) == self.state.marker.as_ref()
                && owner.current_boot_id == self.host_boot
                && s.admitted_node_boot_id == self.host_boot,
            "status exact marker/boot"
        );
        let projection = store.snapshot()?;
        ensure!(
            s.accepted_through >= projection.acked && s.accepted_through < projection.next,
            "central cursor outside existing store"
        );
        if self.state.stage == LocalStage::ActivationPending {
            ensure!(
                owner.phase == Phase::Active && owner.revision == 2 && stored.owner.revision <= 2,
                "activation recovery owner"
            );
        } else {
            ensure!(
                Some(&owner) == self.state.owner.as_ref() && owner == stored.owner,
                "status cannot invent missing boot fence"
            );
        }
        self.install_active(owner)
    }
    pub fn advance_boot(
        &mut self,
        peer: &mut impl AuthenticatedOwnerPeer,
        operation_id: Uuid,
        new_boot_id: Uuid,
        observed_at_ms: u64,
    ) -> Result<()> {
        self.files.check()?;
        ensure!(
            new_boot_id == self.host_boot,
            "new boot must equal current host"
        );
        ensure!(
            matches!(
                self.state.stage,
                LocalStage::Active | LocalStage::BootPending
            ),
            "boot stage"
        );
        let store = self.store.as_ref().context("existing store required")?;
        if self.state.stage == LocalStage::Active {
            let owner = self.state.owner.as_ref().context("owner")?;
            ensure!(owner.current_boot_id != new_boot_id, "unchanged boot");
            ensure!(
                store.managed()?.owner == *owner,
                "local/store boot mismatch"
            );
            // Block before network admission. If this DB commit succeeds but the
            // following intent write fails, reopen status can only unblock the
            // old boot; it cannot guess a new remote operation.
            self.files.step(|| store.install_owner(owner, true))?;
            self.reconciled = false;
            let request = Request {
                version: admission::VERSION,
                operation_id,
                node_id: self.state.limits.node_id,
                operation: Operation::AdvanceBoot {
                    marker: owner.marker.clone(),
                    expected_boot_id: owner.current_boot_id,
                    expected_revision: owner.revision,
                    new_boot_id,
                    next_sequence: store.snapshot()?.next,
                },
            };
            self.save(|s| {
                s.boot = Some(request);
                s.stage = LocalStage::BootPending
            })?;
        } else {
            ensure!(self.state.boot.as_ref().is_some_and(|r|r.operation_id==operation_id&&matches!(r.operation,Operation::AdvanceBoot{new_boot_id:b,..}if b==new_boot_id)),"boot immutable operation");
        }
        self.recover_boot(peer, observed_at_ms)
    }
    pub fn recover_boot(
        &mut self,
        peer: &mut impl AuthenticatedOwnerPeer,
        observed_at_ms: u64,
    ) -> Result<()> {
        self.files.check()?;
        ensure!(
            self.state.stage == LocalStage::BootPending,
            "no boot intent"
        );
        let request = self.state.boot.as_ref().context("boot request")?;
        let LiveReply(r) = self.files.step(|| reply(peer, request))?;
        ensure!(
            r.current_owner == r.receipt.resulting_owner
                && r.current_owner.current_boot_id == self.host_boot
                && r.admitted_node_boot_id == self.host_boot,
            "boot owner advanced; recovery required"
        );
        let Operation::AdvanceBoot {
            expected_boot_id,
            new_boot_id,
            next_sequence,
            ..
        } = &request.operation
        else {
            anyhow::bail!("boot operation")
        };
        let store = self.store.as_ref().context("existing store required")?;
        let projection = store.snapshot()?;
        ensure!(
            r.accepted_through >= projection.acked && r.accepted_through < *next_sequence,
            "boot accepted cursor outside retained prefix"
        );
        let plan = aq_observation::BootTransition {
            operation_id: request.operation_id,
            node_id: request.node_id,
            transport_owner_id: r.current_owner.marker.owner_id,
            expected_boot_id: *expected_boot_id,
            new_boot_id: *new_boot_id,
            expected_next_sequence: *next_sequence,
            owner_receipt: receipt_artifact(&r.receipt)?,
        };
        self.files
            .step(|| store.apply_boot(&plan, observed_at_ms, Some(&r.current_owner)))?;
        self.save(|s| {
            s.owner = Some(r.current_owner);
            s.boot = None;
            s.stage = LocalStage::Active
        })?;
        self.reconciled = true;
        Ok(())
    }
    pub fn with_active<T>(&self, f: impl for<'a> FnOnce(&'a Store) -> Result<T>) -> Result<T> {
        self.files.check()?;
        ensure!(
            self.reconciled
                && self.state.stage == LocalStage::Active
                && self
                    .state
                    .owner
                    .as_ref()
                    .is_some_and(|o| o.current_boot_id == self.host_boot),
            "owner inactive/boot recovery required"
        );
        let store = self.store.as_ref().context("store missing")?;
        store.require_collectible()?;
        ensure!(
            Some(&store.managed()?.owner) == self.state.owner.as_ref(),
            "local/store owner mismatch"
        );
        f(store)
    }
}
fn receipt_artifact(receipt: &admission::Receipt) -> Result<ArtifactRef> {
    Ok(ArtifactRef {
        id: Text::new(format!("transport-receipt:{}", receipt.operation_id))?,
        version: Text::new("1")?,
        digest: receipt.digest()?,
    })
}
impl Store {
    pub(super) fn managed_optional(&self) -> Result<Option<Managed>> {
        let tx = self.db.begin_read()?;
        let table = tx.open_table(META)?;
        let meta: Meta = decode(table.get("state")?.context("meta")?.value())?;
        Ok(meta.managed)
    }
    fn managed(&self) -> Result<Managed> {
        self.managed_optional()?.context("managed store required")
    }
    fn install_owner(&self, owner: &Owner, blocked: bool) -> Result<()> {
        owner.validate()?;
        let tx = self.begin_owner()?;
        let mut meta = read_meta(&tx)?;
        let old = meta.managed.as_ref().context("managed store required")?;
        ensure!(
            old.owner.marker == owner.marker
                && owner.revision >= old.owner.revision
                && owner.current_boot_id == meta.active_boot,
            "owner monotonic marker/boot"
        );
        if owner.revision == old.owner.revision {
            ensure!(&old.owner == owner, "equal owner revision changed");
        }
        meta.managed = Some(Managed {
            owner: owner.clone(),
            blocked,
        });
        validate_managed(&meta)?;
        write_meta(&tx, &meta)?;
        self.commit(tx)
    }
}
