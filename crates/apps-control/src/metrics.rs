//! One environment's meter: what the proxy and the reaper see, gathered per minute into a
//! [`MinuteRow`] (metrics) and a [`UsageAdd`] (metering). Latency is a fixed-bucket histogram,
//! so a minute costs the same however busy it was.

use crate::store::{MinuteRow, UsageAdd};
use serde::Serialize;

/// Upper bounds of the latency buckets, in milliseconds (the last is everything slower).
const BUCKETS_MS: [f64; 16] = [1.0, 2.0, 5.0, 10.0, 20.0, 50.0, 100.0, 200.0, 300.0, 500.0, 1000.0, 2000.0, 5000.0, 10_000.0, 30_000.0, f64::INFINITY];

#[derive(Clone, Debug, Default)]
pub struct Minute {
    pub minute: i64,
    pub requests: i64,
    pub errors: i64,
    hist: [u64; 16],
    pub cpu_seconds: f64,
    pub gb_seconds: f64,
    pub memory_max: i64,
    pub instances_max: i64,
    pub cold_starts: i64,
    pub cold_ms_sum: f64,
    pub egress_bytes: i64,
    pub build_seconds: f64,
}

impl Minute {
    fn new(minute: i64) -> Minute {
        Minute { minute, ..Default::default() }
    }

    fn quantile(&self, q: f64) -> f64 {
        let total: u64 = self.hist.iter().sum();
        if total == 0 {
            return 0.0;
        }
        let want = (total as f64 * q).ceil().max(1.0) as u64;
        let mut seen = 0;
        for (i, n) in self.hist.iter().enumerate() {
            seen += n;
            if seen >= want {
                // The last bucket has no upper bound: report its lower one.
                return if BUCKETS_MS[i].is_finite() { BUCKETS_MS[i] } else { BUCKETS_MS[i - 1] };
            }
        }
        BUCKETS_MS[14]
    }

    pub fn row(&self) -> MinuteRow {
        MinuteRow {
            minute: self.minute,
            requests: self.requests,
            errors: self.errors,
            p50_ms: self.quantile(0.5),
            p95_ms: self.quantile(0.95),
            p99_ms: self.quantile(0.99),
            // Cores used on average over the minute.
            cpu: self.cpu_seconds / 60.0,
            memory_bytes: self.memory_max,
            instances: self.instances_max,
            cold_starts: self.cold_starts,
            cold_start_ms: if self.cold_starts > 0 { self.cold_ms_sum / self.cold_starts as f64 } else { 0.0 },
        }
    }

    pub fn usage(&self) -> UsageAdd {
        UsageAdd { cpu_seconds: self.cpu_seconds, gb_seconds: self.gb_seconds, requests: self.requests, egress_bytes: self.egress_bytes, build_seconds: self.build_seconds }
    }

    fn is_empty(&self) -> bool {
        self.requests == 0 && self.cpu_seconds == 0.0 && self.gb_seconds == 0.0 && self.cold_starts == 0 && self.instances_max == 0 && self.build_seconds == 0.0
    }
}

/// The current minute, and the minutes that ended and wait to be written.
#[derive(Default)]
pub struct Meter {
    cur: Option<Minute>,
    done: Vec<Minute>,
}

impl Meter {
    fn at(&mut self, minute: i64) -> &mut Minute {
        if self.cur.as_ref().is_some_and(|m| m.minute != minute) {
            let m = self.cur.take().expect("some");
            if !m.is_empty() {
                self.done.push(m);
            }
        }
        self.cur.get_or_insert_with(|| Minute::new(minute))
    }

    pub fn request(&mut self, now_ms: i64, latency_ms: f64, status: u16, egress: u64) {
        let m = self.at(now_ms / 60_000);
        m.requests += 1;
        if status >= 500 {
            m.errors += 1;
        }
        let i = BUCKETS_MS.iter().position(|b| latency_ms <= *b).unwrap_or(15);
        m.hist[i] += 1;
        m.egress_bytes += egress as i64;
    }

    pub fn cold_start(&mut self, now_ms: i64, ms: f64) {
        let m = self.at(now_ms / 60_000);
        m.cold_starts += 1;
        m.cold_ms_sum += ms;
    }

    /// What the instances used over the last `seconds`.
    pub fn sample(&mut self, now_ms: i64, cpu_seconds: f64, memory_bytes: u64, seconds: f64, instances: usize) {
        let m = self.at(now_ms / 60_000);
        m.cpu_seconds += cpu_seconds;
        m.gb_seconds += memory_bytes as f64 / 1e9 * seconds;
        m.memory_max = m.memory_max.max(memory_bytes as i64);
        m.instances_max = m.instances_max.max(instances as i64);
    }

    pub fn build(&mut self, now_ms: i64, seconds: f64) {
        self.at(now_ms / 60_000).build_seconds += seconds;
    }

    /// Minutes that ended (and, with `all`, the current one too), to write.
    pub fn take(&mut self, now_ms: i64, all: bool) -> Vec<Minute> {
        self.at(now_ms / 60_000);
        let mut out = std::mem::take(&mut self.done);
        if all && let Some(m) = self.cur.take().filter(|m| !m.is_empty()) {
            out.push(m);
        }
        out
    }

    /// The current minute so far (live numbers for the API, merged over the stored ones).
    pub fn current(&self) -> Option<MinuteRow> {
        self.cur.as_ref().filter(|m| !m.is_empty()).map(Minute::row)
    }
}

/// The prices of a second of real use (docs/apps/README.md, "Prices").
#[derive(Clone, Copy, Debug, PartialEq, Serialize)]
pub struct Prices {
    pub cpu_second: f64,
    pub gb_second: f64,
    pub egress_gb: f64,
    pub build_minute: f64,
}

impl Default for Prices {
    fn default() -> Self {
        Prices { cpu_second: 0.000012, gb_second: 0.000004, egress_gb: 0.05, build_minute: 0.005 }
    }
}

impl Prices {
    pub fn cost(&self, u: &UsageAdd) -> f64 {
        u.cpu_seconds * self.cpu_second + u.gb_seconds * self.gb_second + u.egress_bytes as f64 / 1e9 * self.egress_gb + u.build_seconds / 60.0 * self.build_minute
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn minutes_roll_over_and_idle_costs_nothing() {
        let mut m = Meter::default();
        for i in 0..100 {
            m.request(60_000, if i < 95 { 4.0 } else { 800.0 }, if i == 0 { 502 } else { 200 }, 1000);
        }
        m.cold_start(60_000, 420.0);
        m.sample(60_500, 0.3, 200_000_000, 1.0, 1);
        assert!(m.take(61_000, false).is_empty(), "the minute is still going");
        let done = m.take(120_000, false);
        assert_eq!(done.len(), 1);
        let row = done[0].row();
        assert_eq!((row.requests, row.errors, row.cold_starts), (100, 1, 1));
        assert_eq!((row.p50_ms, row.p99_ms), (5.0, 1000.0));
        let p = Prices::default();
        assert!(p.cost(&done[0].usage()) > 0.0);
        // A minute where nothing ran isn't even written.
        m.sample(180_000, 0.0, 0, 0.0, 0);
        assert!(m.take(240_000, true).is_empty());
        assert_eq!(p.cost(&UsageAdd::default()), 0.0);
    }
}
