//! Idle worker polling; contains no execution authority.
use anyhow::Result;
use std::future::Future;
use tokio::sync::watch;

/// Delay between polls of `run_until_attempt` while no offer is available.
pub const IDLE_POLL_INTERVAL: std::time::Duration = std::time::Duration::from_secs(10);
/// A slot's delay before its first poll and its idle interval (`interval` ±10 %), both random,
/// so the slots of one node do not poll in the same millisecond.
pub fn staggered(interval: std::time::Duration) -> (std::time::Duration, std::time::Duration) {
    use rand::Rng;
    let mut rng = rand::thread_rng();
    let first = interval.min(std::time::Duration::from_secs(10)) / 10;
    (
        first.mul_f64(rng.gen_range(0.0..1.0)),
        interval.mul_f64(rng.gen_range(0.9..1.1)),
    )
}
pub enum PollLoop<T> {
    Offer(T),
    Paused,
    Shutdown,
}

/// A dropped pause sender never pauses; leave the timer and shutdown watch active.
async fn pause_changed(pause: &mut watch::Receiver<bool>) {
    if pause.changed().await.is_err() {
        std::future::pending::<()>().await;
    }
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
        let until = tokio::time::Instant::now() + wait;
        loop {
            tokio::select! {
                _ = tokio::time::sleep_until(until) => break,
                changed = shutdown.changed() => if changed.is_err() { return Ok(PollLoop::Shutdown) } else { break },
                // A notification that leaves the slot unpaused (a sender repeating the
                // same value) keeps the interval; only a pause ends the wait early.
                _ = pause_changed(pause) => if *pause.borrow() { break },
            }
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
    async fn repeated_unpaused_notifications_keep_the_idle_interval() {
        let calls = std::cell::Cell::new(0);
        let (_s, mut shutdown) = watch::channel(false);
        let (p, mut pause) = watch::channel(false);
        let interval = std::time::Duration::from_secs(10);
        let start = tokio::time::Instant::now();
        let sender = async {
            for _ in 0..30 {
                tokio::time::sleep(std::time::Duration::from_millis(500)).await;
                p.send_replace(false);
            }
        };
        let second = std::cell::Cell::new(None);
        let looped = poll_until_offer(
            |_| {
                calls.set(calls.get() + 1);
                if calls.get() == 2 {
                    second.set(Some(start.elapsed()));
                }
                async { Ok((calls.get() == 2).then_some(7)) }
            },
            interval,
            std::time::Duration::ZERO,
            &mut shutdown,
            &mut pause,
        );
        let (found, ()) = tokio::join!(looped, sender);
        assert!(matches!(found.unwrap(), PollLoop::Offer(7)));
        assert_eq!(calls.get(), 2);
        assert_eq!(second.get(), Some(interval));
    }
    #[tokio::test(start_paused = true)]
    async fn a_pause_during_the_wait_ends_it() {
        let (_s, mut shutdown) = watch::channel(false);
        let (p, mut pause) = watch::channel(false);
        let start = tokio::time::Instant::now();
        let sender = async {
            tokio::time::sleep(std::time::Duration::from_secs(3)).await;
            p.send_replace(true);
        };
        let looped = poll_until_offer(
            |_| async { Ok(None::<i32>) },
            std::time::Duration::from_secs(10),
            std::time::Duration::ZERO,
            &mut shutdown,
            &mut pause,
        );
        let (found, ()) = tokio::join!(looped, sender);
        assert!(matches!(found.unwrap(), PollLoop::Paused));
        assert_eq!(start.elapsed(), std::time::Duration::from_secs(3));
    }
    #[tokio::test(start_paused = true)]
    async fn dropped_pause_sender_keeps_idle_schedule() {
        let calls = std::cell::Cell::new(0);
        let (_s, mut shutdown) = watch::channel(false);
        let (p, mut pause) = watch::channel(false);
        drop(p);
        let start = tokio::time::Instant::now();
        let interval = std::time::Duration::from_secs(10);

        let found = poll_until_offer(
            |_| {
                calls.set(calls.get() + 1);
                async { Ok((calls.get() == 2).then_some(7)) }
            },
            interval,
            std::time::Duration::ZERO,
            &mut shutdown,
            &mut pause,
        )
        .await
        .unwrap();

        assert!(matches!(found, PollLoop::Offer(7)));
        assert_eq!(calls.get(), 2);
        assert_eq!(start.elapsed(), interval);
    }
    #[tokio::test(start_paused = true)]
    async fn dropped_pause_sender_does_not_block_shutdown_channel_closure() {
        let calls = std::cell::Cell::new(0);
        let (stop, mut shutdown) = watch::channel(false);
        let (p, mut pause) = watch::channel(false);
        drop(p);
        let start = tokio::time::Instant::now();
        let sender = async {
            tokio::time::sleep(std::time::Duration::from_secs(3)).await;
            drop(stop);
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
        assert_eq!(start.elapsed(), std::time::Duration::from_secs(3));
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
