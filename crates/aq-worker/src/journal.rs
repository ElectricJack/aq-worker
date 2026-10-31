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

pub const STATE_FORMAT: u32 = 4;
pub const MAX_JOURNAL_BYTES: u64 = 32 * 1024 * 1024;
pub const MAX_DIRECTIVES_PER_ATTEMPT: usize = 64;
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Limits {
    pub capacity: Resources,
    pub max_pending_results: usize,
    pub max_pending_bytes: u64,
    pub max_preparing: usize,
    pub minimum_free_disk_bytes: u64,
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
    pub stop_reason: Option<ProcessOutcome>,
    #[serde(default)]
    pub ownership_profile: Option<String>,
    #[serde(default)]
    pub guardian: Option<crate::guardian::GuardianIdentity>,
    #[serde(default)]
    pub start_intent: bool,
    #[serde(default)]
    pub stopped_proof: Option<crate::guardian::GuardianState>,
    #[serde(default)]
    pub unstarted_decline: Option<DeclineRequest>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source: Option<crate::source::SourceState>,
}
pub use aq_session_host::ownership::ProcessEvidence;
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
    #[serde(default)]
    pub local_drain: bool,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WriteFault {
    BeforeWrite,
    AfterFileSync,
    AfterRename,
}

// flock is associated with the open file description, which raw fork children
// inherit even with CLOEXEC. Closing the parent's last descriptor therefore
// does not necessarily release it. An orderly owner release must explicitly
// unlock; an inherited copy must never unlock the still-live parent's guard.
struct OwnerLock {
    file: File,
    owner_pid: u32,
}
impl OwnerLock {
    fn acquire(file: File) -> Result<Self> {
        file.try_lock_exclusive()
            .context("another runtime owns journal")?;
        Ok(Self {
            file,
            owner_pid: std::process::id(),
        })
    }
}
impl Drop for OwnerLock {
    fn drop(&mut self) {
        if self.owner_pid == std::process::id() {
            // Drop cannot return an error. Closing the descriptor still follows;
            // crash/abort paths that skip Drop retain kernel last-FD semantics.
            let _ = FileExt::unlock(&self.file);
        }
    }
}

