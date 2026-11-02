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
    pub fn check_route(&self) -> Result<()> {
        let held = self.dir.metadata()?;
        let named = std::fs::symlink_metadata(&self.path)?;
        ensure!(
            std::fs::canonicalize(&self.path)? == self.path
                && named.is_dir()
                && held.dev() == named.dev()
                && held.ino() == named.ino()
                && named.uid() == unsafe { libc::geteuid() }
                && named.mode() & 0o077 == 0,
            "private directory route changed"
        );
        Ok(())
    }
    pub fn child(&self, member: &str) -> Result<Self> {
        name(member)?;
        self.check_route()?;
        let child = Self::open(&self.path.join(member))?;
        self.check_route()?;
        child.check_route()?;
        Ok(child)
    }
    /// Existing member only; never creates a worker-owned control/lock file.
    pub fn existing(&self, member: &str) -> Result<File> {
        self.check_route()?;
        self.open_member(member, false)
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
        self.write_atomic(member, bytes, false)
    }
    pub fn write_new(&self, member: &str, bytes: &[u8]) -> Result<()> {
        self.check_route()?;
        self.write_atomic(member, bytes, true)
    }
    fn write_atomic(&self, member: &str, bytes: &[u8], new_only: bool) -> Result<()> {
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
            let renamed = if new_only {
                // Linux is the qualified worker platform. Refuse rather than
                // fall back to a replacing rename if NOREPLACE is unavailable.
                unsafe {
                    libc::renameat2(
                        self.dir.as_raw_fd(),
                        temp.as_ptr(),
                        self.dir.as_raw_fd(),
                        dest.as_ptr(),
                        libc::RENAME_NOREPLACE,
                    )
                }
            } else {
                unsafe {
                    libc::renameat(
                        self.dir.as_raw_fd(),
                        temp.as_ptr(),
                        self.dir.as_raw_fd(),
                        dest.as_ptr(),
                    )
                }
            };
            if renamed < 0 {
                return Err(std::io::Error::last_os_error().into());
            }
            self.dir.sync_all()?;
            Ok(())
        })();
        let _ = unsafe { libc::unlinkat(self.dir.as_raw_fd(), temp.as_ptr(), 0) };
        result
    }
    pub fn lock(&self) -> Result<File> {
        self.lock_named("owner.lock")
    }
    pub fn lock_named(&self, member: &str) -> Result<File> {
        self.check_route()?;
        let f = self.open_member(member, true)?;
        fs2::FileExt::try_lock_exclusive(&f)?;
        self.dir.sync_all()?;
        Ok(f)
    }
}

#[cfg(test)]
mod result_publication_tests {
    use super::*;
    fn private_tempdir() -> tempfile::TempDir {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
        dir
    }
    #[test]
    fn create_only_outbox_never_replaces_an_existing_file() {
        let temp = private_tempdir();
        let dir = PrivateDir::open(temp.path()).unwrap();
        dir.write_new("submission.json", b"original").unwrap();
        let error = dir
            .write_new("submission.json", b"replacement")
            .unwrap_err();
        assert!(
            error
                .downcast_ref::<std::io::Error>()
                .is_some_and(|e| e.kind() == std::io::ErrorKind::AlreadyExists)
        );
        assert_eq!(dir.read("submission.json", 32).unwrap(), b"original");
        dir.write("projection.json", b"old").unwrap();
        dir.write("projection.json", b"new").unwrap();
        assert_eq!(
            dir.read("projection.json", 32).unwrap(),
            b"new",
            "existing replaceable journal/projection path remains available"
        );
    }
    #[test]
    fn two_uncoordinated_publications_have_exactly_one_winner() {
        let temp = private_tempdir();
        let gate = std::sync::Arc::new(std::sync::Barrier::new(2));
        let mut threads = vec![];
        for value in [b"one".as_slice(), b"two".as_slice()] {
            let path = temp.path().to_owned();
            let gate = gate.clone();
            let bytes = value.to_vec();
            threads.push(std::thread::spawn(move || {
                let dir = PrivateDir::open(&path).unwrap();
                gate.wait();
                dir.write_new("submission.json", &bytes).is_ok()
            }));
        }
        assert_eq!(
            threads
                .into_iter()
                .map(|thread| thread.join().unwrap())
                .filter(|won| *won)
                .count(),
            1
        );
        assert!(matches!(
            PrivateDir::open(temp.path())
                .unwrap()
                .read("submission.json", 16)
                .unwrap()
                .as_slice(),
            b"one" | b"two"
        ));
    }
}
