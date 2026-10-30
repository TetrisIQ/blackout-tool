//! Bandwidth limit shared by all download threads.

use std::sync::Mutex;
use std::time::{Duration, Instant};

pub struct RateLimiter {
    bytes_per_sec: Option<u64>,
    next_free: Mutex<Instant>,
}

impl RateLimiter {
    /// `None` or `Some(0)` means unlimited.
    pub fn new(bytes_per_sec: Option<u64>) -> Self {
        Self {
            bytes_per_sec: bytes_per_sec.filter(|r| *r > 0),
            next_free: Mutex::new(Instant::now()),
        }
    }

    pub fn rate(&self) -> Option<u64> {
        self.bytes_per_sec
    }

    pub fn is_limited(&self) -> bool {
        self.bytes_per_sec.is_some()
    }

    /// Reserves time for `n` bytes and returns how long the caller must wait before using it.
    /// Callers queue behind each other, so the long-run rate across threads stays at the limit.
    pub fn delay_for(&self, n: u64, now: Instant) -> Duration {
        let Some(rate) = self.bytes_per_sec else {
            return Duration::ZERO;
        };
        let mut next = self.next_free.lock().unwrap_or_else(|e| e.into_inner());
        let start = (*next).max(now);
        *next = start + Duration::from_secs_f64(n as f64 / rate as f64);
        start - now
    }

    pub fn acquire(&self, n: u64) {
        let wait = self.delay_for(n, Instant::now());
        if !wait.is_zero() {
            std::thread::sleep(wait);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unlimited_never_waits() {
        for r in [None, Some(0)] {
            let l = RateLimiter::new(r);
            assert!(!l.is_limited());
            assert_eq!(l.delay_for(1 << 40, Instant::now()), Duration::ZERO);
        }
    }

    #[test]
    fn requests_queue_behind_each_other() {
        let l = RateLimiter::new(Some(1000));
        let t = Instant::now();
        assert_eq!(l.delay_for(500, t), Duration::ZERO, "first request is free");
        assert_eq!(l.delay_for(500, t), Duration::from_millis(500));
        assert_eq!(l.delay_for(1000, t), Duration::from_millis(1000));
    }

    #[test]
    fn idle_time_is_not_banked() {
        let l = RateLimiter::new(Some(1000));
        let t = Instant::now();
        l.delay_for(1000, t);
        let later = t + Duration::from_secs(10);
        assert_eq!(l.delay_for(100, later), Duration::ZERO);
        assert_eq!(l.delay_for(100, later), Duration::from_millis(100));
    }

    #[test]
    fn acquire_actually_sleeps() {
        let l = RateLimiter::new(Some(10_000));
        l.acquire(1000);
        let t = Instant::now();
        l.acquire(1000);
        assert!(t.elapsed() >= Duration::from_millis(80), "{:?}", t.elapsed());
    }
}
