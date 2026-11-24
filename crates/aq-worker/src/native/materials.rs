//! Bounded descriptor-relative installation. Local files are not execution grants.
//! Blocking fsync work belongs on the runtime's bounded blocking lane.
use super::http::{PrivateMaterial, boot_ns, control::Preparation};
use anyhow::{Result, ensure};
use aq_admission::{
    MaterialReceipt,
    transport::{MaterialDelivery, WorkerIdentity},
};
use aq_session_protocol::{ArtifactRef, Digest, Text};
use fs2::FileExt;
use serde::{Deserialize, Serialize};
use std::{
    ffi::{CStr, CString},
    fs::File,
    io::{Read, Write},
    os::{
        fd::{AsRawFd, FromRawFd},
        unix::fs::MetadataExt,
    },
    path::{Component, Path},
};
use uuid::Uuid;
use zeroize::Zeroizing;

const MAX_RECORD: usize = 16_384;
const MAX_MATERIAL: usize = 65_536;
fn canonical_bytes(value: &impl Serialize) -> Result<Vec<u8>> {
    let mut value = serde_json::to_value(value)?;
    value.sort_all_objects();
    Ok(serde_json::to_vec(&value)?)
}
fn canonical_digest(value: &impl Serialize) -> Result<Digest> {
    Ok(Digest::of(&canonical_bytes(value)?))
}
#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Record {
    pub(crate) schema_version: u32,
    pub(crate) operation_id: Uuid,
    pub(crate) preparation_digest: Digest,
    pub(crate) worker: WorkerIdentity,
    pub(crate) kernel_boot_id: Uuid,
    pub(crate) clock_source: String,
    pub(crate) delivery: MaterialDelivery,
    pub(crate) sent_ns: u64,
    pub(crate) received_ns: u64,
    pub(crate) material_synced_ns: u64,
    pub(crate) retrieval_deadline_ns: u64,
    /// C5b: the stored version of a leased subscription login, for its write-back.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) login_version: Option<i64>,
}
impl Record {
    fn validate_clock(&self) -> Result<()> {
        ensure!(
            self.received_ns >= self.sent_ns
                && self.material_synced_ns >= self.received_ns
                && self.material_synced_ns < self.retrieval_deadline_ns
                && self.delivery.clock.expires_at_ms == self.delivery.lease.expires_at_ms
                && self
                    .delivery
                    .clock
                    .client_deadline_ns(self.sent_ns, self.received_ns)?
                    == self.retrieval_deadline_ns,
            "invalid private material record"
        );
        Ok(())
    }
    pub(crate) fn receipt(&self) -> Result<MaterialReceipt> {
        Ok(MaterialReceipt {
            schema_version: 1,
            operation_id: self.operation_id,
            lease_id: self.delivery.lease.lease_id,
            binding: self.delivery.lease.binding.clone(),
            material_digest: self.delivery.lease.material_digest.clone(),
            private_binding: ArtifactRef {
                id: Text::new(format!(
                    "aq.private-material/{}",
                    self.delivery.lease.lease_id
                ))?,
                version: Text::new("1")?,
                digest: canonical_digest(self)?,
            },
            // Server-time provenance watermark, not an assumed synced local clock.
            received_at_ms: self.delivery.clock.server_time_ms,
        })
    }
    pub(crate) fn validate(&self) -> Result<()> {
        self.worker.validate()?;
        self.delivery.lease.validate()?;
        self.delivery.clock.validate()?;
        ensure!(
            self.schema_version == 1
                && !self.operation_id.is_nil()
                && !self.kernel_boot_id.is_nil()
                && self.clock_source == "CLOCK_BOOTTIME",
            "invalid private material record"
        );
        self.validate_clock()?;
        self.receipt()?
            .validate_against(&self.delivery.lease, self.delivery.clock.server_time_ms)?;
        ensure!(
            matches!(self.delivery.lease.binding.session, aq_session_protocol::SessionIdentity::Remote {node_id, boot_id, ..} if node_id == self.worker.node_id && boot_id == self.worker.boot_id),
            "private material boot scope"
        );
        Ok(())
    }
}
pub struct InstalledMaterial {
    directory: File,
    record: Record,
}
impl InstalledMaterial {
    pub(crate) fn metadata(&self) -> &Record {
        &self.record
    }
    pub(crate) fn source_directory(&self) -> Result<File> {
        Ok(self.directory.try_clone()?)
    }
    /// Another descriptor owner over the original installation, not another
    /// installation, journal record, resource claim or retrieval lifetime.
    pub(crate) fn retained_copy(&self) -> Result<Self> {
        drop(self.read_private()?);
        Ok(Self {
            directory: self.directory.try_clone()?,
            record: self.record.clone(),
        })
    }
    pub fn receipt(&self) -> Result<MaterialReceipt> {
        self.record.receipt()
    }
    /// An explicit bounded local read, never a renewed retrieval/execution lease.
    pub fn read_private(&self) -> Result<Zeroizing<Vec<u8>>> {
        private_dir(&self.directory)?;
        let record = read_member(&self.directory, c"record.json", MAX_RECORD)?;
        ensure!(
            record.as_slice() == canonical_bytes(&self.record)?.as_slice(),
            "installed material original record changed"
        );
        let bytes = read_member(&self.directory, c"material.bin", MAX_MATERIAL)?;
        ensure!(
            bytes.len() as u64 == self.record.delivery.lease.material_bytes
                && Digest::of(&bytes) == self.record.delivery.lease.material_digest,
            "installed material bytes changed"
        );
        Ok(bytes)
    }
}
pub struct MaterialStore {
    root: File,
    identity: WorkerIdentity,
    kernel_boot_id: Uuid,
    owner_pid: u32,
    max_leases: usize,
    #[cfg(any(test, feature = "test-support"))]
    next_fault: Option<InstallFault>,
    #[cfg(test)]
    observation_hook: std::sync::Mutex<Option<Box<dyn FnOnce() + Send>>>,
}
#[cfg(any(test, feature = "test-support"))]
#[derive(Clone, Copy)]
pub enum InstallFault {
    AfterFiles,
    AfterPublish,
    PauseAfterFiles,
}
impl Drop for MaterialStore {
    fn drop(&mut self) {
        if self.owner_pid == std::process::id() {
            let _ = FileExt::unlock(&self.root);
        }
    }
}
fn private_entry(meta: &std::fs::Metadata, directory: bool) -> bool {
    if meta.uid() != unsafe { libc::geteuid() } {
        return false;
    }
    let mode = meta.mode() & 0o7777;
    if directory {
        meta.is_dir() && mode == 0o700
    } else {
        meta.is_file() && meta.nlink() == 1 && mode == 0o600
    }
}
fn private_dir(file: &File) -> Result<()> {
    let m = file.metadata()?;
    ensure!(private_entry(&m, true), "material directory is not private");
    Ok(())
}
fn open_relative(parent: &File, name: &CStr, flags: i32) -> std::io::Result<File> {
    let fd = unsafe {
        libc::openat(
            parent.as_raw_fd(),
            name.as_ptr(),
            flags | libc::O_NOFOLLOW | libc::O_CLOEXEC,
            0o600,
        )
    };
    if fd < 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(unsafe { File::from_raw_fd(fd) })
}
fn directory(parent: &File, name: &CStr) -> Result<File> {
    Ok(open_relative(
        parent,
        name,
        libc::O_RDONLY | libc::O_DIRECTORY,
    )?)
}
fn member(dir: &File, name: &CStr, create: bool) -> Result<File> {
    let flags = if create {
        libc::O_WRONLY | libc::O_CREAT | libc::O_EXCL
    } else {
        libc::O_RDONLY
    };
    let file = open_relative(dir, name, flags | libc::O_NONBLOCK)?;
    let m = file.metadata()?;
    ensure!(
        private_entry(&m, false),
        "material member is not private regular file"
    );
    Ok(file)
}
fn read_member(dir: &File, name: &CStr, bound: usize) -> Result<Zeroizing<Vec<u8>>> {
    let file = member(dir, name, false)?;
    bounded_read(file, bound)?.ok_or_else(|| anyhow::anyhow!("material member byte bound"))
}
fn bounded_read(file: File, bound: usize) -> std::io::Result<Option<Zeroizing<Vec<u8>>>> {
    if file.metadata()?.len() > bound as u64 {
        return Ok(None);
    }
    bounded_bytes(file, bound)
}
fn bounded_bytes(file: File, bound: usize) -> std::io::Result<Option<Zeroizing<Vec<u8>>>> {
    let mut bytes = Zeroizing::new(Vec::new());
    file.take(bound as u64 + 1).read_to_end(&mut bytes)?;
    Ok((bytes.len() <= bound).then_some(bytes))
}
fn write_member(dir: &File, name: &CStr, bytes: &[u8]) -> Result<()> {
    let mut file = member(dir, name, true)?;
    file.write_all(bytes)?;
    file.sync_all()?;
    Ok(())
}
fn same(a: &impl Serialize, b: &impl Serialize) -> Result<bool> {
    Ok(serde_json::to_value(a)? == serde_json::to_value(b)?)
}
impl MaterialStore {
    pub fn open(root: &Path, identity: WorkerIdentity, max_leases: usize) -> Result<Self> {
        identity.validate()?;
        ensure!(
            root.is_absolute() && (1..=1024).contains(&max_leases),
            "material root/quota"
        );
        let mut dir = File::open("/")?;
        for component in root.components() {
            match component {
                Component::RootDir => {}
                Component::Normal(name) => {
                    use std::os::unix::ffi::OsStrExt;
                    dir = directory(&dir, &CString::new(name.as_bytes())?)?;
                }
                _ => anyhow::bail!("material root component"),
            }
        }
        private_dir(&dir)?;
        dir.try_lock_exclusive()
            .map_err(|_| anyhow::anyhow!("material root already owned"))?;
        let kernel_boot_id =
            Uuid::parse_str(std::fs::read_to_string("/proc/sys/kernel/random/boot_id")?.trim())?;
        ensure!(!kernel_boot_id.is_nil(), "kernel boot unavailable");
        let store = Self {
            root: dir,
            identity,
            kernel_boot_id,
            owner_pid: std::process::id(),
            max_leases,
            #[cfg(any(test, feature = "test-support"))]
            next_fault: None,
            #[cfg(test)]
            observation_hook: std::sync::Mutex::new(None),
        };
        ensure!(store.count()? <= max_leases, "material root exceeds quota");
        Ok(store)
    }
    // procfs resolves only our retained descriptor, never a caller path. Entry
    // names are bounded and checked; no recursive traversal or pruning occurs.
    fn count(&self) -> Result<usize> {
        let mut count = 0;
        for entry in std::fs::read_dir(format!("/proc/self/fd/{}", self.root.as_raw_fd()))? {
            let name = entry?.file_name();
            let name = name
                .to_str()
                .ok_or_else(|| anyhow::anyhow!("unknown material root member"))?;
            let key = name.strip_suffix(".pending").unwrap_or(name);
            let id = Uuid::parse_str(key)?;
            ensure!(
                !id.is_nil() && id.to_string() == key,
                "unknown material root member"
            );
            count += 1;
            ensure!(count <= self.max_leases, "material root exceeds quota");
        }
        Ok(count)
    }
    /// Historical filesystem evidence only. The expected pin must come from
    /// the independent durable journal, never a self-consistent local record.
    /// This does not refresh retrieval or reconstruct execution authority.
    pub fn reopen_pinned(&self, expected: &MaterialReceipt) -> Result<InstalledMaterial> {
        ensure!(
            self.owner_pid == std::process::id(),
            "forked material owner"
        );
        private_dir(&self.root)?;
        ensure!(!expected.lease_id.is_nil(), "material pin lease identity");
        let name = CString::new(expected.lease_id.to_string())?;
        let dir = directory(&self.root, &name)?;
        private_dir(&dir)?;
        let raw = read_member(&dir, c"record.json", MAX_RECORD)?;
        let record: Record = serde_json::from_slice(&raw)?;
        record.validate()?;
        ensure!(
            canonical_bytes(&record)? == *raw
                && record.worker == self.identity
                && same(&record.receipt()?, expected)?,
            "material independent pin differs"
        );
        let installed = InstalledMaterial {
            directory: dir,
            record,
        };
        installed.read_private()?;
        Ok(installed)
    }
    #[cfg(any(test, feature = "test-support"))]
    pub fn inject_fault(&mut self, fault: InstallFault) {
        self.next_fault = Some(fault);
    }
}

mod installation;
mod observation;

#[cfg(test)]
impl MaterialStore {
    pub(crate) fn test_observation_hook(&mut self, hook: Box<dyn FnOnce() + Send>) {
        *self.observation_hook.get_mut().unwrap() = Some(hook);
    }
}

#[cfg(test)]
mod scoped_transport_tests;
