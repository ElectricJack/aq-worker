//! Private descriptor-relative staging. Unknown bytes and directories are held.
use crate::source::GitManifest;
use anyhow::{Context, Result, ensure};
use aq_worker_protocol::{TransferFile, TransferManifest};
use fs2::FileExt;
use sha2::{Digest as _, Sha256};
use std::{
    ffi::CString,
    fs::{File, Metadata, OpenOptions},
    io::{Read, Seek, SeekFrom, Write},
    os::{
        fd::{AsRawFd, FromRawFd},
        unix::fs::{MetadataExt, OpenOptionsExt},
    },
    path::{Path, PathBuf},
};
fn child(parent: &File, name: &str, create: bool) -> Result<File> {
    let name = CString::new(name)?;
    if create {
        let rc = unsafe { libc::mkdirat(parent.as_raw_fd(), name.as_ptr(), 0o700) };
        ensure!(
            rc == 0 || std::io::Error::last_os_error().raw_os_error() == Some(libc::EEXIST),
            "native source mkdir"
        );
        parent.sync_all()?;
    }
    let fd = unsafe {
        libc::openat(
            parent.as_raw_fd(),
            name.as_ptr(),
            libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
        )
    };
    ensure!(fd >= 0, "native source directory unavailable");
    let f = unsafe { File::from_raw_fd(fd) };
    private(&f)?;
    Ok(f)
}
fn private(f: &File) -> Result<()> {
    let m = f.metadata()?;
    ensure!(
        m.uid() == unsafe { libc::geteuid() } && m.mode() & 0o077 == 0,
        "native source private owner required"
    );
    Ok(())
}
fn member(dir: &File, name: &str, create: bool) -> Result<File> {
    let n = CString::new(name)?;
    let flags = if create {
        libc::O_RDWR | libc::O_CREAT
    } else {
        libc::O_RDONLY
    };
    let fd = unsafe {
        libc::openat(
            dir.as_raw_fd(),
            n.as_ptr(),
            flags | libc::O_NOFOLLOW | libc::O_NONBLOCK | libc::O_CLOEXEC,
            0o600,
        )
    };
    ensure!(fd >= 0, "native source member unavailable");
    let f = unsafe { File::from_raw_fd(fd) };
    private(&f)?;
    let m = f.metadata()?;
    ensure!(
        m.is_file() && m.nlink() == 1,
        "native source regular exclusive member required"
    );
    if create {
        dir.sync_all()?
    }
    Ok(f)
}
fn same_file(a: &File, b: &File) -> Result<()> {
    let a = a.metadata()?;
    let b = b.metadata()?;
    ensure!(
        a.dev() == b.dev() && a.ino() == b.ino(),
        "native source owner path replaced"
    );
    Ok(())
}
fn unchanged(before: &Metadata, file: &File) -> Result<()> {
    let after = file.metadata()?;
    ensure!(
        before.dev() == after.dev()
            && before.ino() == after.ino()
            && before.len() == after.len()
            && before.mtime() == after.mtime()
            && before.mtime_nsec() == after.mtime_nsec()
            && before.ctime() == after.ctime()
            && before.ctime_nsec() == after.ctime_nsec(),
        "native source member mutated while reading"
    );
    Ok(())
}
fn hash_exact(reader: &mut impl Read, expected: u64) -> Result<String> {
    ensure!(
        expected > 0 && expected <= 128 * 1024 * 1024,
        "native source streaming byte bound"
    );
    let mut hash = Sha256::new();
    let mut remaining = expected;
    let mut buffer = [0; 65536];
    while remaining > 0 {
        let limit = remaining.min(buffer.len() as u64) as usize;
        let n = reader.read(&mut buffer[..limit])?;
        ensure!(n > 0, "native source bundle truncated");
        hash.update(&buffer[..n]);
        remaining -= n as u64;
    }
    let mut extra = [0];
    ensure!(
        reader.read(&mut extra)? == 0,
        "native source bundle grew beyond admitted bound"
    );
    Ok(format!("{:x}", hash.finalize()))
}
pub(crate) struct Staging {
    root: File,
    path: PathBuf,
    slot: File,
    incoming: File,
    artifact: String,
    _lock: File,
    owner_pid: u32,
}
impl Staging {
    pub(crate) fn open(root: &Path, manifest: &TransferManifest, owner: &[u8]) -> Result<Self> {
        ensure!(owner.len() <= 16384, "native source owner marker bound");
        let rootfd = OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC)
            .open(root)?;
        private(&rootfd)?;
        let stages = child(&rootfd, ".native-inputs", true)?;
        let artifact = manifest.artifact_id.to_string();
        let slot = child(&stages, &artifact, true)?;
        let artifacts = child(&rootfd, "artifacts", true)?;
        let incoming = child(&artifacts, "incoming", true)?;
        let name = CString::new("owner.json")?;
        let fd = unsafe {
            libc::openat(
                slot.as_raw_fd(),
                name.as_ptr(),
                libc::O_RDWR
                    | libc::O_CREAT
                    | libc::O_EXCL
                    | libc::O_NOFOLLOW
                    | libc::O_NONBLOCK
                    | libc::O_CLOEXEC,
                0o600,
            )
        };
        let fresh = fd >= 0;
        let mut lock = if fresh {
            unsafe { File::from_raw_fd(fd) }
        } else {
            ensure!(
                std::io::Error::last_os_error().raw_os_error() == Some(libc::EEXIST),
                "native source owner creation failed"
            );
            member(&slot, "owner.json", false)?
        };
        FileExt::try_lock_exclusive(&lock).context("native source already owned")?;
        if fresh {
            ensure!(
                child(&incoming, &artifact, false).is_err(),
                "unowned published native source"
            );
            ensure!(
                child(&slot, "download", false).is_err(),
                "unowned native source staging"
            );
            lock.write_all(owner)?;
            lock.sync_all()?;
            slot.sync_all()?
        } else {
            ensure!(
                lock.metadata()?.len() == owner.len() as u64,
                "native source owner marker changed or incomplete"
            );
            let mut old = Vec::new();
            (&lock).take(16385).read_to_end(&mut old)?;
            ensure!(old == owner, "native source owner marker changed")
        };
        Ok(Self {
            root: rootfd,
            path: root.to_owned(),
            slot,
            incoming,
            artifact,
            _lock: lock,
            owner_pid: std::process::id(),
        })
    }
    fn check(&self) -> Result<()> {
        ensure!(
            self.owner_pid == std::process::id(),
            "native source owner process changed"
        );
        let named = OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC)
            .open(&self.path)?;
        same_file(&self.root, &named)?;
        let stages = child(&named, ".native-inputs", false)?;
        let slot = child(&stages, &self.artifact, false)?;
        same_file(&self.slot, &slot)?;
        same_file(&self._lock, &member(&slot, "owner.json", false)?)?;
        let artifacts = child(&named, "artifacts", false)?;
        same_file(&self.incoming, &child(&artifacts, "incoming", false)?)?;
        Ok(())
    }
    fn directory(&self, write: bool) -> Result<File> {
        self.check()?;
        match child(&self.incoming, &self.artifact, false) {
            Ok(dir) => Ok(dir),
            Err(_) => child(&self.slot, "download", write),
        }
    }
    pub(crate) fn append(&self, file: &TransferFile, offset: u64, bytes: &[u8]) -> Result<()> {
        self.check()?;
        ensure!(
            !bytes.is_empty() && bytes.len() <= 1048576,
            "native source chunk bound"
        );
        let dir = self.directory(true)?;
        let name = match file {
            TransferFile::Manifest => "manifest.json",
            TransferFile::Bundle => "objects.bundle",
        };
        let mut f = member(&dir, name, true)?;
        let len = f.metadata()?.len();
        let end = offset
            .checked_add(bytes.len() as u64)
            .context("native source offset overflow")?;
        ensure!(
            len >= offset && len <= end,
            "native source unowned tail or missing committed prefix"
        );
        f.seek(SeekFrom::Start(offset))?;
        if len > offset {
            let mut old = vec![0; (len - offset) as usize];
            f.read_exact(&mut old)?;
            ensure!(
                bytes.starts_with(&old),
                "native source unknown tail changed"
            );
        }
        f.seek(SeekFrom::Start(len))?;
        f.write_all(&bytes[(len - offset) as usize..])?;
        f.sync_all()?;
        dir.sync_all()?;
        self.check()?;
        same_file(&dir, &self.directory(false)?)?;
        same_file(&f, &member(&dir, name, false)?)?;
        Ok(())
    }
    pub(crate) fn verify(&self, m: &TransferManifest) -> Result<GitManifest> {
        let dir = self.directory(false)?;
        ensure!(
            (1..=1048576).contains(&m.manifest_bytes),
            "native source manifest byte bound"
        );
        let mut manifest_file = member(&dir, "manifest.json", false)?;
        let manifest_before = manifest_file.metadata()?;
        let mut bytes = Vec::new();
        (&mut manifest_file).take(1048577).read_to_end(&mut bytes)?;
        unchanged(&manifest_before, &manifest_file)?;
        same_file(&manifest_file, &member(&dir, "manifest.json", false)?)?;
        ensure!(
            bytes.len() as u64 == m.manifest_bytes
                && format!("{:x}", Sha256::digest(&bytes)) == m.manifest_digest,
            "native source manifest digest"
        );
        let native: GitManifest = serde_json::from_slice(&bytes)?;
        ensure!(
            native.format_version == 1
                && native.repository_id == m.source.repository_id
                && native.repository_identity == m.source.logical_identity
                && native.binding_generation == m.source.binding_generation
                && native.transfer_id == m.artifact_id
                && native.object_format == "sha1"
                && native.base == m.source.base
                && native.source == m.checkpoint
                && native.bundle_size == m.bundle_bytes
                && native.bundle_sha256 == m.bundle_digest
                && !native.reference_only,
            "native source manifest originals"
        );
        let mut bundle = member(&dir, "objects.bundle", false)?;
        ensure!(
            bundle.metadata()?.len() == m.bundle_bytes,
            "native source bundle length"
        );
        let bundle_before = bundle.metadata()?;
        let actual_digest = hash_exact(&mut bundle, m.bundle_bytes)?;
        unchanged(&bundle_before, &bundle)?;
        same_file(&bundle, &member(&dir, "objects.bundle", false)?)?;
        unchanged(&manifest_before, &manifest_file)?;
        same_file(&manifest_file, &member(&dir, "manifest.json", false)?)?;
        same_file(&dir, &self.directory(false)?)?;
        self.check()?;
        ensure!(
            Some(actual_digest) == m.bundle_digest,
            "native source bundle digest"
        );
        Ok(native)
    }
    pub(crate) fn publish(&self, manifest: &TransferManifest) -> Result<()> {
        self.check()?;
        if child(&self.incoming, &self.artifact, false).is_ok() {
            self.verify(manifest)?;
            self.incoming.sync_all()?;
            return Ok(());
        }
        let from = CString::new("download")?;
        let to = CString::new(self.artifact.as_str())?;
        let rc = unsafe {
            libc::renameat2(
                self.slot.as_raw_fd(),
                from.as_ptr(),
                self.incoming.as_raw_fd(),
                to.as_ptr(),
                libc::RENAME_NOREPLACE,
            )
        };
        ensure!(rc == 0, "native source publish refused; preserve staging");
        self.slot.sync_all()?;
        self.incoming.sync_all()?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use aq_worker_protocol::{AttemptRef, ObjectAlgorithm, ObjectId, SourceBinding, SourceRole};
    use std::os::unix::fs::{PermissionsExt, symlink};
    use uuid::Uuid;
    fn sample() -> (TransferManifest, Vec<u8>, Vec<u8>) {
        let id = Uuid::new_v4();
        let source = SourceBinding {
            repository_id: Uuid::new_v4(),
            binding_generation: 1,
            logical_identity: "synthetic".into(),
            base: ObjectId {
                algorithm: ObjectAlgorithm::Sha1,
                hex: "a".repeat(40),
            },
        };
        let bundle = vec![b's'; 1048576];
        let digest = format!("{:x}", Sha256::digest(&bundle));
        let native = GitManifest {
            format_version: 1,
            repository_id: source.repository_id,
            repository_identity: source.logical_identity.clone(),
            binding_generation: 1,
            transfer_id: id,
            object_format: "sha1".into(),
            base: source.base.clone(),
            source: source.base.clone(),
            prerequisites: vec![],
            bundle_size: bundle.len() as u64,
            bundle_sha256: Some(digest.clone()),
            producer_installation_id: Uuid::new_v4(),
            reference_only: false,
            caller_correlation: serde_json::json!({}),
        };
        let bytes = serde_json::to_vec(&native).unwrap();
        let m = TransferManifest {
            transfer_id: id,
            artifact_id: id,
            role: SourceRole::Base,
            attempt: AttemptRef {
                attempt_id: Uuid::new_v4(),
                session_id: Uuid::new_v4(),
                generation: 1,
            },
            source: source.clone(),
            checkpoint: source.base,
            resolved_spec_digest: "a".repeat(64),
            manifest_bytes: bytes.len() as u64,
            manifest_digest: format!("{:x}", Sha256::digest(&bytes)),
            bundle_bytes: bundle.len() as u64,
            bundle_digest: Some(digest),
        };
        (m, bytes, bundle)
    }
    fn root() -> tempfile::TempDir {
        let d = tempfile::tempdir().unwrap();
        std::fs::set_permissions(d.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
        std::fs::create_dir(d.path().join("artifacts")).unwrap();
        std::fs::set_permissions(
            d.path().join("artifacts"),
            std::fs::Permissions::from_mode(0o700),
        )
        .unwrap();
        d
    }
    #[test]
    fn fsynced_tail_replays_exactly_and_complete_publish_reopens() {
        let d = root();
        let (m, bytes, bundle) = sample();
        let s = Staging::open(d.path(), &m, b"synthetic immutable owner").unwrap();
        s.append(&TransferFile::Manifest, 0, &bytes[..13]).unwrap();
        // Models a write acknowledged by the filesystem but not by the journal.
        s.append(&TransferFile::Manifest, 0, &bytes).unwrap();
        assert!(
            s.append(&TransferFile::Manifest, 0, b"changed bytes")
                .is_err()
        );
        assert!(
            s.append(&TransferFile::Bundle, 0, &vec![b'x'; 1048577])
                .is_err()
        );
        s.append(&TransferFile::Bundle, 0, &bundle).unwrap();
        s.verify(&m).unwrap();
        s.publish(&m).unwrap();
        drop(s);
        let s = Staging::open(d.path(), &m, b"synthetic immutable owner").unwrap();
        s.verify(&m).unwrap();
        s.publish(&m).unwrap();
        assert!(Staging::open(d.path(), &m, b"synthetic immutable owner").is_err());
    }
    #[test]
    fn digest_missing_prefix_and_unowned_tail_refuse_without_overwrite() {
        let d = root();
        let (m, bytes, bundle) = sample();
        let s = Staging::open(d.path(), &m, b"owner").unwrap();
        assert!(s.append(&TransferFile::Manifest, 1, &bytes).is_err());
        s.append(&TransferFile::Manifest, 0, &bytes).unwrap();
        assert!(s.append(&TransferFile::Manifest, 0, &bytes[..1]).is_err());
        s.append(&TransferFile::Bundle, 0, &bundle).unwrap();
        let p = d
            .path()
            .join(".native-inputs")
            .join(m.artifact_id.to_string())
            .join("download/objects.bundle");
        std::fs::write(&p, vec![b'x'; bundle.len()]).unwrap();
        assert!(s.verify(&m).is_err());
        assert_eq!(std::fs::read(p).unwrap(), vec![b'x'; bundle.len()]);
    }
    #[test]
    fn marker_and_directory_replacement_stop_owned_writes() {
        let d = root();
        let (m, bytes, _) = sample();
        let s = Staging::open(d.path(), &m, b"owner").unwrap();
        let slot = d
            .path()
            .join(".native-inputs")
            .join(m.artifact_id.to_string());
        std::fs::rename(slot.join("owner.json"), slot.join("old-owner.json")).unwrap();
        std::fs::write(slot.join("owner.json"), b"owner").unwrap();
        std::fs::set_permissions(
            slot.join("owner.json"),
            std::fs::Permissions::from_mode(0o600),
        )
        .unwrap();
        assert!(s.append(&TransferFile::Manifest, 0, &bytes).is_err());
        assert!(Staging::open(d.path(), &m, b"different").is_err());
    }
    #[test]
    fn empty_marker_and_foreign_published_artifact_are_held() {
        let d = root();
        let (m, _, _) = sample();
        let s = Staging::open(d.path(), &m, b"owner").unwrap();
        drop(s);
        let marker = d
            .path()
            .join(".native-inputs")
            .join(m.artifact_id.to_string())
            .join("owner.json");
        std::fs::write(&marker, b"").unwrap();
        assert!(Staging::open(d.path(), &m, b"owner").is_err());
        assert!(std::fs::read(marker).unwrap().is_empty());
        let d = root();
        let (m, _, _) = sample();
        let foreign = d
            .path()
            .join("artifacts/incoming")
            .join(m.artifact_id.to_string());
        std::fs::create_dir_all(&foreign).unwrap();
        std::fs::set_permissions(
            foreign.parent().unwrap(),
            std::fs::Permissions::from_mode(0o700),
        )
        .unwrap();
        std::fs::set_permissions(&foreign, std::fs::Permissions::from_mode(0o700)).unwrap();
        std::fs::write(foreign.join("sentinel"), b"held").unwrap();
        assert!(Staging::open(d.path(), &m, b"owner").is_err());
        assert_eq!(std::fs::read(foreign.join("sentinel")).unwrap(), b"held");
    }
    #[test]
    fn symlinks_and_special_members_are_never_followed() {
        let d = root();
        let (m, bytes, _) = sample();
        let s = Staging::open(d.path(), &m, b"owner").unwrap();
        let dir = child(&s.slot, "download", true).unwrap();
        drop(dir);
        let path = d
            .path()
            .join(".native-inputs")
            .join(m.artifact_id.to_string())
            .join("download/manifest.json");
        let target = d.path().join("outside");
        std::fs::write(&target, b"sentinel").unwrap();
        symlink(&target, &path).unwrap();
        assert!(s.append(&TransferFile::Manifest, 0, &bytes).is_err());
        assert_eq!(std::fs::read(target).unwrap(), b"sentinel");
    }
    #[test]
    fn growing_reader_is_bounded_and_metadata_changes_are_refused() {
        struct Growing {
            reads: usize,
        }
        impl Read for Growing {
            fn read(&mut self, bytes: &mut [u8]) -> std::io::Result<usize> {
                bytes.fill(b'x');
                self.reads += bytes.len();
                Ok(bytes.len())
            }
        }
        let mut growing = Growing { reads: 0 };
        assert!(hash_exact(&mut growing, 100000).is_err());
        assert_eq!(growing.reads, 100001);
        assert!(hash_exact(&mut &b"short"[..], 6).is_err());
        let mut file = tempfile::tempfile().unwrap();
        file.write_all(b"initial").unwrap();
        let before = file.metadata().unwrap();
        file.write_all(b"appended").unwrap();
        assert!(unchanged(&before, &file).is_err());
    }
}
