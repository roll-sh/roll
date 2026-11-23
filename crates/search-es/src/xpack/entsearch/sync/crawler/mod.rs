//! The `roll_web_crawler` connector: a website into a `content-` index, as Elastic's Open
//! Crawler would crawl it (docs/search/crawler.md). A crawl is a full sync on #318's runner:
//! robots.txt first, then sitemaps and the seed URLs, then links breadth first by depth, each URL
//! checked against the crawl's domains, its limits, the crawl rules and robots.txt, and fetched
//! politely ([`fetch`]). Pages become Open Crawler's documents ([`html`]) through the sync's
//! [`Sink`]. A crawl that ends `completed` purges what proves gone and keeps what an outage hid
//! ([`Crawl::purge`]).

pub mod config;
pub mod fetch;
pub mod hosts;
pub mod html;
pub mod robots;
pub mod sitemap;
pub mod url;

use self::config::{Ceilings, Fields, L, Plan};
use self::fetch::{Auth, Fetcher};
use self::robots::Robots;
use self::url::Link;
use super::config::Values;
use super::{PAGE_BYTES, PAGE_DOCS, RECORDS, Sink, Stop, enc};
use crate::cluster::Cluster;
use crate::json::{Map, Value};
use crate::obj;
use crate::xpack::{internal, store};
use std::collections::{HashMap, HashSet, VecDeque};
use std::time::{Duration, Instant};

pub const SERVICE: &str = "roll_web_crawler";
/// Node settings.
pub const ENABLED: &str = "roll.crawler.enabled";
pub const BLOCKED_HOSTS: &str = "roll.crawler.blocked_hosts";
pub const MIN_REQUEST_INTERVAL: &str = "roll.crawler.min_request_interval";
/// The limits of a domain that isn't verified (every domain until verification lands).
pub const UNVERIFIED_MAX_URLS: &str = "roll.crawler.unverified.max_unique_url_count";
pub const UNVERIFIED_MIN_INTERVAL: &str = "roll.crawler.unverified.min_request_interval";
pub const UNVERIFIED_CRAWLS_PER_DAY: &str = "roll.crawler.unverified.max_crawls_per_day";

const RETRY_AFTER_MAX: Duration = Duration::from_secs(600);
/// Times one URL is asked again after a `429` or `503` with `Retry-After`.
const RETRIES: usize = 2;
const ROBOTS_REDIRECTS: usize = 5;
/// Sitemaps one crawl reads at most; and how deep sitemap indexes nest.
const MAX_SITEMAPS: usize = 100;
const SITEMAP_INDEX_LEVELS: usize = 2;
const DAY_MS: i64 = 86_400_000;
/// The longest one page may take to parse and read.
const PAGE_PARSE_TIME: Duration = Duration::from_secs(10);
/// A purge is skipped when this many redirects or more mostly led to one URL (a login or a
/// maintenance page).
const FUNNEL_MIN: u64 = 10;
/// The pages that may name one canonical URL as theirs.
const MAX_ALIASES: usize = 100;

/// The mappings of an index a crawl creates (crawler.md, "Documents").
pub fn mappings() -> Value {
    let t = |ty: &str| obj! { "type" => ty };
    obj! { "mappings" => obj! { "properties" => obj! {
        "title" => t("text"), "body" => t("text"), "headings" => t("text"), "meta_description" => t("text"),
        "url" => t("keyword"), "url_host" => t("keyword"), "url_path" => t("keyword"), "url_scheme" => t("keyword"), "additional_urls" => t("keyword"),
        "url_path_dir1" => t("keyword"), "url_path_dir2" => t("keyword"), "url_path_dir3" => t("keyword"),
        "last_crawled_at" => t("date"), "url_port" => t("long"),
        "links" => obj! { "type" => "keyword", "index" => false },
    } } }
}

/// The node's ceilings for crawl limits.
pub fn ceilings(cluster: &Cluster) -> Ceilings {
    let max_sync = cluster
        .config
        .node_setting(super::MAX_SYNC_DURATION)
        .and_then(|v| crate::units::TimeValue::parse(v, super::MAX_SYNC_DURATION).ok())
        .map(|t| t.as_millis())
        .filter(|m| *m > 0)
        .unwrap_or(super::DEFAULT_MAX_DURATION_MS);
    Ceilings::new(|k| cluster.config.node_setting(k).map(str::to_string), max_sync)
}

fn interval(cluster: &Cluster, key: &str, default_ms: u64) -> Duration {
    let ms = cluster.config.node_setting(key).and_then(|v| crate::units::TimeValue::parse(v, key).ok()).map(|t| t.as_millis()).filter(|m| *m >= 0).map_or(default_ms, |m| m as u64);
    Duration::from_millis(ms)
}

fn number(cluster: &Cluster, key: &str, default: u64) -> u64 {
    cluster.config.node_setting(key).and_then(|v| v.parse::<u64>().ok()).filter(|n| *n > 0).unwrap_or(default)
}

/// Why a URL isn't fetched.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Verdict {
    Allowed,
    OutOfDomain,
    /// Over `max_url_length`, `max_url_segments` or `max_url_params`.
    Limit,
    /// `roll.crawler.blocked_hosts`, or a private address.
    Blocked,
    RuleDenied,
    RobotsDisallowed,
    /// The origin's robots.txt couldn't be read: nothing of it is fetched this crawl.
    RobotsUnreachable,
    /// The origin asked to be left alone for longer than [`RETRY_AFTER_MAX`].
    OriginSkipped,
}