pub struct Journal {
    root: PathBuf,
    _owner: OwnerLock,
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
        let owner = OwnerLock::acquire(owner)?;
        // Rust opens Files CLOEXEC; verify rather than relying on an implicit
        // inheritance assumption for the guardian's exec boundary.
        use std::os::fd::AsRawFd;
        ensure!(
            unsafe { libc::fcntl(owner.file.as_raw_fd(), libc::F_GETFD) } & libc::FD_CLOEXEC != 0,
            "journal lock must not survive exec"
        );
        let parent = root
            .parent()
            .filter(|p| !p.as_os_str().is_empty())
            .unwrap_or_else(|| Path::new("."));
        File::open(parent)?.sync_all()?;
        let file = root.join("journal.json");
        let state = if file.exists() {
            ensure!(
                std::fs::metadata(&file)?.len() <= MAX_JOURNAL_BYTES,
                "journal exceeds bounded state size"
            );
            let mut s: State = serde_json::from_slice(&std::fs::read(&file)?)
                .context("parse journal; refusing to advertise capacity")?;
            ensure!(
                matches!(s.format_version, 1 | 2 | 3 | STATE_FORMAT),
                "unsupported state format {}",
                s.format_version
            );
            // Version 1 has no guardian handshake. Missing new fields never
            // establish that a legacy launch was unstarted or stopped.
            s.format_version = STATE_FORMAT;
            ensure!(
                s.identity.node_id == identity.node_id,
                "journal belongs to another node"
            );
            for (id, r) in &s.attempts {
                ensure!(*id == r.offer.attempt.attempt_id, "corrupt attempt key");
                r.offer.validate()?;
                r.acceptance.validate()?;
                if let Some(source) = &r.source {
                    if source.report_sealed {
                        let report = r.report.as_ref().context("sealed report missing")?;
                        report.validate()?;
                        let transfer = source
                            .transfer
                            .as_ref()
                            .context("sealed transfer missing")?;
                        let receipt = source.receipt.as_ref().context("sealed receipt missing")?;
                        ensure!(
                            source.upload.is_some()
                                && report.identity == r.offer.identity
                                && report.attempt == r.offer.attempt
                                && report.resolved_spec_digest == r.offer.spec.resolved_spec_digest
                                && report.supersedes_report_id.is_none()
                                && report.artifacts.len() == 1
                                && report.artifacts[0].artifact_id == transfer.artifact_id
                                && report.artifacts[0].digest == transfer.manifest_digest
                                && Some(report.artifacts[0].bytes)
                                    == transfer.manifest_bytes.checked_add(transfer.bundle_bytes)
                                && report.artifacts[0].central_receipt.as_deref()
                                    == Some(receipt.receipt_id.to_string().as_str()),
                            "corrupt sealed source report"
                        );
                    }
                    if let Some(download) = &source.download {
                        download.validate(&r.offer, &source.binding)?;
                    }
                    if let Some(upload) = &source.upload {
                        upload.validate(
                            source
                                .transfer
                                .as_ref()
                                .context("upload manifest missing")?,
                            s.identity.node_id,
                        )?;
                    }
                    ensure!(
                        r.offer
                            .spec
                            .bindings
                            .as_ref()
                            .and_then(|b| b.source.as_ref())
                            == Some(&source.binding),
                        "corrupt frozen source binding"
                    );
                    ensure!(
                        source.operations.len() <= 64
                            && source.managed_root.as_ref().is_none_or(|p| p.is_absolute()),
                        "corrupt source runtime state"
                    );
                    for op in source.operations.values() {
                        ensure!(
                            !op.operation_id.is_nil()
                                && op.request["operation_id"] == op.operation_id.to_string()
                                && op.request["repository_id"]
                                    == source.binding.repository_id.to_string(),
                            "corrupt source operation binding"
                        );
                    }
                    if let Some(receipt) = &source.receipt {
                        receipt.validate()?;
                        ensure!(
                            source
                                .transfer
                                .as_ref()
                                .is_some_and(|t| t.transfer_id == receipt.transfer_id)
                                && source
                                    .checkpoint
                                    .as_ref()
                                    .is_some_and(|c| c.source == receipt.source)
                                && receipt.repository_id == source.binding.repository_id
                                && receipt.binding_generation == source.binding.binding_generation,
                            "corrupt source retention association"
                        );
                    }
                }
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
                local_drain: false,
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
        let bytes = serde_json::to_vec(&candidate)?;
        ensure!(
            bytes.len() as u64 <= MAX_JOURNAL_BYTES,
            "journal retained-state byte bound reached"
        );
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
            file.write_all(&bytes)?;
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
        let mut pools = BTreeMap::new();
        for r in self.state.attempts.values().filter(|r| {
            r.phase != Phase::Retired
                || r.source
                    .as_ref()
                    .is_some_and(|s| !s.cache_evicted || r.pending_bytes > 0)
        }) {
            if !matches!(
                r.stop_certainty,
                StopCertainty::ConfirmedStopped | StopCertainty::NotStarted
            ) || r
                .source
                .as_ref()
                .is_some_and(|s| s.operations.values().any(|o| o.result.is_none()))
                || matches!(
                    r.phase,
                    Phase::Reserved | Phase::Authorized | Phase::Preparing
                )
            {
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
                .checked_add(if r.phase == Phase::Retired {
                    r.pending_bytes
                } else {
                    r.offer.spec.resources.disk_bytes.max(
                        r.pending_bytes
                            .saturating_add(r.source.as_ref().map_or(0, |s| s.workspace_bytes)),
                    )
                })
                .context("disk accounting overflow")?;
            if let Some(source) = &r.source
                && !source.cache_evicted
            {
                let key = (source.managed_root.clone(), source.binding.repository_id);
                let bytes = pools.entry(key).or_insert(0u64);
                *bytes = (*bytes).max(source.pool_reservation_bytes);
            }
        }
        for bytes in pools.values() {
            total.disk_bytes = total
                .disk_bytes
                .checked_add(*bytes)
                .context("source pool accounting overflow")?;
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

#[cfg(test)]
mod owner_lock_tests {
    use super::*;

    #[test]
    fn forked_guard_drop_cannot_unlock_the_original_owner() {
        let temp = tempfile::TempDir::new().unwrap();
        let path = temp.path().join("owner.lock");
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create_new(true)
            .open(&path)
            .unwrap();
        let owner = OwnerLock::acquire(file).unwrap();
        let pid = unsafe { libc::fork() };
        assert!(pid >= 0, "fork: {}", std::io::Error::last_os_error());
        if pid == 0 {
            // OwnerLock contains only a File and PID. In this forked copy its
            // Drop invokes getpid and File's close; no unlock or allocation.
            drop(owner);
            unsafe { libc::_exit(0) }
        }
        let mut status = 0;
        loop {
            let waited = unsafe { libc::waitpid(pid, &mut status, 0) };
            if waited == pid {
                break;
            }
            assert_eq!(
                std::io::Error::last_os_error().raw_os_error(),
                Some(libc::EINTR)
            );
        }
        assert!(libc::WIFEXITED(status) && libc::WEXITSTATUS(status) == 0);
        let contender = OpenOptions::new()
            .read(true)
            .write(true)
            .open(&path)
            .unwrap();
        assert!(
            contender.try_lock_exclusive().is_err(),
            "child must not release parent's lock"
        );
        drop(owner);
        contender.try_lock_exclusive().unwrap();
    }
}
