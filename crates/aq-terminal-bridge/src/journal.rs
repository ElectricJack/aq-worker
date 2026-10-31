use super::*;
use std::{
    ffi::CString,
    fs::{File, OpenOptions},
    io::{Read, Write},
    os::{
        fd::{AsRawFd, FromRawFd},
        unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt},
    },
    path::PathBuf,
};
const MAX_BYTES: usize = 2 * 1024 * 1024;
// Future receipts are <=1024 bytes each; keep room for all1024 and one64-item
// upload in addition to the admitted canonical metadata.
const MAX_ADMISSION_BYTES: usize = 768 * 1024;
pub(super) fn check_admission(state: &State) -> Result<()> {
    ensure!(
        serde_json::to_vec(state)?.len() <= MAX_ADMISSION_BYTES,
        "bridge admission byte capacity"
    );
    Ok(())
}
#[derive(Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Identity {
    dev: u64,
    ino: u64,
}
fn identity(m: &std::fs::Metadata) -> Identity {
    Identity {
        dev: m.dev(),
        ino: m.ino(),
    }
}
fn private_directory(m: &std::fs::Metadata) -> Result<()> {
    ensure!(
        m.is_dir() && m.uid() == unsafe { libc::geteuid() } && m.mode() & 0o777 == 0o700,
        "unsafe bridge journal directory"
    );
    Ok(())
}
fn private_file(file: &File) -> Result<()> {
    let m = file.metadata()?;
    ensure!(
        m.is_file()
            && m.nlink() == 1
            && m.uid() == unsafe { libc::geteuid() }
            && m.mode() & 0o777 == 0o600,
        "unsafe bridge journal file"
    );
    Ok(())
}
pub(super) struct Owner {
    root: PathBuf,
    directory: File,
    directory_identity: Identity,
    lock: File,
    lock_identity: Identity,
    pid: u32,
    fresh: bool,
    state: Option<(Identity, Digest)>,
}
impl Owner {
    pub fn open(root: &Path, create: bool) -> Result<Self> {
        if create && !root.exists() {
            std::fs::DirBuilder::new().mode(0o700).create(root)?;
        }
        private_directory(&std::fs::symlink_metadata(root)?)?;
        let root = std::fs::canonicalize(root)?;
        let directory = OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_NONBLOCK)
            .open(&root)?;
        private_directory(&directory.metadata()?)?;
        let directory_identity = identity(&directory.metadata()?);
        let lock = open_at(
            &directory,
            "owner.lock",
            libc::O_RDWR
                | if create {
                    libc::O_CREAT | libc::O_EXCL
                } else {
                    0
                },
        )?;
        let fresh = create;
        private_file(&lock)?;
        let lock_identity = identity(&lock.metadata()?);
        fs2::FileExt::try_lock_exclusive(&lock)?;
        let owner = Self {
            root,
            directory,
            directory_identity,
            lock,
            lock_identity,
            pid: std::process::id(),
            fresh,
            state: None,
        };
        owner.verify_namespace()?;
        if fresh {
            owner.lock.sync_all()?;
            owner.directory.sync_all()?;
        }
        owner.verify_namespace()?;
        Ok(owner)
    }
    fn verify_namespace(&self) -> Result<()> {
        ensure!(
            self.pid == std::process::id(),
            "forked bridge owner cannot mutate journal"
        );
        let path = std::fs::symlink_metadata(&self.root)?;
        private_directory(&path)?;
        private_directory(&self.directory.metadata()?)?;
        ensure!(
            identity(&path) == self.directory_identity
                && identity(&self.directory.metadata()?) == self.directory_identity,
            "bridge journal root replaced"
        );
        private_file(&self.lock)?;
        let current = open_at(&self.directory, "owner.lock", libc::O_RDONLY)?;
        private_file(&current)?;
        ensure!(
            identity(&current.metadata()?) == self.lock_identity
                && identity(&self.lock.metadata()?) == self.lock_identity,
            "bridge owner lock replaced"
        );
        Ok(())
    }
    pub fn read(&mut self) -> Result<Option<State>> {
        self.verify_namespace()?;
        let document = self.read_document()?;
        self.verify_namespace()?;
        match document {
            Some((document, inode)) => {
                ensure!(
                    !self.fresh,
                    "bridge journal present without established ownership"
                );
                self.state = Some((inode, document.digest));
                Ok(Some(document.state))
            }
            None => {
                ensure!(
                    self.fresh,
                    "bridge journal missing after prior ownership; explicit recovery required"
                );
                Ok(None)
            }
        }
    }
    /// Fresh backing verification prevents a derived in-memory state from
    /// concealing replacement or corruption of its authoritative journal.
    pub fn verify_current(&self) -> Result<()> {
        self.verify_namespace()?;
        match (self.read_document()?, &self.state) {
            (Some((document, inode)), Some((expected_inode, expected_digest))) => ensure!(
                inode == *expected_inode && document.digest == *expected_digest,
                "bridge journal replaced or changed"
            ),
            (None, None) => ensure!(self.fresh, "missing bridge journal"),
            _ => bail!("bridge journal membership changed"),
        }
        self.verify_namespace()
    }
    fn read_document(&self) -> Result<Option<(Document, Identity)>> {
        let file = match open_at(&self.directory, "bridge.json", libc::O_RDONLY) {
            Ok(file) => file,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(e.into()),
        };
        private_file(&file)?;
        let m = file.metadata()?;
        ensure!(m.len() <= MAX_BYTES as u64, "bridge journal byte capacity");
        let mut bytes = Vec::new();
        file.take(MAX_BYTES as u64 + 1).read_to_end(&mut bytes)?;
        ensure!(bytes.len() <= MAX_BYTES, "bridge journal grew beyond bound");
        let document: Document = serde_json::from_slice(&bytes)
            .map_err(|_| anyhow::anyhow!("invalid bridge journal"))?;
        ensure!(
            document.version == 1
                && document.digest == digest(&document.state)?
                && document.directory == self.directory_identity
                && document.lock == self.lock_identity,
            "bridge journal integrity/version/owner identity"
        );
        Ok(Some((document, identity(&m))))
    }
    pub fn write(&mut self, state: &State) -> Result<()> {
        self.verify_current()?;
        let document = Document {
            version: 1,
            directory: self.directory_identity,
            lock: self.lock_identity,
            digest: digest(state)?,
            state: state.clone(),
        };
        let bytes = serde_json::to_vec(&document)?;
        ensure!(bytes.len() <= MAX_BYTES, "bridge journal byte capacity");
        let temp = format!("{}.tmp", Uuid::new_v4());
        let mut file = open_at(
            &self.directory,
            &temp,
            libc::O_WRONLY | libc::O_CREAT | libc::O_EXCL,
        )?;
        let result = (|| -> Result<()> {
            file.write_all(&bytes)?;
            file.sync_all()?;
            self.test_before_rename()?;
            self.verify_current()?;
            rename_at(&self.directory, &temp, "bridge.json")?;
            self.directory.sync_all()?;
            self.state = Some((identity(&file.metadata()?), document.digest));
            self.fresh = false;
            self.verify_current()
        })();
        if result.is_err() {
            let _ = unlink_at(&self.directory, &temp);
        }
        result
    }
    fn test_before_rename(&self) -> Result<()> {
        #[cfg(feature = "test-support")]
        {
            let arm = match open_at(&self.directory, "test-before-rename.arm", libc::O_RDONLY) {
                Ok(file) => file,
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
                Err(e) => return Err(e.into()),
            };
            private_file(&arm)?;
            unlink_at(&self.directory, "test-before-rename.arm")?;
            let mut reached = open_at(
                &self.directory,
                "test-before-rename.reached",
                libc::O_WRONLY | libc::O_CREAT | libc::O_EXCL,
            )?;
            reached.write_all(b"synthetic persistence boundary\n")?;
            reached.sync_all()?;
            self.directory.sync_all()?;
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
            loop {
                match open_at(
                    &self.directory,
                    "test-before-rename.release",
                    libc::O_RDONLY,
                ) {
                    Ok(file) => {
                        private_file(&file)?;
                        unlink_at(&self.directory, "test-before-rename.release")?;
                        break;
                    }
                    Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                    Err(e) => return Err(e.into()),
                }
                ensure!(
                    std::time::Instant::now() < deadline,
                    "synthetic persistence boundary timeout"
                );
                std::thread::sleep(std::time::Duration::from_millis(1));
            }
        }
        Ok(())
    }
}
impl Drop for Owner {
    fn drop(&mut self) {
        if self.pid == std::process::id() {
            let _ = fs2::FileExt::unlock(&self.lock);
        }
    }
}
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Document {
    version: u32,
    directory: Identity,
    lock: Identity,
    digest: Digest,
    state: State,
}
fn name(value: &str) -> std::io::Result<CString> {
    if value.is_empty() || value.contains('/') {
        return Err(std::io::Error::other("invalid journal member"));
    }
    CString::new(value).map_err(std::io::Error::other)
}
fn open_at(directory: &File, member: &str, flags: i32) -> std::io::Result<File> {
    let member = name(member)?;
    let fd = unsafe {
        libc::openat(
            directory.as_raw_fd(),
            member.as_ptr(),
            flags | libc::O_NOFOLLOW | libc::O_NONBLOCK | libc::O_CLOEXEC,
            0o600,
        )
    };
    if fd < 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(unsafe { File::from_raw_fd(fd) })
}
fn rename_at(directory: &File, from: &str, to: &str) -> std::io::Result<()> {
    let from = name(from)?;
    let to = name(to)?;
    if unsafe {
        libc::renameat(
            directory.as_raw_fd(),
            from.as_ptr(),
            directory.as_raw_fd(),
            to.as_ptr(),
        )
    } < 0
    {
        return Err(std::io::Error::last_os_error());
    }
    Ok(())
}
fn unlink_at(directory: &File, member: &str) -> std::io::Result<()> {
    let member = name(member)?;
    if unsafe { libc::unlinkat(directory.as_raw_fd(), member.as_ptr(), 0) } < 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(())
}