/// The counts a crawl adds to its job's `metadata`.
#[derive(Default)]
struct Stats {
    fetched_pages: u64,
    fetched_bytes: u64,
    robots_disallowed: u64,
    redirected_away: u64,
    crawl_rule_excluded: u64,
    url_limit_excluded: u64,
    blocked: u64,
    dropped_links: u64,
    skipped_origin_urls: u64,
    sitemap_errors: u64,
    errors: Map,
    elastic_crawler_groups: Vec<String>,
}

struct Crawl<'s, 'a> {
    sink: &'s mut Sink<'a>,
    plan: Plan,
    fetcher: Fetcher,
    auth_origin: Option<String>,
    blocked: Vec<String>,
    robots: HashMap<String, Robots>,
    next_at: HashMap<String, Instant>,
    floor: Duration,
    skipped_origins: HashSet<String>,
    /// Hashes of URLs queued or visited.
    known: HashSet<u64>,
    queue: VecDeque<(Link, u64)>,
    /// URLs whose documents are proven gone (404 or 410, `noindex`, excluded), and URLs whose
    /// documents are kept this crawl: errors, and signs that may only be an outage (a redirect off
    /// the crawl or to a URL it already has, a body that isn't HTML), which delete a document only
    /// after three full crawls in a row.
    gone: HashSet<u64>,
    kept: HashSet<u64>,
    /// URLs indexed under themselves this crawl, and the pages naming each as canonical.
    indexed: HashSet<u64>,
    aliases: HashMap<u64, (Link, Vec<Link>)>,
    /// Redirects followed or refused, and how many led to each target.
    redirects: u64,
    redirect_targets: HashMap<u64, u64>,
    visited: u64,
    budget: u64,
    budget_name: &'static str,
    deadline: Instant,
    /// The crawl's start (`last_crawled_at`).
    started_at: String,
    page: super::Page,
    stats: Stats,
    limit_reached: Option<&'static str>,
}

/// Runs a crawl; the new sync cursor.
pub async fn sync(cid: &str, v: &Values, sink: &mut Sink<'_>, full: bool, cursor: &Value, advanced: Option<&Value>, allow_private: bool) -> Result<Value, Stop> {
    let cluster = sink.cluster;
    if cluster.config.node_setting(ENABLED).is_some_and(|v| v.eq_ignore_ascii_case("false")) {
        return Err(Stop::Failed(format!("web crawlers are turned off on this node ({ENABLED})")));
    }
    if !full {
        return Err(Stop::Failed("roll's web crawler runs full crawls only; incremental crawls come later".into()));
    }
    let int = |k: &str| -> Result<Option<i64>, String> { v.text(k).map(|s| s.parse::<i64>().map_err(|_| format!("the connector's [{k}] isn't a number"))).transpose() };
    let fields = Fields { urls: v.list("urls"), sitemap_urls: v.list("sitemap_urls"), sitemap_discovery: v.bool("sitemap_discovery", true), max_crawl_depth: int("max_crawl_depth")?, max_unique_url_count: int("max_unique_url_count")? };
    let plan = config::plan(&fields, advanced, &ceilings(cluster)).map_err(Stop::Failed)?;
    let auth = auth(v, allow_private).map_err(Stop::Failed)?;
    let auth_origin = auth.as_ref().map(|a| a.origin.clone());
    let fetcher = Fetcher::new(allow_private, auth).map_err(Stop::Failed)?;
    // Unverified domains' limits (domain verification comes later: every domain is unverified).
    let unverified = number(cluster, UNVERIFIED_MAX_URLS, 1000);
    let asked = plan.limits.get(L::UniqueUrls);
    let (budget, budget_name) = if unverified < asked { (unverified, "unverified_domain_max_unique_url_count") } else { (asked, "max_unique_url_count") };
    let floor = interval(cluster, MIN_REQUEST_INTERVAL, 500).max(interval(cluster, UNVERIFIED_MIN_INTERVAL, 1000));
    let per_day = number(cluster, UNVERIFIED_CRAWLS_PER_DAY, 4);
    count_crawl(cluster, cid, per_day).await?;
    // The same limit per owner and site across the node, whatever connector or cluster asks.
    let owner = cluster.config.owner.clone().unwrap_or_else(|| format!("cluster {}", cluster.name));
    let mut sites: Vec<String> = plan.domains.iter().filter_map(|d| Link::parse(&format!("{}/", d.origin), None)).map(|l| hosts::site_of(&l)).collect();
    sites.sort();
    sites.dedup();
    if let Err((site, next)) = hosts::record_crawl(&owner, &sites, per_day, crate::time::now_millis()) {
        return Err(Stop::Failed(format!(
            "an unverified site is crawled at most {per_day} times in 24 hours by one owner ({UNVERIFIED_CRAWLS_PER_DAY}): [{site}] can be crawled again after {}",
            chrono::DateTime::from_timestamp_millis(next).unwrap_or_default().to_rfc3339_opts(chrono::SecondsFormat::Secs, true)
        )));
    }
    let blocked = cluster.config.node_setting(BLOCKED_HOSTS).map(|b| b.split(',').map(|h| h.trim().trim_start_matches('.').to_ascii_lowercase()).filter(|h| !h.is_empty()).collect()).unwrap_or_default();
    let index = sink.index.clone();
    let before = doc_count(cluster, &index).await;
    let started_ms = crate::time::now_millis();
    let started_at = chrono::DateTime::from_timestamp_millis(started_ms).unwrap_or_default().to_rfc3339_opts(chrono::SecondsFormat::Millis, true);
    let duration = Duration::from_secs(plan.limits.get(L::Duration));
    let page = sink.new_page();
    let sink_deadline = sink.deadline.checked_sub(Duration::from_secs(60)).unwrap_or(sink.deadline);
    let mut c = Crawl {
        sink,
        plan,
        fetcher,
        auth_origin,
        blocked,
        robots: HashMap::new(),
        next_at: HashMap::new(),
        floor,
        skipped_origins: HashSet::new(),
        known: HashSet::new(),
        queue: VecDeque::new(),
        gone: HashSet::new(),
        kept: HashSet::new(),
        indexed: HashSet::new(),
        aliases: HashMap::new(),
        redirects: 0,
        redirect_targets: HashMap::new(),
        visited: 0,
        budget,
        budget_name,
        // Ends (completed) before the sync's own limit would fail it.
        deadline: (Instant::now() + duration).min(sink_deadline),
        started_at,
        page,
        stats: Stats::default(),
        limit_reached: None,
    };
    let result = c.run(cursor, before).await;
    c.write_stats();
    result
}

