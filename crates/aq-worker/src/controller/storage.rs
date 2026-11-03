//! Locked descriptor-relative storage. No task-controlled paths are opened.
use anyhow::{Result, ensure};
use fs2::FileExt;
use std::{
    ffi::{CStr, CString},
    fs::File,
    io::{Read, Write},
    os::{
        fd::{AsRawFd, FromRawFd},
        unix::{ffi::OsStrExt, fs::MetadataExt},
    },
    path::{Component, Path, PathBuf},
};

pub(super) struct Storage {
    root: File,
    path: PathBuf,
    pid: u32,
    #[cfg(test)]
    pub fault: std::sync::Mutex<Option<WriteFault>>,
}
fn directory(parent: &File, name: &CStr) -> Result<File> {
    let fd = unsafe {
        libc::openat(
            parent.as_raw_fd(),
            name.as_ptr(),
            libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
        )
    };
    ensure!(fd >= 0, "controller directory open failed");
    Ok(unsafe { File::from_raw_fd(fd) })
}
impl Storage {
    pub fn open(path: &Path) -> Result<(Self, bool)> {
        ensure!(path.is_absolute(), "controller root must be absolute");
        let mut names = Vec::new();
        for c in path.components() {
            match c {
                Component::RootDir => {}
                Component::Normal(n) => names.push(CString::new(n.as_bytes())?),
                _ => anyhow::bail!("unsafe controller root"),
            }
        }
        let (leaf, parents) = names
            .split_last()
            .ok_or_else(|| anyhow::anyhow!("cannot own filesystem root"))?;
        let mut parent = File::open("/")?;
        for name in parents {
            parent = directory(&parent, name)?;
        }
        let created = unsafe { libc::mkdirat(parent.as_raw_fd(), leaf.as_ptr(), 0o700) } == 0;
        if !created {
            ensure!(
                std::io::Error::last_os_error().raw_os_error() == Some(libc::EEXIST),
                "controller root create failed"
            );
        }
        if created {
            parent.sync_all()?;
        }
        let root = directory(&parent, leaf)?;
        root.try_lock_exclusive()
            .map_err(|_| anyhow::anyhow!("controller root already owned"))?;
        let s = Self {
            root,
            path: path.to_owned(),
            pid: std::process::id(),
            #[cfg(test)]
            fault: std::sync::Mutex::new(None),
        };
        s.check()?;
        Ok((s, created))
    }
    pub fn check(&self) -> Result<()> {
        let meta = self.root.metadata()?;
        let named = std::fs::symlink_metadata(&self.path)?;
        ensure!(
            self.pid == std::process::id()
                && meta.uid() == unsafe { libc::geteuid() }
                && meta.mode() & 0o7777 == 0o700
                && named.is_dir()
                && named.dev() == meta.dev()
                && named.ino() == meta.ino(),
            "controller root changed"
        );
        Ok(())
    }
    fn member(&self, name: &str, create: bool) -> Result<File> {
        self.check()?;
        ensure!(
            !name.is_empty() && !name.contains('/') && name != "." && name != "..",
            "invalid controller member"
        );
        let name = CString::new(name)?;
        let flags = if create {
            libc::O_WRONLY | libc::O_CREAT | libc::O_EXCL
        } else {
            libc::O_RDONLY
        };
        let fd = unsafe {
            libc::openat(
                self.root.as_raw_fd(),
                name.as_ptr(),
                flags | libc::O_NOFOLLOW | libc::O_NONBLOCK | libc::O_CLOEXEC,
                0o600,
            )
        };
        ensure!(fd >= 0, "controller file open failed");
        let file = unsafe { File::from_raw_fd(fd) };
        let m = file.metadata()?;
        ensure!(
            m.is_file()
                && m.nlink() == 1
                && m.uid() == unsafe { libc::geteuid() }
                && m.mode() & 0o7777 == 0o600,
            "controller file is not private regular storage"
        );
        Ok(file)
    }
    pub fn read(&self, name: &str, max: usize) -> Result<Vec<u8>> {
        let f = self.member(name, false)?;
        ensure!(
            f.metadata()?.len() <= max as u64,
            "controller file too large"
        );
        let mut bytes = Vec::new();
        f.take(max as u64 + 1).read_to_end(&mut bytes)?;
        ensure!(bytes.len() <= max, "controller file too large");
        Ok(bytes)
    }
    pub fn write_private(&self, name: &str, bytes: &[u8]) -> Result<()> {
        let mut f = self.member(name, true)?;
        f.write_all(bytes)?;
        f.sync_all()?;
        self.root.sync_all()?;
        self.check()
    }
    pub fn write_state(&self, bytes: &[u8]) -> Result<()> {
        #[cfg(test)]
        let fault = self.fault.lock().unwrap().take();
        #[cfg(test)]
        ensure!(
            fault != Some(WriteFault::BeforeWrite),
            "injected write failure"
        );
        self.write_private("journal.pending", bytes)?;
        #[cfg(test)]
        ensure!(
            fault != Some(WriteFault::AfterFileSync),
            "injected file sync failure"
        );
        self.check()?;
        ensure!(
            unsafe {
                libc::renameat(
                    self.root.as_raw_fd(),
                    c"journal.pending".as_ptr(),
                    self.root.as_raw_fd(),
                    c"journal.json".as_ptr(),
                )
            } == 0,
            "controller journal rename failed"
        );
        #[cfg(test)]
        ensure!(
            fault != Some(WriteFault::AfterRename),
            "injected directory sync failure"
        );
        self.root.sync_all()?;
        self.check()
    }
    pub fn discard_pending(&self) -> Result<()> {
        // A pre-rename write cannot have authorized an external effect. Read the
        // file safely before unlinking, but never promote it over published state.
        let names = std::fs::read_dir(format!("/proc/self/fd/{}", self.root.as_raw_fd()))?;
        for entry in names {
            if entry?.file_name() == "journal.pending" {
                self.read("journal.pending", super::journal::MAX_BYTES)?;
                ensure!(
                    unsafe {
                        libc::unlinkat(self.root.as_raw_fd(), c"journal.pending".as_ptr(), 0)
                    } == 0,
                    "controller pending removal failed"
                );
                self.root.sync_all()?;
            }
        }
        Ok(())
    }
    pub fn free_bytes(&self) -> Result<u64> {
        self.check()?;
        let mut stat = std::mem::MaybeUninit::<libc::statvfs>::uninit();
        ensure!(
            unsafe { libc::fstatvfs(self.root.as_raw_fd(), stat.as_mut_ptr()) } == 0,
            "controller disk space unavailable"
        );
        let stat = unsafe { stat.assume_init() };
        stat.f_bavail
            .checked_mul(stat.f_frsize)
            .ok_or_else(|| anyhow::anyhow!("disk space overflow"))
    }
}

impl Drop for Storage {
    fn drop(&mut self) {
        // An unrelated fork child may retain the descriptor before exec. Release
        // explicitly in the owner; never unlock the parent's lock in a child.
        if self.pid == std::process::id() {
            let _ = FileExt::unlock(&self.root);
        }
    }
}
#[cfg(test)]
#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum WriteFault {
    BeforeWrite,
    AfterFileSync,
    AfterRename,
}
