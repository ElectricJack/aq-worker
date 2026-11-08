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

#[test]
fn jittered_is_bounded_by_base_doubling_and_cap() {
    use aq_worker::backoff::jittered;
    let mut rng = StdRng::seed_from_u64(3);
    let (base, cap) = (Duration::from_millis(500), Duration::from_secs(8));
    for attempt in [0, 1, 2, 3, 4, 5, 31, u32::MAX] {
        let ceiling = Duration::from_millis(500u64.saturating_mul(1 << attempt.min(31))).min(cap);
        let samples: Vec<_> = (0..200)
            .map(|_| jittered(attempt, base, cap, &mut rng))
            .collect();
        assert!(samples.iter().all(|d| *d <= ceiling), "{attempt}");
        assert!(
            samples.iter().any(|d| *d != samples[0]),
            "{attempt}: jittered"
        );
    }
}

#[test]
fn equal_jittered_never_waits_under_half_its_ceiling() {
    use aq_worker::backoff::equal_jittered;
    let mut rng = StdRng::seed_from_u64(5);
    let (base, cap) = (Duration::from_millis(500), Duration::from_secs(8));
    for attempt in [0, 1, 2, 3, 4, 5, 31, u32::MAX] {
        let ceiling = Duration::from_millis(500u64.saturating_mul(1 << attempt.min(31))).min(cap);
        let samples: Vec<_> = (0..200)
            .map(|_| equal_jittered(attempt, base, cap, &mut rng))
            .collect();
        assert!(
            samples.iter().all(|d| *d >= ceiling / 2 && *d <= ceiling),
            "{attempt}"
        );
        assert!(
            samples.iter().any(|d| *d != samples[0]),
            "{attempt}: jittered"
        );
    }
}
