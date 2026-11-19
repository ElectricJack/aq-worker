//! Bounded decoding and sanitization for files retrieved from an agent sandbox.

pub mod codex_runtime;
pub mod conversation;
pub mod pane;
pub mod redact;
pub mod tar;
pub mod workspace;

use anyhow::{Result, ensure};
use sha2::Digest as _;
use std::{io::Read, os::unix::fs::OpenOptionsExt, path::Path};

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
