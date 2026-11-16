//! Shared caching of a web service's answers in the owner's proxy (Vercel's CDN, Netlify's and
//! Render's edge caching): an answer whose app says a shared cache may keep it (`CDN-Cache-Control:
//! max-age`, else `Cache-Control: s-maxage`) is kept per environment and given to the next GET
//! or HEAD of that path and query, before anything wakes, so a sleeping service answers its cached
//! pages asleep. `stale-while-revalidate` answers a stale one while it's fetched again in the
//! background. Never kept: `private`, `no-store`, `no-cache`, `Vary: *`, an answer setting a
//! cookie, a body over 1 MiB or of unknown length, anything but a 200. A request with
//! `Authorization` neither reads nor fills it. Bounded (32 MiB and 2,000 answers an environment,
//! the least recently used going first) and emptied whenever the environment's live deployment
//! changes.

use axum::http::{HeaderMap, HeaderName, HeaderValue, StatusCode, header};
use bytes::Bytes;
use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, Instant};

const PER_ENV: usize = 32 << 20;
pub const PER_BODY: usize = 1 << 20;
const MAX_ENTRIES: usize = 2_000;

/// How long a shared cache may keep an answer: fresh for `fresh`, then answered stale for
/// `stale` more while it's fetched again.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Policy {
    pub fresh: Duration,
    pub stale: Duration,
}

/// The policy an app's answer gives a shared cache, if any.
pub fn policy(h: &HeaderMap) -> Option<Policy> {
    if h.contains_key(header::SET_COOKIE) || h.get_all(header::VARY).iter().any(|v| v.to_str().is_ok_and(|v| v.split(',').any(|p| p.trim() == "*"))) {
        return None;
    }
    let directives = |name: &str| -> Option<Vec<(String, Option<u64>)>> {
        let v = h.get_all(name).iter().filter_map(|v| v.to_str().ok()).collect::<Vec<_>>().join(",");
        (!v.trim().is_empty()).then(|| {
            v.split(',')
                .map(|d| {
                    let d = d.trim().to_ascii_lowercase();
                    match d.split_once('=') {
                        Some((k, n)) => (k.trim().to_string(), n.trim().trim_matches('"').parse().ok()),
                        None => (d, None),
                    }
                })
                .collect()
        })
    };
    let has = |ds: &[(String, Option<u64>)], k: &str| ds.iter().any(|(d, _)| d == k);
    let num = |ds: &[(String, Option<u64>)], k: &str| ds.iter().find(|(d, _)| d == k).and_then(|(_, n)| *n);
    let cc = directives("cache-control").unwrap_or_default();
    if has(&cc, "private") || has(&cc, "no-store") || has(&cc, "no-cache") {
        return None;
    }
    // CDN-Cache-Control (RFC 9213) speaks to shared caches alone and wins; else s-maxage.
    let (fresh, from) = match directives("cdn-cache-control") {
        Some(cdn) if has(&cdn, "no-store") || has(&cdn, "private") => return None,
        Some(cdn) => (num(&cdn, "max-age")?, cdn),
        None => (num(&cc, "s-maxage")?, cc),
    };
    if fresh == 0 {
        return None;
    }
    let stale = num(&from, "stale-while-revalidate").unwrap_or(0);
    Some(Policy { fresh: Duration::from_secs(fresh.min(31_536_000)), stale: Duration::from_secs(stale.min(31_536_000)) })
}

struct Entry {
    status: StatusCode,
    headers: HeaderMap,
    body: Bytes,
    stored: Instant,
    policy: Policy,
    /// The request headers the answer varies on, as this request had them.
    vary: Vec<(HeaderName, Option<HeaderValue>)>,
    used: u64,
    refreshing: bool,
}

#[derive(Default)]
struct EnvCache {
    entries: HashMap<String, Vec<Entry>>,
    bytes: usize,
    count: usize,
    tick: u64,
}

/// What a lookup found.
pub enum Hit {
    Fresh(axum::response::Response),
    /// Stale but within `stale-while-revalidate`: answer it, and fetch it again (`refresh` says
    /// whether this caller should: one does at a time).
    Stale { response: axum::response::Response, refresh: bool },
}

#[derive(Default)]
pub struct ResponseCache {
    envs: Mutex<HashMap<String, EnvCache>>,
}

/// A request a stale answer's refresh makes (an extension, which no client can send): it skips
/// the lookup and fills the cache again.
#[derive(Clone, Copy, Debug)]
pub struct Refresh;

fn key(path_and_query: &str) -> String {
    path_and_query.to_string()
}

fn matches(vary: &[(HeaderName, Option<HeaderValue>)], req: &HeaderMap) -> bool {
    vary.iter().all(|(n, v)| req.get(n) == v.as_ref())
}

impl ResponseCache {
    /// The answer kept for this GET or HEAD in environment `env` (`service/env`), if any.
    pub fn get(&self, env: &str, path_and_query: &str, req: &HeaderMap, head: bool) -> Option<Hit> {
        let mut envs = self.envs.lock().unwrap_or_else(|p| p.into_inner());
        let e = envs.get_mut(env)?;
        e.tick += 1;
        let tick = e.tick;
        let found = e.entries.get_mut(&key(path_and_query))?.iter_mut().find(|x| matches(&x.vary, req))?;
        let age = found.stored.elapsed();
        if age > found.policy.fresh + found.policy.stale {
            return None;
        }
        found.used = tick;
        let mut r = axum::response::Response::new(if head { axum::body::Body::empty() } else { axum::body::Body::from(found.body.clone()) });
        *r.status_mut() = found.status;
        *r.headers_mut() = found.headers.clone();
        if let Ok(v) = HeaderValue::from_str(&age.as_secs().to_string()) {
            r.headers_mut().insert(header::AGE, v);
        }
        if age <= found.policy.fresh {
            r.headers_mut().insert(HeaderName::from_static("x-roll-cache"), HeaderValue::from_static("HIT"));
            return Some(Hit::Fresh(r));
        }
        r.headers_mut().insert(HeaderName::from_static("x-roll-cache"), HeaderValue::from_static("STALE"));
        let refresh = !found.refreshing;
        found.refreshing = true;
        Some(Hit::Stale { response: r, refresh })
    }

