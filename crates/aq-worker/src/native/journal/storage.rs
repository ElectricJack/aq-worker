//! Descriptor-relative private storage; no request-time path traversal.
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
    path::{Component, Path},
};

pub(super) struct Storage {
    root: File,
    owner_pid: u32,
}
impl Drop for Storage {
    fn drop(&mut self) {
        if self.owner_pid == std::process::id() {
            let _ = FileExt::unlock(&self.root);
        }
    }
}
fn directory(parent: &File, name: &CStr) -> Result<File> {
    let fd = unsafe {
        libc::openat(
            parent.as_raw_fd(),
            name.as_ptr(),
            libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
        )
    };
    ensure!(
        fd >= 0,
        "journal directory open: {}",
        std::io::Error::last_os_error()
    );
    Ok(unsafe { File::from_raw_fd(fd) })
}
impl Storage {
    pub(super) fn open(path: &Path, create: bool) -> Result<Self> {
        ensure!(path.is_absolute(), "journal requires absolute root");
        let mut names = Vec::new();
        for component in path.components() {
            match component {
                Component::RootDir => {}
                Component::Normal(name) => names.push(CString::new(name.as_bytes())?),
                _ => anyhow::bail!("unsafe journal root component"),
            }
        }
        let (leaf, parents) = names
            .split_last()
            .ok_or_else(|| anyhow::anyhow!("journal cannot own filesystem root"))?;
        let mut parent = File::open("/")?;
        for name in parents {
            parent = directory(&parent, name)?;
        }
        if create {
            ensure!(
                unsafe { libc::mkdirat(parent.as_raw_fd(), leaf.as_ptr(), 0o700) } == 0,
                "journal root must be newly created: {}",
                std::io::Error::last_os_error()
            );
            // A failed initialization leaves an existing root, never an empty node.
            parent.sync_all()?;
        }
        let root = directory(&parent, leaf)?;
        let meta = root.metadata()?;
        ensure!(
            meta.uid() == unsafe { libc::geteuid() } && meta.mode() & 0o7777 == 0o700,
            "journal root is not private"
        );
        root.try_lock_exclusive()
            .map_err(|_| anyhow::anyhow!("native journal already owned"))?;
        Ok(Self {
            root,
            owner_pid: std::process::id(),
        })
    }
    fn member(&self, name: &CStr, create: bool) -> Result<File> {
        self.check_owner()?;
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
        ensure!(
            fd >= 0,
            "journal member open: {}",
            std::io::Error::last_os_error()
        );
        let file = unsafe { File::from_raw_fd(fd) };
        let meta = file.metadata()?;
        ensure!(
            meta.is_file()
                && meta.nlink() == 1
                && meta.uid() == unsafe { libc::geteuid() }
                && meta.mode() & 0o7777 == 0o600,
            "journal member is not private regular file"
        );
        Ok(file)
    }
    pub(super) fn check_owner(&self) -> Result<()> {
        ensure!(
            self.owner_pid == std::process::id(),
            "forked process cannot use journal owner"
        );
        let meta = self.root.metadata()?;
        ensure!(
            meta.uid() == unsafe { libc::geteuid() } && meta.mode() & 0o7777 == 0o700,
            "journal root permissions changed"
        );
        Ok(())
    }
    pub(super) fn read(&self, name: &CStr, max: usize) -> Result<Vec<u8>> {
        let file = self.member(name, false)?;
        ensure!(file.metadata()?.len() <= max as u64, "journal byte limit");
        let mut bytes = Vec::new();
        file.take(max as u64 + 1).read_to_end(&mut bytes)?;
        ensure!(bytes.len() <= max, "journal byte limit");
        Ok(bytes)
    }
    pub(super) fn write_new(&self, name: &CStr, bytes: &[u8]) -> Result<()> {
        let mut file = self.member(name, true)?;
        file.write_all(bytes)?;
        file.sync_all()?;
        Ok(())
    }
    pub(super) fn publish(&self) -> Result<()> {
        self.check_owner()?;
        ensure!(
            unsafe {
                libc::renameat(
                    self.root.as_raw_fd(),
                    c"journal.pending".as_ptr(),
                    self.root.as_raw_fd(),
                    c"journal.json".as_ptr(),
                )
            } == 0,
            "journal publish: {}",
            std::io::Error::last_os_error()
        );
        Ok(())
    }
    pub(super) fn sync(&self) -> Result<()> {
        self.root.sync_all()?;
        Ok(())
    }
    pub(super) fn remove_pending(&self) -> Result<()> {
        self.check_owner()?;
        ensure!(
            unsafe { libc::unlinkat(self.root.as_raw_fd(), c"journal.pending".as_ptr(), 0) } == 0,
            "journal pending removal: {}",
            std::io::Error::last_os_error()
        );
        self.sync()
    }
    pub(super) fn members(&self) -> Result<Vec<String>> {
        self.check_owner()?;
        let mut names = Vec::new();
        for entry in std::fs::read_dir(format!("/proc/self/fd/{}", self.root.as_raw_fd()))? {
            let name = entry?
                .file_name()
                .into_string()
                .map_err(|_| anyhow::anyhow!("unknown journal member"))?;
            ensure!(
                name == "journal.json" || name == "journal.pending",
                "unknown journal member"
            );
            names.push(name);
            ensure!(names.len() <= 2, "journal member count");
        }
        Ok(names)
    }
}
