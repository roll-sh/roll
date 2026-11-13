//! Per-grant request limits (docs/permissions.md, "Rate limits"): a grant's `rate_per_minute`, as
//! a GCRA (one timestamp per grant: a full minute's worth may come at once, then one request every
//! `60 s / rate`). Counted in memory on each node: a limit protects the daemon from a runaway
//! agent, so it needs no shared state.

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, Instant};

const WINDOW: Duration = Duration::from_secs(60);
/// Grants tracked before the ones that are idle again are dropped.
const PRUNE_AT: usize = 4096;

/// The highest `rate_per_minute` a grant may have.
pub const MAX_RATE: u32 = 100_000;

#[derive(Default)]
pub struct Limiter {
    /// When each grant's allowance is next fully spent (GCRA's "theoretical arrival time").
    tat: Mutex<HashMap<String, Instant>>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Verdict {
    /// Allowed: requests still allowed right now, and when the allowance is whole again.
    Allowed { remaining: u32, reset: Duration },
    /// Over: when the next request may come.
    Over { retry_after: Duration },
}

impl Limiter {
    pub fn check(&self, grant: &str, per_minute: u32, now: Instant) -> Verdict {
        let per_minute = per_minute.clamp(1, MAX_RATE);
        let interval = WINDOW / per_minute;
        let mut map = self.tat.lock().unwrap_or_else(|e| e.into_inner());
        if map.len() >= PRUNE_AT {
            map.retain(|_, t| *t > now);
        }
        let tat = map.get(grant).copied().filter(|t| *t > now).unwrap_or(now);
        let next = tat + interval;
        let ahead = next - now;
        if ahead > WINDOW {
            return Verdict::Over { retry_after: ahead - WINDOW };
        }
        map.insert(grant.to_string(), next);
        let remaining = ((WINDOW - ahead).as_nanos() / interval.as_nanos().max(1)) as u32;
        Verdict::Allowed { remaining, reset: ahead }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_minutes_worth_then_one_per_interval() {
        let l = Limiter::default();
        let t0 = Instant::now();
        for i in 0..60 {
            assert!(matches!(l.check("g", 60, t0), Verdict::Allowed { remaining, .. } if remaining == 59 - i), "{i}");
        }
        let Verdict::Over { retry_after } = l.check("g", 60, t0) else { panic!("61st allowed") };
        assert_eq!(retry_after, Duration::from_secs(1));
        assert!(matches!(l.check("g", 60, t0 + Duration::from_secs(1)), Verdict::Allowed { .. }));
        assert!(matches!(l.check("other", 60, t0), Verdict::Allowed { remaining: 59, .. }), "grants don't share");
    }
}
