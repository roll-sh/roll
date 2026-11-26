//! Search analytics in the control plane: the metering pass stores what the node handed on with
//! its other observations (`crate::meter::Meter::observe`) in the catalog's rings
//! (`crate::store`, `search_analytics`),
//! and `GET /v1/search/clusters/{id}/analytics` reads them ([`Control::analytics`]) with what
//! the cluster's owner (this node) recorded since the last pass. See `search_es::analytics` for
//! what is recorded and docs/search/README.md, "Search analytics", for the privacy design.

use crate::control::{Control, ControlError, Result, now};
use search_es::analytics::{Bucket, QueryStats};
use serde_json::{Value, json};


/// Rows each list of the answer holds.
pub const LIST_ROWS: usize = 50;
/// Searches a query needs in the range before its click-through counts as low.
pub const LOW_CTR_MIN_SEARCHES: u64 = 2;
/// Click-through under which a query with results is listed as low.
pub const LOW_CTR: f64 = 0.2;

/// Trials each arm needs before a comparison is called (and five successes and five failures in
/// each, for the normal approximation).
pub const MIN_TRIALS: u64 = 100;
/// The p-value under which a difference is called significant.
pub const SIGNIFICANCE: f64 = 0.05;

/// `erfc(x)` (Abramowitz and Stegun 7.1.26, within 1.5e-7).
fn erfc(x: f64) -> f64 {
    let t = 1.0 / (1.0 + 0.327_591_1 * x.abs());
    let poly = t * (0.254_829_592 + t * (-0.284_496_736 + t * (1.421_413_741 + t * (-1.453_152_027 + t * 1.061_405_429))));
    let r = poly * (-x * x).exp();
    if x >= 0.0 { r } else { 2.0 - r }
}

/// The two-sided p-value of a two-proportion z-test of `x1` of `n1` against `x2` of `n2` (pooled
/// standard error); `None` when there's nothing to compare.
pub fn two_proportion_p(x1: u64, n1: u64, x2: u64, n2: u64) -> Option<f64> {
    if n1 == 0 || n2 == 0 {
        return None;
    }
    let (p1, p2) = (x1 as f64 / n1 as f64, x2 as f64 / n2 as f64);
    let pool = (x1 + x2) as f64 / (n1 + n2) as f64;
    let se = (pool * (1.0 - pool) * (1.0 / n1 as f64 + 1.0 / n2 as f64)).sqrt();
    if se == 0.0 {
        return Some(1.0);
    }
    Some(erfc(((p1 - p2) / se).abs() / std::f64::consts::SQRT_2).clamp(0.0, 1.0))
}

/// A variant's rate against the baseline's: the p-value and whether it's called yet.
fn compare(x: u64, n: u64, bx: u64, bn: u64) -> Value {
    let enough = |x: u64, n: u64| n >= MIN_TRIALS && x >= 5 && n - x.min(n) >= 5;
    let p = two_proportion_p(x, n, bx, bn);
    let state = match p {
        _ if !enough(x, n) || !enough(bx, bn) => "not_enough_data",
        Some(p) if p < SIGNIFICANCE => "significant",
        _ => "not_yet_significant",
    };
    json!({ "p_value": p.map(|p| (p * 10_000.0).round() / 10_000.0), "state": state })
}

