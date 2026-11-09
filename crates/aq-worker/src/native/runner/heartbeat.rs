//! Reuse the enrolled connector heartbeat; fatal transport refusal requests stop.
use super::*;
use crate::client::{CallError, Client, run_loop};
use crate::native::http::{FETCH_SPAN, TransportFailed, boot_ns, fetch_retried};
use std::sync::atomic::{AtomicBool, Ordering};
pub(super) struct Heartbeat {
    tasks: [tokio::task::JoinHandle<()>; 2],
    failed: Arc<AtomicBool>,
}
impl Drop for Heartbeat {
    fn drop(&mut self) {
        for task in &self.tasks {
            task.abort();
        }
    }
}
async fn stopped(receiver: &mut watch::Receiver<bool>) {
    while !*receiver.borrow() {
        if receiver.changed().await.is_err() {
            break;
        }
    }
}
impl Heartbeat {
    pub(super) async fn start(
        worker: &WorkerConfig,
        mut external: watch::Receiver<bool>,
    ) -> Result<(Self, watch::Receiver<bool>)> {
        let client = Client::from_config(worker)?;
        let request = aq_protocol::HeartbeatRequest {
            worker_version: env!("CARGO_PKG_VERSION").into(),
            protocol_version: aq_protocol::PROTOCOL_VERSION,
        };
        // #121 answers lock timeouts with 503: a heartbeat only records liveness, so it is resent.
        let beat = || async {
            client.heartbeat(&request).await.map_err(|e| match e {
                CallError::Transient(why) => {
                    anyhow::anyhow!(why).context(TransportFailed("heartbeat refused transiently"))
                }
                other => other.into(),
            })
        };
        let deadline = boot_ns()? + FETCH_SPAN.as_nanos() as u64;
        fetch_retried(deadline, beat)
            .await
            .context("native enrollment heartbeat refused")?;
        let (tx, rx) = watch::channel(false);
        let signal = tx.clone();
        let forwarding = tokio::spawn(async move {
            stopped(&mut external).await;
            let _ = signal.send(true);
        });
        let mut shutdown = rx.clone();
        let failed = Arc::new(AtomicBool::new(false));
        let failed_task = failed.clone();
        let heartbeat = tokio::spawn(async move {
            if run_loop(&client, stopped(&mut shutdown)).await.is_err() {
                failed_task.store(true, Ordering::SeqCst);
                let _ = tx.send(true);
            }
        });
        Ok((
            Self {
                tasks: [forwarding, heartbeat],
                failed,
            },
            rx,
        ))
    }
    pub(super) fn check(&self) -> Result<()> {
        ensure!(
            !self.failed.load(Ordering::SeqCst),
            "native connector heartbeat refused; stop required"
        );
        Ok(())
    }
}
