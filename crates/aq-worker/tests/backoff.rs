use std::time::Duration;

use aq_worker::backoff::delay;
use rand::SeedableRng;
use rand::rngs::StdRng;

#[test]
fn backoff_is_bounded_and_jittered() {
    let mut rng = StdRng::seed_from_u64(7);
    let samples: Vec<Duration> = (0..1000).map(|_| delay(10, &mut rng)).collect();
    assert!(samples.iter().all(|d| *d <= Duration::from_secs(60)));
    assert!(samples.iter().any(|d| *d != samples[0]));
}

#[test]
fn attempt_zero_has_one_second_ceiling() {
    let mut rng = StdRng::seed_from_u64(1);
    assert!((0..200).all(|_| delay(0, &mut rng) <= Duration::from_secs(1)));
    // Huge attempts must not overflow.
    assert!(delay(u32::MAX, &mut rng) <= Duration::from_secs(60));
}
