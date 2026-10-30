//! Single-owner versioned journal. Every acknowledged mutation has crossed file
//! and directory fsync. Spawn and network actions remain separate transactions.
use anyhow::{Context, Result, bail, ensure};
use aq_worker_protocol::*;
use fs2::FileExt;
use serde::{Deserialize, Serialize};
use std::{
    collections::BTreeMap,
    fs::{File, OpenOptions},
    io::Write,
    os::unix::fs::{DirBuilderExt, OpenOptionsExt, PermissionsExt},
    path::{Path, PathBuf},
};
use uuid::Uuid;

pub const STATE_FORMAT: u32 = 1;
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Limits {
    pub capacity: Resources,
    pub max_pending_results: usize,
    pub max_pending_bytes: u64,
    pub max_preparing: usize,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AttemptRecord {
    pub offer: Offer,
    pub acceptance: AcceptRequest,
    pub acceptance_sent_at_ms: u64,
    pub grant: Option<Grant>,
    pub grant_deadline_ms: Option<u64>,
    pub phase: Phase,
    pub launch_id: Option<Uuid>,
    pub process: Option<ProcessEvidence>,
    pub stop_certainty: StopCertainty,
    pub observation_sequence: u64,
    pub pending_observations: BTreeMap<u64, Observation>,
    pub report: Option<ResultReport>,
    pub result: Option<ResultResponse>,
    pub pending_bytes: u64,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProcessEvidence {
    pub pid: u32,
    pub start_ticks: u64,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct State {
    pub format_version: u32,
    pub identity: NodeIdentity,
    pub inventory_sequence: u64,
    pub attempts: BTreeMap<Uuid, AttemptRecord>,
    pub directive_receipts: BTreeMap<Uuid, DirectiveAck>,
    pub directive_contents: BTreeMap<Uuid, Directive>,
    pub reconciliation_required: bool,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WriteFault {
    BeforeWrite,
    AfterFileSync,
    AfterRename,
}

pub struct Journal {
    root: PathBuf,
    _owner: File,
    state: State,
    limits: Limits,
    next_fault: Option<WriteFault>,
    poisoned: bool,
}
impl Journal {
    pub fn open(root: &Path, identity: NodeIdentity, limits: Limits) -> Result<Self> {
        identity.validate()?;
        limits.capacity.validate()?;
        ensure!(
            limits.max_pending_results > 0
                && limits.max_pending_bytes > 0
                && limits.max_preparing > 0,
            "invalid local limits"
        );
        std::fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(root)?;
        ensure!(
            !std::fs::symlink_metadata(root)?.file_type().is_symlink(),
            "journal root cannot be a symlink"
        );
        std::fs::set_permissions(root, std::fs::Permissions::from_mode(0o700))?;
        let owner = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .mode(0o600)
            .open(root.join("owner.lock"))?;
        owner
            .try_lock_exclusive()
            .context("another runtime owns journal")?;
        let file = root.join("journal.json");
        let state = if file.exists() {
            let s: State = serde_json::from_slice(&std::fs::read(&file)?)
                .context("parse journal; refusing to advertise capacity")?;
            ensure!(
                s.format_version == STATE_FORMAT,
                "unsupported state format {}",
                s.format_version
            );
            ensure!(
                s.identity.node_id == identity.node_id,
                "journal belongs to another node"
            );
            for (id, r) in &s.attempts {
                ensure!(*id == r.offer.attempt.attempt_id, "corrupt attempt key");
                r.offer.validate()?;
                r.acceptance.validate()?;
                ensure!(
                    r.acceptance.attempt == r.offer.attempt
                        && r.acceptance.offer_id == r.offer.offer_id
                        && r.acceptance.resolved_spec_digest == r.offer.spec.resolved_spec_digest,
                    "corrupt acceptance binding"
                );
            }
            s
        } else {
            State {
                format_version: STATE_FORMAT,
                identity: identity.clone(),
                inventory_sequence: 0,
                attempts: BTreeMap::new(),
                directive_receipts: BTreeMap::new(),
                directive_contents: BTreeMap::new(),
                reconciliation_required: false,
            }
        };
        let mut journal = Self {
            root: root.into(),
            _owner: owner,
            state,
            limits,
            next_fault: None,
            poisoned: false,
        };
        let changed_boot = journal.state.identity.boot_id != identity.boot_id;
        journal.transaction(|s| {
            if changed_boot {
                s.identity = identity;
                s.reconciliation_required = s.attempts.values().any(|r| r.phase != Phase::Retired);
            }
            Ok(())
        })?;
        Ok(journal)
    }
    pub fn root(&self) -> &Path {
        &self.root
    }
    pub fn state(&self) -> &State {
        &self.state
    }
    pub fn limits(&self) -> &Limits {
        &self.limits
    }
    pub fn inject_write_fault(&mut self, fault: WriteFault) {
        self.next_fault = Some(fault);
    }
    pub fn transaction<T>(&mut self, f: impl FnOnce(&mut State) -> Result<T>) -> Result<T> {
        ensure!(
            !self.poisoned,
            "journal durability uncertain; restart required"
        );
        let mut candidate = self.state.clone();
        let value = f(&mut candidate)?;
        let fault = self.next_fault.take();
        let tmp = self.root.join(format!(".journal.{}.tmp", Uuid::new_v4()));
        let result = (|| -> Result<()> {
            if fault == Some(WriteFault::BeforeWrite) {
                bail!("injected full disk before write");
            }
            let mut file = OpenOptions::new()
                .write(true)
                .create_new(true)
                .mode(0o600)
                .open(&tmp)?;
            file.write_all(&serde_json::to_vec(&candidate)?)?;
            file.sync_all()?;
            if fault == Some(WriteFault::AfterFileSync) {
                bail!("injected failure after file fsync");
            }
            std::fs::rename(&tmp, self.root.join("journal.json"))?;
            // Any subsequent failure is ambiguous: prohibit action until reopened.
            self.poisoned = true;
            if fault == Some(WriteFault::AfterRename) {
                bail!("injected directory sync failure");
            }
            File::open(&self.root)?.sync_all()?;
            Ok(())
        })();
        let _ = std::fs::remove_file(&tmp);
        result?;
        self.state = candidate;
        self.poisoned = false;
        Ok(value)
    }
    pub fn protected_usage(&self) -> Result<Resources> {
        let mut total = Resources {
            slots: 0,
            cpu_millis: 0,
            memory_bytes: 0,
            disk_bytes: 0,
        };
        for r in self
            .state
            .attempts
            .values()
            .filter(|r| r.phase != Phase::Retired)
        {
            if !matches!(
                r.stop_certainty,
                StopCertainty::ConfirmedStopped | StopCertainty::NotStarted
            ) || matches!(
                r.phase,
                Phase::Reserved | Phase::Authorized | Phase::Preparing
            ) {
                total.slots = total
                    .slots
                    .checked_add(r.offer.spec.resources.slots)
                    .context("slot accounting overflow")?;
                total.cpu_millis = total
                    .cpu_millis
                    .checked_add(r.offer.spec.resources.cpu_millis)
                    .context("CPU accounting overflow")?;
                total.memory_bytes = total
                    .memory_bytes
                    .checked_add(r.offer.spec.resources.memory_bytes)
                    .context("memory accounting overflow")?;
            }
            total.disk_bytes = total
                .disk_bytes
                .checked_add(r.offer.spec.resources.disk_bytes)
                .context("disk accounting overflow")?;
        }
        Ok(total)
    }
    pub fn available(&self) -> Result<Resources> {
        let used = self.protected_usage()?;
        let cap = &self.limits.capacity;
        Ok(Resources {
            slots: cap.slots.saturating_sub(used.slots),
            cpu_millis: cap.cpu_millis.saturating_sub(used.cpu_millis),
            memory_bytes: cap.memory_bytes.saturating_sub(used.memory_bytes),
            disk_bytes: cap.disk_bytes.saturating_sub(used.disk_bytes),
        })
    }
}
