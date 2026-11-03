//! Measured node disk for inventory reporting. Measurements sit alongside the
//! configured-capacity reservation accounting and never replace it as admission
//! authority. Anything that cannot be measured is absent, never reported as 0.
use super::BackendError;
use serde::Deserialize;
use std::{ffi::CString, io, mem::MaybeUninit, os::unix::ffi::OsStrExt, path::Path};

/// One filesystem's capacity as statvfs reports it.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct FsStats {
    pub total_bytes: u64,
    pub used_bytes: u64,
    /// Space available to unprivileged writers (`f_bavail`).
    pub free_bytes: u64,
}
impl FsStats {
    /// `used` counts all allocated blocks (`blocks - bfree`), so `used + free`
    /// can be below `total` by the root reserve.
    pub fn from_blocks(fragment: u64, blocks: u64, bfree: u64, bavail: u64) -> Self {
        Self {
            total_bytes: blocks.saturating_mul(fragment),
            used_bytes: blocks.saturating_sub(bfree).saturating_mul(fragment),
            free_bytes: bavail.saturating_mul(fragment),
        }
    }
}

/// statvfs seam so tests measure a fake filesystem.
pub trait Statvfs: Send + Sync {
    fn statvfs(&self, path: &Path) -> io::Result<FsStats>;
}
pub struct HostStatvfs;
impl Statvfs for HostStatvfs {
    fn statvfs(&self, path: &Path) -> io::Result<FsStats> {
        let path = CString::new(path.as_os_str().as_bytes())
            .map_err(|_| io::Error::from(io::ErrorKind::InvalidInput))?;
        let mut raw = MaybeUninit::<libc::statvfs>::uninit();
        // SAFETY: valid NUL-terminated path; statvfs fully initializes `raw` on 0.
        if unsafe { libc::statvfs(path.as_ptr(), raw.as_mut_ptr()) } != 0 {
            return Err(io::Error::last_os_error());
        }
        let raw = unsafe { raw.assume_init() };
        let fragment = if raw.f_frsize > 0 {
            raw.f_frsize
        } else {
            raw.f_bsize
        };
        #[allow(clippy::useless_conversion)]
        Ok(FsStats::from_blocks(
            u64::from(fragment),
            u64::from(raw.f_blocks),
            u64::from(raw.f_bfree),
            u64::from(raw.f_bavail),
        ))
    }
}

/// Backend-measured disk. `preserved` sizes cover only the stopped,
/// not-yet-destroyed sandboxes the controller passed in.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct BackendDiskUsage {
    /// The sandbox filesystem (docker-local: the daemon's GraphRoot).
    pub filesystem: Option<FsStats>,
    pub preserved_bytes: Option<u64>,
    pub retained_snapshot_bytes: Option<u64>,
    pub saved_snapshots_over_allowance: Option<u64>,
}

/// One entry of boat `GET /sandboxes/{id}/snapshots`. Other fields are ignored.
#[derive(Clone, Debug, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BoatSnapshot {
    pub content_size_bytes: u64,
}
/// Accepts a bare array or a `{"snapshots": [...]}` envelope.
pub fn parse_boat_snapshots(body: &[u8]) -> Result<Vec<BoatSnapshot>, BackendError> {
    #[derive(Deserialize)]
    #[serde(untagged)]
    enum Listing {
        Bare(Vec<BoatSnapshot>),
        Envelope { snapshots: Vec<BoatSnapshot> },
    }
    match serde_json::from_slice(body) {
        Ok(Listing::Bare(snapshots) | Listing::Envelope { snapshots }) => Ok(snapshots),
        Err(_) => Err(BackendError::Uncertain(
            super::UnconfirmedReason::BackendFailure,
        )),
    }
}
/// Sums retained snapshot content and counts each sandbox's saved snapshots
/// beyond `allowance`, into `retained_snapshot_bytes` and
/// `saved_snapshots_over_allowance`.
pub fn boat_snapshot_usage<'a>(
    per_sandbox: impl IntoIterator<Item = &'a [BoatSnapshot]>,
    allowance: usize,
    usage: &mut BackendDiskUsage,
) {
    let (mut bytes, mut over) = (0u64, 0u64);
    for snapshots in per_sandbox {
        bytes = snapshots
            .iter()
            .fold(bytes, |sum, s| sum.saturating_add(s.content_size_bytes));
        over = over.saturating_add(snapshots.len().saturating_sub(allowance) as u64);
    }
    usage.retained_snapshot_bytes = Some(bytes);
    usage.saved_snapshots_over_allowance = Some(over);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fs_stats_from_fake_statvfs_blocks() {
        let stats = FsStats::from_blocks(4096, 1_000, 300, 250);
        assert_eq!(
            stats,
            FsStats {
                total_bytes: 4_096_000,
                used_bytes: 2_867_200,
                free_bytes: 1_024_000,
            }
        );
    }

    #[test]
    fn boat_snapshot_bytes_and_over_allowance() {
        let a = parse_boat_snapshots(
            br#"[{"id":"s1","contentSizeBytes":100},{"id":"s2","contentSizeBytes":50},{"id":"s3","contentSizeBytes":7}]"#,
        )
        .unwrap();
        let b = parse_boat_snapshots(br#"{"snapshots":[{"contentSizeBytes":1000}]}"#).unwrap();
        let mut usage = BackendDiskUsage::default();
        boat_snapshot_usage([a.as_slice(), b.as_slice()], 2, &mut usage);
        assert_eq!(usage.retained_snapshot_bytes, Some(1157));
        assert_eq!(usage.saved_snapshots_over_allowance, Some(1));
        assert!(parse_boat_snapshots(br#"[{"id":"s1"}]"#).is_err());
    }
}
