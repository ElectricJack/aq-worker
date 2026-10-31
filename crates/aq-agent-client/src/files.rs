//! Private, bounded, descriptor-relative local state. No workspace-selected paths.
use anyhow::{Result, ensure};
use std::{
    fs::File,
    io::{Read, Write},
    os::{
        fd::{AsRawFd, FromRawFd},
        unix::fs::{MetadataExt, OpenOptionsExt},
    },
    path::{Path, PathBuf},
};
use uuid::Uuid;

pub(crate) struct PrivateDir {
    pub path: PathBuf,
    dir: File,
}
fn name(value: &str) -> Result<std::ffi::CString> {
    ensure!(
        !value.is_empty()
            && value.len() <= 128
            && value
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.'))
            && !matches!(value, "." | ".."),
        "invalid private member"
    );
    Ok(std::ffi::CString::new(value)?)
}
impl PrivateDir {
    pub fn create(path: &Path) -> Result<Self> {
        use std::os::unix::fs::DirBuilderExt;
        ensure!(path.is_absolute(), "private root must be absolute");
        // The trusted caller creates/owns the parent; do not recursively create
        // arbitrary directory trees from an agent request.
        match std::fs::DirBuilder::new().mode(0o700).create(path) {
            Ok(()) => File::open(
                path.parent()
                    .ok_or_else(|| anyhow::anyhow!("private parent absent"))?,
            )?
            .sync_all()?,
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {}
            Err(e) => return Err(e.into()),
        }
        Self::open(path)
    }
    pub fn open(path: &Path) -> Result<Self> {
        ensure!(
            path.is_absolute() && std::fs::canonicalize(path)? == path,
            "private root must be canonical and have no symlink components"
        );
        let dir = std::fs::OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC)
            .open(path)?;
        let m = dir.metadata()?;
        ensure!(
            m.is_dir() && m.uid() == unsafe { libc::geteuid() } && m.mode() & 0o077 == 0,
            "private root ownership or permissions refused"
        );
        Ok(Self {
            path: path.into(),
            dir,
        })
    }
    fn open_member(&self, member: &str, create: bool) -> Result<File> {
        let c = name(member)?;
        let flags = if create {
            libc::O_RDWR | libc::O_CREAT
        } else {
            libc::O_RDONLY
        };
        let fd = unsafe {
            libc::openat(
                self.dir.as_raw_fd(),
                c.as_ptr(),
                flags | libc::O_NOFOLLOW | libc::O_CLOEXEC | libc::O_NONBLOCK,
                0o600,
            )
        };
        if fd < 0 {
            return Err(std::io::Error::last_os_error().into());
        }
        let f = unsafe { File::from_raw_fd(fd) };
        let m = f.metadata()?;
        ensure!(
            m.is_file()
                && m.uid() == unsafe { libc::geteuid() }
                && m.mode() & 0o077 == 0
                && m.nlink() == 1,
            "private file ownership or permissions refused"
        );
        Ok(f)
    }
    pub fn read(&self, member: &str, limit: usize) -> Result<Vec<u8>> {
        let f = self.open_member(member, false)?;
        ensure!(
            f.metadata()?.len() <= limit as u64,
            "private file exceeds bound"
        );
        let mut bytes = vec![];
        f.take(limit as u64 + 1).read_to_end(&mut bytes)?;
        ensure!(bytes.len() <= limit, "private file exceeds bound");
        Ok(bytes)
    }
    pub fn optional(&self, member: &str, limit: usize) -> Result<Option<Vec<u8>>> {
        match self.read(member, limit) {
            Ok(b) => Ok(Some(b)),
            Err(e)
                if e.downcast_ref::<std::io::Error>()
                    .is_some_and(|e| e.kind() == std::io::ErrorKind::NotFound) =>
            {
                Ok(None)
            }
            Err(e) => Err(e),
        }
    }
    pub fn write(&self, member: &str, bytes: &[u8]) -> Result<()> {
        let dest = name(member)?;
        let temp = name(&format!(".{}.tmp", Uuid::new_v4()))?;
        let fd = unsafe {
            libc::openat(
                self.dir.as_raw_fd(),
                temp.as_ptr(),
                libc::O_WRONLY | libc::O_CREAT | libc::O_EXCL | libc::O_NOFOLLOW | libc::O_CLOEXEC,
                0o600,
            )
        };
        if fd < 0 {
            return Err(std::io::Error::last_os_error().into());
        }
        let result = (|| {
            let mut f = unsafe { File::from_raw_fd(fd) };
            f.write_all(bytes)?;
            f.sync_all()?;
            if unsafe {
                libc::renameat(
                    self.dir.as_raw_fd(),
                    temp.as_ptr(),
                    self.dir.as_raw_fd(),
                    dest.as_ptr(),
                )
            } < 0
            {
                return Err(std::io::Error::last_os_error().into());
            }
            self.dir.sync_all()?;
            Ok(())
        })();
        let _ = unsafe { libc::unlinkat(self.dir.as_raw_fd(), temp.as_ptr(), 0) };
        result
    }
    pub fn lock(&self) -> Result<File> {
        let f = self.open_member("owner.lock", true)?;
        fs2::FileExt::try_lock_exclusive(&f)?;
        self.dir.sync_all()?;
        Ok(f)
    }
}
