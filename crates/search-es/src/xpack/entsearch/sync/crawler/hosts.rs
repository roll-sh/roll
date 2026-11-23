//! What every crawl on this node shares per site, outside any cluster's state (so neither more
//! connectors nor more clusters, nor deleting a connector, buys more): an owner's crawls of a
//! site in the last 24 hours, and the site's next request turn, which spaces requests to one
//! site across every crawl on the node. Domain verification (phase 2) moves the daily count into
//! roll's catalogue, fleet-wide.

use super::url::Link;
use std::collections::HashMap;
use std::time::{Duration, Instant};

const DAY_MS: i64 = 86_400_000;

/// The site `link` belongs to: its registrable domain by the public suffix list (private
/// suffixes included, so each `*.github.io` site is its own). An IP literal is its own site,
/// with its port; a host that is a public suffix itself is its own site.
pub fn site_of(link: &Link) -> String {
    let url = link.url();
    let host = url.host_str().unwrap_or_default().to_ascii_lowercase();
    if host.parse::<std::net::IpAddr>().is_ok() || host.starts_with('[') {
        return format!("{host}:{}", url.port_or_known_default().unwrap_or(0));
    }
    let host = host.trim_end_matches('.');
    psl::domain_str(host).unwrap_or(host).to_string()
}

#[derive(Default)]
struct State {
    crawls: HashMap<(String, String), Vec<i64>>,
    next: HashMap<String, Instant>,
}

fn state() -> &'static parking_lot::Mutex<State> {
    static STATE: std::sync::LazyLock<parking_lot::Mutex<State>> = std::sync::LazyLock::new(Default::default);
    &STATE
}

/// The first of `sites` that has had `per_day` crawls by `owner` in the 24 hours before
/// `now_ms`, and when the next may start.
fn full(st: &mut State, owner: &str, sites: &[String], per_day: u64, now_ms: i64) -> Option<(String, i64)> {
    st.crawls.retain(|_, v| {
        v.retain(|t| *t > now_ms - DAY_MS);
        !v.is_empty()
    });
    sites.iter().find_map(|site| {
        let v = st.crawls.get(&(owner.to_string(), site.clone()))?;
        (v.len() as u64 >= per_day).then(|| (site.clone(), v.iter().min().copied().unwrap_or(now_ms) + DAY_MS))
    })
}

/// Whether a crawl of `sites` by `owner` may start, recording nothing.
pub fn check_crawl(owner: &str, sites: &[String], per_day: u64, now_ms: i64) -> Result<(), (String, i64)> {
    full(&mut state().lock(), owner, sites, per_day, now_ms).map_or(Ok(()), Err)
}

/// Records a crawl of `sites` by `owner` at `now_ms`; or, when one of them has had `per_day`
/// crawls by that owner in 24 hours, records nothing and names it and when the next may start.
pub fn record_crawl(owner: &str, sites: &[String], per_day: u64, now_ms: i64) -> Result<(), (String, i64)> {
    let mut st = state().lock();
    if let Some(f) = full(&mut st, owner, sites, per_day, now_ms) {
        return Err(f);
    }
    for site in sites {
        st.crawls.entry((owner.to_string(), site.clone())).or_default().push(now_ms);
    }
    Ok(())
}

/// `site`'s request turn: `None` when it is now (taken: the next one is `gap` later), else when
/// it comes.
pub fn turn(site: &str, gap: Duration) -> Option<Instant> {
    let mut st = state().lock();
    let now = Instant::now();
    if st.next.len() > 10_000 {
        st.next.retain(|_, at| *at > now);
    }
    match st.next.get(site) {
        Some(at) if *at > now => Some(*at),
        _ => {
            st.next.insert(site.to_string(), now + gap);
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sites_and_daily_counts() {
        let site = |u: &str| site_of(&Link::parse(u, None).unwrap());
        assert_eq!(site("https://docs.Example.com/x"), "example.com");
        assert_eq!(site("https://a.b.example.co.uk/"), "example.co.uk");
        assert_eq!(site("https://alice.github.io/x"), "alice.github.io");
        assert_eq!(site("https://shop.bob.me.uk/"), "bob.me.uk");
        assert_eq!(site("http://127.0.0.1:8080/"), "127.0.0.1:8080");
        let sites = vec!["victim.example".to_string()];
        assert!(record_crawl("owner-a", &sites, 2, 1_000).is_ok() && record_crawl("owner-a", &sites, 2, 2_000).is_ok());
        assert_eq!(record_crawl("owner-a", &sites, 2, 3_000), Err(("victim.example".into(), 1_000 + DAY_MS)));
        assert!(record_crawl("owner-b", &sites, 2, 3_000).is_ok(), "another owner counts apart");
        assert!(record_crawl("owner-a", &sites, 2, 1_000 + DAY_MS + 1).is_ok(), "a day later");
    }
}
