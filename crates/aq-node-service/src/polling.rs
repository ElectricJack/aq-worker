//! Idle worker polling; contains no execution authority.
use anyhow::Result;
use std::future::Future;
use tokio::sync::watch;

/// Delay between polls of `run_until_attempt` while no offer is available.
pub const IDLE_POLL_INTERVAL: std::time::Duration = std::time::Duration::from_secs(10);
pub enum PollLoop<T> {
    Offer(T),
    Paused,
    Shutdown,
}
/// Calls `poll` until it yields `Some`, sleeping `interval` between calls. `pause` and
/// `shutdown` are checked before every call and during the sleep; an `Err` ends the loop.
pub async fn poll_until_offer<T, F, Fut>(
    mut poll: F,
    interval: std::time::Duration,
    shutdown: &mut watch::Receiver<bool>,
    pause: &mut watch::Receiver<bool>,
) -> Result<PollLoop<T>>
where
    F: FnMut() -> Fut,
    Fut: Future<Output = Result<Option<T>>>,
{
    loop {
        if *pause.borrow() {
            return Ok(PollLoop::Paused);
        }
        if *shutdown.borrow() {
            return Ok(PollLoop::Shutdown);
        }
        if let Some(offer) = poll().await? {
            return Ok(PollLoop::Offer(offer));
        }
        tokio::select! {
            _ = tokio::time::sleep(interval) => {}
            changed = shutdown.changed() => if changed.is_err() { return Ok(PollLoop::Shutdown) },
            // A dropped pause sender never pauses; the sleep still ends the wait.
            _ = async { if pause.changed().await.is_err() { std::future::pending::<()>().await } } => {}
        }
    }
}
