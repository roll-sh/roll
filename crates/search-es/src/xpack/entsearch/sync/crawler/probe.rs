//! `crawl-test`: one URL through the crawler's own path, writing nothing. The connector's
//! configuration (or, with none, a crawl of that URL's site) gives the plan; the origin's
//! robots.txt is read, the URL decided as a crawl decides it, fetched through the same client and
//! egress check with redirects followed by hand (each hop decided, the credential kept to its
//! origin), and parsed as a crawl parses it. The answer is what would be indexed, which of the
//! page's links would be queued, and why anything would be skipped; it never holds the
//! credential and is capped at [`MAX_ANSWER`].

use super::config::{self, Fields, L};
use super::fetch::Fetcher;
use super::robots::Robots;
use super::url::Link;
use super::{Verdict, decide, html, is_html, robots_of};
use crate::cluster::Cluster;
use crate::json::{Map, Value};
use crate::obj;
use std::collections::HashMap;
use std::time::{Duration, Instant};

/// The most an answer holds: the body and links are cut to fit.
pub const MAX_ANSWER: usize = 1 << 20;
const BODY_SHOWN: usize = 64 << 10;
const LINKS_SHOWN: usize = 200;

impl Verdict {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Verdict::Allowed => "allowed",
            Verdict::OutOfDomain => "out_of_domain",
            Verdict::Limit => "url_limit",
            Verdict::Blocked => "blocked",
            Verdict::RuleDenied => "crawl_rule",
            Verdict::RobotsDisallowed => "robots_disallowed",
            Verdict::RobotsUnreachable => "robots_unreachable",
            Verdict::OriginSkipped => "origin_skipped",
        }
    }
}

