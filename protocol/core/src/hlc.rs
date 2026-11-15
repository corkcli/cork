//! Hybrid logical clock: physical milliseconds plus a counter, so every event
//! the server stamps is strictly after every event it has seen, even when
//! wall clocks disagree. Ordering is `(w, c)`.

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Default, Serialize, Deserialize)]
pub struct Hlc {
    /// Wall-clock milliseconds since the Unix epoch.
    pub w: u64,
    /// Logical counter within one millisecond.
    pub c: u32,
}

impl Hlc {
    pub const fn new(w: u64, c: u32) -> Self {
        Hlc { w, c }
    }
}

/// One node's clock. `now` stamps a local event; `observe` merges a remote
/// stamp. Both are monotonic: each result is greater than the last.
#[derive(Debug, Clone)]
pub struct Clock {
    last: Hlc,
    /// A remote stamp further than this ahead of local time is not adopted
    /// (a skewed or hostile peer cannot drag the clock into the future).
    max_drift_ms: u64,
}

impl Default for Clock {
    fn default() -> Self {
        Clock::new(60_000)
    }
}

impl Clock {
    pub fn new(max_drift_ms: u64) -> Self {
        Clock { last: Hlc::default(), max_drift_ms }
    }

    pub fn last(&self) -> Hlc {
        self.last
    }

    /// Restore after a restart: never go below a stamp already handed out.
    pub fn floor(&mut self, at_least: Hlc) {
        if at_least > self.last {
            self.last = at_least;
        }
    }

    pub fn now(&mut self, phys_ms: u64) -> Hlc {
        self.last = if phys_ms > self.last.w { Hlc::new(phys_ms, 0) } else { Hlc::new(self.last.w, self.last.c.saturating_add(1)) };
        self.last
    }

    pub fn observe(&mut self, remote: Hlc, phys_ms: u64) -> Hlc {
        if remote.w > phys_ms.saturating_add(self.max_drift_ms) {
            return self.now(phys_ms);
        }
        let last = self.last;
        let w = last.w.max(remote.w).max(phys_ms);
        let c = if w == last.w && w == remote.w {
            last.c.max(remote.c).saturating_add(1)
        } else if w == last.w {
            last.c.saturating_add(1)
        } else if w == remote.w {
            remote.c.saturating_add(1)
        } else {
            0
        };
        self.last = Hlc::new(w, c);
        self.last
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn now_is_monotonic_when_wall_clock_stalls_or_goes_back() {
        let mut c = Clock::default();
        let a = c.now(1000);
        let b = c.now(1000);
        let d = c.now(900);
        let e = c.now(1001);
        assert!(a < b && b < d && d < e);
        assert_eq!(e, Hlc::new(1001, 0));
    }

    #[test]
    fn observe_moves_past_remote() {
        let mut c = Clock::default();
        c.now(1000);
        let r = Hlc::new(1500, 7);
        let x = c.observe(r, 1200);
        assert!(x > r);
        assert_eq!(x, Hlc::new(1500, 8));
        let y = c.now(1300);
        assert!(y > x);
    }

    #[test]
    fn observe_ignores_far_future_remote() {
        let mut c = Clock::new(1000);
        let x = c.observe(Hlc::new(1_000_000, 0), 1000);
        assert_eq!(x.w, 1000);
    }

    #[test]
    fn thousands_of_mixed_events_stay_strictly_increasing() {
        let mut c = Clock::default();
        let mut prev = Hlc::default();
        for i in 0..5000u64 {
            let phys = 10_000 + (i % 7) * 3 - (i % 5);
            let x = if i % 3 == 0 { c.observe(Hlc::new(phys + (i % 11), (i % 4) as u32), phys) } else { c.now(phys) };
            assert!(x > prev, "{x:?} <= {prev:?} at {i}");
            prev = x;
        }
    }
}
