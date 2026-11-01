//! Canonical owner-only host root identity shared by closed profiles.
use super::*;
use std::{
    fs::{File, OpenOptions},
    os::unix::fs::{MetadataExt, OpenOptionsExt},
    path::{Path, PathBuf},
};
pub(super) struct RootGuard {
    file: File,
    path: PathBuf,
    device: u64,
    inode: u64,
}
impl RootGuard {
    pub(super) fn open(path: &Path, expected: Option<(u64, u64)>) -> Result<Self> {
        ensure!(
            path.is_absolute()
                && path.as_os_str().len() <= 2048
                && std::fs::canonicalize(path)? == path,
            "native host root must be canonical"
        );
        let file = OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_NONBLOCK | libc::O_CLOEXEC)
            .open(path)?;
        let m = file.metadata()?;
        ensure!(
            m.is_dir() && m.uid() == unsafe { libc::geteuid() } && m.mode() & 0o777 == 0o700,
            "native host root must be private owner0700"
        );
        if let Some((dev, ino)) = expected {
            ensure!(
                m.dev() == dev && m.ino() == ino,
                "native host root replaced"
            );
        }
        Ok(Self {
            file,
            path: path.into(),
            device: m.dev(),
            inode: m.ino(),
        })
    }
    pub(super) fn identity(&self) -> (u64, u64) {
        (self.device, self.inode)
    }
    pub(super) fn verify(&self) -> Result<()> {
        let saved = self.file.metadata()?;
        let now = std::fs::symlink_metadata(&self.path)?;
        ensure!(
            saved.is_dir()
                && now.is_dir()
                && saved.dev() == self.device
                && saved.ino() == self.inode
                && now.dev() == self.device
                && now.ino() == self.inode
                && now.uid() == unsafe { libc::geteuid() }
                && now.mode() & 0o777 == 0o700,
            "native host root namespace changed"
        );
        Ok(())
    }
}
