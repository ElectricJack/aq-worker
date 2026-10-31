//! CLOCK_BOOTTIME advances across Linux suspend. Restart requires reconciliation.
use std::sync::{
    Arc,
    atomic::{AtomicU64, Ordering},
};
pub trait Clock: Clone {
    fn now_ms(&self) -> u64;
}
#[derive(Clone, Copy)]
pub struct BootClock;
pub fn raw_boot_ms() -> u64 {
    let mut ts = libc::timespec {
        tv_sec: 0,
        tv_nsec: 0,
    };
    // SAFETY: valid writable timespec; Linux CLOCK_BOOTTIME advances on suspend.
    assert_eq!(
        unsafe { libc::clock_gettime(libc::CLOCK_BOOTTIME, &mut ts) },
        0,
        "CLOCK_BOOTTIME unavailable"
    );
    (ts.tv_sec as u64)
        .saturating_mul(1000)
        .saturating_add(ts.tv_nsec as u64 / 1_000_000)
}
impl Clock for BootClock {
    fn now_ms(&self) -> u64 {
        let offset = std::env::var_os("AQ_WORKER_CLOCK_OFFSET_FILE")
            .map(|p| {
                std::fs::read_to_string(p)
                    .expect("fixture clock offset file")
                    .trim()
                    .parse::<u64>()
                    .expect("fixture clock offset integer")
            })
            .unwrap_or(0);
        raw_boot_ms().saturating_add(offset)
    }
}
#[derive(Clone, Default)]
pub struct FakeClock(Arc<AtomicU64>);
impl FakeClock {
    pub fn new(ms: u64) -> Self {
        Self(Arc::new(AtomicU64::new(ms)))
    }
    pub fn advance(&self, ms: u64) {
        self.0.fetch_add(ms, Ordering::SeqCst);
    }
}
impl Clock for FakeClock {
    fn now_ms(&self) -> u64 {
        self.0.load(Ordering::SeqCst)
    }
}