/// The credential and its origin, from the sealed fields.
fn auth(v: &Values, allow_private: bool) -> Result<Option<Auth>, String> {
    let header = match (v.text("auth_header"), v.text("auth_username"), v.text("auth_password")) {
        (Some(h), _, _) => h,
        (None, Some(u), p) => {
            use base64::Engine;
            format!("Basic {}", base64::engine::general_purpose::STANDARD.encode(format!("{u}:{}", p.unwrap_or_default())))
        }
        _ => return Ok(None),
    };
    let origin = v.text("auth_origin").ok_or("the connector has a credential but no [auth_origin]: the one origin it may be sent to")?;
    let origin = config::domain_origin(&origin).map_err(|e| format!("[auth_origin]: {e}"))?;
    let loopback = || origin.starts_with("http://127.") || origin.starts_with("http://[::1]") || origin.starts_with("http://localhost");
    if !origin.starts_with("https://") && !(allow_private && loopback()) {
        return Err(format!("[auth_origin] is [{origin}]: a credential is only ever sent over https"));
    }
    if header.contains(['\r', '\n']) {
        return Err("[auth_header] holds a line break".into());
    }
    Ok(Some(Auth { origin, header }))
}

/// Records a crawl's start, refusing a fifth crawl in 24 hours (an unverified domain's limit).
async fn count_crawl(cluster: &Cluster, cid: &str, per_day: u64) -> Result<(), Stop> {
    let (cid, now) = (cid.to_string(), crate::time::now_millis());
    let refused = cluster
        .update(move |st| {
            let mut rec = st.metadata.xpack.get(RECORDS, &cid).and_then(Value::as_object).cloned().unwrap_or_default();
            let mut crawls: Vec<i64> = rec.get("crawls").and_then(Value::as_array).map(|a| a.iter().filter_map(Value::as_i64).filter(|t| *t > now - DAY_MS).collect()).unwrap_or_default();
            if crawls.len() as u64 >= per_day {
                return Ok(crawls.iter().min().map(|t| t + DAY_MS));
            }
            crawls.push(now);
            rec.insert("crawls".into(), Value::Arr(crawls.into_iter().map(Value::from).collect()));
            store::put(st, RECORDS, &cid, Value::Obj(rec), now)?;
            Ok(None)
        })
        .await
        .map_err(|e| Stop::Failed(format!("the crawl can't be recorded: {}", e.reason())))?;
    match refused {
        Some(next) => Err(Stop::Failed(format!(
            "an unverified domain is crawled at most {per_day} times in 24 hours ({UNVERIFIED_CRAWLS_PER_DAY}); the next crawl can start after {}",
            chrono::DateTime::from_timestamp_millis(next).unwrap_or_default().to_rfc3339_opts(chrono::SecondsFormat::Secs, true)
        ))),
        None => Ok(()),
    }
}

async fn doc_count(cluster: &Cluster, index: &str) -> u64 {
    let _ = internal::call(cluster, "POST", &format!("/{}/_refresh", enc(index)), None).await;
    internal::call(cluster, "GET", &format!("/{}/_count", enc(index)), None).await.ok().and_then(|v| v.get("count").and_then(Value::as_i64)).unwrap_or(0).max(0) as u64
}

fn is_html(content_type: &str, body: &[u8]) -> bool {
    if content_type.is_empty() {
        let head = String::from_utf8_lossy(&body[..body.len().min(256)]).trim_start().to_ascii_lowercase();
        return head.starts_with("<!doctype html") || head.starts_with("<html");
    }
    content_type.starts_with("text/html") || content_type.starts_with("application/xhtml+xml")
}

impl Crawl<'_, '_> {
    async fn run(&mut self, cursor: &Value, before: u64) -> Result<Value, Stop> {
        // robots.txt of every domain first: its sitemaps seed the crawl.
        let origins: Vec<String> = self.plan.domains.iter().map(|d| d.origin.clone()).collect();
        let mut sitemaps: Vec<(Link, usize)> = self.plan.sitemaps.iter().cloned().map(|l| (l, 0)).collect();
        for o in origins {
            let r = self.load_robots(&o).await?;
            if r.elastic_crawler_group {
                self.stats.elastic_crawler_groups.push(o.clone());
            }
            if self.plan.sitemap_discovery {
                sitemaps.extend(r.sitemaps.iter().filter_map(|s| Link::parse(s, None)).map(|l| (l, 0)));
            }
            self.robots.insert(o, r);
        }
        for s in self.plan.seeds.clone() {
            self.enqueue(s, 1);
        }
        self.read_sitemaps(sitemaps).await?;
        while let Some((link, depth)) = self.queue.pop_front() {
            if Instant::now() >= self.deadline {
                self.limit_reached = Some("max_duration");
                break;
            }
            if self.visited >= self.budget {
                self.limit_reached = Some(self.budget_name);
                break;
            }
            if self.skipped_origins.contains(&link.origin()) {
                self.stats.skipped_origin_urls += 1;
                continue;
            }
            self.visit(link, depth).await?;
            self.tick().await?;
        }
        self.flush().await?;
        self.purge(cursor, before).await
    }

