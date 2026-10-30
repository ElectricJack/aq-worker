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
impl Clock for BootClock {
    fn now_ms(&self) -> u64 {
        let mut ts = libc::timespec {
            tv_sec: 0,
            tv_nsec: 0,
        };
        // SAFETY: valid writable timespec; Linux CLOCK_BOOTTIME is suspend-aware.
        let rc = unsafe { libc::clock_gettime(libc::CLOCK_BOOTTIME, &mut ts) };
        assert_eq!(
            rc, 0,
            "CLOCK_BOOTTIME unavailable; refusing uncertain clock"
        );
        (ts.tv_sec as u64)
            .saturating_mul(1000)
            .saturating_add(ts.tv_nsec as u64 / 1_000_000)
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