    /// Keep a 200's answer (its app's own headers, before roll's cookies) for this request.
    pub fn put(&self, env: &str, path_and_query: &str, req: &HeaderMap, headers: &HeaderMap, body: Bytes, policy: Policy) {
        let status = StatusCode::OK;
        if body.len() > PER_BODY {
            return;
        }
        let vary: Vec<(HeaderName, Option<HeaderValue>)> = headers
            .get_all(header::VARY)
            .iter()
            .filter_map(|v| v.to_str().ok())
            .flat_map(|v| v.split(',').map(|p| p.trim().to_ascii_lowercase()).collect::<Vec<_>>())
            .filter(|p| !p.is_empty())
            .filter_map(|p| HeaderName::from_bytes(p.as_bytes()).ok())
            .map(|n| {
                let v = req.get(&n).cloned();
                (n, v)
            })
            .collect();
        let mut kept = headers.clone();
        for h in [header::SET_COOKIE, header::AGE, header::CONNECTION, header::TRANSFER_ENCODING] {
            kept.remove(h);
        }
        let mut envs = self.envs.lock().unwrap_or_else(|p| p.into_inner());
        let e = envs.entry(env.to_string()).or_default();
        e.tick += 1;
        let size = body.len() + path_and_query.len() + 256;
        let list = e.entries.entry(key(path_and_query)).or_default();
        if let Some(i) = list.iter().position(|x| x.vary == vary) {
            let old = list.remove(i);
            e.bytes -= old.body.len() + path_and_query.len() + 256;
            e.count -= 1;
        }
        list.push(Entry { status, headers: kept, body, stored: Instant::now(), policy, vary, used: e.tick, refreshing: false });
        e.bytes += size;
        e.count += 1;
        // Over its bounds: the least recently used go first.
        while e.bytes > PER_ENV || e.count > MAX_ENTRIES {
            let Some((k, i)) = e.entries.iter().flat_map(|(k, l)| l.iter().enumerate().map(move |(i, x)| (k, i, x.used))).min_by_key(|(_, _, u)| *u).map(|(k, i, _)| (k.clone(), i)) else { break };
            if let Some(l) = e.entries.get_mut(&k) {
                let gone = l.remove(i);
                e.bytes -= gone.body.len() + k.len() + 256;
                e.count -= 1;
                if l.is_empty() {
                    e.entries.remove(&k);
                }
            }
        }
    }

    /// A refresh that didn't keep a new answer: the stale one may be refreshed again.
    pub fn refresh_failed(&self, env: &str, path_and_query: &str) {
        let mut envs = self.envs.lock().unwrap_or_else(|p| p.into_inner());
        if let Some(list) = envs.get_mut(env).and_then(|e| e.entries.get_mut(&key(path_and_query))) {
            for x in list {
                x.refreshing = false;
            }
        }
    }

    /// Everything kept for an environment goes (its live deployment changed).
    pub fn clear(&self, env: &str) {
        self.envs.lock().unwrap_or_else(|p| p.into_inner()).remove(env);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn h(pairs: &[(&'static str, &'static str)]) -> HeaderMap {
        let mut m = HeaderMap::new();
        for (k, v) in pairs {
            m.append(HeaderName::from_static(k), HeaderValue::from_static(v));
        }
        m
    }

    #[test]
    fn only_what_a_shared_cache_may_keep_is_kept() {
        let p = |pairs: &[(&'static str, &'static str)]| policy(&h(pairs));
        assert_eq!(p(&[("cache-control", "public, s-maxage=60, stale-while-revalidate=30")]), Some(Policy { fresh: Duration::from_secs(60), stale: Duration::from_secs(30) }));
        assert_eq!(p(&[("cache-control", "max-age=60"), ("cdn-cache-control", "max-age=10")]).map(|p| p.fresh), Some(Duration::from_secs(10)));
        assert_eq!(p(&[("cache-control", "public, max-age=60")]), None, "a browser's max-age isn't a shared cache's");
        assert_eq!(p(&[("cache-control", "private, s-maxage=60")]), None);
        assert_eq!(p(&[("cache-control", "s-maxage=60"), ("set-cookie", "a=b")]), None);
        assert_eq!(p(&[("cache-control", "s-maxage=60"), ("vary", "*")]), None);
    }

    #[test]
    fn an_answer_is_kept_per_vary_and_goes_stale_then_away() {
        let c = ResponseCache::default();
        let pol = Policy { fresh: Duration::from_secs(60), stale: Duration::ZERO };
        let resp = h(&[("cache-control", "s-maxage=60"), ("vary", "accept-language")]);
        c.put("s/production", "/a?x=1", &h(&[("accept-language", "en")]), &resp, Bytes::from_static(b"en"), pol);
        assert!(matches!(c.get("s/production", "/a?x=1", &h(&[("accept-language", "en")]), false), Some(Hit::Fresh(_))));
        assert!(c.get("s/production", "/a?x=1", &h(&[("accept-language", "de")]), false).is_none());
        assert!(c.get("s/production", "/a?x=2", &h(&[("accept-language", "en")]), false).is_none());
        c.clear("s/production");
        assert!(c.get("s/production", "/a?x=1", &h(&[("accept-language", "en")]), false).is_none());
    }
}