    /// Writes the crawl's counts into the job's metadata (the heartbeat sends them).
    fn write_stats(&mut self) {
        let s = &self.stats;
        let m = &mut self.sink.counters.metadata;
        let n = |v: u64| Value::from(v as i64);
        m.insert("fetched_page_count".into(), n(s.fetched_pages));
        m.insert("fetched_bytes".into(), n(s.fetched_bytes));
        m.insert("robots_disallowed_count".into(), n(s.robots_disallowed));
        m.insert("redirected_away_count".into(), n(s.redirected_away));
        m.insert("crawl_rule_excluded_count".into(), n(s.crawl_rule_excluded));
        m.insert("url_limit_excluded_count".into(), n(s.url_limit_excluded));
        m.insert("blocked_url_count".into(), n(s.blocked));
        m.insert("dropped_link_count".into(), n(s.dropped_links));
        m.insert("skipped_origin_url_count".into(), n(s.skipped_origin_urls));
        m.insert("sitemap_error_count".into(), n(s.sitemap_errors));
        m.insert("error_count".into(), Value::Obj(s.errors.clone()));
        if !s.elastic_crawler_groups.is_empty() {
            m.insert("robots_elastic_crawler_group_ignored".into(), Value::Arr(s.elastic_crawler_groups.iter().cloned().map(Value::from).collect()));
        }
        m.insert("limit_reached".into(), self.limit_reached.map_or(Value::Null, Value::from));
    }

    async fn tick(&mut self) -> Result<(), Stop> {
        self.write_stats();
        self.sink.throttle().await;
        self.sink.check().await
    }

    fn error(&mut self, class: &str) {
        let n = self.stats.errors.get(class).and_then(Value::as_i64).unwrap_or(0);
        self.stats.errors.insert(class.into(), (n + 1).into());
    }

    /// Indexes the page of documents so far.
    async fn flush(&mut self) -> Result<(), Stop> {
        let page = std::mem::replace(&mut self.page, self.sink.new_page());
        self.sink.send(page).await
    }

    fn verdict(&self, link: &Link) -> Verdict {
        let limits = &self.plan.limits;
        if link.as_str().len() > limits.usize(L::UrlLength) || link.segments() > limits.usize(L::UrlSegments) || link.params() > limits.usize(L::UrlParams) {
            return Verdict::Limit;
        }
        let Some(domain) = self.plan.domain(link) else { return Verdict::OutOfDomain };
        let host = link.url().host_str().unwrap_or_default().to_ascii_lowercase();
        if self.fetcher.refused(link) || self.blocked.iter().any(|b| host == *b || host.ends_with(&format!(".{b}"))) {
            return Verdict::Blocked;
        }
        if self.skipped_origins.contains(&domain.origin) {
            return Verdict::OriginSkipped;
        }
        if domain.rule_for(link).is_some_and(|r| !r.allow) {
            return Verdict::RuleDenied;
        }
        match self.robots.get(&domain.origin) {
            Some(r) if r.unreachable_origin() => Verdict::RobotsUnreachable,
            Some(r) if !r.allows(&link.request_uri()) => Verdict::RobotsDisallowed,
            _ => Verdict::Allowed,
        }
    }

    /// Counts a verdict that keeps a URL out.
    fn count(&mut self, v: Verdict) {
        match v {
            Verdict::RobotsDisallowed | Verdict::RobotsUnreachable => self.stats.robots_disallowed += 1,
            Verdict::RuleDenied => self.stats.crawl_rule_excluded += 1,
            Verdict::Limit => self.stats.url_limit_excluded += 1,
            Verdict::Blocked => self.stats.blocked += 1,
            Verdict::OriginSkipped => self.stats.skipped_origin_urls += 1,
            Verdict::OutOfDomain | Verdict::Allowed => {}
        }
    }

    /// Queues `link` at `depth` if it may be fetched and fits.
    fn enqueue(&mut self, link: Link, depth: u64) {
        if depth > self.plan.limits.get(L::Depth) || self.known.contains(&link.hash64()) {
            return;
        }
        match self.verdict(&link) {
            Verdict::Allowed => {}
            v => {
                self.count(v);
                return;
            }
        }
        if self.queue.len() >= self.plan.limits.usize(L::QueueSize) {
            self.stats.dropped_links += 1;
            return;
        }
        self.known.insert(link.hash64());
        self.queue.push_back((link, depth));
    }

    /// Sleeps until `at`, or the crawl's deadline if that comes first.
    async fn sleep_until(&self, at: Instant) -> Result<(), Stop> {
        tokio::select! {
            _ = tokio::time::sleep_until(at.min(self.deadline).into()) => Ok(()),
            s = self.sink.stopped() => Err(s),
        }
    }

    /// Waits for `link`'s turn: one request at a time per origin, spaced by the larger of its
    /// `Crawl-delay` and the node's interval, and one at a time per site across every crawl on
    /// the node. No wait outlasts the crawl's deadline, which then ends it.
    async fn wait_turn(&mut self, link: &Link) -> Result<(), Stop> {
        let origin = link.origin();
        let delay = self.robots.get(&origin).and_then(|r| r.crawl_delay).map(|d| Duration::from_secs_f64(d.min(86_400.0))).unwrap_or_default();
        let gap = self.floor.max(delay);
        if let Some(at) = self.next_at.get(&origin).copied()
            && at > Instant::now()
        {
            self.sleep_until(at).await?;
        }
        let site = hosts::site_of(link);
        while let Some(at) = hosts::turn(&site, self.floor) {
            if Instant::now() >= self.deadline {
                break;
            }
            self.sleep_until(at).await?;
        }
        self.next_at.insert(origin, Instant::now() + gap);
        Ok(())
    }

