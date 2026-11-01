//! One closed derived Codex auth input, pinned by the existing host intent.
//! Private bytes are never serialized in launch metadata or command arguments.
use super::*;
use std::os::fd::{AsRawFd, FromRawFd};
use zeroize::Zeroizing;
const MAX_MATERIAL: u64 = 65536;
#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct SourcePin {
    owner: ProcessEvidence,
    directory_fd: i32,
    device: u64,
    inode: u64,
    bytes: u64,
    digest: Digest,
}
#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct PrivateInputRecord {
    source: SourcePin,
    home_device: u64,
    home_inode: u64,
    auth_device: u64,
    auth_inode: u64,
    auth_bytes: u64,
    auth_digest: Digest,
}
impl std::fmt::Debug for PrivateInputRecord {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("PrivateInputRecord(<private pins>)")
    }
}
/// Constructed from an actual retained material directory. Keeping this input
/// alive keeps its descriptor pin valid; it grants no material/exec permission.
pub struct PrivateLaunchInput {
    source: File,
    pin: SourcePin,
    auth: Zeroizing<Vec<u8>>,
}
fn directory(file: &File, name: &std::ffi::CStr) -> Result<File> {
    let fd = unsafe {
        libc::openat(
            file.as_raw_fd(),
            name.as_ptr(),
            libc::O_RDONLY | libc::O_DIRECTORY | libc::O_CLOEXEC | libc::O_NOFOLLOW,
        )
    };
    ensure!(fd >= 0, "private input directory unavailable");
    Ok(unsafe { File::from_raw_fd(fd) })
}
fn private_directory(file: &File) -> Result<std::fs::Metadata> {
    let m = file.metadata()?;
    ensure!(
        m.is_dir() && m.uid() == unsafe { libc::geteuid() } && m.mode() & 0o7777 == 0o700,
        "private input directory identity/mode"
    );
    Ok(m)
}
fn read_member(
    dir: &File,
    name: &std::ffi::CStr,
    limit: u64,
) -> Result<(Zeroizing<Vec<u8>>, std::fs::Metadata)> {
    let fd = unsafe {
        libc::openat(
            dir.as_raw_fd(),
            name.as_ptr(),
            libc::O_RDONLY | libc::O_CLOEXEC | libc::O_NOFOLLOW | libc::O_NONBLOCK,
        )
    };
    ensure!(fd >= 0, "private input file unavailable");
    let mut file = unsafe { File::from_raw_fd(fd) };
    let m = file.metadata()?;
    ensure!(
        m.is_file()
            && m.uid() == unsafe { libc::geteuid() }
            && m.mode() & 0o7777 == 0o600
            && m.nlink() == 1
            && m.len() > 0
            && m.len() <= limit,
        "private input file identity/mode/bound"
    );
    let mut bytes = Zeroizing::new(Vec::with_capacity(m.len() as usize));
    (&mut file).take(limit + 1).read_to_end(&mut bytes)?;
    ensure!(
        bytes.len() as u64 == m.len() && file.metadata()?.len() == m.len(),
        "private input changed during read"
    );
    Ok((bytes, m))
}
fn read_source(pin: &SourcePin) -> Result<Zeroizing<Vec<u8>>> {
    let live = ownership::verified_pidfd(&pin.owner)?;
    ensure!(
        ownership::pidfd_is_live(&live)?
            && pin.directory_fd >= 3
            && pin.bytes > 0
            && pin.bytes <= MAX_MATERIAL,
        "private source owner/bytes"
    );
    let path = format!("/proc/{}/fd/{}", pin.owner.pid, pin.directory_fd);
    // This is the exact verified process's fd namespace, not a caller-chosen
    // filesystem symlink. The opened directory must match the retained pin.
    let dir = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_DIRECTORY | libc::O_CLOEXEC)
        .open(path)?;
    let m = private_directory(&dir)?;
    ensure!(
        m.dev() == pin.device && m.ino() == pin.inode,
        "private source descriptor changed"
    );
    let (bytes, _) = read_member(&dir, c"material.bin", MAX_MATERIAL)?;
    ensure!(
        bytes.len() as u64 == pin.bytes
            && Digest::of(&bytes) == pin.digest
            && ownership::pidfd_is_live(&live)?,
        "private original material changed"
    );
    Ok(bytes)
}
fn derive_auth(key: &[u8]) -> Result<Zeroizing<Vec<u8>>> {
    ensure!(
        !key.is_empty() && key.len() <= 4096 && key.iter().all(|b| b.is_ascii_graphic()),
        "Codex API key byte format"
    );
    let mut out = Zeroizing::new(Vec::with_capacity(key.len() + 32));
    out.extend_from_slice(b"{\"OPENAI_API_KEY\":");
    serde_json::to_writer(&mut *out, std::str::from_utf8(key)?)?;
    out.push(b'}');
    Ok(out)
}
impl PrivateLaunchInput {
    pub fn codex_api_key(source: File, digest: Digest, bytes: u64) -> Result<Self> {
        let m = private_directory(&source)?;
        let pin = SourcePin {
            owner: ownership::process_evidence(std::process::id())?,
            directory_fd: source.as_raw_fd(),
            device: m.dev(),
            inode: m.ino(),
            bytes,
            digest,
        };
        let key = read_source(&pin)?;
        let auth = derive_auth(&key)?;
        Ok(Self { source, pin, auth })
    }
}
/// Empty files are reserved first; the record enters the existing host intent
/// before publication. This owner never adopts an existing auth directory.
pub(crate) struct Publication {
    input: PrivateLaunchInput,
    root: File,
    home: File,
    auth: File,
    record: PrivateInputRecord,
}
impl Publication {
    pub(crate) fn reserve(root: &Path, input: PrivateLaunchInput) -> Result<Self> {
        let root = OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_DIRECTORY | libc::O_CLOEXEC | libc::O_NOFOLLOW)
            .open(root)?;
        private_directory(&root)?;
        ensure!(
            unsafe { libc::mkdirat(root.as_raw_fd(), c"codex-home".as_ptr(), 0o700) } == 0,
            "private home already exists or cannot be reserved"
        );
        let home = directory(&root, c"codex-home")?;
        let h = private_directory(&home)?;
        let fd = unsafe {
            libc::openat(
                home.as_raw_fd(),
                c"auth.json".as_ptr(),
                libc::O_WRONLY | libc::O_CREAT | libc::O_EXCL | libc::O_NOFOLLOW | libc::O_CLOEXEC,
                0o600,
            )
        };
        ensure!(fd >= 0, "private auth reservation refused");
        let auth = unsafe { File::from_raw_fd(fd) };
        let a = auth.metadata()?;
        let record = PrivateInputRecord {
            source: input.pin.clone(),
            home_device: h.dev(),
            home_inode: h.ino(),
            auth_device: a.dev(),
            auth_inode: a.ino(),
            auth_bytes: input.auth.len() as u64,
            auth_digest: Digest::of(&input.auth),
        };
        home.sync_all()?;
        root.sync_all()?;
        Ok(Self {
            input,
            root,
            home,
            auth,
            record,
        })
    }
    pub(crate) fn record(&self) -> PrivateInputRecord {
        self.record.clone()
    }
    pub(crate) fn publish(mut self) -> Result<File> {
        ensure!(
            derive_auth(&read_source(&self.input.pin)?)?.as_slice() == self.input.auth.as_slice(),
            "private input changed before publication"
        );
        self.auth.write_all(&self.input.auth)?;
        self.auth.sync_all()?;
        self.home.sync_all()?;
        self.root.sync_all()?;
        verify_record(&self.root, &self.record, true)?;
        Ok(self.input.source)
    }
}
fn verify_record(root: &File, r: &PrivateInputRecord, original: bool) -> Result<()> {
    let home = directory(root, c"codex-home")?;
    let h = private_directory(&home)?;
    ensure!(
        h.dev() == r.home_device && h.ino() == r.home_inode,
        "private home replaced"
    );
    let (bytes, a) = read_member(&home, c"auth.json", 8192)?;
    ensure!(
        a.dev() == r.auth_device
            && a.ino() == r.auth_inode
            && bytes.len() as u64 == r.auth_bytes
            && Digest::of(&bytes) == r.auth_digest,
        "private auth replaced or changed"
    );
    if original {
        ensure!(
            derive_auth(&read_source(&r.source)?)?.as_slice() == bytes.as_slice(),
            "private auth/original material mismatch"
        );
    }
    Ok(())
}
pub(crate) fn verify_at_exec(config: &SealedLaunch, root: &Path) -> Result<()> {
    if let Some(r) = &config.private_input {
        let spec = config
            .observed
            .as_ref()
            .context("private inputs require observed native profile")?;
        ensure!(
            spec.native_exec()
                && r.source.owner == spec.owner
                && config.command.environment.get("CODEX_HOME")
                    == Some(
                        &root
                            .join("codex-home")
                            .to_str()
                            .context("private home UTF8")?
                            .to_owned()
                    ),
            "private input exact native owner/home"
        );
        let root = OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_DIRECTORY | libc::O_CLOEXEC | libc::O_NOFOLLOW)
            .open(root)?;
        private_directory(&root)?;
        verify_record(&root, r, true)?;
    }
    Ok(())
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PrivateCleanup {
    pub auth_removed: bool,
    pub home_removed: bool,
}
impl SessionHandle {
    pub fn cleanup_private_inputs(&mut self, proof: &StoppedProof) -> Result<PrivateCleanup> {
        ensure!(
            proof.guardian() == &self.identity
                && proof.observation().phase == HostPhase::Stopped
                && proof.observation().kernel_wait_echild,
            "private cleanup requires exact ECHILD proof"
        );
        let config: SealedLaunch = read_json(&self.directory.join("launch.json"))?;
        ensure!(
            config.intent()? == self.identity.intent,
            "private cleanup configuration changed"
        );
        let Some(r) = config.private_input else {
            return Ok(PrivateCleanup {
                auth_removed: false,
                home_removed: false,
            });
        };
        let root = OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_DIRECTORY | libc::O_CLOEXEC | libc::O_NOFOLLOW)
            .open(&self.directory)?;
        private_directory(&root)?;
        match std::fs::symlink_metadata(self.directory.join("codex-home")) {
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                self.private_source.take();
                return Ok(PrivateCleanup {
                    auth_removed: true,
                    home_removed: true,
                });
            }
            Err(e) => return Err(e.into()),
            Ok(_) => {}
        }
        let home = directory(&root, c"codex-home")?;
        let h = private_directory(&home)?;
        ensure!(
            h.dev() == r.home_device && h.ino() == r.home_inode,
            "private cleanup home replaced"
        );
        let auth_exists =
            match std::fs::symlink_metadata(self.directory.join("codex-home/auth.json")) {
                Ok(_) => true,
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => false,
                Err(e) => return Err(e.into()),
            };
        if auth_exists {
            verify_record(&root, &r, false)?;
            ensure!(
                unsafe { libc::unlinkat(home.as_raw_fd(), c"auth.json".as_ptr(), 0) } == 0,
                "private auth cleanup failed"
            );
            home.sync_all()?;
        }
        let removed =
            unsafe { libc::unlinkat(root.as_raw_fd(), c"codex-home".as_ptr(), libc::AT_REMOVEDIR) }
                == 0;
        if !removed {
            ensure!(
                std::io::Error::last_os_error().raw_os_error() == Some(libc::ENOTEMPTY),
                "private home cleanup failed"
            );
        }
        root.sync_all()?;
        self.private_source.take();
        Ok(PrivateCleanup {
            auth_removed: true,
            home_removed: removed,
        })
    }
}