/// Experiments' results, by application and experiment: each variant's trials (a user's query in
/// an hour), clicks, conversions, value, click-through and conversion rate, the share of trials
/// that found nothing and its searches' 95th percentile latency (a bucket's bound: under 1, 2, 4
/// … ms), against the baseline
/// (the variant named `control`, else the first by name).
pub fn experiments_json(counts: &[(search_es::analytics::VariantKey, search_es::analytics::VariantStats)]) -> Value {
    let mut by: std::collections::BTreeMap<(String, String), std::collections::BTreeMap<String, search_es::analytics::VariantStats>> = std::collections::BTreeMap::new();
    for ((app, exp, v), s) in counts {
        by.entry((app.clone(), exp.clone())).or_default().entry(v.clone()).or_default().add(s);
    }
    let out: Vec<Value> = by
        .into_iter()
        .map(|((app, exp), vs)| {
            let baseline = if vs.contains_key("control") { "control".to_string() } else { vs.keys().next().cloned().unwrap_or_default() };
            let b = vs.get(&baseline).copied().unwrap_or_default();
            let variants: Vec<Value> = vs
                .iter()
                .map(|(name, s)| {
                    let mut v = json!({
                        "name": name,
                        "searches": s.searches,
                        "clicks": s.clicks,
                        "conversions": s.conversions,
                        "value": (s.value * 100.0).round() / 100.0,
                        "ctr": ratio(s.clicks, s.searches),
                        "conversion_rate": ratio(s.conversions, s.searches),
                        "no_hits": s.no_hits,
                        "no_hit_rate": ratio(s.no_hits, s.searches),
                        "p95_latency_ms": s.p95_ms(),
                    });
                    if *name != baseline {
                        v["vs_baseline"] = json!({
                            "ctr": compare(s.clicks, s.searches, b.clicks, b.searches),
                            "conversion_rate": compare(s.conversions, s.searches, b.conversions, b.searches),
                        });
                    }
                    v
                })
                .collect();
            json!({ "application": app, "experiment": exp, "baseline": baseline, "variants": variants })
        })
        .collect();
    Value::Array(out)
}

/// A range the route answers: which ring it reads and how far back.
struct Range {
    res: i64,
    seconds: i64,
}

impl Range {
    fn parse(range: &str) -> Option<Range> {
        match range {
            "24h" => Some(Range { res: 3600, seconds: 86_400 }),
            "7d" => Some(Range { res: 3600, seconds: 7 * 86_400 }),
            "30d" => Some(Range { res: 86_400, seconds: 30 * 86_400 }),
            _ => None,
        }
    }
}

fn ratio(a: u64, b: u64) -> Option<f64> {
    (b > 0).then(|| (a as f64 / b as f64 * 10_000.0).round() / 10_000.0)
}

fn query_json(q: &str, s: &QueryStats) -> Value {
    json!({
        "query": q,
        "searches": s.searches,
        "no_hits": s.no_hits,
        "avg_hits": ratio(s.hits, s.searches.saturating_sub(s.no_hits)).map(|v| (v * 10.0).round() / 10.0),
        "avg_latency_ms": ratio(s.latency_us, s.searches).map(|us| (us / 100.0).round() / 10.0),
        "clicks": s.clicks,
        "ctr": ratio(s.clicks, s.searches),
        "avg_position": ratio(s.position_sum, s.positioned).map(|v| (v * 10.0).round() / 10.0),
        "conversions": s.conversions,
        "conversion_rate": ratio(s.conversions, s.searches),
        "value": s.value_milli as f64 / 1000.0,
    })
}

