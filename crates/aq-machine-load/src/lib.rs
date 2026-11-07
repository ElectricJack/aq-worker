//! Machine load sampling for the swarm page (spec 0007 §6).
//!
//! The report types mirror the pinned `worker-machine/v1` contract.

pub mod host;
pub mod proc;

use std::future::Future;
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::Context;
use serde::{Deserialize, Serialize};

pub use host::{HOST_FRESH_MS, HostLoad, HostSample, read_host_load};
pub use proc::{CpuTimes, cpu_busy_bps, parse_meminfo, parse_proc_stat};

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MachineKind {
    #[default]
    Local,
    Cloud,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LoadSource {
    Linux,
    WindowsHost,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HostProbe {
    NotApplicable,
    Fresh,
    Stale,
    Missing,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MachineIdentity {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub key: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
    pub kind: MachineKind,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MachineReport {
    pub schema_version: u32,
    pub machine: MachineIdentity,
    pub observed_at_ms: u64,
    pub source: LoadSource,
    pub host_probe: HostProbe,
    pub logical_cpus: u32,
    pub memory_total_bytes: u64,
    pub memory_used_bytes: u64,
    pub cpu_busy_bps: u16,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub disk_total_bytes: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub disk_free_bytes: Option<u64>,
}

fn default_interval_secs() -> u64 {
    15
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MachineConfig {
    #[serde(default)]
    pub key: Option<String>,
    #[serde(default)]
    pub label: Option<String>,
    #[serde(default)]
    pub kind: MachineKind,
    #[serde(default)]
    pub host_load_file: Option<PathBuf>,
    #[serde(default)]
    pub disk_path: Option<PathBuf>,
    #[serde(default = "default_interval_secs")]
    pub interval_secs: u64,
}

impl Default for MachineConfig {
    fn default() -> Self {
        Self {
            key: None,
            label: None,
            kind: MachineKind::Local,
            host_load_file: None,
            disk_path: None,
            interval_secs: default_interval_secs(),
        }
    }
}

impl MachineConfig {
    /// A missing file gives the default; a file over 64 KiB or an invalid one is an error.
    pub fn load_optional(path: &Path) -> anyhow::Result<Self> {
        let meta = match std::fs::metadata(path) {
            Ok(m) => m,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Self::default()),
            Err(e) => return Err(e).with_context(|| format!("stat {}", path.display())),
        };
        anyhow::ensure!(meta.len() <= 64 * 1024, "{} is larger than 64 KiB", path.display());
        let text = std::fs::read_to_string(path).with_context(|| format!("read {}", path.display()))?;
        let cfg: Self = serde_json::from_str(&text).with_context(|| format!("parse {}", path.display()))?;
        anyhow::ensure!((5..=300).contains(&cfg.interval_secs), "interval_secs must be 5..=300");
        if let Some(k) = &cfg.key {
            let ok = (1..=63).contains(&k.len())
                && k.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
                && !k.starts_with('-');
            anyhow::ensure!(ok, "machine key must be 1-63 characters of [a-z0-9-] starting with [a-z0-9]");
        }
        if let Some(l) = &cfg.label {
            anyhow::ensure!(
                (1..=64).contains(&l.chars().count()) && !l.chars().any(char::is_control),
                "machine label must be 1-64 characters without control characters"
            );
        }
        Ok(cfg)
    }
}

pub fn is_wsl() -> bool {
    std::fs::read_to_string("/proc/sys/kernel/osrelease")
        .map(|s| s.to_ascii_lowercase().contains("microsoft"))
        .unwrap_or(false)
}

#[allow(clippy::useless_conversion, clippy::unnecessary_cast)] // statvfs field widths differ by target
fn disk_usage(path: &Path) -> Option<(u64, u64)> {
    use std::os::unix::ffi::OsStrExt;
    let c = std::ffi::CString::new(path.as_os_str().as_bytes()).ok()?;
    let mut s: libc::statvfs = unsafe { std::mem::zeroed() };
    // SAFETY: `c` is a valid NUL-terminated path and `s` is a valid out pointer.
    if unsafe { libc::statvfs(c.as_ptr(), &mut s) } != 0 {
        return None;
    }
    let frsize = u64::from(s.f_frsize);
    let total = (s.f_blocks as u64).checked_mul(frsize)?;
    let free = (s.f_bavail as u64).checked_mul(frsize)?.min(total);
    Some((total, free))
}

pub struct Sampler {
    config: MachineConfig,
    state_dir: PathBuf,
    prev: Option<CpuTimes>,
    wsl: bool,
}

impl Sampler {
    pub fn new(config: MachineConfig, state_dir: PathBuf) -> Self {
        let prev = std::fs::read_to_string("/proc/stat").ok().and_then(|t| parse_proc_stat(&t));
        Self { config, state_dir, prev, wsl: is_wsl() }
    }

    pub fn sample(&mut self, now_ms: u64) -> anyhow::Result<MachineReport> {
        let stat = std::fs::read_to_string("/proc/stat").context("read /proc/stat")?;
        let cur = parse_proc_stat(&stat).context("parse /proc/stat")?;
        let cpu = self.prev.as_ref().map_or(0, |p| cpu_busy_bps(p, &cur));
        self.prev = Some(cur);
        let mem = std::fs::read_to_string("/proc/meminfo").context("read /proc/meminfo")?;
        let (total, avail) = parse_meminfo(&mem).context("parse /proc/meminfo")?;
        let disk = disk_usage(self.config.disk_path.as_deref().unwrap_or(&self.state_dir));

        let mut report = MachineReport {
            schema_version: 1,
            machine: MachineIdentity {
                key: self.config.key.clone(),
                label: self.config.label.clone(),
                kind: self.config.kind,
            },
            observed_at_ms: now_ms,
            source: LoadSource::Linux,
            host_probe: if self.wsl { HostProbe::Missing } else { HostProbe::NotApplicable },
            logical_cpus: cur.logical_cpus.max(1),
            memory_total_bytes: total,
            memory_used_bytes: total.saturating_sub(avail),
            cpu_busy_bps: cpu,
            disk_total_bytes: disk.map(|d| d.0),
            disk_free_bytes: disk.map(|d| d.1),
        };
        if let Some(file) = &self.config.host_load_file {
            match read_host_load(file, now_ms) {
                HostLoad::Fresh(h) if h.memory_total_bytes > 0 && h.logical_cpus > 0 => {
                    report.source = LoadSource::WindowsHost;
                    report.host_probe = HostProbe::Fresh;
                    report.logical_cpus = h.logical_cpus;
                    report.cpu_busy_bps = h.cpu_busy_bps;
                    report.memory_total_bytes = h.memory_total_bytes;
                    report.memory_used_bytes = h.memory_used_bytes;
                    if let (Some(t), Some(f)) = (h.disk_total_bytes, h.disk_free_bytes)
                        && f <= t
                    {
                        report.disk_total_bytes = Some(t);
                        report.disk_free_bytes = Some(f);
                    }
                }
                HostLoad::Stale => report.host_probe = HostProbe::Stale,
                _ => report.host_probe = HostProbe::Missing,
            }
        }
        Ok(report)
    }
}

pub fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_millis() as u64)
}

pub async fn report_loop<F, Fut>(
    mut sampler: Sampler,
    interval: Duration,
    mut send: F,
    shutdown: impl Future<Output = ()>,
) where
    F: FnMut(MachineReport) -> Fut,
    Fut: Future<Output = Result<(), String>>,
{
    tokio::pin!(shutdown);
    let mut failures = 0u32;
    loop {
        tokio::select! {
            () = &mut shutdown => return,
            () = tokio::time::sleep(interval) => {}
        }
        let report = match sampler.sample(now_ms()) {
            Ok(r) => r,
            Err(e) => {
                tracing::warn!(error = %e, "machine load sample failed");
                continue;
            }
        };
        match send(report).await {
            Ok(()) => failures = 0,
            Err(m) => {
                failures = failures.saturating_add(1);
                if failures == 1 || failures.is_multiple_of(40) {
                    tracing::warn!(error = %m, failures, "machine report failed");
                } else {
                    tracing::debug!(error = %m, failures, "machine report failed");
                }
            }
        }
    }
}

#[cfg(test)]
mod tests;
