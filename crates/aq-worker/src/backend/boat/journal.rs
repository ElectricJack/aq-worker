//! Intent before side effect: the journal is authoritative because boat names
//! carry only node/attempt, not boot/digest/deadline. No secrets are stored here.
use super::*;
use fs2::FileExt;
use serde::{Deserialize, Serialize};
use std::{
    collections::BTreeMap,
    fs::{File, OpenOptions},
    io::{Read, Write},
    os::unix::fs::OpenOptionsExt,
    path::PathBuf,
};
#[derive(Clone, Serialize, Deserialize)]
pub(super) struct Record {
    pub request: CreateRequest,
    pub backend_id: Option<String>,
    pub container: bool,
    pub material: bool,
    pub start_intent: bool,
    pub destroyed: bool,
    #[serde(default)]
    pub deletion: Option<String>,
}
#[derive(Clone, Serialize, Deserialize)]
pub(super) struct OrphanDeletion {
    pub entry: InventoryEntry,
    pub operation: Option<String>,
    pub destroyed: bool,
}
#[derive(Serialize, Deserialize)]
struct Disk {
    version: u32,
    node: Uuid,
    org: String,
    records: BTreeMap<String, Record>,
    #[serde(default)]
    orphans: BTreeMap<String, OrphanDeletion>,
    #[serde(default)]
    vm_time: BTreeMap<Uuid, super::vm_time::Report>,
}
pub(super) struct Journal {
    path: PathBuf,
    _lock: File,
    disk: Disk,
    poisoned: bool,
}
impl Journal {
    pub fn open(c: &BoatConfig) -> Result<Self, BackendError> {
        let path = &c.journal_path;
        let lock = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW)
            .open(path.with_extension("lock"))
            .map_err(|_| unknown())?;
        lock.try_lock_exclusive()
            .map_err(|_| BackendError::Conflict)?;
        let disk = match OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_NOFOLLOW)
            .open(path)
        {
            Ok(file) => {
                let mut bytes = Vec::new();
                file.take(16 * 1024 * 1024 + 1)
                    .read_to_end(&mut bytes)
                    .map_err(|_| unknown())?;
                if bytes.len() > 16 * 1024 * 1024 {
                    return Err(unknown());
                }
                serde_json::from_slice::<Disk>(&bytes).map_err(|_| unknown())?
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Disk {
                version: 1,
                node: c.node_id,
                org: c.attempt_org.clone(),
                records: BTreeMap::new(),
                orphans: BTreeMap::new(),
                vm_time: BTreeMap::new(),
            },
            Err(_) => return Err(unknown()),
        };
        if disk.version != 1 || disk.node != c.node_id || disk.org != c.attempt_org {
            return Err(BackendError::Conflict);
        }
        if disk.records.len() + disk.orphans.len() > 4096 {
            return Err(unknown());
        }
        if disk.vm_time.len() > 4096 {
            return Err(unknown());
        }
        let mut sandboxes = std::collections::BTreeSet::new();
        for (id, report) in &disk.vm_time {
            report.fact.validate()?;
            if *id != report.fact.attempt_id || !sandboxes.insert(&report.fact.sandbox_id) {
                return Err(unknown());
            }
        }
        let mut ids = std::collections::BTreeSet::new();
        for (name, r) in &disk.records {
            if r.backend_id.as_ref().is_some_and(|id| !ids.insert(id)) {
                return Err(unknown());
            }
            r.request.validate()?;
            if r.deletion
                .as_ref()
                .is_some_and(|id| !super::config::safe_id(id))
            {
                return Err(unknown());
            }
            if name != &r.request.name
                || r.request.owner.node_id != c.node_id
                || r.backend_id
                    .as_ref()
                    .is_some_and(|id| !super::config::safe_id(id))
            {
                return Err(unknown());
            }
        }
        for (id, o) in &disk.orphans {
            if id != &o.entry.sandbox.backend_id
                || !super::config::safe_id(id)
                || o.entry.sandbox.kind != BackendKind::Boat
                || o.entry.sandbox.owner.node_id != c.node_id
                || !o.entry.sandbox.owner.boot_id.is_nil()
                || super::policy::attempt(&o.entry.name, c.node_id)
                    != Some(o.entry.sandbox.owner.attempt_id)
                || o.operation
                    .as_ref()
                    .is_some_and(|id| !super::config::safe_id(id))
                || !ids.insert(id)
            {
                return Err(unknown());
            }
        }
        Ok(Self {
            path: path.clone(),
            _lock: lock,
            disk,
            poisoned: false,
        })
    }
    pub fn check(&self) -> Result<(), BackendError> {
        if self.poisoned {
            Err(unknown())
        } else {
            Ok(())
        }
    }
    pub fn fact(&self, id: Uuid) -> Option<&super::vm_time::Report> {
        self.disk.vm_time.get(&id)
    }
    pub fn pending_facts(&self) -> Vec<VmTimeFact> {
        self.disk
            .vm_time
            .values()
            .filter(|r| r.pending)
            .map(|r| r.fact.clone())
            .collect()
    }
    pub fn set_fact(&mut self, fact: VmTimeFact) -> Result<(), BackendError> {
        self.check()?;
        fact.validate()?;
        if let Some(old) = self.fact(fact.attempt_id) {
            if !fact.succeeds(&old.fact) {
                return Err(BackendError::Conflict);
            }
            if fact == old.fact {
                return Ok(());
            }
        } else if self.disk.vm_time.len() >= 4096 {
            return Err(BackendError::InvalidRequest("VM-time journal full"));
        }
        if self
            .disk
            .vm_time
            .values()
            .any(|r| r.fact.sandbox_id == fact.sandbox_id && r.fact.attempt_id != fact.attempt_id)
        {
            return Err(BackendError::Conflict);
        }
        self.poisoned = true;
        self.disk.vm_time.insert(
            fact.attempt_id,
            super::vm_time::Report {
                fact,
                pending: true,
            },
        );
        self.persist()
    }
    pub fn ack_fact(&mut self, id: Uuid) -> Result<(), BackendError> {
        self.check()?;
        self.poisoned = true;
        self.disk.vm_time.get_mut(&id).ok_or_else(unknown)?.pending = false;
        self.persist()
    }
    pub fn get(&self, name: &str) -> Option<&Record> {
        self.disk.records.get(name)
    }
    pub fn by_id(&self, id: &str) -> Option<&Record> {
        self.disk
            .records
            .values()
            .find(|r| r.backend_id.as_deref() == Some(id))
    }
    pub fn set(&mut self, name: String, r: Record) -> Result<(), BackendError> {
        self.check()?;
        if !self.disk.records.contains_key(&name)
            && self.disk.records.len() + self.disk.orphans.len() >= 4096
        {
            return Err(BackendError::InvalidRequest("boat journal full"));
        }
        if r.backend_id.as_ref().is_some_and(|id| {
            self.disk
                .records
                .iter()
                .any(|(other, record)| other != &name && record.backend_id.as_ref() == Some(id))
        }) {
            return Err(BackendError::Conflict);
        }
        self.poisoned = true;
        self.disk.records.insert(name, r);
        self.persist()
    }
    pub fn orphan(&self, id: &str) -> Option<&OrphanDeletion> {
        self.disk.orphans.get(id)
    }
    pub fn set_orphan(&mut self, o: OrphanDeletion) -> Result<(), BackendError> {
        self.check()?;
        let id = o.entry.sandbox.backend_id.clone();
        if !self.disk.orphans.contains_key(&id)
            && self.disk.records.len() + self.disk.orphans.len() >= 4096
        {
            return Err(BackendError::InvalidRequest("boat journal full"));
        }
        self.poisoned = true;
        self.disk.orphans.insert(id, o);
        self.persist()
    }
    fn persist(&mut self) -> Result<(), BackendError> {
        let parent = self.path.parent().ok_or_else(unknown)?;
        let mut tmp = tempfile::NamedTempFile::new_in(parent).map_err(|_| unknown())?;
        tmp.write_all(&serde_json::to_vec(&self.disk).map_err(|_| unknown())?)
            .map_err(|_| unknown())?;
        tmp.as_file().sync_all().map_err(|_| unknown())?;
        tmp.persist(&self.path).map_err(|_| unknown())?;
        File::open(parent)
            .and_then(|f| f.sync_all())
            .map_err(|_| unknown())?;
        self.poisoned = false;
        Ok(())
    }
}