    /// One polite request; `clean`: every hop before it was at the credential's origin.
    async fn get(&mut self, link: &Link, clean: bool, max: usize) -> Result<Result<fetch::Response, fetch::Failure>, Stop> {
        self.wait_turn(link).await?;
        let r = tokio::select! {
            r = self.fetcher.get(link, clean, max) => r,
            s = self.sink.stopped() => return Err(s),
        };
        if let Ok(resp) = &r {
            self.sink.read(resp.wire_bytes);
            self.stats.fetched_bytes += resp.wire_bytes;
        }
        Ok(r)
    }

    fn on_auth_origin(&self, link: &Link) -> bool {
        self.auth_origin.as_deref() == Some(link.origin().as_str())
    }

    /// An origin's robots.txt (RFC 9309 section 2.3.1): a 2xx is read, a 4xx (but 429) allows
    /// everything, and a 5xx, a 429, a timeout or a network error disallows the origin for this
    /// crawl. Redirects are followed up to five times, to other origins too (`http` to `https`,
    /// the apex to `www`), each hop's address checked and the credential never sent after a hop
    /// leaves its origin; more is unreachable.
    async fn load_robots(&mut self, origin: &str) -> Result<Robots, Stop> {
        let Some(mut at) = Link::parse(&format!("{origin}/robots.txt"), None) else { return Ok(Robots::unreachable()) };
        let mut clean = true;
        for _ in 0..=ROBOTS_REDIRECTS {
            let host = at.url().host_str().unwrap_or_default().to_ascii_lowercase();
            if self.fetcher.refused(&at) || self.blocked.iter().any(|b| host == *b || host.ends_with(&format!(".{b}"))) {
                return Ok(Robots::unreachable());
            }
            let resp = match self.get(&at, clean, robots::MAX_BYTES).await? {
                Ok(r) => r,
                // Too large: what was read can't be trusted to be whole, so nothing is allowed.
                Err(_) => return Ok(Robots::unreachable()),
            };
            return Ok(match resp.status {
                200..=299 => Robots::parse(&String::from_utf8_lossy(&resp.body)),
                300..=399 => match resp.location.as_deref().and_then(|l| Link::parse(l, Some(at.url()))) {
                    Some(next) => {
                        clean &= self.on_auth_origin(&at);
                        at = next;
                        continue;
                    }
                    None => Robots::unreachable(),
                },
                429 => Robots::unreachable(),
                400..=499 => Robots::allow_all(),
                _ => Robots::unreachable(),
            });
        }
        Ok(Robots::unreachable())
    }

    /// Sitemaps (and the sitemaps their indexes list, two levels deep): their pages are queued
    /// at depth 1.
    async fn read_sitemaps(&mut self, mut todo: Vec<(Link, usize)>) -> Result<(), Stop> {
        let mut read = 0usize;
        let mut seen = HashSet::new();
        // Checked as they are listed, so an index of 50,000 sitemaps keeps only what can be read.
        let mut admit = |c: &mut Self, todo: &mut Vec<(Link, usize)>, read: usize, link: Link, level: usize| {
            if !seen.insert(link.hash64()) {
                return;
            }
            if read + todo.len() >= MAX_SITEMAPS {
                c.stats.sitemap_errors += 1;
                return;
            }
            match c.verdict(&link) {
                Verdict::Allowed => todo.push((link, level)),
                v => c.count(v),
            }
        };
        let mut queue = Vec::new();
        for (l, level) in todo.drain(..) {
            admit(self, &mut queue, 0, l, level);
        }
        queue.reverse();
        let mut todo = queue;
        while let Some((link, level)) = todo.pop() {
            read += 1;
            let max = self.plan.limits.usize(L::ResponseSize);
            let clean = self.on_auth_origin(&link);
            let body = match self.get(&link, clean, max).await? {
                Ok(r) if (200..300).contains(&r.status) => r.body,
                _ => {
                    self.stats.sitemap_errors += 1;
                    continue;
                }
            };
            let mut held = self.sink.reserve();
            if held.add(body.len() as u64 * 2, "crawled sitemap").is_err() {
                self.stats.sitemap_errors += 1;
                continue;
            }
            let parsed = sitemap::gunzip(body, max).and_then(|xml| sitemap::parse(&xml));
            match parsed {
                Ok(sitemap::Sitemap::Pages(locs)) => {
                    for l in locs.iter().filter_map(|l| Link::parse(l, None)) {
                        self.enqueue(l, 1);
                    }
                }
                Ok(sitemap::Sitemap::Index(locs)) if level < SITEMAP_INDEX_LEVELS => {
                    let mut more = Vec::new();
                    for l in locs.iter().filter_map(|l| Link::parse(l, None)) {
                        if read + todo.len() + more.len() >= MAX_SITEMAPS {
                            self.stats.sitemap_errors += 1;
                            break;
                        }
                        admit(self, &mut more, read + todo.len(), l, level + 1);
                    }
                    todo.extend(more.into_iter().rev());
                }
                Ok(sitemap::Sitemap::Index(_)) | Err(_) => self.stats.sitemap_errors += 1,
            }
            drop(held);
            self.tick().await?;
        }
        Ok(())
    }

