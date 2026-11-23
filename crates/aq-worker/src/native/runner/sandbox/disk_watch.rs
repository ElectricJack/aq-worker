//! Runner disk watchdog (0005 SBX21 storage budget).
//!
//! Boat alerts only at 85% and 95% user-file disk use, and at 95% snapshot
//! and stop fail, which breaks preserve-before-destroy. The runner therefore
//! samples the VM user-files mount with `statvfs` every `disk_watch_interval`
//! (default 10 s). At 80% or more it fails the attempt with
//! `sandbox_disk_limit_exceeded`, then completes the handoff or, failing
//! that, requests stop-and-archive. Both carry the bytes used.
//!
//! This unit does not depend on a backend. Wiring it into the sandbox runner
//! loop is left to the boat backend integration.
use serde::{Deserialize, Serialize};
use std::{
    ffi::CString,
    future::Future,
    io,
    os::unix::ffi::OsStrExt,
    path::{Path, PathBuf},
    time::Duration,
};

/// Stable failure code for an attempt stopped by the disk watchdog.
pub const SANDBOX_DISK_LIMIT_EXCEEDED: &str = "sandbox_disk_limit_exceeded";
pub const DEFAULT_DISK_WATCH_INTERVAL: Duration = Duration::from_secs(10);
pub const DEFAULT_DISK_LIMIT_PERCENT: u8 = 80;

/// Filesystem usage with `df` semantics: blocks reserved for root count as
/// neither used nor available, so `used + available` is the usable size.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct DiskUsage {
    pub used_bytes: u64,
    pub available_bytes: u64,
}
impl DiskUsage {
    /// `fragment_size` is `f_frsize`; block counts are in that unit.
    pub fn from_statvfs(
        fragment_size: u64,
        blocks: u64,
        free_blocks: u64,
        available_blocks: u64,
    ) -> Self {
        Self {
            used_bytes: blocks
                .saturating_sub(free_blocks)
                .saturating_mul(fragment_size),
            available_bytes: available_blocks.saturating_mul(fragment_size),
        }
    }
    pub fn usable_bytes(&self) -> u64 {
        self.used_bytes.saturating_add(self.available_bytes)
    }
    /// True when used / (used + available) >= `percent`. A filesystem with no
    /// usable space counts as full.
    pub fn reaches_percent(&self, percent: u8) -> bool {
        let usable = u128::from(self.usable_bytes());
        usable == 0 || u128::from(self.used_bytes) * 100 >= u128::from(percent) * usable
    }
}

pub trait DiskProbe {
    fn usage(&mut self) -> io::Result<DiskUsage>;
}

/// `statvfs(2)` on one mount point.
#[derive(Debug, Clone)]
pub struct StatvfsProbe {
    path: PathBuf,
}
impl StatvfsProbe {
    pub fn new(path: impl Into<PathBuf>) -> Self {
        Self { path: path.into() }
    }
    pub fn path(&self) -> &Path {
        &self.path
    }
}
impl DiskProbe for StatvfsProbe {
    fn usage(&mut self) -> io::Result<DiskUsage> {
        let path = CString::new(self.path.as_os_str().as_bytes())
            .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "path contains NUL"))?;
        let mut stat = std::mem::MaybeUninit::<libc::statvfs>::zeroed();
        // SAFETY: `path` is NUL-terminated and `stat` is a valid out pointer.
        if unsafe { libc::statvfs(path.as_ptr(), stat.as_mut_ptr()) } != 0 {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: statvfs returned 0, so it initialized `stat`.
        let stat = unsafe { stat.assume_init() };
        #[allow(clippy::useless_conversion)]
        Ok(DiskUsage::from_statvfs(
            u64::from(stat.f_frsize),
            u64::from(stat.f_blocks),
            u64::from(stat.f_bfree),
            u64::from(stat.f_bavail),
        ))
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DiskWatchConfig {
    /// The VM user-files mount.
    pub mount: PathBuf,
    #[serde(default = "default_interval", with = "duration_ms")]
    pub disk_watch_interval: Duration,
    #[serde(default = "default_limit_percent")]
    pub limit_percent: u8,
}
impl DiskWatchConfig {
    pub fn new(mount: impl Into<PathBuf>) -> Self {
        Self {
            mount: mount.into(),
            disk_watch_interval: DEFAULT_DISK_WATCH_INTERVAL,
            limit_percent: DEFAULT_DISK_LIMIT_PERCENT,
        }
    }
    pub fn validate(&self) -> io::Result<()> {
        if self.disk_watch_interval.is_zero() || !(1..=100).contains(&self.limit_percent) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "disk watch needs a nonzero interval and a limit of 1-100%",
            ));
        }
        Ok(())
    }
}
fn default_interval() -> Duration {
    DEFAULT_DISK_WATCH_INTERVAL
}
fn default_limit_percent() -> u8 {
    DEFAULT_DISK_LIMIT_PERCENT
}
mod duration_ms {
    use serde::{Deserialize, Deserializer, Serializer};
    use std::time::Duration;
    pub fn serialize<S: Serializer>(d: &Duration, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_u64(u64::try_from(d.as_millis()).unwrap_or(u64::MAX))
    }
    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<Duration, D::Error> {
        u64::deserialize(d).map(Duration::from_millis)
    }
}

