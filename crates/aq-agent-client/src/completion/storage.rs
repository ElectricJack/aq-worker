//! Retained descriptor-relative local channel storage. A path or JSON file is
//! never execution authority. The worker publishes control files after fsync.
use anyhow::{Context, Result, ensure};
use aq_agent_result_local as wire;
use fs2::FileExt;
use serde::{Deserialize, Serialize};
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

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DirectoryPin {
    pub device: u64,
    pub inode: u64,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StoragePins {
    pub root: DirectoryPin,
    pub submissions: DirectoryPin,
}
pub struct ChannelStorage {
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
    pub fn create(root_path: &Path, workspace: &Path) -> Result<Self> {
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
    pub fn pins(&self) -> &StoragePins {
        &self.pins
    }
    pub fn root(&self) -> &Path {
        &self.root_path
    }
    pub fn submissions(&self) -> &Path {
        &self.submissions_path
    }
    pub fn verify(&self) -> Result<()> {
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
    pub fn publish(&self, binding: &wire::Binding, presence: &wire::Presence) -> Result<()> {
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
    pub fn intake_open(&self) -> Result<bool> {
        self.verify()?;
        Ok(self.accepting.load(Ordering::SeqCst)
            && self
                .owner_lock
                .lock()
                .map_err(|_| anyhow::anyhow!("result lock poisoned"))?
                .is_some())
    }
    pub fn close_intake(&self) {
        self.accepting.store(false, Ordering::SeqCst);
    }
    pub fn release_presence(&self) {
        self.close_intake();
        if self.pid == std::process::id()
            && let Ok(mut held) = self.owner_lock.lock()
        {
            held.take();
        }
    }
    pub fn verify_control(&self, binding: &wire::Binding, presence: &wire::Presence) -> Result<()> {
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
    pub fn read_submission(&self) -> Result<Option<Vec<u8>>> {
        self.verify()?;
        super::read_submission_bytes(&self.submissions)
    }
    pub fn write_ack(&self, ack: &wire::Acknowledgment) -> Result<()> {
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
/// Read the original outbox after stopped recovery has taken the state lock.
pub fn retained_submission(
    path: &Path,
    pins: &StoragePins,
    binding: &wire::Binding,
) -> Result<Option<wire::Submission>> {
    let root = directory(path)?;
    let submissions = directory(&path.join(wire::SUBMISSIONS_DIRECTORY))?;
    ensure!(
        pin(&root)? == pins.root && pin(&submissions)? == pins.submissions,
        "retained result storage differs from the launch"
    );
    let writer = member(&submissions, c"writer.lock", false, 0o600)?;
    writer
        .try_lock_exclusive()
        .context("retained submission is still being written")?;
    let bytes = super::read_submission_bytes(&submissions)?;
    bytes
        .map(|bytes| {
            let s: wire::Submission = wire::decode(&bytes)?;
            ensure!(
                &s.binding == binding && wire::encode(&s)? == bytes,
                "retained result submission differs from the launch"
            );
            Ok(s)
        })
        .transpose()
}
impl Drop for ChannelStorage {
    fn drop(&mut self) {
        self.release_presence();
    }
}
/// A process-retained private namespace. Existing names are never adopted on a
/// fresh owner; only this actual owner can allocate another attempt child.
pub struct Namespace {
    owner: File,
    path: PathBuf,
    pin: DirectoryPin,
    pid: u32,
}
impl Namespace {
    pub fn create(path: &Path) -> Result<Self> {
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
    pub fn attempt(&self, path: &Path, id: uuid::Uuid) -> Result<PathBuf> {
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
