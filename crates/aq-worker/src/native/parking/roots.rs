//! Retained directory identities, never reconstructed from a serialized pin.
use crate::continuation::history::{DirectoryPin, WorkspacePins};
use anyhow::{Result, ensure};
use std::{
    fs::{File, OpenOptions},
    os::unix::fs::{MetadataExt, OpenOptionsExt},
    path::{Path, PathBuf},
};

struct DirectoryOwner {
    file: File,
    path: PathBuf,
    pin: DirectoryPin,
    private: bool,
}
impl DirectoryOwner {
    fn open(path: &Path, private: bool) -> Result<Self> {
        ensure!(
            path.is_absolute()
                && path.as_os_str().len() <= 2048
                && std::fs::canonicalize(path)? == path,
            "parking directory must be canonical"
        );
        let file = OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_NONBLOCK | libc::O_CLOEXEC)
            .open(path)?;
        let metadata = file.metadata()?;
        let owner = Self {
            file,
            path: path.into(),
            pin: DirectoryPin {
                device: metadata.dev(),
                inode: metadata.ino(),
            },
            private,
        };
        owner.verify()?;
        Ok(owner)
    }
    fn verify(&self) -> Result<()> {
        let saved = self.file.metadata()?;
        let current = std::fs::symlink_metadata(&self.path)?;
        ensure!(
            saved.is_dir()
                && current.is_dir()
                && saved.dev() == self.pin.device
                && saved.ino() == self.pin.inode
                && current.dev() == self.pin.device
                && current.ino() == self.pin.inode
                && current.uid() == unsafe { libc::geteuid() }
                && current.mode() & if self.private { 0o077 } else { 0o022 } == 0
                && std::fs::canonicalize(&self.path)? == self.path,
            "parking retained directory route/owner changed"
        );
        Ok(())
    }
}
pub(super) struct WorkspaceRoots {
    managed: DirectoryOwner,
    workspace: DirectoryOwner,
}
impl WorkspaceRoots {
    pub(super) fn open(managed: &Path, workspace: &Path) -> Result<Self> {
        ensure!(
            workspace.starts_with(managed.join("workspaces"))
                && workspace != managed.join("workspaces"),
            "parking workspace outside managed root"
        );
        Ok(Self {
            managed: DirectoryOwner::open(managed, true)?,
            workspace: DirectoryOwner::open(workspace, false)?,
        })
    }
    pub(super) fn pins(&self) -> WorkspacePins {
        WorkspacePins {
            source_root: self.managed.pin.clone(),
            workspace: self.workspace.pin.clone(),
        }
    }
    pub(super) fn verify(&self, expected: &WorkspacePins) -> Result<()> {
        ensure!(
            self.managed.pin == expected.source_root && self.workspace.pin == expected.workspace,
            "parking immutable root pins changed"
        );
        self.managed.verify()?;
        self.workspace.verify()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;
    #[test]
    fn pinned_workspace_refuses_replaced_routes_and_foreign_roots() {
        let root = tempfile::tempdir().unwrap();
        std::fs::set_permissions(root.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
        std::fs::create_dir(root.path().join("workspaces")).unwrap();
        let workspace = root.path().join("workspaces/exact");
        std::fs::create_dir(&workspace).unwrap();
        let held = WorkspaceRoots::open(root.path(), &workspace).unwrap();
        let pins = held.pins();
        held.verify(&pins).unwrap();
        std::fs::rename(&workspace, root.path().join("workspaces/retained")).unwrap();
        std::fs::create_dir(&workspace).unwrap();
        assert!(held.verify(&pins).is_err());
        assert!(WorkspaceRoots::open(root.path(), root.path()).is_err());
    }
    #[test]
    fn nonprivate_root_and_symlink_workspace_are_not_opened_from_history() {
        let root = tempfile::tempdir().unwrap();
        std::fs::set_permissions(root.path(), std::fs::Permissions::from_mode(0o755)).unwrap();
        std::fs::create_dir(root.path().join("workspaces")).unwrap();
        let path = root.path().join("workspaces/exact");
        std::fs::create_dir(&path).unwrap();
        assert!(WorkspaceRoots::open(root.path(), &path).is_err());
        std::fs::set_permissions(root.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
        let link = root.path().join("workspaces/alias");
        std::os::unix::fs::symlink(&path, &link).unwrap();
        assert!(WorkspaceRoots::open(root.path(), &link).is_err());
    }
}
