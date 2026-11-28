//! How reliably this process reaches the server: connection tuning shared by its HTTP clients,
//! and a rolling failure rate that becomes a sandbox admission deferral while it is high.
//!
//! Only transport failures count (connect, timeout, reset; never an HTTP status). A node that
//! heartbeats but cannot complete requests reports the deferral with its machine report and
//! stops asking for work until the rate recovers; failures age out of the window, so a node
//! that stopped asking still recovers.
use aq_machine_load::AdmissionDeferral;
use std::{
    collections::VecDeque,
    sync::Mutex,
    time::{Duration, Instant},
};

/// The admission deferral code the machine report carries while the transport is degraded.
pub const DEGRADED_CODE: &str = "server_transport_degraded";
/// Idle pooled connections are dropped before a gateway (nginx's default `keepalive_timeout`
/// is 75 s; Tailscale and NAT paths are shorter) closes them. A request sent on a connection
/// the far end already closed is a transport failure whose disposition is unknown.
pub const POOL_IDLE_TIMEOUT: Duration = Duration::from_secs(20);
/// TCP keepalive probes: keep a path's mapping alive and find a dead one before a request does.
pub const TCP_KEEPALIVE: Duration = Duration::from_secs(15);
/// Outcomes older than this are forgotten.
pub const WINDOW: Duration = Duration::from_secs(120);
/// At least this many failures in the window, and at least half of its outcomes, degrade.
pub const MIN_FAILURES: usize = 4;

/// Connection settings for a client of the server, which may sit behind a gateway.
pub fn tune(builder: reqwest::ClientBuilder) -> reqwest::ClientBuilder {
    builder
        .pool_idle_timeout(POOL_IDLE_TIMEOUT)
        .tcp_keepalive(TCP_KEEPALIVE)
}

/// A reqwest error with its `source()` chain (connect, reset, timeout), without the URL.
pub fn chain(e: reqwest::Error) -> String {
    let e = e.without_url();
    let mut message = e.to_string();
    let mut source = std::error::Error::source(&e);
    while let Some(s) = source {
        message.push_str(": ");
        message.push_str(&s.to_string());
        source = s.source();
    }
    message
}

#[derive(Default)]
pub struct Health {
    outcomes: VecDeque<(Instant, bool)>,
}
impl Health {
    pub fn record(&mut self, now: Instant, ok: bool) {
        self.prune(now);
        self.outcomes.push_back((now, ok));
    }
    fn prune(&mut self, now: Instant) {
        while let Some((at, _)) = self.outcomes.front() {
            if now.saturating_duration_since(*at) <= WINDOW {
                break;
            }
            self.outcomes.pop_front();
        }
    }
    /// (failures, outcomes) in the window ending `now`.
    pub fn counts(&mut self, now: Instant) -> (usize, usize) {
        self.prune(now);
        let failed = self.outcomes.iter().filter(|(_, ok)| !ok).count();
        (failed, self.outcomes.len())
    }
    pub fn degraded(&mut self, now: Instant) -> bool {
        let (failed, total) = self.counts(now);
        failed >= MIN_FAILURES && failed * 2 >= total
    }
}

static HEALTH: Mutex<Option<Health>> = Mutex::new(None);
static SINCE_MS: Mutex<Option<u64>> = Mutex::new(None);
fn with<T>(f: impl FnOnce(&mut Health) -> T) -> T {
    let mut guard = HEALTH.lock().unwrap_or_else(|e| e.into_inner());
    f(guard.get_or_insert_with(Health::default))
}

/// Records one request to the server: `ok` unless it failed in transport.
pub fn record(ok: bool) {
    with(|h| h.record(Instant::now(), ok));
}
/// Records the outcome of a send; the error is passed through.
pub fn observe<T, E>(result: Result<T, E>) -> Result<T, E> {
    record(result.is_ok());
    result
}
/// The deferral to report while the failure rate is high; `since_ms` holds across checks.
pub fn deferral() -> Option<AdmissionDeferral> {
    let degraded = with(|h| h.degraded(Instant::now()));
    let mut since = SINCE_MS.lock().unwrap_or_else(|e| e.into_inner());
    if !degraded {
        *since = None;
        return None;
    }
    let mut deferral = AdmissionDeferral::new(DEGRADED_CODE, None);
    deferral.since_ms = *since.get_or_insert(deferral.since_ms);
    Some(deferral)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_failure_burst_degrades_and_ages_out() {
        let start = Instant::now();
        let mut health = Health::default();
        for i in 0..3 {
            health.record(start + Duration::from_secs(i), false);
        }
        assert!(!health.degraded(start + Duration::from_secs(3)));
        health.record(start + Duration::from_secs(4), false);
        assert!(health.degraded(start + Duration::from_secs(5)));
        // No new traffic: the failures leave the window.
        assert!(!health.degraded(start + WINDOW + Duration::from_secs(5)));
    }

    #[test]
    fn mostly_successful_traffic_is_not_degraded() {
        let start = Instant::now();
        let mut health = Health::default();
        for i in 0..40 {
            health.record(start + Duration::from_secs(i), i % 8 != 0);
        }
        let now = start + Duration::from_secs(41);
        assert_eq!(health.counts(now), (5, 40));
        assert!(!health.degraded(now));
    }

    #[test]
    fn recovery_through_successes() {
        let start = Instant::now();
        let mut health = Health::default();
        for i in 0..5 {
            health.record(start + Duration::from_secs(i), false);
        }
        for i in 5..30 {
            health.record(start + Duration::from_secs(i), true);
        }
        assert!(!health.degraded(start + Duration::from_secs(30)));
    }

    #[test]
    fn the_process_deferral_reports_and_clears() {
        // The only test that touches the process-wide state; the margins absorb other tests' traffic.
        for _ in 0..50 {
            record(false);
        }
        let first = deferral().expect("degraded");
        assert_eq!(first.code, DEGRADED_CODE);
        assert_eq!(deferral().unwrap().since_ms, first.since_ms);
        for _ in 0..200 {
            record(true);
        }
        assert_eq!(deferral(), None);
    }
}
