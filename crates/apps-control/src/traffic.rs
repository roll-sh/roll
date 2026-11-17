//! Errors and latency by route (Vercel's Observability, Render's slowest-requests view): an
//! environment's access log over its last minutes, grouped by method and path with id-like
//! segments folded (`/users/42/orders` and `/users/7/orders` are `/users/:id/orders`). Read from
//! the log the proxy already writes (`http:<service>/<env>`, a JSON line a request), so nothing
//! new is kept and a request costs nothing more. docs/apps/README.md, "Errors and latency by
//! route".

use crate::{Apps, Result};
use serde_json::{Value, json};
use std::collections::{BTreeSet, HashMap};
use std::sync::Arc;

/// The most access lines one answer reads (newest first): the counts say when it stopped there.
const MAX_LINES: usize = 50_000;
/// Groups kept apart before the rest are counted as one (`*`): a path that folds badly can't
/// make the map grow with every request.
const MAX_GROUPS: usize = 1_000;
/// Routes answered: the busiest, the most 5xx and the slowest, each this many at most.
const TOP: usize = 50;

#[derive(Default)]
struct Group {
    requests: u64,
    errors: u64,
    client_errors: u64,
    ms: Vec<u32>,
}

impl Apps {
    /// `GET …/environments/{env}/routes?minutes=` (5–60, default 60): the routes that answered,
    /// with their requests, 5xx, 4xx and latency.
    pub async fn routes(self: &Arc<Self>, service: &str, env: &str, minutes: i64) -> Result<Value> {
        self.service_env(service, env).await?;
        let minutes = minutes.clamp(5, 60);
        let since = code_sql::now_ms() - minutes * 60_000;
        let (lines, cut) = self.logs.read(&format!("http:{service}/{env}"), |_| true, Some(since), None, MAX_LINES).await;
        let cut = cut || lines.len() >= MAX_LINES;
        let mut groups: HashMap<(String, String), Group> = HashMap::new();
        // The same requests by the deployment and the instance that answered (Render's
        // instance breakdown; a canary beside the live one).
        let (mut by_dep, mut by_inst): (HashMap<String, Group>, HashMap<String, Group>) = (HashMap::new(), HashMap::new());
        let mut total = 0u64;
        // Newest first: the groups kept apart are the latest traffic's, not an old burst's.
        for l in lines.iter().rev() {
            let Ok(v) = serde_json::from_str::<Value>(&l.text) else { continue };
            let (Some(method), Some(path), Some(status)) = (v["method"].as_str(), v["path"].as_str(), v["status"].as_u64()) else { continue };
            total += 1;
            let mut key = (method.chars().take(16).collect::<String>(), route_of(path));
            if !groups.contains_key(&key) && groups.len() >= MAX_GROUPS {
                key = ("*".into(), "*".into());
            }
            let ms = v["ms"].as_u64().unwrap_or(0).min(u32::MAX as u64) as u32;
            let count = |g: &mut Group| {
                g.requests += 1;
                g.errors += u64::from(status >= 500);
                g.client_errors += u64::from((400..500).contains(&status));
                g.ms.push(ms);
            };
            count(groups.entry(key).or_default());
            if let Some(d) = v["deployment"].as_str().filter(|d| by_dep.contains_key(*d) || by_dep.len() < MAX_GROUPS) {
                count(by_dep.entry(d.to_string()).or_default());
            }
            if let Some(i) = v["instance"].as_str().filter(|i| *i != "site" && (by_inst.contains_key(*i) || by_inst.len() < MAX_GROUPS)) {
                count(by_inst.entry(i.to_string()).or_default());
            }
        }
        let mut rows: Vec<(String, String, Group)> = groups.into_iter().map(|((m, r), g)| (m, r, g)).collect();
        for (_, _, g) in &mut rows {
            g.ms.sort_unstable();
        }
        let pct = |g: &Group, p: f64| g.ms.get(((g.ms.len() as f64 * p).ceil() as usize).saturating_sub(1)).copied().unwrap_or(0);
        // The busiest, the most failing and the slowest (of those asked enough to say): each list's top.
        let mut keep: BTreeSet<usize> = BTreeSet::new();
        let mut order: Vec<usize> = (0..rows.len()).collect();
        order.sort_by(|a, b| rows[*b].2.requests.cmp(&rows[*a].2.requests).then_with(|| rows[*a].1.cmp(&rows[*b].1)));
        keep.extend(order.iter().take(TOP));
        order.sort_by(|a, b| rows[*b].2.errors.cmp(&rows[*a].2.errors));
        keep.extend(order.iter().filter(|i| rows[**i].2.errors > 0).take(TOP));
        order.sort_by_key(|i| std::cmp::Reverse(pct(&rows[*i].2, 0.95)));
        keep.extend(order.iter().filter(|i| rows[**i].2.requests >= 5).take(TOP));
        let mut out: Vec<Value> = keep
            .into_iter()
            .map(|i| {
                let (m, r, g) = &rows[i];
                json!({ "method": m, "route": r, "requests": g.requests, "errors": g.errors, "client_errors": g.client_errors, "p50_ms": pct(g, 0.5), "p95_ms": pct(g, 0.95), "max_ms": g.ms.last().copied().unwrap_or(0) })
            })
            .collect();
        out.sort_by(|a, b| b["requests"].as_u64().cmp(&a["requests"].as_u64()).then_with(|| a["route"].as_str().cmp(&b["route"].as_str())));
        // The busiest first, each split's at most TOP.
        let split = |m: HashMap<String, Group>, what: &str| -> Vec<Value> {
            let mut v: Vec<(String, Group)> = m.into_iter().collect();
            v.sort_by(|a, b| b.1.requests.cmp(&a.1.requests).then_with(|| a.0.cmp(&b.0)));
            v.into_iter()
                .take(TOP)
                .map(|(k, mut g)| {
                    g.ms.sort_unstable();
                    json!({ what: k, "requests": g.requests, "errors": g.errors, "client_errors": g.client_errors, "p50_ms": pct(&g, 0.5), "p95_ms": pct(&g, 0.95), "max_ms": g.ms.last().copied().unwrap_or(0) })
                })
                .collect()
        };
        Ok(json!({ "minutes": minutes, "requests": total, "cut": cut, "routes": out, "deployments": split(by_dep, "deployment"), "instances": split(by_inst, "instance") }))
    }
}

