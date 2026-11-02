//! Disposable output publication is independent of host stop/control. A missing
//! human Watch is normal; exact pending exchanges remain owned by the relay.
use crate::native::launch::PhaseOutputRelay;
pub(crate) struct OutputPublisher {
    upload: tokio::task::JoinHandle<()>,
    polling: tokio::task::JoinHandle<()>,
}
impl OutputPublisher {
    pub(crate) fn start(relay: PhaseOutputRelay) -> Self {
        let polling_relay = relay.clone();
        let polling = tokio::spawn(async move {
            loop {
                // Separate task, mutex and transport lane from output upload.
                // A slow/409/ambiguous upload cannot starve a fresh Watch poll.
                let _ = polling_relay.poll_watch_once().await;
                tokio::time::sleep(std::time::Duration::from_millis(250)).await;
            }
        });
        let upload = tokio::spawn(async move {
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
        });
        Self { upload, polling }
    }
}
impl Drop for OutputPublisher {
    fn drop(&mut self) {
        self.upload.abort();
        self.polling.abort();
    }
}

/// V1 interactive control stays independent of disposable output and physical stop.
pub(super) struct ControlPublisher {
    task: tokio::task::JoinHandle<()>,
    authentication_required: std::sync::Arc<std::sync::atomic::AtomicBool>,
}
impl ControlPublisher {
    pub(super) fn start(control: crate::native::launch::PhaseControlRelay) -> Self {
        use std::sync::{
            Arc,
            atomic::{AtomicBool, Ordering},
        };
        let authentication_required = Arc::new(AtomicBool::new(false));
        let status = authentication_required.clone();
        let task = tokio::spawn(async move {
            let started = std::time::Instant::now();
            loop {
                if started.elapsed().as_millis() >= u128::from(aq_codex_mapper::READY_DELAY_MS) {
                    if let Ok(view) = control.current_viewport().await {
                        if aq_codex_mapper::classify_current_viewport(&view)
                            == aq_codex_mapper::InteractiveViewport::AuthenticationRequired
                        {
                            status.store(true, Ordering::SeqCst);
                            return;
                        }
                    }
                }
                // The existing bridge retains original pending control exchanges.
                let _ = control.poll_control_once().await;
                tokio::time::sleep(std::time::Duration::from_millis(250)).await;
            }
        });
        Self {
            task,
            authentication_required,
        }
    }
    pub(super) fn authentication_required(&self) -> bool {
        self.authentication_required
            .load(std::sync::atomic::Ordering::SeqCst)
    }
}
impl Drop for ControlPublisher {
    fn drop(&mut self) {
        self.task.abort();
    }
}
