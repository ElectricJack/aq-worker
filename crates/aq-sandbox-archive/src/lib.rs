//! Bounded decoding and sanitization for files retrieved from an agent sandbox.

pub mod build_cache;
pub mod codex_runtime;
pub mod conversation;
pub mod docker_engine;
pub mod inputs;
pub mod pane;
pub mod placement_api;
pub mod projects;
pub mod query_replies;
pub mod redact;
pub mod staging;
pub mod tar;
pub mod task_postgres;
pub mod workspace;

use anyhow::{Result, ensure};
use sha2::Digest as _;
use std::{
    io::{Read, Write},
    os::unix::fs::OpenOptionsExt,
    path::{Path, PathBuf},
};

/// Hex SHA-256 and size of a regular file of 1..=`max` bytes (a pinned program);
/// symbolic links are not followed.
pub fn hash_file(path: &Path, max: u64) -> Result<(String, u64)> {
    let mut f = std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK | libc::O_CLOEXEC)
        .open(path)?;
    let m = f.metadata()?;
    ensure!(
        m.is_file() && m.len() > 0 && m.len() <= max,
        "pinned program bound"
    );
    let mut hash = sha2::Sha256::new();
    let mut scratch = vec![0u8; 1 << 20];
    let mut total = 0u64;
    loop {
        let n = f.read(&mut scratch)?;
        if n == 0 {
            break;
        }
        total += n as u64;
        ensure!(total <= max, "pinned program grew");
        hash.update(&scratch[..n]);
    }
    Ok((format!("{:x}", hash.finalize()), total))
}

/// A new 0600 file, synced; never through an existing path or link.
pub fn private_write(path: &Path, bytes: &[u8]) -> Result<()> {
    let mut f = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .open(path)?;
    f.write_all(bytes)?;
    f.sync_all()?;
    Ok(())
}
/// Deletes the private token file unless the driver was started (it then owns
/// the file and deletes it after reading).
pub struct TokenGuard(pub Option<PathBuf>);
impl TokenGuard {
    pub fn disarm(&mut self) {
        self.0 = None;
    }
}
impl Drop for TokenGuard {
    fn drop(&mut self) {
        if let Some(path) = self.0.take() {
            let _ = std::fs::remove_file(path);
        }
    }
}