/// A visitor: a day, an address and a user agent (counted, never kept).
type Visitor = (i64, String, String);
/// Views and the visitors that made them.
type Counted = (u64, std::collections::HashSet<Visitor>);

/// Lines one analytics answer reads at most (newest first).
const MAX_ANALYTICS_LINES: usize = 200_000;

impl Apps {
    /// `GET …/environments/{env}/analytics?minutes=` (60 to 10,080, default 1,440): visitors and
    /// page views from the access log already kept (Vercel's Web Analytics, Plausible's way: no
    /// script, no cookie). A page view is a browser's page load answered 2xx or 304; a visitor is
    /// one address and user agent on one day, counted in memory for this answer and never kept.
    /// Top pages, referring sites (their hosts) and countries (the edge's), and a series.
    pub async fn analytics(self: &Arc<Self>, service: &str, env: &str, minutes: i64) -> Result<Value> {
        self.service_env(service, env).await?;
        let minutes = minutes.clamp(60, 10_080);
        let now = code_sql::now_ms();
        let since = now - minutes * 60_000;
        let step = if minutes <= 360 { 5 } else if minutes <= 2_880 { 60 } else { 360 };
        let (lines, cut) = self.logs.read(&format!("http:{service}/{env}"), |_| true, Some(since), None, MAX_ANALYTICS_LINES).await;
        let cut = cut || lines.len() >= MAX_ANALYTICS_LINES;
        let day = |t: i64| t / 86_400_000;
        let mut visitors: std::collections::HashSet<Visitor> = std::collections::HashSet::new();
        let mut pages: HashMap<String, Counted> = HashMap::new();
        let mut referrers: HashMap<String, u64> = HashMap::new();
        let mut countries: HashMap<String, u64> = HashMap::new();
        let buckets = (minutes / step) as usize + 1;
        let mut series: Vec<Counted> = (0..buckets).map(|_| (0, Default::default())).collect();
        let mut views = 0u64;
        for l in &lines {
            let Ok(v) = serde_json::from_str::<Value>(&l.text) else { continue };
            let status = v["status"].as_u64().unwrap_or(0);
            if v["page"] != true || !((200..300).contains(&status) || status == 304) {
                continue;
            }
            views += 1;
            let who = (day(l.t), v["ip"].as_str().unwrap_or("").to_string(), v["ua"].as_str().unwrap_or("").to_string());
            let path = v["path"].as_str().unwrap_or("/").split(['?', '#']).next().unwrap_or("/").chars().take(200).collect::<String>();
            if pages.len() < MAX_GROUPS || pages.contains_key(&path) {
                let p = pages.entry(path).or_default();
                p.0 += 1;
                p.1.insert(who.clone());
            }
            if let Some(r) = v["ref"].as_str()
                && (referrers.len() < MAX_GROUPS || referrers.contains_key(r))
            {
                *referrers.entry(r.to_string()).or_default() += 1;
            }
            if let Some(c) = v["country"].as_str() {
                *countries.entry(c.to_string()).or_default() += 1;
            }
            let b = (((l.t - since).max(0) / 60_000) / step) as usize;
            if let Some(s) = series.get_mut(b) {
                s.0 += 1;
                s.1.insert(who.clone());
            }
            visitors.insert(who);
        }
        let top = |m: HashMap<String, u64>, key: &str| {
            let mut v: Vec<(String, u64)> = m.into_iter().collect();
            v.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
            v.into_iter().take(TOP).map(|(k, n)| json!({ key: k, "views": n })).collect::<Vec<_>>()
        };
        let mut page_rows: Vec<(String, u64, usize)> = pages.into_iter().map(|(p, (n, w))| (p, n, w.len())).collect();
        page_rows.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
        let series: Vec<Value> = series.into_iter().enumerate().map(|(i, (n, w))| json!({ "t": since + i as i64 * step * 60_000, "views": n, "visitors": w.len() })).collect();
        Ok(json!({
            "minutes": minutes,
            "step": step,
            "page_views": views,
            "visitors": visitors.len(),
            "pages": page_rows.into_iter().take(TOP).map(|(p, n, w)| json!({ "path": p, "views": n, "visitors": w })).collect::<Vec<_>>(),
            "referrers": top(referrers, "host"),
            "countries": top(countries, "country"),
            "series": series,
            "cut": cut,
        }))
    }
}