    /// Fetches `link` (following redirects by hand) and indexes the page it leads to. Records
    /// what the URL proved: indexed, gone ([`Crawl::gone`]) or kept by an error
    /// ([`Crawl::kept`]).
    async fn visit(&mut self, link: Link, depth: u64) -> Result<(), Stop> {
        self.visited += 1;
        let src = link.hash64();
        let mut at = link;
        let mut clean = true;
        let mut hops = 0u64;
        let mut retries = 0usize;
        let max = self.plan.limits.usize(L::ResponseSize);
        let resp = loop {
            let r = match self.get(&at, clean, max).await? {
                Ok(r) => r,
                Err(f) => {
                    self.error(f.class());
                    self.kept.insert(src);
                    return Ok(());
                }
            };
            self.stats.fetched_pages += 1;
            match r.status {
                300..=399 if r.location.is_some() => {
                    hops += 1;
                    let next = r.location.as_deref().and_then(|l| Link::parse(l, Some(at.url())));
                    let Some(next) = next.filter(|_| hops <= self.plan.limits.get(L::Redirects)) else {
                        self.error("redirects");
                        self.kept.insert(src);
                        return Ok(());
                    };
                    // Every way a redirect ends short of a page is only a strike: a login, a
                    // maintenance page or a moved site redirects everything at once.
                    self.redirects += 1;
                    *self.redirect_targets.entry(next.hash64()).or_default() += 1;
                    match self.verdict(&next) {
                        Verdict::Allowed => {}
                        v => {
                            if v == Verdict::OutOfDomain {
                                self.stats.redirected_away += 1;
                            }
                            self.count(v);
                            self.kept.insert(src);
                            return Ok(());
                        }
                    }
                    // A redirect to a URL the crawl already has: the page is crawled there.
                    if !self.known.insert(next.hash64()) {
                        self.kept.insert(src);
                        return Ok(());
                    }
                    // Once a hop leaves the credential's origin, no later hop gets it.
                    clean &= self.on_auth_origin(&at);
                    at = next;
                }
                429 | 503 if r.retry_after.is_some() && retries < RETRIES => {
                    let wait = r.retry_after.as_deref().and_then(|v| fetch::retry_after(v, chrono::Utc::now())).unwrap_or(RETRY_AFTER_MAX);
                    if wait > RETRY_AFTER_MAX {
                        self.skipped_origins.insert(at.origin());
                        self.error(&r.status.to_string());
                        self.kept.insert(src);
                        return Ok(());
                    }
                    retries += 1;
                    self.next_at.insert(at.origin(), Instant::now() + wait);
                }
                200..=299 => break r,
                404 | 410 => {
                    self.gone.insert(src);
                    return Ok(());
                }
                s => {
                    self.error(if s >= 500 { "5xx" } else { "4xx" });
                    self.kept.insert(src);
                    return Ok(());
                }
            }
        };
        if !is_html(&resp.content_type, &resp.body) {
            // Not a page (roll indexes HTML only), or an error page served as text: a strike.
            self.error("unsupported_content_type");
            self.kept.insert(src);
            return Ok(());
        }
        self.index_page(src, at, depth, resp.body).await
    }

    /// Parses and indexes a page: the document under its canonical URL, its links queued.
    async fn index_page(&mut self, src: u64, at: Link, depth: u64, body: Vec<u8>) -> Result<(), Stop> {
        let mut held = self.sink.reserve();
        if let Err(e) = held.add(body.len() as u64 * 2, "crawled page") {
            return self.failed(src, &at, e.reason());
        }
        let text = String::from_utf8_lossy(&body).into_owned();
        drop(body);
        let tags = self.plan.domain(&at).map(|d| d.exclude_tags.clone()).unwrap_or_default();
        // Off the async worker, against a deadline: a page can't hold a core past it.
        let (url, limits) = (at.clone(), self.plan.limits.clone());
        let deadline = (Instant::now() + PAGE_PARSE_TIME).min(self.deadline.max(Instant::now() + Duration::from_secs(1)));
        let parsed = tokio::task::spawn_blocking(move || {
            let r = html::read_page(&text, held, &url, &tags, &limits, deadline);
            (r, text)
        })
        .await;
        let (result, text) = match parsed {
            Ok(r) => r,
            Err(_) => return self.failed(src, &at, "the page's parse stopped on an internal error"),
        };
        let (page, held) = match result {
            Ok(p) => p,
            Err(why) => return self.failed(src, &at, &why),
        };
        if !page.nofollow {
            for l in page.follow.iter().cloned() {
                if Instant::now() >= self.deadline {
                    break;
                }
                self.enqueue(l, depth + 1);
            }
        }
        if page.noindex {
            self.gone.insert(src);
            return Ok(());
        }
        // A canonical URL is honoured only once the crawl has fetched it and indexed it as
        // itself: then this page is one of its `additional_urls`, not a document of its own.
        // Until then the page is stored under its own URL, and the canonical one is queued.
        if let Some(c) = page.canonical.clone().filter(|c| c != &at && self.verdict(c) == Verdict::Allowed) {
            let entry = self.aliases.entry(c.hash64()).or_insert_with(|| (c.clone(), Vec::new()));
            if entry.1.len() < MAX_ALIASES && !entry.1.contains(&at) {
                entry.1.push(at.clone());
            }
            if self.indexed.contains(&c.hash64()) {
                self.gone.insert(src);
                return Ok(());
            }
            self.enqueue(c, depth);
        }
        self.indexed.insert(at.hash64());
        let primary = at;
        let doc = document(&primary, None, page, self.plan.full_html.then_some(text), &self.started_at);
        let id = primary.id();
        let mut page_buf = std::mem::replace(&mut self.page, self.sink.new_page());
        let pushed = self.sink.push_doc(&mut page_buf, &id, doc);
        self.page = page_buf;
        drop(held);
        pushed?;
        if self.page.sizes.len() >= PAGE_DOCS || self.page.bytes() >= PAGE_BYTES {
            self.flush().await?;
        }
        Ok(())
    }