/// Runs `crawl-test` for `url` on `cluster`, with connector `connector`'s configuration (it must
/// be a `roll_web_crawler`) or a crawl of the URL's site.
pub async fn test(cluster: &Cluster, connector: Option<&str>, url: &str) -> Result<Value, String> {
    let egress = crate::egress::Egress::of_cluster(cluster, super::super::ALLOW_PRIVATE);
    let link = Link::parse(url, None).ok_or_else(|| format!("[{url}] isn't an http or https URL"))?;
    let (fields, advanced, values) = match connector {
        Some(cid) => {
            let c = cluster.state().metadata.xpack.get(super::super::CONNECTORS, cid).cloned().ok_or_else(|| format!("no connector [{cid}]"))?;
            if c.get("service_type").and_then(Value::as_str) != Some(super::SERVICE) {
                return Err(format!("connector [{cid}] isn't a {} connector", super::SERVICE));
            }
            let values = super::super::config::Values::open(cluster, &c).map_err(|e| e.reason().to_string())?;
            let active = c.get("filtering").and_then(Value::as_array).and_then(|f| f.first()).and_then(|f| f.get("active")).cloned().unwrap_or(Value::Null);
            let advanced = super::super::rules::advanced(&active).cloned();
            (super::fields_of(&values)?, advanced, Some(values))
        }
        None => (Fields { urls: vec![format!("{}/", link.origin())], sitemap_urls: Vec::new(), sitemap_discovery: false, max_crawl_depth: None, max_unique_url_count: None }, None, None),
    };
    let ceil = super::ceilings(cluster);
    let plan = tokio::task::spawn_blocking(move || config::plan(&fields, advanced.as_ref(), &ceil)).await.map_err(|_| "building the crawl's rules stopped on an internal error".to_string())??;
    let auth = match &values {
        Some(v) => super::auth(v, egress.allow_private())?,
        None => None,
    };
    let auth_origin = auth.as_ref().map(|a| a.origin.clone());
    let fetcher = Fetcher::new(egress, auth)?;
    let blocked = super::blocked_hosts(cluster);
    let scrub = |e: String| values.as_ref().map_or(e.clone(), |v| v.scrub(&e));
    let mut answer = Map::new();
    answer.insert("url".into(), link.as_str().into());

    // robots.txt of the URL's origin, when the URL is on the crawl.
    let mut robots: HashMap<String, Robots> = HashMap::new();
    if let Some(d) = plan.domain(&link) {
        let (r, read) = fetch_robots(&fetcher, &d.origin, auth_origin.as_deref()).await;
        answer.insert("robots".into(), obj! { "origin" => d.origin.clone(), "read" => read, "allows" => r.allows(&link.request_uri()), "crawl_delay" => r.crawl_delay.map_or(Value::Null, Value::from) });
        robots.insert(d.origin.clone(), r);
    }
    let verdict = |l: &Link| decide(&plan, &fetcher, &blocked, &robots, l);
    let rule = |l: &Link| plan.domain(l).and_then(|d| d.rule_for(l)).map(|r| r.describe.clone());
    let v = verdict(&link);
    answer.insert("verdict".into(), v.as_str().into());
    if let Some(r) = rule(&link) {
        answer.insert("rule".into(), r.into());
    }
    if v != Verdict::Allowed {
        return Ok(Value::Obj(answer));
    }

    // The page, redirects followed by hand as a crawl follows them.
    let max = plan.limits.usize(L::ResponseSize);
    let (mut at, mut clean, mut hops) = (link.clone(), true, Vec::new());
    let resp = loop {
        let r = match fetcher.get(&at, clean, max).await {
            Ok(r) => r,
            Err(f) => {
                answer.insert("error".into(), format!("fetching [{}] failed: {}", at.as_str(), f.class()).into());
                answer.insert("redirects".into(), Value::Arr(hops));
                return Ok(Value::Obj(answer));
            }
        };
        if !(300..400).contains(&r.status) || r.location.is_none() {
            break r;
        }
        let next = r.location.as_deref().and_then(|l| Link::parse(l, Some(at.url())));
        let Some(next) = next.filter(|_| hops.len() < plan.limits.usize(L::Redirects)) else {
            answer.insert("error".into(), "too many redirects, or one to nowhere".into());
            break r;
        };
        let nv = verdict(&next);
        hops.push(obj! { "from" => at.as_str(), "status" => r.status as i64, "to" => next.as_str(), "verdict" => nv.as_str() });
        if nv != Verdict::Allowed {
            answer.insert("redirects".into(), Value::Arr(hops));
            answer.insert("verdict".into(), format!("redirected_{}", nv.as_str()).into());
            return Ok(Value::Obj(answer));
        }
        clean &= auth_origin.as_deref() == Some(at.origin().as_str());
        at = next;
    };
    answer.insert("redirects".into(), Value::Arr(hops));
    answer.insert("status".into(), (resp.status as i64).into());
    answer.insert("content_type".into(), resp.content_type.clone().into());
    if !(200..300).contains(&resp.status) || !is_html(&resp.content_type, &resp.body) {
        let why = if (200..300).contains(&resp.status) { "not an HTML page: nothing would be indexed" } else { "not a page: nothing would be indexed" };
        answer.insert("indexed".into(), false.into());
        answer.insert("reason".into(), why.into());
        return Ok(Value::Obj(answer));
    }

    // Parsed as a crawl parses it: off the async workers, against a deadline, charged.
    let mut held = crate::breaker::Reservation::new(cluster.breakers.request(cluster, &cluster.state()));
    held.add(resp.body.len() as u64 * 2, "crawl-test page").map_err(|e| e.reason().to_string())?;
    let text = String::from_utf8_lossy(&resp.body).into_owned();
    let tags = plan.domain(&at).map(|d| d.exclude_tags.clone()).unwrap_or_default();
    let (page_url, limits, full_html) = (at.clone(), plan.limits.clone(), plan.full_html);
    let deadline = Instant::now() + super::PAGE_PARSE_TIME;
    let parsed = tokio::task::spawn_blocking(move || {
        let r = html::read_page(&text, held, &page_url, &tags, &limits, deadline);
        (r, text)
    })
    .await
    .map_err(|_| "parsing the page stopped on an internal error".to_string())?;
    let (page, _held) = match parsed.0 {
        Ok(p) => p,
        Err(why) => {
            answer.insert("indexed".into(), false.into());
            answer.insert("reason".into(), scrub(why).into());
            return Ok(Value::Obj(answer));
        }
    };
    answer.insert("noindex".into(), page.noindex.into());
    answer.insert("nofollow".into(), page.nofollow.into());
    if let Some(c) = &page.canonical {
        answer.insert("canonical".into(), c.as_str().into());
    }
    let links: Vec<Value> = if page.nofollow {
        Vec::new()
    } else {
        page.follow.iter().take(LINKS_SHOWN).map(|l| obj! { "url" => l.as_str(), "verdict" => verdict(l).as_str() }).collect()
    };
    let more = page.follow.len().saturating_sub(LINKS_SHOWN);
    let indexed = !page.noindex && !(page.title.is_empty() && page.body.is_empty());
    answer.insert("indexed".into(), indexed.into());
    if page.noindex {
        answer.insert("reason".into(), "the page says noindex".into());
    } else if !indexed {
        answer.insert("reason".into(), "the page has no title and no text".into());
    }
    let mut doc = super::document(&at, None, page, full_html.then_some(parsed.1), &chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true));
    // What's shown is cut to fit the answer; what a crawl indexes isn't.
    for k in ["body", "full_html"] {
        if let Some(Value::Str(s)) = doc.get_mut(k)
            && s.len() > BODY_SHOWN
        {
            let mut at = BODY_SHOWN;
            while !s.is_char_boundary(at) {
                at -= 1;
            }
            s.truncate(at);
            answer.insert(format!("{k}_truncated"), true.into());
        }
    }
    answer.insert("document".into(), Value::Obj(doc));
    answer.insert("links".into(), Value::Arr(links));
    if more > 0 {
        answer.insert("links_not_shown".into(), (more as i64).into());
    }
    let out = Value::Obj(answer);
    if out.to_json().len() > MAX_ANSWER {
        return Err("the answer would be larger than 1 MiB".into());
    }
    Ok(out)
}

/// An origin's robots.txt, as a crawl reads it (redirects within the site, at most five), and
/// what was found: `ok`, `missing` (a 4xx: everything allowed) or `unreachable` (nothing is).
async fn fetch_robots(fetcher: &Fetcher, origin: &str, auth_origin: Option<&str>) -> (Robots, &'static str) {
    let Some(mut at) = Link::parse(&format!("{origin}/robots.txt"), None) else { return (Robots::unreachable(), "unreachable") };
    let site = super::hosts::site_of(&at);
    let mut clean = true;
    let started = Instant::now();
    for _ in 0..=5 {
        if super::hosts::site_of(&at) != site || started.elapsed() > Duration::from_secs(60) {
            break;
        }
        let Ok(resp) = fetcher.get(&at, clean, super::robots::MAX_BYTES).await else { break };
        if (300..400).contains(&resp.status) {
            let Some(next) = resp.location.as_deref().and_then(|l| Link::parse(l, Some(at.url()))) else { break };
            clean &= auth_origin == Some(at.origin().as_str());
            at = next;
            continue;
        }
        let r = robots_of(&resp);
        let read = match resp.status {
            200..=299 => "ok",
            429 => "unreachable",
            400..=499 => "missing",
            _ => "unreachable",
        };
        return (r, read);
    }
    (Robots::unreachable(), "unreachable")
}
