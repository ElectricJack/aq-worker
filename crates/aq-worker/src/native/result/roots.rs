//! Actual retained source directory custody; no reopen from history.
use super::history::DirectoryPin;
use anyhow::{Result, ensure};
use std::{
    fs::{File, OpenOptions},
    os::unix::fs::{MetadataExt, OpenOptionsExt},
    path::{Path, PathBuf},
};
pub(super) struct SourceRoots {
    git: File,
    workspace: File,
    git_path: PathBuf,
    workspace_path: PathBuf,
    pid: u32,
}
fn open(path: &Path) -> Result<File> {
    ensure!(
        path.is_absolute() && path.canonicalize()? == path,
        "result source canonical route"
    );
    let f = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .open(path)?;
    let m = f.metadata()?;
    ensure!(
        m.is_dir() && m.uid() == unsafe { libc::geteuid() } && m.mode() & 0o022 == 0,
        "result source directory owner/mode"
    );
    Ok(f)
}
fn pin(f: &File) -> Result<DirectoryPin> {
    let m = f.metadata()?;
    Ok(DirectoryPin {
        device: m.dev(),
        inode: m.ino(),
    })
}
impl SourceRoots {
    pub(super) fn new(git: &Path, workspace: &Path) -> Result<Self> {
        ensure!(
            workspace.starts_with(git) && workspace != git,
            "result source workspace outside original Git root"
        );
        Ok(Self {
            git: open(git)?,
            workspace: open(workspace)?,
            git_path: git.into(),
            workspace_path: workspace.into(),
            pid: std::process::id(),
        })
    }
    pub(super) fn pins(&self) -> Result<(DirectoryPin, DirectoryPin)> {
        Ok((pin(&self.git)?, pin(&self.workspace)?))
    }
    pub(super) fn verify(&self) -> Result<()> {
        ensure!(
            self.pid == std::process::id()
                && pin(&self.git)? == pin(&open(&self.git_path)?)?
                && pin(&self.workspace)? == pin(&open(&self.workspace_path)?)?,
            "result source root/workspace custody changed"
        );
        Ok(())
    }
}
