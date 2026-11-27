//! In-memory login rate limiting: after `MAX_FAILURES` failures within
//! `WINDOW` a key (account or client IP) is locked until the oldest failure
//! falls out of the window.

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, Instant};

pub const MAX_FAILURES: usize = 5;
pub const WINDOW: Duration = Duration::from_secs(15 * 60);
pub const LOCKED_MESSAGE: &str = "Te veel mislukte pogingen. Probeer het over 15 minuten opnieuw.";

#[derive(Debug, Default)]
pub struct RateLimiter {
    failures: Mutex<HashMap<String, Vec<Instant>>>,
}

impl RateLimiter {
    pub fn new() -> Self {
        Self::default()
    }

    /// `true` when any of the keys is locked.
    pub fn is_locked(&self, keys: &[String]) -> bool {
        self.is_locked_at(keys, Instant::now())
    }

    pub fn record_failure(&self, keys: &[String]) {
        self.record_failure_at(keys, Instant::now());
    }

    /// Clear the failures of a key after a successful login.
    pub fn reset(&self, key: &str) {
        self.lock().remove(key);
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, HashMap<String, Vec<Instant>>> {
        self.failures.lock().unwrap_or_else(|p| p.into_inner())
    }

    fn is_locked_at(&self, keys: &[String], now: Instant) -> bool {
        let mut map = self.lock();
        keys.iter().any(|k| {
            let Some(list) = map.get_mut(k) else {
                return false;
            };
            list.retain(|t| now.duration_since(*t) < WINDOW);
            list.len() >= MAX_FAILURES
        })
    }

    fn record_failure_at(&self, keys: &[String], now: Instant) {
        let mut map = self.lock();
        // Keep the map from growing without bound.
        if map.len() > 10_000 {
            map.retain(|_, l| l.iter().any(|t| now.duration_since(*t) < WINDOW));
        }
        for k in keys {
            let list = map.entry(k.clone()).or_default();
            list.retain(|t| now.duration_since(*t) < WINDOW);
            list.push(now);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn locks_after_max_failures_and_unlocks_after_window() {
        let rl = RateLimiter::new();
        let keys = vec!["acct:a".to_string(), "ip:1".to_string()];
        let t0 = Instant::now();
        for i in 0..MAX_FAILURES {
            assert!(!rl.is_locked_at(&keys, t0), "attempt {i}");
            rl.record_failure_at(&keys, t0);
        }
        assert!(rl.is_locked_at(&keys, t0));
        // Same IP, other account: still locked via the IP key.
        assert!(rl.is_locked_at(&["acct:b".into(), "ip:1".into()], t0));
        // Other IP and account: free.
        assert!(!rl.is_locked_at(&["acct:b".into(), "ip:2".into()], t0));
        assert!(!rl.is_locked_at(&keys, t0 + WINDOW + Duration::from_secs(1)));
    }

    #[test]
    fn reset_clears_a_key() {
        let rl = RateLimiter::new();
        let keys = vec!["acct:a".to_string()];
        for _ in 0..MAX_FAILURES {
            rl.record_failure(&keys);
        }
        assert!(rl.is_locked(&keys));
        rl.reset("acct:a");
        assert!(!rl.is_locked(&keys));
    }
}
