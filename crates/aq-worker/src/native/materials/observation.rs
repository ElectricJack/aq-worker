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
    let flags = libc::O_RDONLY | libc::O_NONBLOCK | if dir { libc::O_DIRECTORY } else { 0 };
    let file = open_relative(parent, name, flags).map_err(io)?;
    let meta = file.metadata().map_err(io)?;
    if !private_entry(&meta, dir) {
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
    bounded_bytes(file.try_clone().map_err(io)?, max)
        .map_err(io)?
        .ok_or(Failure::Changed)
}
struct ObservedFile {
    file: File,
    initial: Identity,
}
impl ObservedFile {
    fn capture(dir: &File, name: &CStr) -> std::result::Result<Self, Failure> {
        let file = open(dir, name, false)?;
        let initial = signature(&file)?;
        Ok(Self { file, initial })
    }
    fn check(&self, current: &File) -> std::result::Result<(), Failure> {
        if signature(current)? != self.initial {
            return Err(Failure::Changed);
        }
        Ok(())
    }
    fn check_name(&self, dir: &File, name: &CStr) -> std::result::Result<(), Failure> {
        self.check(&open(dir, name, false)?)
    }
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
        let record_file = ObservedFile::capture(&dir, c"record.json")?;
        let record = self.read_record(&record_file.file, expected)?;
        #[cfg(test)]
        if let Some(hook) = self
            .observation_hook
            .lock()
            .map_err(|_| Failure::Unreadable)?
            .take()
        {
            hook();
        }
        let material = ObservedFile::capture(&dir, c"material.bin")?;
        let (digest, size) = observed_material(&material.file, &record)?;
        // Verify both held files, then the namespace entries, in the original order.
        record_file.check(&record_file.file)?;
        material.check(&material.file)?;
        record_file.check_name(&dir, c"record.json")?;
        material.check_name(&dir, c"material.bin")?;
        if signature(&open(&self.root, &name, true)?)? != directory {
            return Err(Failure::Changed);
        }
        private_dir(&self.root).map_err(|_| Failure::Unreadable)?;
        Ok(Presence::Present {
            installation: Box::new(record),
            observed_material_digest: digest,
            observed_material_bytes: size,
        })
    }
    fn read_record(
        &self,
        file: &File,
        expected: &MaterialReceipt,
    ) -> std::result::Result<wire::PrivateInstallationRecord, Failure> {
        let raw = read(file, MAX_RECORD)?;
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
        Ok(record)
    }
}
fn observed_material(
    file: &File,
    record: &wire::PrivateInstallationRecord,
) -> std::result::Result<(Digest, u64), Failure> {
    let bytes = read(file, MAX_MATERIAL)?;
    let digest = Digest::of(&bytes);
    let size = bytes.len() as u64;
    drop(bytes);
    if digest != record.delivery.lease.material_digest
        || size != record.delivery.lease.material_bytes
    {
        return Err(Failure::Changed);
    }
    Ok((digest, size))
}
