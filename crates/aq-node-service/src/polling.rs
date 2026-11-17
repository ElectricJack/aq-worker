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
/// Calls `poll(true)` until it yields `Some`, sleeping `interval` between calls. `pause` and
/// `shutdown` are checked before every call and during the sleep; an `Err` ends the loop.
///
/// For the first `hold` (the pause after a failed attempt) the calls are `poll(false)`: the
/// poll then only reports (inventory, on the same schedule) and asks for no offer, so a slot
/// that waits is still shown with a current capability report. The first `poll(true)` comes
/// when the hold ends, not at the next interval.
pub async fn poll_until_offer<T, F, Fut>(
    mut poll: F,
    interval: std::time::Duration,
    hold: std::time::Duration,
    shutdown: &mut watch::Receiver<bool>,
    pause: &mut watch::Receiver<bool>,
) -> Result<PollLoop<T>>
where
    F: FnMut(bool) -> Fut,
    Fut: Future<Output = Result<Option<T>>>,
{
    let resume = tokio::time::Instant::now() + hold;
    loop {
        if *pause.borrow() {
            return Ok(PollLoop::Paused);
        }
        if *shutdown.borrow() {
            return Ok(PollLoop::Shutdown);
        }
        let held = resume.saturating_duration_since(tokio::time::Instant::now());
        if let Some(offer) = poll(held.is_zero()).await? {
            return Ok(PollLoop::Offer(offer));
        }
        let held = resume.saturating_duration_since(tokio::time::Instant::now());
        let wait = if held.is_zero() {
            interval
        } else {
            held.min(interval)
        };
        tokio::select! {
            _ = tokio::time::sleep(wait) => {}
            changed = shutdown.changed() => if changed.is_err() { return Ok(PollLoop::Shutdown) },
            // A dropped pause sender never pauses; the sleep still ends the wait.
            _ = async { if pause.changed().await.is_err() { std::future::pending::<()>().await } } => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[tokio::test(start_paused = true)]
    async fn poll_until_offer_returns_the_third_result() {
        let calls = std::cell::Cell::new(0);
        let (_s, mut shutdown) = watch::channel(false);
        let (_p, mut pause) = watch::channel(false);
        let found = poll_until_offer(
            |_| {
                calls.set(calls.get() + 1);
                let n = calls.get();
                async move { Ok((n == 3).then_some(7)) }
            },
            std::time::Duration::from_secs(10),
            std::time::Duration::ZERO,
            &mut shutdown,
            &mut pause,
        )
        .await
        .unwrap();
        assert!(matches!(found, PollLoop::Offer(7)));
        assert_eq!(calls.get(), 3);
    }
    #[tokio::test(start_paused = true)]
    async fn poll_until_offer_honours_pause_before_polling() {
        let calls = std::cell::Cell::new(0);
        let (_s, mut shutdown) = watch::channel(false);
        let (_p, mut pause) = watch::channel(true);
        let found = poll_until_offer(
            |_| {
                calls.set(calls.get() + 1);
                async { Ok(Some(1)) }
            },
            std::time::Duration::from_secs(10),
            std::time::Duration::ZERO,
            &mut shutdown,
            &mut pause,
        )
        .await
        .unwrap();
        assert!(matches!(found, PollLoop::<i32>::Paused));
        assert_eq!(calls.get(), 0);
    }
    #[tokio::test(start_paused = true)]
    async fn poll_until_offer_shutdown_during_sleep() {
        let calls = std::cell::Cell::new(0);
        let (stop, mut shutdown) = watch::channel(false);
        let (_p, mut pause) = watch::channel(false);
        let sender = async {
            tokio::time::sleep(std::time::Duration::from_secs(3)).await;
            stop.send(true).unwrap();
        };
        let looped = poll_until_offer(
            |_| {
                calls.set(calls.get() + 1);
                async { Ok(None::<i32>) }
            },
            std::time::Duration::from_secs(10),
            std::time::Duration::ZERO,
            &mut shutdown,
            &mut pause,
        );
        let (found, ()) = tokio::join!(looped, sender);
        assert!(matches!(found.unwrap(), PollLoop::Shutdown));
        assert_eq!(calls.get(), 1);
    }
    #[tokio::test(start_paused = true)]
    async fn poll_until_offer_propagates_errors() {
        let (_s, mut shutdown) = watch::channel(false);
        let (_p, mut pause) = watch::channel(false);
        let error = poll_until_offer(
            |_| async { Err::<Option<i32>, _>(anyhow::anyhow!("boom")) },
            std::time::Duration::from_secs(10),
            std::time::Duration::ZERO,
            &mut shutdown,
            &mut pause,
        )
        .await;
        assert!(error.is_err_and(|e| e.to_string() == "boom"));
    }
    /// During a hold the poll only reports, on the idle schedule, and the first real poll
    /// comes exactly when the hold ends.
    #[tokio::test(start_paused = true)]
    async fn a_held_poll_reports_on_schedule_and_asks_for_offers_when_the_hold_ends() {
        let seen = std::cell::RefCell::new(Vec::new());
        let (_s, mut shutdown) = watch::channel(false);
        let (_p, mut pause) = watch::channel(false);
        let start = tokio::time::Instant::now();
        let found = poll_until_offer(
            |offers: bool| {
                seen.borrow_mut().push((start.elapsed().as_secs(), offers));
                async move { Ok(offers.then_some(7)) }
            },
            std::time::Duration::from_secs(10),
            std::time::Duration::from_secs(24),
            &mut shutdown,
            &mut pause,
        )
        .await
        .unwrap();
        assert!(matches!(found, PollLoop::Offer(7)));
        assert_eq!(
            *seen.borrow(),
            [(0, false), (10, false), (20, false), (24, true)]
        );
    }
}
