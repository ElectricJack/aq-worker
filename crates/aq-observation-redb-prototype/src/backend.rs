//! Private positional IO with a physical file cap and deterministic instrumentation.
use anyhow::{Result, ensure};
use redb::{BackendError, StorageBackend};
use serde::{Deserialize, Serialize};
use std::{
    fs::File,
    io,
    ops::Bound,
    os::{
        fd::{AsRawFd, FromRawFd},
        unix::fs::{FileExt, MetadataExt, OpenOptionsExt},
    },
    path::Path,
    sync::{Arc, Mutex},
};
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct IoStats {
    pub read_bytes: u64,
    pub written_bytes: u64,
    pub syncs: u64,
    pub resizes: u64,
    pub operations: u64,
    pub peak_file_bytes: u64,
}
#[derive(Clone, Copy, Debug)]
pub enum Action {
    Error,
    Exit,
}
#[derive(Clone, Copy, Debug)]
pub struct Fault {
    pub operation: u64,
    pub after: bool,
    pub action: Action,
}
#[derive(Debug, Default)]
struct Control {
    stats: IoStats,
    fault: Option<Fault>,
}
#[derive(Clone, Debug, Default)]
pub struct Monitor(Arc<Mutex<Control>>);
impl Monitor {
    pub fn reset(&self) {
        let mut c = self.0.lock().unwrap();
        c.stats = IoStats::default();
        c.fault = None;
    }
    pub fn fault(&self, fault: Fault) {
        self.0.lock().unwrap().fault = Some(fault);
    }
    pub fn stats(&self) -> IoStats {
        self.0.lock().unwrap().stats.clone()
    }
    fn boundary(&self, after: bool) -> io::Result<()> {
        let mut c = self.0.lock().unwrap();
        if !after {
            c.stats.operations += 1;
        }
        if let Some(f) = c.fault
            && f.operation == c.stats.operations
            && f.after == after
        {
            match f.action {
                Action::Error => return Err(io::Error::other("prototype injected IO fault")),
                Action::Exit => std::process::exit(77),
            }
        }
        Ok(())
    }
}
#[derive(Debug)]
pub struct CappedFile {
    file: File,
    limit: u64,
    monitor: Monitor,
}
impl CappedFile {
    pub fn open(directory: &File, create: bool, limit: u64, monitor: Monitor) -> Result<Self> {
        let directory_meta = directory.metadata()?;
        ensure!(
            directory_meta.is_dir()
                && directory_meta.uid() == unsafe { libc::geteuid() }
                && directory_meta.mode() & 0o077 == 0,
            "private backend directory required"
        );
        let name = c"store.redb";
        let flags = libc::O_RDWR
            | libc::O_NOFOLLOW
            | libc::O_NONBLOCK
            | libc::O_CLOEXEC
            | if create {
                libc::O_CREAT | libc::O_EXCL
            } else {
                0
            };
        let fd = unsafe { libc::openat(directory.as_raw_fd(), name.as_ptr(), flags, 0o600) };
        if fd < 0 {
            return Err(io::Error::last_os_error().into());
        }
        let file = unsafe { File::from_raw_fd(fd) };
        let m = file.metadata()?;
        ensure!(
            m.is_file()
                && m.uid() == unsafe { libc::geteuid() }
                && m.mode() & 0o077 == 0
                && m.nlink() == 1
                && m.len() <= limit,
            "private capped database file refused"
        );
        // redb4.4.0's pinned format has a512-byte super-header. Its stable
        // fd/backend entry point is create-or-open, so reject a truncated
        // existing file before the engine can initialize an empty database.
        // https://github.com/cberner/redb/blob/7061f7e60d4573311c0636e161d417b93e9fd2b1/docs/design.md#database-super-header
        ensure!(
            create || m.len() >= 512,
            "existing database shorter than pinned512-byte super-header; preserve without initialization"
        );
        Ok(Self {
            file,
            limit,
            monitor,
        })
    }
    fn end(&self, offset: u64, len: usize) -> io::Result<u64> {
        let end = offset
            .checked_add(len as u64)
            .ok_or_else(|| io::Error::other("offset overflow"))?;
        if end > self.limit {
            return Err(io::Error::other("physical database cap"));
        }
        Ok(end)
    }
}
fn whole(start: Bound<u64>, end: Bound<u64>) -> bool {
    matches!(start, Bound::Unbounded | Bound::Included(0)) && matches!(end, Bound::Unbounded)
}
impl StorageBackend for CappedFile {
    fn len(&self) -> io::Result<u64> {
        Ok(self.file.metadata()?.len())
    }
    fn read(&self, offset: u64, out: &mut [u8]) -> io::Result<()> {
        self.end(offset, out.len())?;
        self.file.read_exact_at(out, offset)?;
        self.monitor.0.lock().unwrap().stats.read_bytes += out.len() as u64;
        Ok(())
    }
    fn set_len(&self, len: u64) -> io::Result<()> {
        if len > self.limit {
            return Err(io::Error::other("physical database cap"));
        }
        self.monitor.boundary(false)?;
        self.file.set_len(len)?;
        {
            let mut c = self.monitor.0.lock().unwrap();
            c.stats.resizes += 1;
            c.stats.peak_file_bytes = c.stats.peak_file_bytes.max(len);
        }
        self.monitor.boundary(true)
    }
    fn write(&self, offset: u64, data: &[u8]) -> io::Result<()> {
        let end = self.end(offset, data.len())?;
        self.monitor.boundary(false)?;
        self.file.write_all_at(data, offset)?;
        {
            let mut c = self.monitor.0.lock().unwrap();
            c.stats.written_bytes += data.len() as u64;
            c.stats.peak_file_bytes = c.stats.peak_file_bytes.max(end);
        }
        self.monitor.boundary(true)
    }
    fn sync_data(&self) -> io::Result<()> {
        self.monitor.boundary(false)?;
        self.file.sync_all()?;
        self.monitor.0.lock().unwrap().stats.syncs += 1;
        self.monitor.boundary(true)
    }
    fn try_lock_range(
        &self,
        start: Bound<u64>,
        end: Bound<u64>,
    ) -> std::result::Result<bool, BackendError> {
        if !whole(start, end) {
            return Err(BackendError::Unsupported);
        }
        match fs2::FileExt::try_lock_exclusive(&self.file) {
            Ok(()) => Ok(true),
            Err(e) if e.kind() == io::ErrorKind::WouldBlock => Ok(false),
            Err(e) => Err(e.into()),
        }
    }
    fn unlock_range(
        &self,
        start: Bound<u64>,
        end: Bound<u64>,
    ) -> std::result::Result<(), BackendError> {
        if !whole(start, end) {
            return Err(BackendError::Unsupported);
        }
        fs2::FileExt::unlock(&self.file).map_err(Into::into)
    }
    fn close(&self) -> io::Result<()> {
        fs2::FileExt::unlock(&self.file)
    }
}
pub fn private_root(root: &Path, create: bool) -> Result<(File, File)> {
    ensure!(root.is_absolute(), "absolute root required");
    if create {
        use std::os::unix::fs::DirBuilderExt;
        std::fs::DirBuilder::new().mode(0o700).create(root)?;
        File::open(root.parent().unwrap())?.sync_all()?;
    }
    ensure!(
        std::fs::canonicalize(root)? == root,
        "canonical root required"
    );
    let directory = std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .open(root)?;
    let m = directory.metadata()?;
    ensure!(
        m.uid() == unsafe { libc::geteuid() } && m.mode() & 0o077 == 0,
        "private directory required"
    );
    for entry in std::fs::read_dir(root)? {
        ensure!(
            matches!(
                entry?.file_name().to_str(),
                Some("owner.lock" | "store.redb")
            ),
            "unknown member retained for recovery"
        );
    }
    let fd = unsafe {
        libc::openat(
            directory.as_raw_fd(),
            c"owner.lock".as_ptr(),
            libc::O_RDWR | libc::O_CREAT | libc::O_NOFOLLOW | libc::O_CLOEXEC | libc::O_NONBLOCK,
            0o600,
        )
    };
    if fd < 0 {
        return Err(io::Error::last_os_error().into());
    }
    let owner = unsafe { File::from_raw_fd(fd) };
    let m = owner.metadata()?;
    ensure!(
        m.is_file()
            && m.nlink() == 1
            && m.uid() == unsafe { libc::geteuid() }
            && m.mode() & 0o077 == 0,
        "private owner lock refused"
    );
    fs2::FileExt::try_lock_exclusive(&owner)?;
    directory.sync_all()?;
    Ok((directory, owner))
}