    /// A page that couldn't be made a document (the breaker, nesting past 512 elements, its
    /// parse deadline): a failed document, kept.
    fn failed(&mut self, src: u64, at: &Link, why: &str) -> Result<(), Stop> {
        self.error("unparsed");
        self.sink.counters.failed += 1;
        self.sink.counters.metadata.insert("last_failed_document".into(), super::capped(format!("[{}] {why}", at.as_str())).into());
        self.kept.insert(src);
        Ok(())
    }

    /// The purge of a crawl that ended. Documents the crawl didn't refresh are fetched again
    /// (within its limits) and deleted when something proves them gone: a 404 or 410,
    /// `noindex`, a URL now excluded by the crawl's domains, rules or robots.txt. Errors (5xx,
    /// other 4xx, timeouts, network errors, an unreadable robots.txt) and signs that may only be
    /// an outage (a redirect off the crawl or onto a URL it already has, a body that isn't HTML)
    /// keep a document, until three full crawls in a row kept it. Nothing is deleted when more
    /// than half the index's documents weren't refreshed (gone, kept or unchecked: a site down,
    /// behind a login, in maintenance or wrongly deployed), or when most redirects led to one URL.
    /// The new cursor holds the starts of the last two full crawls.
    async fn purge(&mut self, cursor: &Value, before: u64) -> Result<Value, Stop> {
        let cluster = self.sink.cluster;
        let index = self.sink.index.clone();
        let mut crawls: Vec<String> = cursor.get("full_crawls").and_then(Value::as_array).map(|a| a.iter().filter_map(Value::as_str).map(str::to_string).collect()).unwrap_or_default();
        let strike_before = (crawls.len() >= 2).then(|| crawls[crawls.len() - 2].clone()).and_then(|t| ms_of(&t));
        let _ = internal::call(cluster, "POST", &format!("/{}/_refresh", enc(&index)), None).await;
        let mut delete: Vec<String> = Vec::new();
        let (mut gone, mut kept, mut struck, mut unchecked) = (0u64, 0u64, 0u64, 0u64);
        let mut recheck: Vec<(String, Link, Option<i64>)> = Vec::new();
        let max_recheck = self.plan.limits.usize(L::QueueSize);
        let listing = |e: crate::error::EsError| Stop::Failed(format!("listing [{index}] for the purge failed: {}", e.reason()));
        let first = obj! { "size" => 1000, "_source" => crate::arr!["url", "last_crawled_at"], "sort" => crate::arr!["_doc"], "query" => obj! { "range" => obj! { "last_crawled_at" => obj! { "lt" => self.started_at.clone() } } } };
        let mut page = internal::call(cluster, "POST", &format!("/{}/_search?scroll=2m", enc(&index)), Some(&first)).await.map_err(listing)?;
        let mut scroll = page.get("_scroll_id").and_then(Value::as_str).map(str::to_string);
        // What this crawl learnt of one stale document.
        let mut settle = |c: &Self, id: String, k: u64, last: Option<i64>, delete: &mut Vec<String>| {
            if c.gone.contains(&k) {
                gone += 1;
                delete.push(id);
            } else if c.kept.contains(&k) {
                kept += 1;
                if strike_before.is_some_and(|s| last.is_some_and(|l| l < s)) {
                    struck += 1;
                    delete.push(id);
                }
            }
        };
        loop {
            let hits = page.pointer(&["hits", "hits"]).and_then(Value::as_array).cloned().unwrap_or_default();
            if hits.is_empty() {
                break;
            }
            for h in hits {
                let Some(id) = h.get("_id").and_then(Value::as_str) else { continue };
                let Some(link) = h.pointer(&["_source", "url"]).and_then(Value::as_str).and_then(|u| Link::parse(u, None)) else { continue };
                let last = h.pointer(&["_source", "last_crawled_at"]).and_then(Value::as_str).and_then(ms_of);
                let k = link.hash64();
                if self.gone.contains(&k) || self.kept.contains(&k) {
                    settle(self, id.to_string(), k, last, &mut delete);
                } else if recheck.len() < max_recheck {
                    recheck.push((id.to_string(), link, last));
                } else {
                    unchecked += 1;
                }
            }
            self.sink.check().await?;
            let Some(sid) = scroll.clone() else { break };
            page = internal::call(cluster, "POST", "/_search/scroll", Some(&obj! { "scroll" => "2m", "scroll_id" => sid })).await.map_err(listing)?;
            if let Some(s) = page.get("_scroll_id").and_then(Value::as_str) {
                scroll = Some(s.to_string());
            }
        }
        if let Some(sid) = scroll {
            let _ = internal::call(cluster, "DELETE", "/_search/scroll", Some(&obj! { "scroll_id" => crate::arr![sid] })).await;
        }
        // The rest is fetched again, within the crawl's limits.
        for (id, link, last) in recheck {
            if self.visited >= self.budget || Instant::now() >= self.deadline {
                unchecked += 1;
                continue;
            }
            let k = link.hash64();
            match self.verdict(&link) {
                Verdict::Allowed => {
                    if !self.known.insert(k) {
                        unchecked += 1;
                        continue;
                    }
                    self.visit(link, self.plan.limits.get(L::Depth)).await?;
                    self.tick().await?;
                }
                Verdict::OutOfDomain | Verdict::RuleDenied | Verdict::RobotsDisallowed | Verdict::Limit => {
                    self.gone.insert(k);
                }
                Verdict::RobotsUnreachable => {
                    self.kept.insert(k);
                }
                Verdict::Blocked | Verdict::OriginSkipped => {
                    unchecked += 1;
                    continue;
                }
            }
            settle(self, id, k, last, &mut delete);
        }
        self.flush().await?;
        self.apply_aliases().await?;
        let not_refreshed = gone + kept + unchecked;
        let funnel = self.redirect_targets.values().max().copied().unwrap_or(0);
        let skip = if not_refreshed > 0 && not_refreshed * 2 > before {
            Some(format!("{not_refreshed} of the index's {before} documents weren't refreshed ({gone} gone, {kept} kept by errors or redirects, {unchecked} unchecked): nothing was deleted"))
        } else if self.redirects >= FUNNEL_MIN && funnel * 2 > self.redirects {
            Some(format!("{funnel} of {} redirects led to one URL, as a login or maintenance page does: nothing was deleted", self.redirects))
        } else {
            None
        };
        let m = &mut self.sink.counters.metadata;
        m.insert("gone_document_count".into(), (gone as i64).into());
        m.insert("kept_document_count".into(), (kept as i64).into());
        m.insert("purge_unchecked_count".into(), (unchecked as i64).into());
        m.insert("purge_skipped".into(), skip.is_some().into());
        match skip {
            Some(why) => {
                m.insert("purge_skipped_reason".into(), why.into());
            }
            None => {
                m.insert("deleted_after_three_kept_crawls_count".into(), (struck as i64).into());
                for chunk in delete.chunks(500) {
                    let mut body = String::new();
                    for id in chunk {
                        body.push_str(&obj! { "delete" => obj! { "_id" => id.clone() } }.to_json());
                        body.push('\n');
                    }
                    self.bulk(body).await?;
                }
            }
        }
        crawls.push(self.started_at.clone());
        let keep_from = crawls.len().saturating_sub(2);
        Ok(obj! { "full_crawls" => Value::Arr(crawls.split_off(keep_from).into_iter().map(Value::from).collect()) })
    }

