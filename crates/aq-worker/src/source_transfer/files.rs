use super::*;
/// Descriptor-relative access to documented transport members only. Reject
/// symlinks and special files before reading, including FIFOs that would block.
fn artifact_directory(
    root: &Path,
    artifact: Option<Uuid>,
    incoming: bool,
    write: bool,
) -> Result<File> {
    ensure!(
        artifact.is_none_or(|id| !id.is_nil()),
        "nil artifact directory"
    );
    let mut dir = std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .open(root)?;
    let mut components = vec!["artifacts".to_string()];
    if incoming {
        components.push("incoming".into());
    }
    if let Some(artifact) = artifact {
        components.push(artifact.to_string());
    }
    for name in components {
        let c = std::ffi::CString::new(name)?;
        if write {
            let rc = unsafe { libc::mkdirat(dir.as_raw_fd(), c.as_ptr(), 0o700) };
            if rc != 0 && std::io::Error::last_os_error().raw_os_error() != Some(libc::EEXIST) {
                return Err(std::io::Error::last_os_error().into());
            }
            dir.sync_all()?;
        }
        let fd = unsafe {
            libc::openat(
                dir.as_raw_fd(),
                c.as_ptr(),
                libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
            )
        };
        if fd < 0 {
            return Err(std::io::Error::last_os_error())
                .context("artifact directory unavailable or unsafe");
        }
        dir = unsafe { File::from_raw_fd(fd) };
    }
    Ok(dir)
}
pub(crate) fn artifact_file(
    root: &Path,
    artifact: Uuid,
    incoming: bool,
    name: &str,
    write: bool,
) -> Result<File> {
    ensure!(
        !artifact.is_nil() && matches!(name, "manifest.json" | "objects.bundle"),
        "invalid artifact member"
    );
    let dir = artifact_directory(root, Some(artifact), incoming, write)?;
    let c = std::ffi::CString::new(name)?;
    let flags = if write {
        libc::O_RDWR | libc::O_CREAT
    } else {
        libc::O_RDONLY
    };
    let fd = unsafe {
        libc::openat(
            dir.as_raw_fd(),
            c.as_ptr(),
            flags | libc::O_NOFOLLOW | libc::O_NONBLOCK | libc::O_CLOEXEC,
            0o600,
        )
    };
    ensure!(fd >= 0, "artifact member unavailable or unsafe");
    let file = unsafe { File::from_raw_fd(fd) };
    let metadata = file.metadata()?;
    ensure!(
        metadata.is_file() && metadata.nlink() == 1 && metadata.uid() == unsafe { libc::geteuid() },
        "artifact member must be a regular file"
    );
    if write {
        dir.sync_all()?;
    }
    Ok(file)
}

/// Only named input members of one journal-owned UUID are removed. Unknown
/// children or unsafe directories stop cleanup; no recursive traversal occurs.
#[cfg(feature = "synthetic-runtime")]
pub(crate) fn discard_incoming(root: &Path, artifact: Uuid) -> Result<()> {
    let parent = match artifact_directory(root, None, true, false) {
        Ok(parent) => parent,
        Err(e)
            if e.downcast_ref::<std::io::Error>()
                .is_some_and(|e| e.kind() == std::io::ErrorKind::NotFound) =>
        {
            return Ok(());
        }
        Err(e) => return Err(e),
    };
    let name = std::ffi::CString::new(artifact.to_string())?;
    let fd = unsafe {
        libc::openat(
            parent.as_raw_fd(),
            name.as_ptr(),
            libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
        )
    };
    if fd < 0 {
        let e = std::io::Error::last_os_error();
        if e.kind() == std::io::ErrorKind::NotFound {
            return Ok(());
        }
        return Err(e).context("unsafe input retirement directory");
    }
    let dir = unsafe { File::from_raw_fd(fd) };
    for member in [c"manifest.json", c"objects.bundle"] {
        let rc = unsafe { libc::unlinkat(dir.as_raw_fd(), member.as_ptr(), 0) };
        if rc != 0 && std::io::Error::last_os_error().kind() != std::io::ErrorKind::NotFound {
            return Err(std::io::Error::last_os_error().into());
        }
    }
    dir.sync_all()?;
    let rc = unsafe { libc::unlinkat(parent.as_raw_fd(), name.as_ptr(), libc::AT_REMOVEDIR) };
    if rc != 0 && std::io::Error::last_os_error().kind() != std::io::ErrorKind::NotFound {
        return Err(std::io::Error::last_os_error())
            .context("input retirement has unexpected members");
    }
    parent.sync_all()?;
    Ok(())
}
pub(super) fn read_range(
    root: &Path,
    artifact: Uuid,
    file: &TransferFile,
    offset: u64,
    length: usize,
    size: u64,
) -> Result<Vec<u8>> {
    ensure!(
        length <= MAX_CHUNK_BYTES
            && offset
                .checked_add(length as u64)
                .is_some_and(|end| end <= size),
        "invalid artifact range"
    );
    let name = match file {
        TransferFile::Manifest => "manifest.json",
        TransferFile::Bundle => "objects.bundle",
    };
    let mut f = artifact_file(root, artifact, false, name, false)?;
    ensure!(
        f.metadata()?.len() == size,
        "retained artifact length changed"
    );
    f.seek(SeekFrom::Start(offset))?;
    let mut bytes = vec![0; length];
    f.read_exact(&mut bytes)?;
    Ok(bytes)
}
