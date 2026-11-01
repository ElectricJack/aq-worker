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
}
impl Record {
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
                && self.clock_source == "CLOCK_BOOTTIME"
                && self.received_ns >= self.sent_ns
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
    pub fn receipt(&self) -> Result<MaterialReceipt> {
        self.record.receipt()
    }
    /// An explicit bounded local read, never a renewed retrieval/execution lease.
    pub fn read_private(&self) -> Result<Zeroizing<Vec<u8>>> {
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
fn private_dir(file: &File) -> Result<()> {
    let m = file.metadata()?;
    ensure!(
        m.is_dir() && m.uid() == unsafe { libc::geteuid() } && m.mode() & 0o7777 == 0o700,
        "material directory is not private"
    );
    Ok(())
}
fn directory(parent: &File, name: &CStr) -> Result<File> {
    let fd = unsafe {
        libc::openat(
            parent.as_raw_fd(),
            name.as_ptr(),
            libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
        )
    };
    if fd < 0 {
        return Err(std::io::Error::last_os_error().into());
    }
    Ok(unsafe { File::from_raw_fd(fd) })
}
fn member(dir: &File, name: &CStr, create: bool) -> Result<File> {
    let flags = if create {
        libc::O_WRONLY | libc::O_CREAT | libc::O_EXCL
    } else {
        libc::O_RDONLY
    };
    let fd = unsafe {
        libc::openat(
            dir.as_raw_fd(),
            name.as_ptr(),
            flags | libc::O_NOFOLLOW | libc::O_NONBLOCK | libc::O_CLOEXEC,
            0o600,
        )
    };
    if fd < 0 {
        return Err(std::io::Error::last_os_error().into());
    }
    let file = unsafe { File::from_raw_fd(fd) };
    let m = file.metadata()?;
    ensure!(
        m.is_file()
            && m.nlink() == 1
            && m.uid() == unsafe { libc::geteuid() }
            && m.mode() & 0o7777 == 0o600,
        "material member is not private regular file"
    );
    Ok(file)
}
fn read_member(dir: &File, name: &CStr, bound: usize) -> Result<Zeroizing<Vec<u8>>> {
    let file = member(dir, name, false)?;
    ensure!(
        file.metadata()?.len() <= bound as u64,
        "material member byte bound"
    );
    let mut bytes = Zeroizing::new(Vec::new());
    file.take(bound as u64 + 1).read_to_end(&mut bytes)?;
    ensure!(bytes.len() <= bound, "material member byte bound");
    Ok(bytes)
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
    pub fn install(
        &mut self,
        material: PrivateMaterial,
        preparation: &Preparation,
    ) -> Result<InstalledMaterial> {
        self.install_parts(
            material,
            &preparation.plan().binding,
            &preparation.plan().materials,
            canonical_digest(preparation.plan())?,
            preparation.deadline_ns(),
        )
    }
    pub(crate) fn install_context(
        &mut self,
        material: PrivateMaterial,
        preparation: &super::preparation::InputContext,
    ) -> Result<InstalledMaterial> {
        self.install_parts(
            material,
            preparation.binding(),
            preparation.materials(),
            preparation.digest()?,
            preparation.deadline_ns(),
        )
    }
    fn install_parts(
        &mut self,
        material: PrivateMaterial,
        binding: &aq_session_protocol::LaunchBinding,
        materials: &[aq_admission::MaterialLease],
        origin_digest: Digest,
        deadline_ns: u64,
    ) -> Result<InstalledMaterial> {
        ensure!(
            self.owner_pid == std::process::id(),
            "forked material owner"
        );
        private_dir(&self.root)?;
        let lease = &material.delivery.lease;
        ensure!(
            matches!(lease.binding.session, aq_session_protocol::SessionIdentity::Remote {node_id, boot_id, ..} if node_id == self.identity.node_id && boot_id == self.identity.boot_id)
                && lease.binding == *binding
                && materials.iter().any(|v| same(v, lease).unwrap_or(false)),
            "material preparation scope"
        );
        let computed = material.delivery.verify_received(
            &lease.binding,
            &material.bytes,
            material.sent_ns,
            material.received_ns,
        )?;
        ensure!(
            computed == material.retrieval_deadline_ns && boot_ns()? < computed.min(deadline_ns),
            "material installation expired"
        );
        let name = CString::new(lease.lease_id.to_string())?;
        match directory(&self.root, &name) {
            Ok(dir) => {
                private_dir(&dir)?;
                let raw = read_member(&dir, c"record.json", MAX_RECORD)?;
                let record: Record = serde_json::from_slice(&raw)?;
                record.validate()?;
                ensure!(
                    canonical_bytes(&record)? == *raw
                        && record.operation_id == material.operation_id
                        && record.preparation_digest == origin_digest.clone()
                        && record.worker == self.identity
                        && record.kernel_boot_id == self.kernel_boot_id
                        && same(&record.delivery.lease, lease)?,
                    "material replay changed"
                );
                let result = InstalledMaterial {
                    directory: dir,
                    record,
                };
                ensure!(
                    result.read_private()?.as_slice() == material.bytes.as_slice(),
                    "material replay bytes changed"
                );
                // If publication's ACK was lost, repeat the durability boundary.
                self.root.sync_all()?;
                ensure!(
                    boot_ns()?
                        < result
                            .record
                            .retrieval_deadline_ns
                            .min(computed)
                            .min(deadline_ns),
                    "material replay cannot revive expiry"
                );
                return Ok(result);
            }
            Err(e)
                if e.downcast_ref::<std::io::Error>()
                    .is_some_and(|e| e.kind() == std::io::ErrorKind::NotFound) => {}
            Err(e) => return Err(e),
        }
        ensure!(
            self.count()? < self.max_leases,
            "material capacity exhausted"
        );
        let pending = CString::new(format!("{}.pending", lease.lease_id))?;
        let rc = unsafe { libc::mkdirat(self.root.as_raw_fd(), pending.as_ptr(), 0o700) };
        ensure!(
            rc == 0,
            "material staging exists or unavailable; recovery required"
        );
        self.root.sync_all()?;
        let dir = directory(&self.root, &pending)?;
        private_dir(&dir)?;
        write_member(&dir, c"material.bin", &material.bytes)?;
        let record = Record {
            schema_version: 1,
            operation_id: material.operation_id,
            preparation_digest: origin_digest.clone(),
            worker: self.identity.clone(),
            kernel_boot_id: self.kernel_boot_id,
            clock_source: "CLOCK_BOOTTIME".into(),
            delivery: material.delivery.clone(),
            sent_ns: material.sent_ns,
            received_ns: material.received_ns,
            material_synced_ns: boot_ns()?,
            retrieval_deadline_ns: computed,
        };
        record.validate()?;
        let raw = canonical_bytes(&record)?;
        ensure!(raw.len() <= MAX_RECORD, "material record byte bound");
        write_member(&dir, c"record.json", &raw)?;
        dir.sync_all()?;
        #[cfg(any(test, feature = "test-support"))]
        let fault = self.next_fault.take();
        #[cfg(any(test, feature = "test-support"))]
        match fault {
            Some(InstallFault::AfterFiles) => {
                anyhow::bail!("injected material prepublication loss")
            }
            Some(InstallFault::PauseAfterFiles) => {
                std::thread::sleep(std::time::Duration::from_millis(200))
            }
            _ => {}
        }
        let rc = unsafe {
            libc::renameat2(
                self.root.as_raw_fd(),
                pending.as_ptr(),
                self.root.as_raw_fd(),
                name.as_ptr(),
                libc::RENAME_NOREPLACE,
            )
        };
        ensure!(rc == 0, "material publication uncertain; recovery required");
        self.root.sync_all()?;
        #[cfg(any(test, feature = "test-support"))]
        if matches!(fault, Some(InstallFault::AfterPublish)) {
            anyhow::bail!("injected material publication ACK loss");
        }
        ensure!(
            boot_ns()? < computed.min(deadline_ns),
            "material publication expired"
        );
        Ok(InstalledMaterial {
            directory: dir,
            record,
        })
    }
}

mod observation;

#[cfg(test)]
impl MaterialStore {
    pub(crate) fn test_observation_hook(&mut self, hook: Box<dyn FnOnce() + Send>) {
        *self.observation_hook.get_mut().unwrap() = Some(hook);
    }
}
