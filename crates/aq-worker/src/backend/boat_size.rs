//! Boat sandbox size placement (0005 SBX21 storage budget).
//!
//! Boat fixes user-file disk per size: small/default 51 GB, large 111 GB,
//! xlarge 251 GB (xlarge needs the $100 plan). Boat alerts only at 85%/95%,
//! and at 95% snapshot and stop fail. The runner watchdog therefore fails an
//! attempt at 80%, so placement may use at most 80% of each size's disk,
//! rounded down to a whole GB: 40, 88 and 200 GB.
//!
//! This is a backend-independent unit. A boat `PlacementBackend` should call
//! [`BoatSizeCatalog::select`] before creating a sandbox.
use serde::{Deserialize, Serialize};
use std::fmt;

/// Boat reports sizes in decimal GB. Using decimal here never overcommits.
pub const GB: u64 = 1_000_000_000;
/// Share of a size's disk that placement and the watchdog may use.
pub const DISK_BUDGET_PERCENT: u64 = 80;
/// Stable refusal code for a disk request no available size can hold.
pub const DISK_REQUEST_UNSATISFIABLE: &str = "backend_disk_request_unsatisfiable";

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum BoatSize {
    Small,
    Large,
    Xlarge,
}

/// One boat size. `None` CPU or memory means this catalog does not limit it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct BoatSizeSpec {
    pub size: BoatSize,
    pub disk_bytes: u64,
    pub cpu_millis: Option<u32>,
    pub memory_bytes: Option<u64>,
}
impl BoatSizeSpec {
    pub const fn disk_only(size: BoatSize, disk_bytes: u64) -> Self {
        Self {
            size,
            disk_bytes,
            cpu_millis: None,
            memory_bytes: None,
        }
    }
    /// 80% of the size's disk, rounded down to a whole GB.
    pub fn disk_budget_bytes(&self) -> u64 {
        let budget = u128::from(self.disk_bytes) * u128::from(DISK_BUDGET_PERCENT) / 100;
        (budget / u128::from(GB) * u128::from(GB)) as u64
    }
    fn fits(&self, request: &BoatPlacementRequest) -> bool {
        request.disk_bytes <= self.disk_budget_bytes()
            && self.cpu_millis.is_none_or(|c| request.cpu_millis <= c)
            && self.memory_bytes.is_none_or(|m| request.memory_bytes <= m)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct BoatPlacementRequest {
    pub disk_bytes: u64,
    pub cpu_millis: u32,
    pub memory_bytes: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct DiskRequestUnsatisfiable {
    pub requested_disk_bytes: u64,
    /// Largest disk budget of any available size that also meets CPU/memory.
    /// Zero if no size meets them.
    pub max_disk_budget_bytes: u64,
}
impl DiskRequestUnsatisfiable {
    pub fn code(&self) -> &'static str {
        DISK_REQUEST_UNSATISFIABLE
    }
}
impl fmt::Display for DiskRequestUnsatisfiable {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{}: requested {} disk bytes, largest boat budget {}",
            DISK_REQUEST_UNSATISFIABLE, self.requested_disk_bytes, self.max_disk_budget_bytes
        )
    }
}
impl std::error::Error for DiskRequestUnsatisfiable {}

