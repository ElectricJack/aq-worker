//! Reader for the Windows host-load file written by `aq-host-load.ps1`.

use std::io::Read;
use std::path::Path;

use serde::Deserialize;

pub const HOST_FRESH_MS: u64 = 30_000;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HostSample {
    pub observed_at_ms: u64,
    pub logical_cpus: u32,
    pub cpu_busy_bps: u16,
    pub memory_total_bytes: u64,
    pub memory_used_bytes: u64,
    pub disk_total_bytes: Option<u64>,
    pub disk_free_bytes: Option<u64>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum HostLoad {
    Fresh(HostSample),
    Stale,
    Missing,
}

#[derive(Deserialize)]
struct Raw {
    schema: String,
    observed_at_ms: u64,
    logical_cpus: u32,
    cpu_busy_bps: u16,
    memory_total_bytes: u64,
    memory_used_bytes: u64,
    #[serde(default)]
    disk_total_bytes: Option<u64>,
    #[serde(default)]
    disk_free_bytes: Option<u64>,
}

pub fn read_host_load(path: &Path, now_ms: u64) -> HostLoad {
    let mut buf = Vec::new();
    let read = std::fs::File::open(path).and_then(|f| f.take(4096).read_to_end(&mut buf));
    if read.is_err() {
        return HostLoad::Missing;
    }
    let Ok(raw) = serde_json::from_slice::<Raw>(&buf) else {
        return HostLoad::Missing;
    };
    if raw.schema != "aq-host-load-1" {
        return HostLoad::Missing;
    }
    if now_ms.abs_diff(raw.observed_at_ms) > HOST_FRESH_MS {
        return HostLoad::Stale;
    }
    HostLoad::Fresh(HostSample {
        observed_at_ms: raw.observed_at_ms,
        logical_cpus: raw.logical_cpus,
        cpu_busy_bps: raw.cpu_busy_bps.min(10000),
        memory_total_bytes: raw.memory_total_bytes,
        memory_used_bytes: raw.memory_used_bytes.min(raw.memory_total_bytes),
        disk_total_bytes: raw.disk_total_bytes,
        disk_free_bytes: raw.disk_free_bytes,
    })
}
