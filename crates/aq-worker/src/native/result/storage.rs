//! Retained descriptor-relative local channel storage. A path or JSON file is
//! never execution authority. The worker publishes control files after fsync.
use super::history::{DirectoryPin, StoragePins};
use anyhow::{Context, Result, ensure};
use aq_agent_result_local as wire;
use fs2::FileExt;
use std::{
    ffi::CStr,
    fs::{File, OpenOptions},
    io::{Read, Write},
    os::{
        fd::{AsRawFd, FromRawFd},
        unix::{
            ffi::OsStrExt,
            fs::{DirBuilderExt, MetadataExt, OpenOptionsExt},
        },
    },
    path::{Path, PathBuf},
    sync::{
        Mutex,
        atomic::{AtomicBool, Ordering},
    },
};

pub(super) struct ChannelStorage {
    root: File,
    submissions: File,
    root_path: PathBuf,
    submissions_path: PathBuf,
    pins: StoragePins,
    owner_lock: Mutex<Option<File>>,
    accepting: AtomicBool,
    pid: u32,
}
fn directory(path: &Path) -> Result<File> {
    ensure!(
        path.is_absolute() && path.canonicalize()? == path,
        "result directory route is not canonical"
    );
    Ok(OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_NONBLOCK | libc::O_CLOEXEC)
        .open(path)?)
}
fn pin(file: &File) -> Result<DirectoryPin> {
    let m = file.metadata()?;
    ensure!(
        m.is_dir() && m.uid() == unsafe { libc::geteuid() } && m.mode() & 0o7777 == 0o700,
        "result directory is not private"
    );
    Ok(DirectoryPin {
        device: m.dev(),
        inode: m.ino(),
    })
}
fn member(root: &File, name: &CStr, create: bool, mode: u32) -> Result<File> {
    let flags = if create {
        libc::O_WRONLY | libc::O_CREAT | libc::O_EXCL
    } else {
        libc::O_RDONLY
    };
    let fd = unsafe {
        libc::openat(
            root.as_raw_fd(),
            name.as_ptr(),
            flags | libc::O_NOFOLLOW | libc::O_NONBLOCK | libc::O_CLOEXEC,
            mode,
        )
    };
    ensure!(
        fd >= 0,
        "result member open: {}",
        std::io::Error::last_os_error()
    );
    let file = unsafe { File::from_raw_fd(fd) };
    let m = file.metadata()?;
    ensure!(
        m.is_file()
            && m.nlink() == 1
            && m.uid() == unsafe { libc::geteuid() }
            && m.mode() & 0o7777 == mode,
        "result member owner/mode/type"
    );
    Ok(file)
}
impl ChannelStorage {
    pub(super) fn create(root_path: &Path, workspace: &Path) -> Result<Self> {
        ensure!(
            root_path.is_absolute()
                && root_path.as_os_str().len() <= 2048
                && !root_path.as_os_str().as_bytes().contains(&0),
            "result root path bound"
        );
        let parent = root_path.parent().context("result root parent absent")?;
        let parent_owner = directory(parent)?;
        pin(&parent_owner)?;
        let workspace = workspace.canonicalize()?;
        ensure!(
            !root_path.starts_with(&workspace) && !workspace.starts_with(root_path),
            "result channel must be disjoint from captured workspace"
        );
        std::fs::DirBuilder::new().mode(0o700).create(root_path)?;
        parent_owner.sync_all()?;
        let root = directory(root_path)?;
        let submissions_path = root_path.join(wire::SUBMISSIONS_DIRECTORY);
        std::fs::DirBuilder::new()
            .mode(0o700)
            .create(&submissions_path)?;
        let submissions = directory(&submissions_path)?;
        member(&submissions, c"writer.lock", true, 0o600)?.sync_all()?;
        member(&root, c"owner.lock", true, 0o400)?.sync_all()?;
        submissions.sync_all()?;
        root.sync_all()?;
        let pins = StoragePins {
            root: pin(&root)?,
            submissions: pin(&submissions)?,
        };
        Ok(Self {
            root,
            submissions,
            root_path: root_path.into(),
            submissions_path,
            pins,
            owner_lock: Mutex::new(None),
            accepting: AtomicBool::new(false),
            pid: std::process::id(),
        })
    }
    pub(super) fn pins(&self) -> &StoragePins {
        &self.pins
    }
    pub(super) fn root(&self) -> &Path {
        &self.root_path
    }
    pub(super) fn submissions(&self) -> &Path {
        &self.submissions_path
    }
    pub(super) fn verify(&self) -> Result<()> {
        ensure!(
            self.pid == std::process::id(),
            "forked process cannot use result storage"
        );
        ensure!(
            pin(&self.root)? == self.pins.root && pin(&self.submissions)? == self.pins.submissions,
            "result retained directory identity changed"
        );
        ensure!(
            pin(&directory(&self.root_path)?)? == self.pins.root
                && pin(&directory(&self.submissions_path)?)? == self.pins.submissions,
            "result named directory route changed"
        );
        Ok(())
    }
    fn write_control_new(&self, name: &CStr, bytes: &[u8]) -> Result<()> {
        self.verify()?;
        ensure!(bytes.len() <= wire::MAX_BYTES, "result control bound");
        let mut file = member(&self.root, name, true, 0o400)?;
        file.write_all(bytes)?;
        file.sync_all()?;
        self.root.sync_all()?;
        Ok(())
    }
    pub(super) fn publish(&self, binding: &wire::Binding, presence: &wire::Presence) -> Result<()> {
        self.write_control_new(c"binding.json", &wire::encode(binding)?)?;
        self.write_control_new(c"presence.json", &wire::encode(presence)?)?;
        let file = member(&self.root, c"owner.lock", false, 0o400)?;
        file.try_lock_exclusive()
            .context("result intake already owned")?;
        let mut held = self
            .owner_lock
            .lock()
            .map_err(|_| anyhow::anyhow!("result lock poisoned"))?;
        ensure!(held.is_none(), "result intake already published");
        *held = Some(file);
        self.accepting.store(true, Ordering::SeqCst);
        Ok(())
    }
    pub(super) fn intake_open(&self) -> Result<bool> {
        self.verify()?;
        Ok(self.accepting.load(Ordering::SeqCst)
            && self
                .owner_lock
                .lock()
                .map_err(|_| anyhow::anyhow!("result lock poisoned"))?
                .is_some())
    }
    pub(super) fn close_intake(&self) {
        self.accepting.store(false, Ordering::SeqCst);
    }
    pub(super) fn release_presence(&self) {
        self.close_intake();
        if self.pid == std::process::id()
            && let Ok(mut held) = self.owner_lock.lock()
        {
            held.take();
        }
    }
    pub(super) fn verify_control(
        &self,
        binding: &wire::Binding,
        presence: &wire::Presence,
    ) -> Result<()> {
        self.verify()?;
        for (name, expected) in [
            (c"binding.json", wire::encode(binding)?),
            (c"presence.json", wire::encode(presence)?),
        ] {
            let file = member(&self.root, name, false, 0o400)?;
            ensure!(
                file.metadata()?.len() <= wire::MAX_BYTES as u64,
                "result control grew"
            );
            let mut bytes = Vec::new();
            file.take(wire::MAX_BYTES as u64 + 1)
                .read_to_end(&mut bytes)?;
            ensure!(bytes == expected, "result control bytes changed");
        }
        Ok(())
    }
    pub(super) fn read_submission(&self) -> Result<Option<Vec<u8>>> {
        self.verify()?;
        let fd = unsafe {
            libc::openat(
                self.submissions.as_raw_fd(),
                c"submission.json".as_ptr(),
                libc::O_RDONLY | libc::O_NOFOLLOW | libc::O_NONBLOCK | libc::O_CLOEXEC,
            )
        };
        if fd < 0 {
            let e = std::io::Error::last_os_error();
            if e.raw_os_error() == Some(libc::ENOENT) {
                return Ok(None);
            }
            return Err(e.into());
        }
        let mut file = unsafe { File::from_raw_fd(fd) };
        let before = file.metadata()?;
        ensure!(
            before.is_file()
                && before.nlink() == 1
                && before.uid() == unsafe { libc::geteuid() }
                && before.mode() & 0o7777 == 0o600
                && before.len() <= wire::MAX_BYTES as u64,
            "result submission owner/type/mode/bound"
        );
        let mut bytes = Vec::new();
        Read::by_ref(&mut file)
            .take(wire::MAX_BYTES as u64 + 1)
            .read_to_end(&mut bytes)?;
        let after = file.metadata()?;
        ensure!(
            bytes.len() == before.len() as usize
                && before.len() == after.len()
                && before.dev() == after.dev()
                && before.ino() == after.ino()
                && before.mtime() == after.mtime()
                && before.mtime_nsec() == after.mtime_nsec()
                && before.ctime() == after.ctime()
                && before.ctime_nsec() == after.ctime_nsec(),
            "result submission changed during read"
        );
        Ok(Some(bytes))
    }
    pub(super) fn write_ack(&self, ack: &wire::Acknowledgment) -> Result<()> {
        self.verify()?;
        let bytes = wire::encode(ack)?;
        // A previous ambiguous projection never changes the retained journal.
        // Refuse a leftover pending file rather than replacing its unknown bytes.
        let mut file = member(&self.root, c"ack.pending", true, 0o400)?;
        file.write_all(&bytes)?;
        file.sync_all()?;
        ensure!(
            unsafe {
                libc::renameat(
                    self.root.as_raw_fd(),
                    c"ack.pending".as_ptr(),
                    self.root.as_raw_fd(),
                    c"ack.json".as_ptr(),
                )
            } == 0,
            "result ack rename: {}",
            std::io::Error::last_os_error()
        );
        self.root.sync_all()?;
        Ok(())
    }
}
impl Drop for ChannelStorage {
    fn drop(&mut self) {
        self.release_presence();
    }
}
/// A process-retained private namespace. Existing names are never adopted on a
/// fresh owner; only this actual owner can allocate another attempt child.
pub(super) struct Namespace {
    owner: File,
    path: PathBuf,
    pin: DirectoryPin,
    pid: u32,
}
impl Namespace {
    pub(super) fn create(path: &Path) -> Result<Self> {
        let parent = path.parent().context("result namespace parent absent")?;
        let parent_owner = directory(parent)?;
        pin(&parent_owner)?;
        std::fs::DirBuilder::new().mode(0o700).create(path)?;
        parent_owner.sync_all()?;
        let owner = directory(path)?;
        let pinned = pin(&owner)?;
        owner.sync_all()?;
        Ok(Self {
            owner,
            path: path.into(),
            pin: pinned,
            pid: std::process::id(),
        })
    }
    pub(super) fn attempt(&self, path: &Path, id: uuid::Uuid) -> Result<PathBuf> {
        ensure!(
            !id.is_nil()
                && path == self.path
                && self.pid == std::process::id()
                && pin(&self.owner)? == self.pin
                && pin(&directory(&self.path)?)? == self.pin,
            "result namespace owner/route changed"
        );
        Ok(self.path.join(id.to_string()))
    }
}
