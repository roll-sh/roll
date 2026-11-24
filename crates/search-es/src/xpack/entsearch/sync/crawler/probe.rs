//! `crawl-test`: one URL through the crawler's own path, writing nothing. The connector's
//! configuration (or, with none, a crawl of that URL's site) gives the plan; each origin's
//! robots.txt is read before anything on it is decided, the URL decided as a crawl decides it,
//! fetched through the same client and egress check with redirects followed by hand (each hop
//! decided, the credential kept to its origin), and parsed as a crawl parses it. Every request
//! waits for its site's turn, as a crawl's do (`hosts::turn`, at the unverified gap), and none is
//! made while crawlers are off on the node. The answer is what would be indexed, which of the
//! page's links would be queued, and why anything would be skipped; it never holds the
//! credential and is capped at [`MAX_ANSWER`].

use super::config::{self, Fields, Plan, L};
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
/// How long one test may take, every request and wait included.
const TIME: Duration = Duration::from_secs(75);
/// Origins whose robots.txt is read besides the URL's own and its redirects': links on the
/// others are shown as `robots_not_read`.
const MORE_ROBOTS: usize = 5;
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
    if !super::enabled(|k| cluster.config.node_setting(k)) {
        return Err(format!("web crawlers are turned off on this node ({})", super::ENABLED));
    }
    let floor = super::unverified_floor(|k| cluster.config.node_setting(k));
    let deadline = Instant::now() + TIME;
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
    let mut robots = Robotses { fetcher: &fetcher, blocked: &blocked, auth_origin: auth_origin.as_deref(), floor, deadline, more: MORE_ROBOTS, got: HashMap::new() };
    if let Some(d) = plan.domain(&link) {
        let read = robots.load(&plan, &link, true).await?;
        let r = &robots.got[&d.origin];
        answer.insert("robots".into(), obj! { "origin" => d.origin.clone(), "read" => read, "allows" => r.allows(&link.request_uri()), "crawl_delay" => r.crawl_delay.map_or(Value::Null, Value::from) });
    }
    let rule = |l: &Link| plan.domain(l).and_then(|d| d.rule_for(l)).map(|r| r.describe.clone());
    let v = decide(&plan, &fetcher, &blocked, &robots.got, &link);
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
        super::polite_turn(&at, floor, deadline.min(Instant::now() + super::TURN_WAIT)).await?;
        let got = tokio::time::timeout_at(deadline.into(), fetcher.get(&at, clean, max)).await.map_err(|_| out_of_time())?;
        let r = match got {
            Ok(r) => r,
            Err(f) => {
                answer.insert("error".into(), format!("fetching [{}] failed: {}", at.as_str(), f.shown()).into());
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
        // A hop to another of the crawl's origins is decided with that origin's robots.txt.
        robots.load(&plan, &next, true).await?;
        let nv = decide(&plan, &fetcher, &blocked, &robots.got, &next);
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
    let extraction = plan.domain(&at).map(|d| d.extraction.clone()).unwrap_or_default();
    let (page_url, limits, full_html) = (at.clone(), plan.limits.clone(), plan.full_html);
    let parse_by = deadline.min(Instant::now() + super::PAGE_PARSE_TIME);
    let parsed = tokio::task::spawn_blocking(move || {
        let r = html::read_page(&text, held, &page_url, &tags, &extraction, &limits, parse_by);
        (r, text)
    })
    .await
    .map_err(|_| "parsing the page stopped on an internal error".to_string())?;
    let (result, text) = parsed;
    let (mut page, held) = match result {
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
    let mut follow = std::mem::take(&mut page.follow);
    if page.nofollow {
        follow.clear();
    }
    let more = follow.len().saturating_sub(LINKS_SHOWN);
    follow.truncate(LINKS_SHOWN);
    let indexed = !page.noindex && !(page.title.is_empty() && page.body.is_empty());
    answer.insert("indexed".into(), indexed.into());
    if page.noindex {
        answer.insert("reason".into(), "the page says noindex".into());
    } else if !indexed {
        answer.insert("reason".into(), "the page has no title and no text".into());
    }
    let mut doc = super::document(&at, None, page, full_html.then_some(text), &chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true));
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
    // The page is done with: its memory and charge go before the links wait on robots.txt.
    drop((held, resp));

    // Each link's verdict. robots.txt is read for the URL's origin and its redirects', and for
    // [`MORE_ROBOTS`] more; a link on another of the crawl's origins is `robots_not_read`.
    let mut links: Vec<Value> = Vec::new();
    for l in &follow {
        // A link's robots.txt that can't be read in time (a busy site, the deadline) leaves the
        // link unread; the document stays in the answer.
        let read = robots.load(&plan, l, false).await.unwrap_or("not read");
        let v = decide(&plan, &fetcher, &blocked, &robots.got, l);
        let shown = if v == Verdict::Allowed && read == "not read" { "robots_not_read" } else { v.as_str() };
        links.push(obj! { "url" => l.as_str(), "verdict" => shown });
    }
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

/// The robots.txt of each origin `crawl-test` meets, read once.
struct Robotses<'a> {
    fetcher: &'a Fetcher,
    blocked: &'a [String],
    auth_origin: Option<&'a str>,
    floor: Duration,
    /// The test's end: nothing is waited for or fetched past it.
    deadline: Instant,
    /// Origins not yet read that may still be (beyond the ones the test must read).
    more: usize,
    got: HashMap<String, Robots>,
}

impl Robotses<'_> {
    /// Reads the robots.txt of `link`'s origin when it is one of `plan`'s and not read yet
    /// (`must`: the URL's and its redirects'; else only while [`Self::more`] lasts); what was
    /// found: `ok`, `missing` (a 4xx: everything allowed), `unreachable` (nothing is), `read`
    /// (before), or `not read`.
    async fn load(&mut self, plan: &Plan, link: &Link, must: bool) -> Result<&'static str, String> {
        let Some(d) = plan.domain(link) else { return Ok("off the crawl") };
        if self.got.contains_key(&d.origin) {
            return Ok("read");
        }
        if !must {
            if self.more == 0 || Instant::now() >= self.deadline {
                return Ok("not read");
            }
            self.more -= 1;
        }
        let (r, read) = self.fetch(&d.origin).await?;
        self.got.insert(d.origin.clone(), r);
        Ok(read)
    }

    /// An origin's robots.txt, as a crawl reads it (redirects within the site, at most five,
    /// never to a blocked or refused host), each request in the site's turn.
    async fn fetch(&self, origin: &str) -> Result<(Robots, &'static str), String> {
        let Some(mut at) = Link::parse(&format!("{origin}/robots.txt"), None) else { return Ok((Robots::unreachable(), "unreachable")) };
        let site = super::hosts::site_of(&at);
        let mut clean = true;
        for _ in 0..=5 {
            if self.fetcher.refused(&at) || super::is_blocked(&at, self.blocked) || super::hosts::site_of(&at) != site {
                break;
            }
            super::polite_turn(&at, self.floor, self.deadline.min(Instant::now() + super::TURN_WAIT)).await?;
            let got = tokio::time::timeout_at(self.deadline.into(), self.fetcher.get(&at, clean, super::robots::MAX_BYTES)).await.map_err(|_| out_of_time())?;
            let Ok(resp) = got else { break };
            if (300..400).contains(&resp.status) {
                let Some(next) = resp.location.as_deref().and_then(|l| Link::parse(l, Some(at.url()))) else { break };
                clean &= self.auth_origin == Some(at.origin().as_str());
                at = next;
                continue;
            }
            let read = match resp.status {
                200..=299 => "ok",
                429 => "unreachable",
                400..=499 => "missing",
                _ => "unreachable",
            };
            return Ok((robots_of(&resp), read));
        }
        Ok((Robots::unreachable(), "unreachable"))
    }
}

fn out_of_time() -> String {
    format!("the test ran out of time ({}s); a smaller plan, or a URL nearer its site's root, takes less", TIME.as_secs())
}
