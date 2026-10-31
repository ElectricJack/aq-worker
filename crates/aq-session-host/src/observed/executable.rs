use crate::*;
use sha2::{Digest as _, Sha256};
use std::{
    collections::BTreeMap,
    os::{
        fd::{AsRawFd, FromRawFd},
        unix::fs::FileExt,
    },
    sync::{Arc, Mutex},
};
pub const MAX_ELF_BYTES: u64 = 4 * 1024 * 1024;
pub const MAX_ELF_POOL_BYTES: u64 = 16 * 1024 * 1024;
pub const MAX_ELF_POOL_ENTRIES: usize = 4;
const SCRATCH: usize = 64 * 1024;
const SEALS: i32 = libc::F_SEAL_WRITE | libc::F_SEAL_GROW | libc::F_SEAL_SHRINK | libc::F_SEAL_SEAL;
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ElfIdentity {
    pub artifact: ArtifactRef,
    pub bytes: u64,
    pub device: u64,
    pub inode: u64,
}
struct Image {
    identity: ElfIdentity,
    source: PathBuf,
    file: File,
}
struct PoolState {
    bytes: u64,
    entries: BTreeMap<(String, String, String), Arc<Image>>,
}
struct Pool {
    pid: u32,
    bytes: u64,
    entries: usize,
    state: Mutex<PoolState>,
}
/// Explicit process-local immutable image store. No eviction or implicit budget
/// reset. Runtime admission must separately account for old owners after crash.
#[derive(Clone)]
pub struct SealedElfPool {
    inner: Arc<Pool>,
}
/// Only constructed after bounded full-byte verification and kernel sealing.
/// Handles reuse the actual immutable object, not mutable path metadata.
pub struct QualifiedElf {
    image: Arc<Image>,
    _pool: Arc<Pool>,
}
impl std::fmt::Debug for QualifiedElf {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("QualifiedElf")
            .field("artifact", &self.image.identity.artifact)
            .field("bytes", &self.image.identity.bytes)
            .finish_non_exhaustive()
    }
}
impl SealedElfPool {
    pub fn new(bytes: u64, entries: usize) -> Result<Self> {
        ensure!(
            bytes > 0
                && bytes <= MAX_ELF_POOL_BYTES
                && entries > 0
                && entries <= MAX_ELF_POOL_ENTRIES,
            "sealed ELF pool bounds"
        );
        Ok(Self {
            inner: Arc::new(Pool {
                pid: std::process::id(),
                bytes,
                entries,
                state: Mutex::new(PoolState {
                    bytes: 0,
                    entries: BTreeMap::new(),
                }),
            }),
        })
    }
    pub fn load_verified(&self, source: &Path, expected: &ArtifactRef) -> Result<QualifiedElf> {
        ensure!(
            std::process::id() == self.inner.pid,
            "sealed ELF pool belongs to original process"
        );
        expected.id.validate()?;
        expected.version.validate()?;
        ensure!(
            source.is_absolute() && std::fs::canonicalize(source)? == source,
            "ELF source must be canonical"
        );
        let key = (
            expected.id.as_str().to_owned(),
            expected.version.as_str().to_owned(),
            expected.digest.as_str().to_owned(),
        );
        let mut state = self
            .inner
            .state
            .try_lock()
            .map_err(|_| anyhow::anyhow!("sealed ELF pool busy or poisoned"))?;
        if let Some(image) = state.entries.get(&key) {
            ensure!(image.source == source, "sealed ELF source binding changed");
            verify_file(&image.file, &image.identity)?;
            return Ok(QualifiedElf {
                image: image.clone(),
                _pool: self.inner.clone(),
            });
        }
        ensure!(
            state.entries.len() < self.inner.entries,
            "sealed ELF entry quota"
        );
        let mut source_file = OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK | libc::O_CLOEXEC)
            .open(source)?;
        let metadata = source_file.metadata()?;
        ensure!(
            metadata.is_file() && metadata.len() >= 64 && metadata.len() <= MAX_ELF_BYTES,
            "small ELF byte bound"
        );
        let reserved = state
            .bytes
            .checked_add(metadata.len())
            .context("ELF quota overflow")?;
        ensure!(reserved <= self.inner.bytes, "sealed ELF byte quota");
        let fd = unsafe {
            libc::memfd_create(
                c"aq-observed-elf".as_ptr(),
                libc::MFD_CLOEXEC | libc::MFD_ALLOW_SEALING | libc::MFD_EXEC,
            )
        };
        ensure!(fd >= 0, "executable memfd unavailable");
        let mut file = unsafe { File::from_raw_fd(fd) };
        file.set_permissions(std::fs::Permissions::from_mode(0o500))?;
        let mut hash = Sha256::new();
        let mut written = 0u64;
        let mut scratch = [0u8; SCRATCH];
        let mut header = Vec::new();
        loop {
            let count = source_file.read(&mut scratch)?;
            if count == 0 {
                break;
            }
            written = written
                .checked_add(count as u64)
                .context("ELF length overflow")?;
            ensure!(
                written <= metadata.len() && written <= MAX_ELF_BYTES,
                "ELF source changed size"
            );
            if header.len() < 64 {
                let take = (64 - header.len()).min(count);
                header.extend_from_slice(&scratch[..take]);
            }
            hash.update(&scratch[..count]);
            file.write_all(&scratch[..count])?;
        }
        ensure!(
            written == metadata.len()
                && format!("{:x}", hash.finalize()) == expected.digest.as_str(),
            "ELF exact bytes mismatch"
        );
        ensure!(
            header.get(..4) == Some(b"\x7fELF")
                && header[4] == 2
                && header[5] == 1
                && header[6] == 1,
            "unsupported ELF header"
        );
        #[cfg(target_arch = "x86_64")]
        ensure!(
            u16::from_le_bytes([header[18], header[19]]) == 62,
            "ELF architecture mismatch"
        );
        #[cfg(target_arch = "aarch64")]
        ensure!(
            u16::from_le_bytes([header[18], header[19]]) == 183,
            "ELF architecture mismatch"
        );
        ensure!(
            matches!(u16::from_le_bytes([header[16], header[17]]), 2 | 3),
            "ELF executable type"
        );
        ensure!(
            unsafe { libc::fcntl(file.as_raw_fd(), libc::F_ADD_SEALS, SEALS) } == 0,
            "ELF immutable seals unavailable"
        );
        let m = file.metadata()?;
        let identity = ElfIdentity {
            artifact: expected.clone(),
            bytes: written,
            device: m.dev(),
            inode: m.ino(),
        };
        verify_file(&file, &identity)?;
        let image = Arc::new(Image {
            identity,
            source: source.into(),
            file,
        });
        state.bytes = reserved;
        state.entries.insert(key, image.clone());
        Ok(QualifiedElf {
            image,
            _pool: self.inner.clone(),
        })
    }
    pub fn charged_bytes(&self) -> Result<u64> {
        ensure!(
            std::process::id() == self.inner.pid,
            "sealed ELF owner changed"
        );
        Ok(self
            .inner
            .state
            .try_lock()
            .map_err(|_| anyhow::anyhow!("sealed ELF pool busy or poisoned"))?
            .bytes)
    }
}
impl QualifiedElf {
    pub fn artifact(&self) -> &ArtifactRef {
        &self.image.identity.artifact
    }
    pub fn bytes(&self) -> u64 {
        self.image.identity.bytes
    }
    pub(crate) fn identity(&self) -> &ElfIdentity {
        &self.image.identity
    }
    pub(crate) fn source(&self) -> &Path {
        &self.image.source
    }
    pub(crate) fn file(&self) -> &File {
        &self.image.file
    }
}
pub(crate) fn verify_file(file: &File, expected: &ElfIdentity) -> Result<()> {
    let m = file.metadata()?;
    ensure!(
        m.is_file()
            && m.nlink() == 0
            && m.dev() == expected.device
            && m.ino() == expected.inode
            && m.len() == expected.bytes
            && (64..=MAX_ELF_BYTES).contains(&m.len()),
        "sealed ELF identity changed"
    );
    let seals = unsafe { libc::fcntl(file.as_raw_fd(), libc::F_GET_SEALS) };
    ensure!(
        seals >= 0 && seals & SEALS == SEALS,
        "ELF must have immutable kernel seals"
    );
    let mut hash = Sha256::new();
    let mut offset = 0;
    let mut scratch = [0u8; SCRATCH];
    while offset < expected.bytes {
        let count = file.read_at(
            &mut scratch[..(expected.bytes - offset).min(SCRATCH as u64) as usize],
            offset,
        )?;
        ensure!(count > 0, "sealed ELF truncated");
        hash.update(&scratch[..count]);
        offset += count as u64;
    }
    ensure!(
        format!("{:x}", hash.finalize()) == expected.artifact.digest.as_str(),
        "sealed ELF content mismatch"
    );
    Ok(())
}
#[cfg(test)]
mod tests {
    use super::*;
    use aq_session_protocol::Text;
    fn bytes() -> Vec<u8> {
        let mut b = vec![0; 128];
        b[..7].copy_from_slice(b"\x7fELF\x02\x01\x01");
        b[16] = 2;
        b[18] = if cfg!(target_arch = "aarch64") {
            183
        } else {
            62
        };
        b
    }
    fn reference(b: &[u8]) -> ArtifactRef {
        ArtifactRef {
            id: Text::new("synthetic.elf").unwrap(),
            version: Text::new("1").unwrap(),
            digest: Digest::of(b),
        }
    }
    #[test]
    fn sealed_actual_bytes_survive_path_change_and_cannot_be_written_or_grown() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("elf");
        let bytes = bytes();
        std::fs::write(&path, &bytes).unwrap();
        let pool = SealedElfPool::new(1024, 2).unwrap();
        let image = pool.load_verified(&path, &reference(&bytes)).unwrap();
        assert_eq!(pool.charged_bytes().unwrap(), 128);
        assert!(image.file().write_at(b"x", 0).is_err());
        assert!(image.file().set_len(129).is_err());
        std::fs::write(&path, b"mutated backing path").unwrap();
        let again = pool.load_verified(&path, &reference(&bytes)).unwrap();
        assert_eq!(image.identity(), again.identity());
        verify_file(again.file(), again.identity()).unwrap();
        assert_eq!(pool.charged_bytes().unwrap(), 128);
    }
    #[test]
    fn byte_entry_digest_header_and_nonregular_sources_refuse_before_admission() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("elf");
        let bytes = bytes();
        std::fs::write(&path, &bytes).unwrap();
        let pool = SealedElfPool::new(127, 1).unwrap();
        assert!(pool.load_verified(&path, &reference(&bytes)).is_err());
        assert_eq!(pool.charged_bytes().unwrap(), 0);
        let pool = SealedElfPool::new(1024, 1).unwrap();
        let mut bad = reference(&bytes);
        bad.digest = Digest::of(b"wrong");
        assert!(pool.load_verified(&path, &bad).is_err());
        assert_eq!(pool.charged_bytes().unwrap(), 0);
        pool.load_verified(&path, &reference(&bytes)).unwrap();
        let mut second = reference(&bytes);
        second.id = Text::new("other").unwrap();
        assert!(pool.load_verified(&path, &second).is_err());
        let link = dir.path().join("link");
        std::os::unix::fs::symlink(&path, &link).unwrap();
        assert!(pool.load_verified(&link, &reference(&bytes)).is_err());
    }
}
