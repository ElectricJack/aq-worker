//! The driver's private inputs: the leased Claude token file and the other 0600 files.
use anyhow::{Context, Result, ensure};
use std::{os::unix::fs::OpenOptionsExt, path::Path};
use zeroize::Zeroizing;

/// Token file contents to the token: one line, trailing CR/LF trimmed,
/// ASCII-graphic only (C6b install rule).
pub fn parse_token(bytes: &[u8]) -> Result<Zeroizing<Vec<u8>>> {
    let mut end = bytes.len();
    while end > 0 && matches!(bytes[end - 1], b'\n' | b'\r') {
        end -= 1;
    }
    let token = Zeroizing::new(bytes[..end].to_vec());
    ensure!(
        (16..=4096).contains(&token.len()) && token.iter().all(u8::is_ascii_graphic),
        "Claude login token shape refused"
    );
    Ok(token)
}

/// Reads a regular file of this user, owner-only, at most `max` bytes (no symlink).
pub fn read_private(path: &Path, max: u64) -> Result<Zeroizing<Vec<u8>>> {
    use std::io::Read;
    use std::os::unix::fs::MetadataExt;
    let file = std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .open(path)
        .context("private input unavailable")?;
    let meta = file.metadata()?;
    ensure!(
        meta.is_file()
            && meta.mode() & 0o077 == 0
            && meta.uid() == unsafe { libc::geteuid() }
            && meta.len() <= max,
        "private input owner/mode/bound"
    );
    let mut bytes = Zeroizing::new(Vec::new());
    file.take(max + 1).read_to_end(&mut bytes)?;
    ensure!(bytes.len() as u64 <= max, "private input grew");
    Ok(bytes)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn token_trims_newlines_and_refuses_bad_shapes() {
        assert_eq!(
            &parse_token(b"fixture-login-abcdefghijklmnop\r\n").unwrap()[..],
            b"fixture-login-abcdefghijklmnop"
        );
        assert!(parse_token(b"short\n").is_err());
        assert!(parse_token(b"fixture-login-abc defghijklmnop\n").is_err());
        assert!(parse_token(b"fixture-login-abcdefghijklmnop\nsecond\n").is_err());
    }
}
