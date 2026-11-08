//! Retry delays with full jitter.

use std::time::Duration;

use rand::Rng;

/// `uniform(0, min(60s, 1s * 2^attempt))`. Attempt 0 means a 1 s ceiling.
pub fn delay(attempt: u32, rng: &mut impl Rng) -> Duration {
    jittered(attempt, SECOND, SECOND * 60, rng)
}
const SECOND: Duration = Duration::from_secs(1);

/// `uniform(0, min(cap, base * 2^attempt))`, never overflowing.
pub fn jittered(attempt: u32, base: Duration, cap: Duration, rng: &mut impl Rng) -> Duration {
    let ceiling = base.saturating_mul(1u32 << attempt.min(31)).min(cap);
    Duration::from_millis(rng.gen_range(0..=ceiling.as_millis() as u64))
}