/// Sizes available on this boat plan, smallest first.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BoatSizeCatalog {
    sizes: Vec<BoatSizeSpec>,
}
impl BoatSizeCatalog {
    /// Boat disk per size. Set `xlarge_available` only on the $100 plan.
    pub fn standard(xlarge_available: bool) -> Self {
        let mut sizes = vec![
            BoatSizeSpec::disk_only(BoatSize::Small, 51 * GB),
            BoatSizeSpec::disk_only(BoatSize::Large, 111 * GB),
        ];
        if xlarge_available {
            sizes.push(BoatSizeSpec::disk_only(BoatSize::Xlarge, 251 * GB));
        }
        Self::new(sizes)
    }
    pub fn new(mut sizes: Vec<BoatSizeSpec>) -> Self {
        sizes.sort_by_key(|s| (s.disk_bytes, s.size));
        Self { sizes }
    }
    pub fn sizes(&self) -> &[BoatSizeSpec] {
        &self.sizes
    }
    /// The smallest size whose disk budget holds the request and whose
    /// CPU/memory satisfy it. Never rounds a request down to fit.
    pub fn select(
        &self,
        request: &BoatPlacementRequest,
    ) -> Result<BoatSizeSpec, DiskRequestUnsatisfiable> {
        self.sizes
            .iter()
            .find(|s| s.fits(request))
            .copied()
            .ok_or_else(|| DiskRequestUnsatisfiable {
                requested_disk_bytes: request.disk_bytes,
                max_disk_budget_bytes: self
                    .sizes
                    .iter()
                    .filter(|s| {
                        s.fits(&BoatPlacementRequest {
                            disk_bytes: 0,
                            ..*request
                        })
                    })
                    .map(BoatSizeSpec::disk_budget_bytes)
                    .max()
                    .unwrap_or(0),
            })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn disk(disk_bytes: u64) -> BoatPlacementRequest {
        BoatPlacementRequest {
            disk_bytes,
            cpu_millis: 1000,
            memory_bytes: 1 << 30,
        }
    }

    #[test]
    fn standard_budgets_are_eighty_percent_rounded_down_to_whole_gb() {
        let budgets: Vec<_> = BoatSizeCatalog::standard(true)
            .sizes()
            .iter()
            .map(|s| (s.size, s.disk_budget_bytes()))
            .collect();
        assert_eq!(
            budgets,
            [
                (BoatSize::Small, 40 * GB),
                (BoatSize::Large, 88 * GB),
                (BoatSize::Xlarge, 200 * GB),
            ]
        );
    }

    #[test]
    fn boat_size_mapping_at_40_88_200_gb_thresholds() {
        let catalog = BoatSizeCatalog::standard(true);
        for (bytes, size) in [
            (1, BoatSize::Small),
            (40 * GB, BoatSize::Small),
            (40 * GB + 1, BoatSize::Large),
            (88 * GB, BoatSize::Large),
            (88 * GB + 1, BoatSize::Xlarge),
            (200 * GB, BoatSize::Xlarge),
        ] {
            assert_eq!(catalog.select(&disk(bytes)).unwrap().size, size, "{bytes}");
        }
    }

    #[test]
    fn boat_size_mapping_refuses_above_200_gb() {
        let err = BoatSizeCatalog::standard(true)
            .select(&disk(200 * GB + 1))
            .unwrap_err();
        assert_eq!(err.code(), "backend_disk_request_unsatisfiable");
        assert_eq!(err.requested_disk_bytes, 200 * GB + 1);
        assert_eq!(err.max_disk_budget_bytes, 200 * GB);
    }

    #[test]
    fn boat_size_mapping_without_xlarge_plan_refuses_above_88_gb() {
        let catalog = BoatSizeCatalog::standard(false);
        assert_eq!(
            catalog.select(&disk(88 * GB)).unwrap().size,
            BoatSize::Large
        );
        let err = catalog.select(&disk(88 * GB + 1)).unwrap_err();
        assert_eq!(err.code(), DISK_REQUEST_UNSATISFIABLE);
        assert_eq!(err.max_disk_budget_bytes, 88 * GB);
    }

    #[test]
    fn boat_size_mapping_moves_up_for_cpu_or_memory() {
        let catalog = BoatSizeCatalog::new(vec![
            BoatSizeSpec {
                cpu_millis: Some(2000),
                memory_bytes: Some(4 * GB),
                ..BoatSizeSpec::disk_only(BoatSize::Small, 51 * GB)
            },
            BoatSizeSpec {
                cpu_millis: Some(4000),
                memory_bytes: Some(8 * GB),
                ..BoatSizeSpec::disk_only(BoatSize::Large, 111 * GB)
            },
        ]);
        let small = disk(10 * GB);
        assert_eq!(catalog.select(&small).unwrap().size, BoatSize::Small);
        let more_cpu = BoatPlacementRequest {
            cpu_millis: 3000,
            ..small
        };
        assert_eq!(catalog.select(&more_cpu).unwrap().size, BoatSize::Large);
        let more_memory = BoatPlacementRequest {
            memory_bytes: 5 * GB,
            ..small
        };
        assert_eq!(catalog.select(&more_memory).unwrap().size, BoatSize::Large);
        let too_much_cpu = BoatPlacementRequest {
            cpu_millis: 8000,
            ..small
        };
        let err = catalog.select(&too_much_cpu).unwrap_err();
        assert_eq!(err.max_disk_budget_bytes, 0);
    }
}