impl Control {
    /// A cluster's search analytics over `range` (`24h`, `7d` or `30d`), of index `index` or of
    /// every index: totals, top queries, queries with no results and queries with a low
    /// click-through. Runs on the cluster's owner (`per_cluster`), which adds what it recorded
    /// since the last metering pass.
    pub async fn analytics(&self, id: &str, index: Option<&str>, range: &str) -> Result<Value> {
        let c = self.get(id).await?;
        let Some(r) = Range::parse(range) else {
            return Err(ControlError::Invalid(format!("range must be 24h, 7d or 30d, not {range}")));
        };
        let from = {
            let t = now() - r.seconds;
            t - t.rem_euclid(r.res) + r.res
        };
        // Turned off or deleted and not handed on yet: the catalog's rows are gone already.
        let pending = self.node.peek_observations(&c.name, &c.id).analytics;
        // Experiments: since each started, whatever the range (with what this node counted since
        // the last pass).
        let mut counts = self.store.experiments(&c.id).await?;
        counts.extend(pending.experiments.iter().cloned());
        let gone = |i: &str| pending.cleared.iter().any(|c| c == i) || pending.enabled.as_ref().is_some_and(|on| !on.iter().any(|o| o == i));
        let mut total = Bucket::new(4 * LIST_ROWS, 2 * LIST_ROWS);
        let mut indices: Vec<String> = Vec::new();
        // Per bucket of the range's resolution: searches, searches with no results, clicks,
        // conversions.
        let mut points: std::collections::BTreeMap<i64, [u64; 4]> = std::collections::BTreeMap::new();
        let mut point = |at: i64, b: &Bucket| {
            let p = points.entry(at - at.rem_euclid(r.res)).or_default();
            p[0] += b.searches;
            p[1] += b.no_hits;
            p[2] += b.clicks;
            p[3] += b.conversions;
        };
        for (i, at, b) in self.store.analytics_since(&c.id, index, r.res, from).await? {
            if gone(&i) {
                continue;
            }
            if !indices.contains(&i) {
                indices.push(i.clone());
            }
            point(at, &b);
            total.merge(&b);
        }
        for (i, hour, b) in &pending.hours {
            if index.is_some_and(|x| x != i) || (*hour as i64).saturating_mul(3600) < from {
                continue;
            }
            if !indices.contains(i) {
                indices.push(i.clone());
            }
            point((*hour as i64).saturating_mul(3600), b);
            total.merge(b);
        }
        indices.sort_unstable();
        let ranked = total.top.ranked();
        let top: Vec<Value> = ranked.iter().take(LIST_ROWS).map(|(q, s)| query_json(q, s)).collect();
        let no_results: Vec<Value> = total
            .empty
            .ranked()
            .iter()
            .take(LIST_ROWS)
            .map(|(q, s)| {
                // The query's full figures when the top list has it.
                total.top.get(q).map_or_else(|| json!({ "query": q, "searches": s.searches, "no_hits": s.searches }), |full| query_json(q, full))
            })
            .collect();
        // Click-through is only meaningful once a client reports clicks.
        let mut low: Vec<&(&str, &QueryStats)> = if total.clicks > 0 {
            ranked.iter().filter(|(_, s)| s.searches >= LOW_CTR_MIN_SEARCHES && s.no_hits < s.searches && (s.clicks as f64) < LOW_CTR * s.searches as f64).collect()
        } else {
            Vec::new()
        };
        low.sort_by(|a, b| (a.1.clicks as f64 / a.1.searches as f64).total_cmp(&(b.1.clicks as f64 / b.1.searches as f64)).then(b.1.searches.cmp(&a.1.searches)));
        let low_ctr: Vec<Value> = low.iter().take(LIST_ROWS).map(|(q, s)| query_json(q, s)).collect();
        Ok(json!({
            "range": range,
            "index": index,
            "from": from,
            "step_s": r.res,
            "indices": indices,
            "points": points.iter().map(|(at, p)| json!({ "at": at, "searches": p[0], "no_hits": p[1], "clicks": p[2], "conversions": p[3] })).collect::<Vec<_>>(),
            "totals": {
                "searches": total.searches,
                "no_hits": total.no_hits,
                "no_hit_rate": ratio(total.no_hits, total.searches),
                "clicks": total.clicks,
                "ctr": ratio(total.clicks, total.searches),
                "conversions": total.conversions,
                "conversion_rate": ratio(total.conversions, total.searches),
                "value": total.value_milli as f64 / 1000.0,
            },
            "top": top,
            "no_results": no_results,
            "low_ctr": low_ctr,
            "experiments": experiments_json(&counts),
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn two_proportions() {
        // 120 of 1,000 against 100 of 1,000: z ≈ 1.43, p ≈ 0.153 (not significant).
        assert!((two_proportion_p(120, 1000, 100, 1000).unwrap() - 0.153).abs() < 0.002);
        // 150 of 1,000 against 100 of 1,000: z ≈ 3.38, p ≈ 0.0007.
        assert!(two_proportion_p(150, 1000, 100, 1000).unwrap() < 0.001);
        assert_eq!(two_proportion_p(0, 10, 0, 10), Some(1.0));
        assert_eq!(two_proportion_p(1, 0, 1, 10), None);
    }
}
