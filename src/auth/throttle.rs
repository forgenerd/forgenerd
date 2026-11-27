//! Brute-force protection for password logins: after too many failures from one client
//! address within a window, further attempts from it are refused until the window ends.
//! Kept in memory; a restart clears it, which is acceptable for a single-node panel.

use std::collections::HashMap;
use std::net::IpAddr;
use std::sync::Mutex;
use std::time::{Duration, Instant};

pub const MAX_FAILURES: u32 = 10;
pub const WINDOW: Duration = Duration::from_secs(15 * 60);
/// Bounds memory under a flood from many addresses; expired entries go first.
const MAX_TRACKED: usize = 10_000;

#[derive(Default)]
pub struct LoginThrottle {
    /// Failures per address and when the first of them happened.
    failures: Mutex<HashMap<IpAddr, (u32, Instant)>>,
}

impl LoginThrottle {
    /// `Err(retry_after)` when `ip` must wait before trying again.
    pub fn check(&self, ip: IpAddr) -> Result<(), Duration> {
        let mut map = self.lock();
        match map.get(&ip) {
            Some((_, since)) if since.elapsed() >= WINDOW => {
                map.remove(&ip);
                Ok(())
            }
            Some((count, since)) if *count >= MAX_FAILURES => Err(WINDOW
                .saturating_sub(since.elapsed())
                .max(Duration::from_secs(1))),
            _ => Ok(()),
        }
    }

    pub fn record_failure(&self, ip: IpAddr) {
        let mut map = self.lock();
        if map.len() >= MAX_TRACKED && !map.contains_key(&ip) {
            map.retain(|_, (_, since)| since.elapsed() < WINDOW);
            if map.len() >= MAX_TRACKED {
                return;
            }
        }
        let entry = map.entry(ip).or_insert((0, Instant::now()));
        if entry.1.elapsed() >= WINDOW {
            *entry = (0, Instant::now());
        }
        entry.0 += 1;
    }

    pub fn reset(&self, ip: IpAddr) {
        self.lock().remove(&ip);
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, HashMap<IpAddr, (u32, Instant)>> {
        self.failures
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn blocks_after_max_failures_per_address() {
        let throttle = LoginThrottle::default();
        let (a, b): (IpAddr, IpAddr) = ("192.0.2.1".parse().unwrap(), "192.0.2.2".parse().unwrap());
        for _ in 0..MAX_FAILURES {
            assert!(throttle.check(a).is_ok());
            throttle.record_failure(a);
        }
        let wait = throttle.check(a).unwrap_err();
        assert!(wait > WINDOW - Duration::from_secs(5) && wait <= WINDOW);
        assert!(throttle.check(b).is_ok(), "other addresses are unaffected");
        throttle.reset(a);
        assert!(throttle.check(a).is_ok());
    }
}
