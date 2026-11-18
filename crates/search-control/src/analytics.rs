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
        let pending = self.node.peek_observations(&c.name).analytics;
        let gone = |i: &str| pending.cleared.iter().any(|c| c == i) || pending.enabled.as_ref().is_some_and(|on| !on.iter().any(|o| o == i));
        let mut total = Bucket::new(4 * LIST_ROWS, 2 * LIST_ROWS);
        let mut indices: Vec<String> = Vec::new();
        // Per bucket of the range's resolution: searches, searches with no results, clicks.
        let mut points: std::collections::BTreeMap<i64, [u64; 3]> = std::collections::BTreeMap::new();
        let mut point = |at: i64, b: &Bucket| {
            let p = points.entry(at - at.rem_euclid(r.res)).or_default();
            p[0] += b.searches;
            p[1] += b.no_hits;
            p[2] += b.clicks;
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
            "points": points.iter().map(|(at, p)| json!({ "at": at, "searches": p[0], "no_hits": p[1], "clicks": p[2] })).collect::<Vec<_>>(),
            "totals": {
                "searches": total.searches,
                "no_hits": total.no_hits,
                "no_hit_rate": ratio(total.no_hits, total.searches),
                "clicks": total.clicks,
                "ctr": ratio(total.clicks, total.searches),
            },
            "top": top,
            "no_results": no_results,
            "low_ctr": low_ctr,
        }))
    }
}
