//! Disposable output publication is independent of host stop/control. A missing
//! human Watch is normal; exact pending exchanges remain owned by the relay.
use crate::native::launch::PhaseOutputRelay;
pub(super) struct OutputPublisher(tokio::task::JoinHandle<()>);
impl OutputPublisher {
    pub(super) fn start(relay: PhaseOutputRelay) -> Self {
        Self(tokio::spawn(async move {
            let mut registered = false;
            let mut failures = 0_u32;
            loop {
                let result = if registered {
                    relay.upload_once().await.map(|_| ())
                } else {
                    relay.register().await.map(|_| {
                        registered = true;
                    })
                };
                if result.is_ok() {
                    failures = 0;
                } else {
                    failures = failures.saturating_add(1);
                }
                // Errors do not discard pending bytes or grant any new right.
                // Transport retry is explicit, bounded and output-only; no
                // execution/terminal-input operation uses this policy.
                let base = if failures == 0 {
                    250
                } else {
                    1000_u64.saturating_mul(u64::from(failures.min(4)))
                };
                let jitter = u64::from(rand::random::<u8>());
                tokio::time::sleep(std::time::Duration::from_millis(base + jitter)).await;
            }
        }))
    }
}
impl Drop for OutputPublisher {
    fn drop(&mut self) {
        self.0.abort();
    }
}
