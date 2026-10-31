//! Private bounded owner journal. An uncertain write requires reopen; retained
//! temporary files are recovery holds, never guessed committed state.
use super::*;
use std::{
    fs::OpenOptions,
    io::{Read, Write},
    os::{
        fd::{AsRawFd, FromRawFd},
        unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt},
    },
};
const MAX: usize = 65536;
pub(super) struct IntentFiles {
    root: PathBuf,
    directory: File,
    lock: File,
    current: Option<File>,
    digest: Option<Digest>,
    pub monitor: Monitor,
    uncertain: Cell<bool>,
}
fn private(file: &File, directory: bool) -> Result<()> {
    let m = file.metadata()?;
    ensure!(
        m.uid() == unsafe { libc::geteuid() }
            && m.mode() & 0o077 == 0
            && if directory {
                m.is_dir()
            } else {
                m.is_file() && m.nlink() == 1
            },
        "private owner member required"
    );
    Ok(())
}
fn matches(path: &Path, file: &File) -> Result<()> {
    let a = std::fs::symlink_metadata(path)?;
    let b = file.metadata()?;
    ensure!(
        a.dev() == b.dev()
            && a.ino() == b.ino()
            && a.mode() == b.mode()
            && a.uid() == b.uid()
            && a.nlink() == b.nlink(),
        "owner member replaced"
    );
    Ok(())
}
fn open(path: &Path, write: bool, create: bool) -> Result<File> {
    let f = OpenOptions::new()
        .read(true)
        .write(write)
        .create_new(create)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC | libc::O_NONBLOCK)
        .open(path)?;
    private(&f, false)?;
    Ok(f)
}
fn bytes(file: &File) -> Result<Vec<u8>> {
    private(file, false)?;
    ensure!(file.metadata()?.len() <= MAX as u64, "owner journal bound");
    let mut reader = file.try_clone()?;
    use std::io::Seek;
    reader.rewind()?;
    let mut b = vec![];
    reader.take((MAX + 1) as u64).read_to_end(&mut b)?;
    ensure!(b.len() <= MAX, "owner journal bound");
    Ok(b)
}
impl IntentFiles {
    pub fn acquire(root: &Path, create: bool) -> Result<Self> {
        ensure!(root.is_absolute(), "absolute owner root");
        if create {
            std::fs::DirBuilder::new().mode(0o700).create(root)?;
            File::open(root.parent().context("owner parent")?)?.sync_all()?;
        }
        ensure!(std::fs::canonicalize(root)? == root, "canonical owner root");
        let directory = OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC)
            .open(root)?;
        private(&directory, true)?;
        for e in std::fs::read_dir(root)? {
            let e = e?;
            ensure!(
                matches!(
                    e.file_name().to_str(),
                    Some("owner.lock" | "intent.json" | "store")
                ),
                "unknown owner member retained; recovery required"
            );
        }
        let lock = if create {
            open(&root.join("owner.lock"), true, true)?
        } else {
            open(&root.join("owner.lock"), true, false)?
        };
        fs2::FileExt::try_lock_exclusive(&lock)?;
        directory.sync_all()?;
        let current = if create {
            ensure!(
                !root.join("intent.json").try_exists()?,
                "unexpected owner journal"
            );
            None
        } else {
            Some(open(&root.join("intent.json"), false, false)?)
        };
        let digest = current
            .as_ref()
            .map(|f| bytes(f).map(|b| Digest::of(&b)))
            .transpose()?;
        let this = Self {
            root: root.into(),
            directory,
            lock,
            current,
            digest,
            monitor: Monitor::default(),
            uncertain: Cell::new(false),
        };
        this.check()?;
        Ok(this)
    }
    pub fn check(&self) -> Result<()> {
        ensure!(
            !self.uncertain.get(),
            "owner intent uncertain; reopen required"
        );
        self.check_members()
    }
    fn check_members(&self) -> Result<()> {
        private(&self.directory, true)?;
        private(&self.lock, false)?;
        ensure!(
            std::fs::canonicalize(&self.root)? == self.root,
            "owner root changed"
        );
        matches(&self.root, &self.directory)?;
        matches(&self.root.join("owner.lock"), &self.lock)?;
        if let Some(file) = &self.current {
            matches(&self.root.join("intent.json"), file)?;
            ensure!(
                Some(Digest::of(&bytes(file)?)) == self.digest,
                "owner journal changed"
            );
        } else {
            ensure!(
                !self.root.join("intent.json").try_exists()?,
                "unexpected intent file"
            );
        }
        Ok(())
    }
    pub fn read<T: DeserializeOwned>(&self) -> Result<T> {
        self.check()?;
        decode(&bytes(
            self.current.as_ref().context("owner intent absent")?,
        )?)
    }
    fn open_member(&self, name: &std::ffi::CStr, write: bool, create: bool) -> Result<File> {
        let flags = libc::O_NOFOLLOW
            | libc::O_CLOEXEC
            | libc::O_NONBLOCK
            | if write { libc::O_RDWR } else { libc::O_RDONLY }
            | if create {
                libc::O_CREAT | libc::O_EXCL
            } else {
                0
            };
        let fd = unsafe { libc::openat(self.directory.as_raw_fd(), name.as_ptr(), flags, 0o600) };
        ensure!(
            fd >= 0,
            "owner member open: {}",
            std::io::Error::last_os_error()
        );
        let file = unsafe { File::from_raw_fd(fd) };
        private(&file, false)?;
        Ok(file)
    }
    pub(super) fn step<T>(&self, f: impl FnOnce() -> Result<T>) -> Result<T> {
        self.monitor.boundary(false)?;
        let value = f()?;
        self.monitor.boundary(true)?;
        Ok(value)
    }
    pub fn persist<T: Serialize>(&mut self, value: &T) -> Result<()> {
        self.check()?;
        let b = encode(value)?;
        ensure!(b.len() <= MAX, "owner intent bound");
        self.uncertain.set(true);
        let mut file = self.step(|| self.open_member(c"intent.pending", true, true))?;
        self.step(|| {
            file.write_all(&b)?;
            Ok(())
        })?;
        self.step(|| {
            file.sync_all()?;
            Ok(())
        })?;
        // Held root and lock identity are rechecked without consulting uncertain
        // state. No external IO or credentials occur inside this critical step.
        self.check_members()?;
        matches(&self.root.join("intent.pending"), &file)?;
        ensure!(
            Digest::of(&bytes(&file)?) == Digest::of(&b),
            "pending intent replaced"
        );
        self.step(|| {
            let result = unsafe {
                libc::renameat(
                    self.directory.as_raw_fd(),
                    c"intent.pending".as_ptr(),
                    self.directory.as_raw_fd(),
                    c"intent.json".as_ptr(),
                )
            };
            ensure!(
                result == 0,
                "intent rename: {}",
                std::io::Error::last_os_error()
            );
            Ok(())
        })?;
        self.step(|| {
            self.directory.sync_all()?;
            Ok(())
        })?;
        self.current = Some(self.open_member(c"intent.json", false, false)?);
        self.digest = Some(Digest::of(&b));
        self.uncertain.set(false);
        self.check()
    }
    pub fn store_path(&self) -> PathBuf {
        self.root.join("store")
    }
}