    /// Canonical URLs the crawl fetched and indexed as themselves: the pages naming each go in
    /// its `additional_urls`, and the documents those pages got under their own URLs this crawl
    /// are deleted.
    async fn apply_aliases(&mut self) -> Result<(), Stop> {
        let mut body = String::new();
        for (k, (canonical, pages)) in std::mem::take(&mut self.aliases) {
            if !self.indexed.contains(&k) {
                continue;
            }
            let mut urls: Vec<String> = pages.iter().map(|p| p.as_str().to_string()).collect();
            urls.sort();
            body.push_str(&obj! { "update" => obj! { "_id" => canonical.id() } }.to_json());
            body.push('\n');
            body.push_str(&obj! { "doc" => obj! { "additional_urls" => Value::Arr(urls.into_iter().map(Value::from).collect()) } }.to_json());
            body.push('\n');
            for p in pages.iter().filter(|p| self.indexed.contains(&p.hash64())) {
                body.push_str(&obj! { "delete" => obj! { "_id" => p.id() } }.to_json());
                body.push('\n');
            }
        }
        if body.is_empty() { Ok(()) } else { self.bulk(body).await }
    }

    /// A `_bulk` of updates and deletes of the crawler's own documents, into its index (as
    /// `Sink::delete_unseen` sends its deletes); counts what it deleted.
    async fn bulk(&mut self, body: String) -> Result<(), Stop> {
        let index = self.sink.index.clone();
        let resp = internal::call_ndjson(self.sink.cluster, "POST", &format!("/{}/_bulk", enc(&index)), body).await.map_err(|e| Stop::Failed(format!("writing to [{index}] failed: {}", e.reason())))?;
        let n = resp.get("items").and_then(Value::as_array).map_or(0, |a| a.iter().filter(|i| i.pointer(&["delete", "status"]).and_then(Value::as_i64) == Some(200)).count());
        self.sink.counters.deleted += n as u64;
        self.sink.check().await
    }
}

fn ms_of(t: &str) -> Option<i64> {
    chrono::DateTime::parse_from_rfc3339(t).ok().map(|d| d.timestamp_millis())
}

/// A page's document, with Open Crawler's fields (`DocumentMapper#create_html_doc`); empty
/// values are left out, and a page's own fields never overwrite these.
fn document(url: &Link, additional: Option<String>, page: html::Page, full_html: Option<String>, started_at: &str) -> Map {
    let mut d = Map::new();
    let mut put = |k: &str, v: Value| {
        let empty = match &v {
            Value::Null => true,
            Value::Str(s) => s.is_empty(),
            Value::Arr(a) => a.is_empty(),
            _ => false,
        };
        if !empty {
            d.insert(k.into(), v);
        }
    };
    let strs = |v: Vec<String>| Value::Arr(v.into_iter().map(Value::from).collect());
    let u = url.url();
    put("id", url.id().into());
    put("last_crawled_at", started_at.into());
    put("title", page.title.into());
    put("body", page.body.into());
    put("meta_keywords", page.keywords.into());
    put("meta_description", page.description.into());
    put("links", strs(page.links));
    put("headings", strs(page.headings));
    put("full_html", full_html.map_or(Value::Null, Value::from));
    put("url", url.as_str().into());
    put("additional_urls", additional.map_or(Value::Null, |a| strs(vec![a])));
    put("url_scheme", u.scheme().into());
    put("url_host", u.host_str().unwrap_or_default().into());
    put("url_port", u.port_or_known_default().map_or(Value::Null, |p| Value::from(p as i64)));
    put("url_path", u.path().into());
    let dirs: Vec<&str> = u.path().split('/').collect();
    for (i, key) in ["url_path_dir1", "url_path_dir2", "url_path_dir3"].into_iter().enumerate() {
        put(key, dirs.get(i + 1).map_or(Value::Null, |s| Value::from(*s)));
    }
    for (k, v) in page.fields {
        if !d.contains_key(&k) {
            d.insert(k, v.into());
        }
    }
    d
}
