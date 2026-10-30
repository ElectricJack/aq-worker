//! Reconnect delay with full jitter.

use std::time::Duration;

use rand::Rng;

const CAP_MS: u64 = 60_000;

/// `uniform(0, min(60s, 1s * 2^attempt))`. Attempt 0 means a 1 s ceiling.
pub fn delay(attempt: u32, rng: &mut impl Rng) -> Duration {
    let ceiling = 1_000u64.checked_shl(attempt).unwrap_or(CAP_MS).min(CAP_MS);
    // A shift can wrap high bits away; anything past 2^6 s is capped anyway.
    let ceiling = if attempt >= 6 { CAP_MS } else { ceiling };
    Duration::from_millis(rng.gen_range(0..=ceiling))
}
