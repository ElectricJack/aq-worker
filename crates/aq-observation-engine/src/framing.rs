//! No unbounded line buffering. A caller persists consumption with facts, never
//! this framing result alone. Partial suffixes remain unconsumed.
use anyhow::{Result, ensure};
use serde::{Deserialize, Serialize};
pub const MAX_RECORD_BYTES: usize = 64 * 1024;
pub const MAX_READ_BYTES: usize = 256 * 1024;
pub const MAX_RECORDS: usize = 32;
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Frame {
    pub start: u64,
    pub bytes: Vec<u8>,
    pub oversized: bool,
}
impl Frame {
    pub fn end(&self) -> Result<u64> {
        self.start
            .checked_add(self.bytes.len() as u64)
            .ok_or_else(|| anyhow::anyhow!("source cursor overflow"))
    }
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Framed {
    pub records: Vec<Frame>,
    pub consumed: usize,
    pub partial: bool,
    pub discarding_oversized: bool,
}
pub fn frame(bytes: &[u8], start: u64, mut discarding: bool) -> Result<Framed> {
    ensure!(bytes.len() <= MAX_READ_BYTES, "source read bound");
    let mut consumed = 0;
    let mut records = vec![];
    let mut partial = false;
    while consumed < bytes.len() && records.len() < MAX_RECORDS {
        let remaining = &bytes[consumed..];
        let newline = remaining.iter().position(|b| *b == b'\n');
        let length = match newline {
            Some(n) if n < MAX_RECORD_BYTES => n + 1,
            _ => 0,
        };
        let (length, oversized) = if length > 0 {
            (length, discarding)
        } else if remaining.len() >= MAX_RECORD_BYTES {
            (MAX_RECORD_BYTES, true)
        } else {
            partial = true;
            break;
        };
        let record = Frame {
            start: start
                .checked_add(consumed as u64)
                .ok_or_else(|| anyhow::anyhow!("source cursor overflow"))?,
            bytes: remaining[..length].to_vec(),
            oversized,
        };
        discarding = oversized && !record.bytes.ends_with(b"\n");
        consumed += length;
        records.push(record);
    }
    Ok(Framed {
        records,
        consumed,
        partial,
        discarding_oversized: discarding,
    })
}
