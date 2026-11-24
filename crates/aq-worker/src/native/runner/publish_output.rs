//! Disposable output publication is independent of host stop/control. A missing
//! human Watch is normal; exact pending exchanges remain owned by the relay.
use crate::native::launch::PhaseOutputRelay;

#[derive(Default)]
struct PublisherStatus {
    successes: u64,
    failures: u64,
    consecutive_failures: u64,
    reported_activity: bool,
    last: Option<(&'static str, bool, std::time::Instant)>,
}
impl PublisherStatus {
    fn record<E>(
        &mut self,
        relay: &PhaseOutputRelay,
        stage: &'static str,
        outcome: Result<&'static str, E>,
    ) {
        let succeeded = outcome.is_ok();
        let disposition = outcome.unwrap_or("failed");
        if succeeded {
            self.successes = self.successes.saturating_add(1);
            self.consecutive_failures = 0;
        } else {
            self.failures = self.failures.saturating_add(1);
            self.consecutive_failures = self.consecutive_failures.saturating_add(1);
        }
        let first_activity =
            !self.reported_activity && matches!(disposition, "acknowledged" | "watch_present");
        if first_activity || self.status_changed(disposition, succeeded) {
            // Fixed labels and public counters only. Never format transport
            // errors, terminal bytes, headers, credentials or filesystem paths.
            let tail = relay.status().ok();
            tracing::info!(
                terminal_id = %relay.terminal().terminal_id,
                stage,
                disposition,
                successes = self.successes,
                failures = self.failures,
                consecutive_failures = self.consecutive_failures,
                next_sequence = ?tail.map(|s| s.next_sequence),
                retained_bytes = ?tail.map(|s| s.retained_bytes),
                discontinuous = ?tail.map(|s| s.discontinuous),
                draining = ?tail.map(|s| s.draining),
                "native output publisher status"
            );
            self.last = Some((disposition, succeeded, std::time::Instant::now()));
            self.reported_activity |= first_activity;
        }
    }
    fn status_changed(&self, disposition: &'static str, succeeded: bool) -> bool {
        self.last.is_none_or(|(previous, ok, at)| {
            ok != succeeded
                || (!succeeded
                    && self.consecutive_failures.is_power_of_two()
                    && at.elapsed() >= std::time::Duration::from_secs(1))
                || (previous != disposition && at.elapsed() >= std::time::Duration::from_secs(5))
        })
    }
}
pub(crate) struct OutputPublisher {
    upload: tokio::task::JoinHandle<()>,
    polling: tokio::task::JoinHandle<()>,
}
impl OutputPublisher {
    pub(crate) fn start(relay: PhaseOutputRelay) -> Self {
        let polling = tokio::spawn(poll_watches(relay.clone()));
        let upload = tokio::spawn(upload_output(relay));
        Self { upload, polling }
    }
}
async fn poll_watches(relay: PhaseOutputRelay) {
    let mut status = PublisherStatus::default();
    loop {
        // Separate task, mutex and transport lane from output upload.
        // A slow/409/ambiguous upload cannot starve a fresh Watch poll.
        let result = relay.poll_watch_once().await;
        let outcome = result.map(|count| {
            if count == 0 {
                "no_watch"
            } else {
                "watch_present"
            }
        });
        status.record(&relay, "watch", outcome);
        tokio::time::sleep(std::time::Duration::from_millis(250)).await;
    }
}

fn upload_interval(failures: u32) -> std::time::Duration {
    if failures == 0 {
        return std::time::Duration::from_millis(50);
    }
    let base = 1000_u64.saturating_mul(u64::from(failures.min(4)));
    std::time::Duration::from_millis(base + u64::from(rand::random::<u8>()))
}

async fn upload_output(relay: PhaseOutputRelay) {
    let mut registered = false;
    let mut failures = 0_u32;
    let mut registration_status = PublisherStatus::default();
    let mut upload_status = PublisherStatus::default();
    loop {
        let result = if registered {
            let result = relay.upload_once().await;
            upload_status.record(
                &relay,
                "upload",
                result.as_ref().map(|ack| {
                    if ack.is_some() {
                        "acknowledged"
                    } else {
                        "idle"
                    }
                }),
            );
            result.map(|_| ())
        } else {
            let result = relay.register().await;
            registration_status.record(&relay, "register", result.as_ref().map(|_| "registered"));
            result.map(|_| {
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
        tokio::time::sleep(upload_interval(failures)).await;
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
async fn requires_authentication(control: &crate::native::launch::PhaseControlRelay) -> bool {
    control.current_viewport().await.is_ok_and(|view| {
        aq_codex_mapper::classify_current_viewport(&view)
            == aq_codex_mapper::InteractiveViewport::AuthenticationRequired
    })
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
                if started.elapsed().as_millis() >= u128::from(aq_codex_mapper::READY_DELAY_MS)
                    && requires_authentication(&control).await
                {
                    status.store(true, Ordering::SeqCst);
                    return;
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