/// A path as a route: no query, at most 12 segments, an id-like segment `:id`.
pub fn route_of(path: &str) -> String {
    let path = path.split(['?', '#']).next().unwrap_or("/");
    let mut out = String::new();
    for seg in path.split('/').filter(|s| !s.is_empty()).take(12) {
        out.push('/');
        out.push_str(if id_like(seg) { ":id" } else { seg });
    }
    if out.is_empty() {
        out.push('/');
    }
    out.chars().take(200).collect()
}

/// A number, a UUID or hash, a long token, or a mix of letters and digits that reads as a
/// generated id (`usr_8f3k2j9d`, `a1b2c3d4e5`), not a word (`v2`, `page-2`, `index.html`).
fn id_like(s: &str) -> bool {
    let b = s.as_bytes();
    let digits = b.iter().filter(|c| c.is_ascii_digit()).count();
    b.iter().all(u8::is_ascii_digit)
        || (b.len() >= 16 && b.iter().all(|c| c.is_ascii_hexdigit() || *c == b'-') && digits > 0)
        || b.len() > 40
        || (b.len() >= 8 && digits >= 2 && b.iter().any(u8::is_ascii_alphabetic) && b.iter().all(|c| c.is_ascii_alphanumeric() || *c == b'_' || *c == b'-'))
}

#[cfg(test)]
mod tests {
    use super::route_of;

    #[test]
    fn ids_fold_and_words_stay() {
        assert_eq!(route_of("/users/42/orders?page=2"), "/users/:id/orders");
        assert_eq!(route_of("/u/3f2c1a9e-7b4d-4e1a-9c2b-1d2e3f4a5b6c"), "/u/:id");
        assert_eq!(route_of("/api/v2/items/usr_8f3k2j9d"), "/api/v2/items/:id");
        assert_eq!(route_of("/blog/page-2/index.html"), "/blog/page-2/index.html");
        assert_eq!(route_of(""), "/");
        assert_eq!(route_of("//a//b/"), "/a/b");
    }
}
