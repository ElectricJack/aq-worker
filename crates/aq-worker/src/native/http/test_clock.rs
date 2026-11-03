//! Explicit deadline transitions for tests that still perform real HTTP and fsync.
//! Tokio's paused time does not control Linux CLOCK_BOOTTIME. Scope this override
//! to one runtime, including its spawned tasks and blocking storage workers, so
//! concurrently running tests keep independent clocks. Production has no override.
use std::{
    collections::HashMap,
    sync::{Mutex, OnceLock},
};
use tokio::runtime::{Handle, Id};

fn clocks() -> &'static Mutex<HashMap<Id, u64>> {
    static CLOCKS: OnceLock<Mutex<HashMap<Id, u64>>> = OnceLock::new();
    CLOCKS.get_or_init(Mutex::default)
}

pub(super) fn now() -> Option<u64> {
    let runtime = Handle::try_current().ok()?.id();
    clocks().lock().unwrap().get(&runtime).copied()
}

pub(crate) struct TestClock {
    runtime: Id,
}

impl TestClock {
    pub(crate) fn new() -> Self {
        let runtime = Handle::current().id();
        assert!(now().is_none(), "test clock already installed");
        // Keep historical boot namespaces valid by starting from the kernel.
        let initial = super::boot_ns().unwrap();
        assert!(clocks().lock().unwrap().insert(runtime, initial).is_none());
        Self { runtime }
    }

    pub(crate) fn advance_to(&self, target: u64) {
        let mut clocks = clocks().lock().unwrap();
        let now = clocks.get_mut(&self.runtime).unwrap();
        assert!(target >= *now, "test boot clock cannot move backwards");
        *now = target;
    }
}

impl Drop for TestClock {
    fn drop(&mut self) {
        clocks().lock().unwrap().remove(&self.runtime).unwrap();
    }
}

/// Cross an existing cap, or wait for the real kernel cap in unmodified tests.
pub(crate) async fn expire(deadline: u64) {
    let runtime = Handle::current().id();
    {
        let mut clocks = clocks().lock().unwrap();
        if let Some(now) = clocks.get_mut(&runtime) {
            *now = (*now).max(deadline.checked_add(1).unwrap());
            return;
        }
    }
    while super::boot_ns().unwrap() <= deadline {
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
}

#[test]
fn runtime_clock_reaches_async_and_blocking_workers_and_cleans_up() {
    let first = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .unwrap();
    let second = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(1)
        .enable_all()
        .build()
        .unwrap();
    first.block_on(async {
        let clock = TestClock::new();
        let future = super::boot_ns().unwrap() + 3_600_000_000_000;
        clock.advance_to(future);
        assert_eq!(
            tokio::spawn(async { super::boot_ns().unwrap() })
                .await
                .unwrap(),
            future
        );
        assert_eq!(
            tokio::task::spawn_blocking(|| super::boot_ns().unwrap())
                .await
                .unwrap(),
            future
        );
        let other = second.spawn(async {
            assert!(now().is_none());
        });
        other.await.unwrap();
        expire(future).await;
        assert_eq!(super::boot_ns().unwrap(), future + 1);
        drop(clock);
        assert!(now().is_none());
        assert!(super::boot_ns().unwrap() < future);
        let kernel_deadline = super::boot_ns().unwrap() + 1_000_000;
        expire(kernel_deadline).await;
        assert!(super::boot_ns().unwrap() > kernel_deadline);
    });
}
