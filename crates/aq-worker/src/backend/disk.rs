//! Measured node disk for inventory reporting. Measurements sit alongside the
//! configured-capacity reservation accounting and never replace it as admission
//! authority. Anything that cannot be measured is absent, never reported as 0.
use super::BackendError;
pub use aq_sandbox_archive::docker_engine::{FsStats, HostStatvfs, Statvfs};
use serde::Deserialize;

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
