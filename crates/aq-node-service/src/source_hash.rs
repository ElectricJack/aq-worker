//! Exact bounded streaming digests; no file-presence or execution authority.
use anyhow::{Result, ensure};
use sha2::{Digest as _, Sha256};
use std::io::Read;

pub fn exact(reader: &mut impl Read, expected: u64, maximum: u64) -> Result<String> {
    ensure!(
        expected > 0 && expected <= maximum,
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
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn streams_above_legacy_limit_and_refuses_growth_truncation_and_oversize() {
        let size = 129 * 1024 * 1024;
        assert!(
            exact(
                &mut std::io::repeat(b'x').take(size),
                size,
                1024 * 1024 * 1024
            )
            .is_ok()
        );
        assert!(exact(&mut &b"short"[..], 6, 100).is_err());
        assert!(exact(&mut &b"too long"[..], 3, 100).is_err());
        assert!(exact(&mut &b"x"[..], 101, 100).is_err());
    }
}
