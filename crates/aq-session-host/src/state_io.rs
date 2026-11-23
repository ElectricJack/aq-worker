//! Bounded reads and atomic publication of durable session files.
use super::*;

pub(super) fn read_bytes(path: &Path, max: u64) -> Result<Vec<u8>> {
    let file = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(path)?;
    ensure!(
        file.metadata()?.file_type().is_file(),
        "local material/state must be regular file"
    );
    ensure!(file.metadata()?.len() <= max, "local file too large");
    let mut bytes = Vec::new();
    file.take(max + 1).read_to_end(&mut bytes)?;
    ensure!(bytes.len() as u64 <= max, "local file grew beyond bound");
    Ok(bytes)
}
pub(super) fn read_json<T: serde::de::DeserializeOwned>(path: &Path) -> Result<T> {
    Ok(serde_json::from_slice(&read_bytes(path, MAX_LOCAL_FILE)?)?)
}
pub fn durable_new(path: &Path, bytes: &[u8]) -> Result<()> {
    durable_write_with(path, false, |file| file.write_all(bytes))
}
pub(super) fn durable_write_with(
    path: &Path,
    replace: bool,
    write: impl FnOnce(&mut File) -> std::io::Result<()>,
) -> Result<()> {
    use std::{ffi::CString, os::unix::ffi::OsStrExt};

    let temp = path.with_extension(format!("{}.tmp", Uuid::new_v4()));
    let source = CString::new(temp.as_os_str().as_bytes())?;
    let destination = CString::new(path.as_os_str().as_bytes())?;
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&temp)?;
    let result = (|| {
        write(&mut file)?;
        file.sync_all()?;
        // Readers treat existence as publication. Never expose partial bytes.
        if replace {
            std::fs::rename(&temp, path)?;
        } else {
            // Preserve exclusive creation, including existing symlinks.
            let renamed = unsafe {
                libc::renameat2(
                    libc::AT_FDCWD,
                    source.as_ptr(),
                    libc::AT_FDCWD,
                    destination.as_ptr(),
                    libc::RENAME_NOREPLACE,
                )
            };
            if renamed != 0 {
                return Err(std::io::Error::last_os_error().into());
            }
        }
        File::open(path.parent().context("state parent")?)?.sync_all()?;
        Ok(())
    })();
    // Only remove the temporary file we exclusively created. After a successful
    // rename it is already absent; a failed write must not publish a request.
    let _ = std::fs::remove_file(&temp);
    result
}
pub(super) fn durable_replace(path: &Path, bytes: &[u8]) -> Result<()> {
    durable_write_with(path, true, |file| file.write_all(bytes))
}
