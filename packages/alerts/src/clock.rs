//! Time sources.

use std::sync::atomic::{AtomicI64, Ordering};

/// Source of the current time in Unix seconds.
pub trait Clock: Send + Sync {
    /// Current time (Unix seconds).
    fn now(&self) -> i64;
}

/// Wall-clock time.
#[derive(Debug, Clone, Copy, Default)]
pub struct SystemClock;

impl Clock for SystemClock {
    fn now(&self) -> i64 {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| i64::try_from(d.as_secs()).unwrap_or(i64::MAX))
    }
}

/// Manually controlled clock for tests.
#[derive(Debug, Default)]
pub struct FakeClock(AtomicI64);

impl FakeClock {
    /// Creates a clock reading `now`.
    #[must_use]
    pub const fn new(now: i64) -> Self {
        Self(AtomicI64::new(now))
    }

    /// Sets the current time.
    pub fn set(&self, now: i64) {
        self.0.store(now, Ordering::SeqCst);
    }

    /// Advances the clock by `secs`.
    pub fn advance(&self, secs: i64) {
        self.0.fetch_add(secs, Ordering::SeqCst);
    }
}

impl Clock for FakeClock {
    fn now(&self) -> i64 {
        self.0.load(Ordering::SeqCst)
    }
}
