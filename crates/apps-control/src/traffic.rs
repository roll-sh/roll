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
            let g = groups.entry(key).or_default();
            g.requests += 1;
            g.errors += u64::from(status >= 500);
            g.client_errors += u64::from((400..500).contains(&status));
            g.ms.push(v["ms"].as_u64().unwrap_or(0).min(u32::MAX as u64) as u32);
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
        Ok(json!({ "minutes": minutes, "requests": total, "cut": cut, "routes": out }))
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
