//! One bounded actual private read, without treating retrieval history as authority.
use super::*;
use aq_admission::scoped_presence::{self as wire, MissingReason, Presence, UnknownReason};
#[derive(Clone, Copy)]
enum Failure {
    Missing,
    Changed,
    Unreadable,
    BootUnknown,
}
fn failure(value: Failure) -> Presence {
    match value {
        Failure::Missing => Presence::Missing {
            reason: MissingReason::Missing,
        },
        Failure::Changed => Presence::Missing {
            reason: MissingReason::Changed,
        },
        Failure::Unreadable => Presence::Unknown {
            reason: UnknownReason::Unreadable,
        },
        Failure::BootUnknown => Presence::Unknown {
            reason: UnknownReason::BootUnknown,
        },
    }
}
fn io(error: std::io::Error) -> Failure {
    if error.kind() == std::io::ErrorKind::NotFound {
        Failure::Missing
    } else {
        Failure::Unreadable
    }
}
fn open(parent: &File, name: &CStr, dir: bool) -> std::result::Result<File, Failure> {
    let fd = unsafe {
        libc::openat(
            parent.as_raw_fd(),
            name.as_ptr(),
            libc::O_RDONLY
                | libc::O_NOFOLLOW
                | libc::O_NONBLOCK
                | libc::O_CLOEXEC
                | if dir { libc::O_DIRECTORY } else { 0 },
        )
    };
    if fd < 0 {
        return Err(io(std::io::Error::last_os_error()));
    }
    let file = unsafe { File::from_raw_fd(fd) };
    let meta = file.metadata().map_err(io)?;
    if meta.uid() != unsafe { libc::geteuid() }
        || meta.mode() & 0o7777 != if dir { 0o700 } else { 0o600 }
        || if dir {
            !meta.is_dir()
        } else {
            !meta.is_file() || meta.nlink() != 1
        }
    {
        return Err(Failure::Unreadable);
    }
    Ok(file)
}
#[derive(PartialEq, Eq)]
struct Identity {
    dev: u64,
    ino: u64,
    len: u64,
    mtime: i64,
    mtime_ns: i64,
    ctime: i64,
    ctime_ns: i64,
    mode: u32,
    uid: u32,
    links: u64,
}
fn signature(file: &File) -> std::result::Result<Identity, Failure> {
    let m = file.metadata().map_err(io)?;
    Ok(Identity {
        dev: m.dev(),
        ino: m.ino(),
        len: m.len(),
        mtime: m.mtime(),
        mtime_ns: m.mtime_nsec(),
        ctime: m.ctime(),
        ctime_ns: m.ctime_nsec(),
        mode: m.mode(),
        uid: m.uid(),
        links: m.nlink(),
    })
}

fn read(file: &File, max: usize) -> std::result::Result<Zeroizing<Vec<u8>>, Failure> {
    if file.metadata().map_err(io)?.len() > max as u64 {
        return Err(Failure::Changed);
    }
    let mut bytes = Zeroizing::new(Vec::new());
    file.try_clone()
        .map_err(io)?
        .take(max as u64 + 1)
        .read_to_end(&mut bytes)
        .map_err(io)?;
    if bytes.len() > max {
        return Err(Failure::Changed);
    }
    Ok(bytes)
}
impl MaterialStore {
    /// Scope/permit acquisition belongs to the opaque grant owner before this
    /// call. No current retrieval-lease permission is inferred or required here.
    pub(crate) fn observe_scoped(&self, expected: &MaterialReceipt) -> Presence {
        match self.observe_actual(expected) {
            Ok(present) => present,
            Err(error) => failure(error),
        }
    }
    fn observe_actual(&self, expected: &MaterialReceipt) -> std::result::Result<Presence, Failure> {
        if self.owner_pid != std::process::id() {
            return Err(Failure::BootUnknown);
        }
        private_dir(&self.root).map_err(|_| Failure::Unreadable)?;
        let name = CString::new(expected.lease_id.to_string()).map_err(|_| Failure::Unreadable)?;
        let dir = open(&self.root, &name, true)?;
        let directory = signature(&dir)?;
        let record_file = open(&dir, c"record.json", false)?;
        let record_signature = signature(&record_file)?;
        let raw = read(&record_file, MAX_RECORD)?;
        let record: wire::PrivateInstallationRecord =
            serde_json::from_slice(&raw).map_err(|_| Failure::Unreadable)?;
        record.validate().map_err(|_| Failure::Unreadable)?;
        if record.worker != self.identity || record.kernel_boot_id != self.kernel_boot_id {
            return Err(Failure::BootUnknown);
        }
        if canonical_bytes(&record).map_err(|_| Failure::Unreadable)? != *raw
            || !same(
                &record.receipt().map_err(|_| Failure::Unreadable)?,
                expected,
            )
            .map_err(|_| Failure::Unreadable)?
        {
            return Err(Failure::Changed);
        }
        #[cfg(test)]
        if let Some(hook) = self
            .observation_hook
            .lock()
            .map_err(|_| Failure::Unreadable)?
            .take()
        {
            hook();
        }
        let material = open(&dir, c"material.bin", false)?;
        let material_signature = signature(&material)?;
        let bytes = read(&material, MAX_MATERIAL)?;
        let digest = Digest::of(&bytes);
        let size = bytes.len() as u64;
        drop(bytes);
        if digest != record.delivery.lease.material_digest
            || size != record.delivery.lease.material_bytes
        {
            return Err(Failure::Changed);
        }
        // Verify the held descriptors and their current namespace entries after
        // I/O. No symlink/path fallback or stale detached inode becomes Present.
        if signature(&record_file)? != record_signature
            || signature(&material)? != material_signature
            || signature(&open(&dir, c"record.json", false)?)? != record_signature
            || signature(&open(&dir, c"material.bin", false)?)? != material_signature
            || signature(&open(&self.root, &name, true)?)? != directory
        {
            return Err(Failure::Changed);
        }
        private_dir(&self.root).map_err(|_| Failure::Unreadable)?;
        Ok(Presence::Present {
            installation: Box::new(record),
            observed_material_digest: digest,
            observed_material_bytes: size,
        })
    }
}
