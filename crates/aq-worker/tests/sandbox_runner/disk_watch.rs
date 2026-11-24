use aq_worker::native::runner::sandbox::disk_watch::*;
use std::{
    collections::VecDeque,
    sync::{Arc, Mutex},
};
use std::{io, path::PathBuf, time::Duration};

const BLOCK: u64 = 4096;
/// 1000 usable blocks plus 50 reserved for root.
const BLOCKS: u64 = 1050;

#[derive(Debug, Clone, PartialEq, Eq)]
enum Event {
    Sample(u64),
    Sleep(Duration),
    Fail(String, u64),
    Handoff,
    StopAndArchive(u64),
}
type Log = Arc<Mutex<Vec<Event>>>;

/// Fake statvfs: each call returns the next used-percent of the usable space.
struct FakeStatvfs {
    percents: VecDeque<u64>,
    log: Log,
}
impl DiskProbe for FakeStatvfs {
    fn usage(&mut self) -> io::Result<DiskUsage> {
        let percent = self.percents.pop_front().expect("watchdog sampled too far");
        self.log.lock().unwrap().push(Event::Sample(percent));
        let used = percent * 10;
        let free = BLOCKS - used;
        Ok(DiskUsage::from_statvfs(BLOCK, BLOCKS, free, free - 50))
    }
}

struct FakeRunner {
    handoff: anyhow::Result<bool>,
    log: Log,
}
impl DiskLimitHandler for FakeRunner {
    async fn fail_attempt(&mut self, f: &DiskLimitExceeded) -> anyhow::Result<()> {
        self.log
            .lock()
            .unwrap()
            .push(Event::Fail(f.code.clone(), f.used_bytes));
        Ok(())
    }
    async fn complete_handoff(&mut self, _: &DiskLimitExceeded) -> anyhow::Result<bool> {
        self.log.lock().unwrap().push(Event::Handoff);
        std::mem::replace(&mut self.handoff, Ok(false))
    }
    async fn stop_and_archive(&mut self, f: &DiskLimitExceeded) -> anyhow::Result<()> {
        self.log
            .lock()
            .unwrap()
            .push(Event::StopAndArchive(f.used_bytes));
        Ok(())
    }
}

async fn run(
    percents: &[u64],
    handoff: anyhow::Result<bool>,
) -> (DiskLimitExceeded, DiskLimitPreservation, Vec<Event>) {
    let log = Log::default();
    let mut probe = FakeStatvfs {
        percents: percents.iter().copied().collect(),
        log: log.clone(),
    };
    let mut runner = FakeRunner {
        handoff,
        log: log.clone(),
    };
    let sleeps = log.clone();
    let (failure, preserved) = watch(
        &DiskWatchConfig::new("/user-files"),
        &mut probe,
        &mut runner,
        move |d| {
            sleeps.lock().unwrap().push(Event::Sleep(d));
            std::future::ready(())
        },
    )
    .await
    .unwrap();
    let events = log.lock().unwrap().clone();
    (failure, preserved, events)
}

#[tokio::test]
async fn disk_watch_fails_at_80_percent_and_requests_stop_archive_before_85() {
    let ten = DEFAULT_DISK_WATCH_INTERVAL;
    let (failure, preserved, events) = run(&[50, 79, 80, 85, 95], Ok(false)).await;
    let used = 800 * BLOCK;
    assert_eq!(
        events,
        [
            Event::Sample(50),
            Event::Sleep(ten),
            Event::Sample(79),
            Event::Sleep(ten),
            Event::Sample(80),
            Event::Fail(SANDBOX_DISK_LIMIT_EXCEEDED.into(), used),
            Event::Handoff,
            Event::StopAndArchive(used),
        ]
    );
    assert_eq!(preserved, DiskLimitPreservation::StopAndArchiveRequested);
    assert_eq!(failure.code, "sandbox_disk_limit_exceeded");
    assert_eq!(failure.used_bytes, used);
    assert_eq!(failure.usable_bytes, 1000 * BLOCK);
    assert_eq!(failure.mount, PathBuf::from("/user-files"));
}

#[tokio::test]
async fn disk_watch_completes_handoff_instead_of_stop_archive() {
    let (_, preserved, events) = run(&[81], Ok(true)).await;
    assert_eq!(preserved, DiskLimitPreservation::HandoffCompleted);
    assert!(!events.iter().any(|e| matches!(e, Event::StopAndArchive(_))));
}

#[tokio::test]
async fn disk_watch_failed_handoff_falls_back_to_stop_archive() {
    let (_, preserved, events) = run(&[90], Err(anyhow::anyhow!("upload failed"))).await;
    assert_eq!(preserved, DiskLimitPreservation::StopAndArchiveRequested);
    assert_eq!(events.last(), Some(&Event::StopAndArchive(900 * BLOCK)));
}

#[test]
fn disk_usage_threshold_uses_df_semantics() {
    // 79.9% of usable space; root-reserved blocks are not counted.
    let below = DiskUsage {
        used_bytes: 799,
        available_bytes: 201,
    };
    assert!(!below.reaches_percent(80));
    let at = DiskUsage {
        used_bytes: 800,
        available_bytes: 200,
    };
    assert!(at.reaches_percent(80));
    let empty = DiskUsage {
        used_bytes: 0,
        available_bytes: 0,
    };
    assert!(empty.reaches_percent(80));
}

#[test]
fn disk_watch_config_defaults_to_ten_seconds_and_80_percent() {
    let config: DiskWatchConfig = serde_json::from_str(r#"{"mount":"/user-files"}"#).unwrap();
    assert_eq!(config, DiskWatchConfig::new("/user-files"));
    assert_eq!(config.disk_watch_interval, Duration::from_secs(10));
    assert_eq!(config.limit_percent, 80);
    let zero = DiskWatchConfig {
        disk_watch_interval: Duration::ZERO,
        ..config
    };
    assert!(zero.validate().is_err());
}

#[test]
fn statvfs_probe_reads_a_real_mount() {
    let dir = tempfile::tempdir().unwrap();
    let usage = StatvfsProbe::new(dir.path()).usage().unwrap();
    assert!(usage.usable_bytes() > 0);
    assert!(
        StatvfsProbe::new(dir.path().join("missing"))
            .usage()
            .is_err()
    );
}
