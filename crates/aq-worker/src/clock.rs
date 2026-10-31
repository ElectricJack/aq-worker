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
    aq_session_host::ownership::raw_boot_ms()
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
