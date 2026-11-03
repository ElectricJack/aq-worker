//! Runner-owned files. Public names describe layout, never credential contents.
use anyhow::{Result, ensure};
use std::{
    fs::{self, OpenOptions},
    io::{Read, Write},
    os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt, PermissionsExt},
    path::{Path, PathBuf},
};
use zeroize::Zeroizing;
const MAX_SECRET: u64 = 4 * 1024 * 1024;
pub struct RunnerFiles {
    root: PathBuf,
}
impl RunnerFiles {
    pub fn create(root: &Path) -> Result<Self> {
        fs::DirBuilder::new().mode(0o700).create(root)?;
        let root = root.canonicalize()?;
        fs::DirBuilder::new()
            .mode(0o700)
            .create(root.join("state"))?;
        Ok(Self { root })
    }
    pub fn root(&self) -> &Path {
        &self.root
    }
    pub fn write_secret(&self, name: &str, bytes: &[u8]) -> Result<()> {
        ensure!(
            matches!(name, "permit.json" | "provider.key" | "access.json"),
            "unknown runner file"
        );
        ensure!(
            !bytes.is_empty() && bytes.len() as u64 <= MAX_SECRET,
            "runner file size"
        );
        check_directory(&self.root)?;
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW)
            .open(self.root.join(name))?;
        file.write_all(bytes)?;
        file.sync_all()?;
        fs::File::open(&self.root)?.sync_all()?;
        Ok(())
    }
    pub fn read_secret(path: &Path) -> Result<Zeroizing<Vec<u8>>> {
        // Open first and inspect the exact descriptor; never follow a leaf symlink.
        check_directory(
            path.parent()
                .ok_or_else(|| anyhow::anyhow!("runner file parent"))?,
        )?;
        let mut file = OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
            .open(path)?;
        let meta = file.metadata()?;
        ensure!(
            meta.is_file()
                && meta.uid() == unsafe { libc::geteuid() }
                && meta.mode() & 0o7777 == 0o600
                && meta.nlink() == 1
                && meta.len() > 0
                && meta.len() <= MAX_SECRET,
            "runner file ownership/mode/size"
        );
        let mut bytes = Zeroizing::new(Vec::new());
        (&mut file).take(MAX_SECRET + 1).read_to_end(&mut bytes)?;
        ensure!(
            !bytes.is_empty() && bytes.len() as u64 <= MAX_SECRET,
            "runner file grew beyond bound"
        );
        Ok(bytes)
    }
}
pub fn check_directory(path: &Path) -> Result<()> {
    let meta = fs::symlink_metadata(path)?;
    ensure!(
        meta.is_dir()
            && meta.uid() == unsafe { libc::geteuid() }
            && meta.permissions().mode() & 0o7777 == 0o700,
        "runner directory ownership/mode"
    );
    Ok(())
}