/// Attempt failure reported by the watchdog.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DiskLimitExceeded {
    pub code: String,
    pub mount: PathBuf,
    pub used_bytes: u64,
    pub usable_bytes: u64,
    pub limit_percent: u8,
}

/// How the watchdog preserved the attempt after failing it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum DiskLimitPreservation {
    HandoffCompleted,
    StopAndArchiveRequested,
}

/// Runner actions the watchdog drives once the limit is reached.
pub trait DiskLimitHandler {
    /// Mark the attempt failed with `failure.code`.
    fn fail_attempt(
        &mut self,
        failure: &DiskLimitExceeded,
    ) -> impl Future<Output = anyhow::Result<()>> + Send;
    /// Finish an in-progress handoff. `Ok(false)` means there was none to finish.
    fn complete_handoff(
        &mut self,
        failure: &DiskLimitExceeded,
    ) -> impl Future<Output = anyhow::Result<bool>> + Send;
    /// Ask the backend to stop the sandbox and archive its disk.
    fn stop_and_archive(
        &mut self,
        failure: &DiskLimitExceeded,
    ) -> impl Future<Output = anyhow::Result<()>> + Send;
}

/// One check. `Some` means the limit is reached.
pub fn check(
    config: &DiskWatchConfig,
    probe: &mut impl DiskProbe,
) -> io::Result<Option<DiskLimitExceeded>> {
    let usage = probe.usage()?;
    Ok(usage
        .reaches_percent(config.limit_percent)
        .then(|| DiskLimitExceeded {
            code: SANDBOX_DISK_LIMIT_EXCEEDED.into(),
            mount: config.mount.clone(),
            used_bytes: usage.used_bytes,
            usable_bytes: usage.usable_bytes(),
            limit_percent: config.limit_percent,
        }))
}

/// Samples until the limit is reached, then fails the attempt and preserves
/// it. Returns only after acting, or on a probe/handler error. A failed
/// handoff falls back to stop-and-archive. The caller cancels this future
/// when the attempt ends. `sleep` is injected so tests need no real time.
pub async fn watch<P, H, S, F>(
    config: &DiskWatchConfig,
    probe: &mut P,
    handler: &mut H,
    mut sleep: S,
) -> anyhow::Result<(DiskLimitExceeded, DiskLimitPreservation)>
where
    P: DiskProbe,
    H: DiskLimitHandler,
    S: FnMut(Duration) -> F,
    F: Future<Output = ()>,
{
    config.validate()?;
    loop {
        if let Some(failure) = check(config, probe)? {
            handler.fail_attempt(&failure).await?;
            let preserved = match handler.complete_handoff(&failure).await {
                Ok(true) => DiskLimitPreservation::HandoffCompleted,
                Ok(false) | Err(_) => {
                    handler.stop_and_archive(&failure).await?;
                    DiskLimitPreservation::StopAndArchiveRequested
                }
            };
            return Ok((failure, preserved));
        }
        sleep(config.disk_watch_interval).await;
    }
}
