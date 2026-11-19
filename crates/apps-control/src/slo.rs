//! Service level objectives and error-budget burn alerts (Cloud Monitoring's SLOs, the SRE
//! workbook's multiwindow, multi-burn-rate alerts; docs/apps/README.md, "SLOs"): `run.slo` says
//! what share of requests must succeed (not 5xx), and optionally what share of minutes must keep
//! p95 under `latency_ms`, over `window_days`. The budget is what may fail; it burning 14.4 times
//! as fast as the window allows over an hour and its last five minutes, or 6 times over six
//! hours and their last half hour, is `alert.slo` (once, until it slows).

use crate::model::Slo;
use crate::store::MinuteRow;
use serde::Serialize;

/// The page-worthy burns: the long window and the short one (minutes), and the rate.
pub const BURNS: [(i64, i64, f64, &str); 2] = [(60, 5, 14.4, "1h"), (360, 30, 6.0, "6h")];
/// The most minutes the burn alerts read.
pub const LONGEST: i64 = 360;

/// How one indicator stands over some time: what counted, what failed it.
#[derive(Clone, Copy, Debug, Default, PartialEq, Serialize)]
pub struct Tally {
    pub total: i64,
    pub bad: i64,
}

impl Tally {
    /// How fast the budget burns: the share that failed over the share the objective allows.
    pub fn burn(&self, objective: f64) -> f64 {
        if self.total == 0 {
            return 0.0;
        }
        (self.bad as f64 / self.total as f64) / (1.0 - objective / 100.0)
    }

    /// The share of the window's budget left (negative once spent); None with nothing counted.
    pub fn budget_left(&self, objective: f64) -> Option<f64> {
        (self.total > 0).then(|| 1.0 - self.burn(objective))
    }
}

/// Requests that succeeded: every request counts, a 5xx fails it.
pub fn availability(rows: &[MinuteRow]) -> Tally {
    rows.iter().fold(Tally::default(), |t, r| Tally { total: t.total + r.requests, bad: t.bad + r.errors })
}

/// Minutes that kept p95 under `ms`: each minute with requests counts (`per` of them for a row
/// that stands for an hour, which fails whole when its slowest minute did).
pub fn latency(rows: &[MinuteRow], ms: u32, per: i64) -> Tally {
    rows.iter().filter(|r| r.requests > 0).fold(Tally::default(), |t, r| Tally { total: t.total + per, bad: t.bad + if r.p95_ms > ms as f64 { per } else { 0 } })
}

/// The burn alert that fires on these minutes (oldest first, the last one finished before
/// `now_minute`), if one does: the indicator, its burn rate over the long window, which window.
/// An indicator needs enough in the short window to say anything: 20 requests, or 3 minutes.
pub fn burning(slo: &Slo, rows: &[MinuteRow], now_minute: i64) -> Option<(&'static str, f64, &'static str)> {
    let within = |m: i64| rows.iter().filter(|r| r.minute >= now_minute - m && r.minute < now_minute).cloned().collect::<Vec<_>>();
    for (long, short, rate, name) in BURNS {
        let (l, s) = (within(long), within(short));
        let (la, sa) = (availability(&l), availability(&s));
        if sa.total >= 20 && la.burn(slo.objective) >= rate && sa.burn(slo.objective) >= rate {
            return Some(("availability", la.burn(slo.objective), name));
        }
        if let Some(ms) = slo.latency_ms {
            let (ll, sl) = (latency(&l, ms, 1), latency(&s, ms, 1));
            if sl.total >= 3 && ll.burn(slo.objective) >= rate && sl.burn(slo.objective) >= rate {
                return Some(("latency", ll.burn(slo.objective), name));
            }
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_budget_burns_by_the_share_that_failed() {
        let slo = Slo { objective: 99.0, latency_ms: Some(300), window_days: 28 };
        let row = |minute: i64, requests: i64, errors: i64, p95: f64| MinuteRow { minute, requests, errors, p95_ms: p95, ..Default::default() };
        // 1% may fail: 1% failing burns at 1, 20% at 20.
        assert!((availability(&[row(0, 100, 1, 0.0)]).burn(99.0) - 1.0).abs() < 1e-9);
        assert!((availability(&[row(0, 100, 20, 0.0)]).budget_left(99.0).unwrap() + 19.0).abs() < 1e-9);
        assert_eq!(Tally::default().budget_left(99.0), None);
        // An hour at 20% errors: past 14.4 over the hour and its last five minutes.
        let now = 1000;
        let hot: Vec<MinuteRow> = (1..=60).map(|i| row(now - i, 100, 20, 100.0)).collect();
        assert_eq!(burning(&slo, &hot, now).map(|(i, _, w)| (i, w)), Some(("availability", "1h")));
        // Stopped five minutes ago: the fast alert clears, the slow one (six hours, the last half
        // hour) still holds; stopped half an hour ago, neither.
        let stopped = |ago: i64| (1..=60).map(|i| row(now - i, 100, if i <= ago { 0 } else { 20 }, 100.0)).collect::<Vec<_>>();
        assert_eq!(burning(&slo, &stopped(5), now).map(|(_, _, w)| w), Some("6h"));
        assert_eq!(burning(&slo, &stopped(30), now), None);
        // Slow minutes burn the latency indicator, and too little traffic says nothing.
        let slow: Vec<MinuteRow> = (1..=60).map(|i| row(now - i, 50, 0, 900.0)).collect();
        assert_eq!(burning(&slo, &slow, now).map(|(i, _, _)| i), Some("latency"));
        let quiet: Vec<MinuteRow> = (1..=15).map(|i| row(now - i, 1, 1, 0.0)).collect();
        assert_eq!(burning(&slo, &quiet, now), None, "15 requests, every one failed: too few to say");
        // An hour's row stands for its minutes.
        assert_eq!(latency(&[row(5, 10, 0, 900.0)], 300, 60), Tally { total: 60, bad: 60 });
    }
}
