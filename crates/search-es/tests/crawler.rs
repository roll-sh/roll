//! roll's web crawler (`roll_web_crawler`, docs/search/crawler.md) against fixture sites the test
//! serves on loopback (`roll.connectors.allow_private_hosts`): a crawl end to end (robots.txt,
//! sitemaps, canonical URLs, nofollow, redirects off the crawl, the credential kept on its
//! origin, crawl rules, the purge through outages that redirect everything, the daily limit),
//! hostile sites (a compression bomb, a million-node page, an endless calendar, a sitemap
//! declaring entities), and one crawl without `allow_private_hosts`, refusing loopback.

use search_es::json::{Value, parse};
use search_es::xpack::entsearch::sync::crawler::url::Link;
use search_es::{Cluster, ClusterConfig, EsRequest, MemoryStore};
use std::collections::HashMap;
use std::io::Write;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

const SECRET: &str = "Bearer crawler-secret-4471";

#[derive(Clone)]
struct Resp {
    status: u16,
    headers: Vec<(String, String)>,
    body: Vec<u8>,
}

/// Requests a site got: (target, Authorization).
type Log = Arc<Mutex<Vec<(String, Option<String>)>>>;

/// A site on loopback: routes by request target, and the requests it got.
#[derive(Clone)]
struct Site {
    base: String,
    routes: Arc<Mutex<HashMap<String, Resp>>>,
    log: Log,
    /// Every request but robots.txt and the target itself redirects here (a login, maintenance).
    redirect_all: Arc<Mutex<Option<String>>>,
}

impl Site {
    async fn start() -> Site {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let site = Site { base, routes: Arc::default(), log: Arc::default(), redirect_all: Arc::default() };
        let s = site.clone();
        tokio::spawn(async move {
            loop {
                let Ok((mut sock, _)) = listener.accept().await else { return };
                let s = s.clone();
                tokio::spawn(async move {
                    let mut buf = Vec::new();
                    let mut chunk = [0u8; 4096];
                    while !buf.windows(4).any(|w| w == b"\r\n\r\n") && buf.len() < 65536 {
                        match sock.read(&mut chunk).await {
                            Ok(0) | Err(_) => return,
                            Ok(n) => buf.extend_from_slice(&chunk[..n]),
                        }
                    }
                    let head = String::from_utf8_lossy(&buf).into_owned();
                    let target = head.split_whitespace().nth(1).unwrap_or("/").to_string();
                    let auth = head.lines().find_map(|l| l.split_once(':').filter(|(k, _)| k.eq_ignore_ascii_case("authorization")).map(|(_, v)| v.trim().to_string()));
                    s.log.lock().unwrap().push((target.clone(), auth));
                    let away = s.redirect_all.lock().unwrap().clone().filter(|to| target != "/robots.txt" && !to.ends_with(&target));
                    let r = match away {
                        Some(to) => Resp { status: 302, headers: vec![("Location".into(), to)], body: vec![] },
                        None => s.routes.lock().unwrap().get(&target).cloned().unwrap_or(Resp { status: 404, headers: vec![], body: b"no".to_vec() }),
                    };
                    let mut out = format!("HTTP/1.1 {} X\r\nContent-Length: {}\r\nConnection: close\r\n", r.status, r.body.len());
                    for (k, v) in &r.headers {
                        out.push_str(&format!("{k}: {v}\r\n"));
                    }
                    out.push_str("\r\n");
                    let _ = sock.write_all(out.as_bytes()).await;
                    let _ = sock.write_all(&r.body).await;
                });
            }
        });
        site
    }

    fn url(&self, path: &str) -> String {
        format!("{}{path}", self.base)
    }

    fn set(&self, path: &str, status: u16, headers: &[(&str, &str)], body: impl Into<Vec<u8>>) {
        let headers = headers.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect();
        self.routes.lock().unwrap().insert(path.into(), Resp { status, headers, body: body.into() });
    }

    fn page(&self, path: &str, html: &str) {
        self.set(path, 200, &[("Content-Type", "text/html; charset=utf-8")], html);
    }

    fn redirect(&self, path: &str, to: &str) {
        self.set(path, 302, &[("Location", to)], "");
    }

    fn hits(&self, path: &str) -> usize {
        self.log.lock().unwrap().iter().filter(|(p, _)| p == path).count()
    }

    fn requests(&self) -> Vec<(String, Option<String>)> {
        self.log.lock().unwrap().clone()
    }
}

async fn cluster(settings: &[(&str, &str)]) -> Arc<Cluster> {
    use search_es::xpack::entsearch::sync::slots::{NODE_MAX, OWNER_MAX, Slots};
    let mut cfg = ClusterConfig::new("crawler");
    cfg.connector_slots = Some(Slots::new(NODE_MAX, OWNER_MAX));
    cfg.sealer = search_es::seal::Sealer::from_secret("0123456789abcdef0123456789abcdef-crawler");
    for (k, v) in [
        ("roll.connectors.allow_private_hosts", "true"),
        ("roll.connectors.max_docs_per_second", "100000"),
        ("roll.connectors.max_bytes_per_second", "1073741824"),
        ("roll.crawler.min_request_interval", "1ms"),
        ("roll.crawler.unverified.min_request_interval", "1ms"),
    ]
    .into_iter()
    .chain(settings.iter().copied())
    {
        cfg.node_settings.push((k.into(), v.into()));
    }
    let c = Arc::new(Cluster::open(cfg, Arc::new(MemoryStore::new())).await.unwrap());
    c.enable_background_tasks();
    c
}

async fn call(c: &Cluster, method: &str, uri: &str, body: Option<&str>) -> (u16, Value, String) {
    let mut r = EsRequest::new(method, uri);
    if let Some(b) = body {
        r.headers.push(("content-type".into(), "application/json".into()));
        r.body = b.to_string().into();
    }
    let resp = search_es::handle(c, r).await;
    let text = String::from_utf8_lossy(&resp.body).into_owned();
    (resp.status, parse(&resp.body).ok().flatten().unwrap_or(Value::Null), text)
}

fn at<'a>(v: &'a Value, path: &str) -> &'a Value {
    path.split('.').fold(v, |cur, p| match cur {
        Value::Arr(a) => p.parse::<usize>().ok().and_then(|i| a.get(i)).unwrap_or(&Value::Null),
        other => other.get(p).unwrap_or(&Value::Null),
    })
}

fn num(v: &Value, path: &str) -> i64 {
    at(v, path).as_i64().unwrap_or(-1)
}

/// A crawler connector with these configuration `values` and advanced `snippet`, validated and
/// active.
async fn crawler(c: &Cluster, values: &str, snippet: Option<&str>) {
    let (s, _, t) = call(c, "PUT", "/_connector/site", Some(r#"{"index_name":"content-site","is_native":true,"service_type":"roll_web_crawler"}"#)).await;
    assert_eq!(s, 201, "{t}");
    let (s, _, t) = call(c, "PUT", "/_connector/site/_configuration", Some(&format!(r#"{{"values":{values}}}"#))).await;
    assert_eq!(s, 200, "{t}");
    let Some(snippet) = snippet else { return };
    let (s, _, t) = call(c, "PUT", "/_connector/site/_filtering", Some(&format!(r#"{{"advanced_snippet":{{"value":{snippet}}}}}"#))).await;
    assert_eq!(s, 200, "{t}");
    let deadline = std::time::Instant::now() + Duration::from_secs(20);
    loop {
        let (_, v, _) = call(c, "GET", "/_connector/site", None).await;
        match at(&v, "filtering.0.draft.validation.state").as_str() {
            Some("valid") => break,
            Some("invalid") => panic!("{v:?}"),
            _ => {}
        }
        assert!(std::time::Instant::now() < deadline, "{v:?}");
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert_eq!(call(c, "PUT", "/_connector/site/_filtering/_activate", None).await.0, 200);
}

/// Runs a full crawl to its end; the job.
async fn crawl(c: &Cluster) -> Value {
    let (s, v, t) = call(c, "POST", "/_connector/_sync_job", Some(r#"{"id":"site","job_type":"full"}"#)).await;
    assert_eq!(s, 201, "{t}");
    let id = at(&v, "id").as_str().unwrap().to_string();
    let deadline = std::time::Instant::now() + Duration::from_secs(120);
    loop {
        let (_, v, _) = call(c, "GET", &format!("/_connector/_sync_job/{id}"), None).await;
        if matches!(at(&v, "status").as_str(), Some("completed" | "error" | "canceled")) {
            call(c, "POST", "/content-site/_refresh", None).await;
            return v;
        }
        assert!(std::time::Instant::now() < deadline, "the crawl didn't finish: {v:?}");
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

async fn doc(c: &Cluster, url: &str) -> Option<Value> {
    let id = Link::parse(url, None).unwrap().id();
    let (s, v, _) = call(c, "GET", &format!("/content-site/_doc/{id}"), None).await;
    (s == 200).then(|| at(&v, "_source").clone())
}

fn gzip(bytes: &[u8]) -> Vec<u8> {
    let mut e = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::fast());
    e.write_all(bytes).unwrap();
    e.finish().unwrap()
}

#[tokio::test]
async fn a_site_crawled_end_to_end() {
    let (a, b, off) = (Site::start().await, Site::start().await, Site::start().await);
    a.set("/robots.txt", 200, &[], format!("User-agent: *\nDisallow: /private\nSitemap: {}\n", a.url("/sitemap_index.xml")));
    a.set("/sitemap_index.xml", 200, &[("Content-Type", "application/xml")], format!("<sitemapindex><sitemap><loc>{}</loc></sitemap></sitemapindex>", a.url("/sitemap.xml.gz")));
    a.set("/sitemap.xml.gz", 200, &[("Content-Type", "application/gzip")], gzip(format!("<urlset><url><loc>{}</loc></url><url><loc>{}</loc></url></urlset>", a.url("/from-sitemap"), off.url("/elsewhere")).as_bytes()));
    a.page(
        "/",
        r#"<html><head><title>Home</title></head><body><header>site nav</header><p>welcome home</p>
        <a href="/a">a</a><a href="/canon">canon</a><a href="/dup">dup</a><a href="/nofollow-page">nf</a><a href="/noindex">ni</a><a href="/blog/post">blog</a>
        <a href="/private/x">private</a><a href="/off">off</a><a href="/to-b">b</a><a rel="nofollow" href="/hidden">hidden</a></body></html>"#,
    );
    a.page("/a", "<title>Page A</title><h1>Alpha</h1><p>alpha text</p>");
    let canonical = format!(r#"<link rel="canonical" href="{}"><title>Canon</title><p>the canonical body</p>"#, a.url("/canon"));
    a.page("/dup", &canonical);
    a.page("/canon", &canonical);
    a.page("/nofollow-page", r#"<meta name="robots" content="nofollow"><p>nofollow page</p><a href="/never">never</a>"#);
    a.page("/noindex", r#"<meta name="robots" content="noindex"><a href="/via-noindex">next</a>"#);
    a.page("/via-noindex", "<p>found through a noindex page</p>");
    a.page("/from-sitemap", "<p>only in the sitemap</p>");
    a.redirect("/off", &off.url("/x"));
    a.redirect("/to-b", &b.url("/landing"));
    b.page("/landing", "<p>bee landing</p>");

    let c = cluster(&[("roll.crawler.unverified.max_crawls_per_day", "8")]).await;
    let values = format!(r#"{{"urls":"{}","auth_origin":"{}","auth_header":"{SECRET}"}}"#, a.url("/"), a.base);
    let snippet = format!(r#"{{"domains":[{{"url":"{}","exclude_tags":["header"],"crawl_rules":[{{"policy":"deny","type":"begins","pattern":"/blog"}}]}},{{"url":"{}"}}]}}"#, a.base, b.base);
    crawler(&c, &values, Some(&snippet)).await;
    let (_, conn, _) = call(&c, "GET", "/_connector/site", None).await;
    assert_eq!(at(&conn, "configuration.auth_header.value").as_str(), Some("********"));

    // The first crawl.
    let job = crawl(&c).await;
    assert_eq!(at(&job, "status").as_str(), Some("completed"), "{job:?}");
    assert_eq!(num(&job, "indexed_document_count"), 7, "/, /a, /canon, /nofollow-page, /via-noindex, /from-sitemap, B's /landing: {job:?}");
    assert_eq!((num(&job, "metadata.redirected_away_count"), num(&job, "metadata.crawl_rule_excluded_count")), (1, 1), "{job:?}");
    for path in ["/never", "/hidden", "/blog/post", "/private/x"] {
        assert_eq!(a.hits(path), 0, "{path} isn't fetched");
    }
    assert!(off.requests().is_empty(), "nothing off the crawl is fetched: {:?}", off.requests());
    assert!(doc(&c, &a.url("/noindex")).await.is_none());
    let home = doc(&c, &a.url("/")).await.unwrap();
    assert_eq!(at(&home, "title").as_str(), Some("Home"));
    assert!(at(&home, "body").as_str().is_some_and(|b| b.contains("welcome home") && !b.contains("site nav")), "{home:?}");
    assert_eq!((at(&home, "url_scheme").as_str(), num(&home, "url_port")), (Some("http"), a.base.rsplit(':').next().unwrap().parse().unwrap()));
    // /dup names /canon, which the crawl fetched as itself: /dup is one of its additional URLs.
    let canon = doc(&c, &a.url("/canon")).await.unwrap();
    assert_eq!(at(&canon, "additional_urls.0").as_str(), Some(a.url("/dup").as_str()), "{canon:?}");
    assert!(doc(&c, &a.url("/dup")).await.is_none());
    let (_, m, _) = call(&c, "GET", "/content-site/_mapping", None).await;
    assert_eq!((at(&m, "content-site.mappings.properties.url.type").as_str(), at(&m, "content-site.mappings.properties.last_crawled_at.type").as_str()), (Some("keyword"), Some("date")));
    // The credential went to its origin only: never to B, which A redirected to.
    assert!(a.requests().iter().any(|(p, h)| p == "/" && h.as_deref() == Some(SECRET)));
    assert!(b.requests().iter().all(|(_, h)| h.is_none()), "{:?}", b.requests());
    let (_, jobs, text) = call(&c, "GET", "/_connector/_sync_job", None).await;
    assert!(!text.contains("crawler-secret-4471") && !format!("{:?}", c.state().metadata.xpack).contains("crawler-secret-4471"), "{jobs:?}");

    // A 503 window keeps /a; a 404 and a page robots.txt now disallows are deleted.
    a.set("/a", 503, &[], "busy");
    a.set("/from-sitemap", 404, &[], "gone");
    a.set("/robots.txt", 200, &[], format!("User-agent: *\nDisallow: /private\nDisallow: /via-noindex\nSitemap: {}\n", a.url("/sitemap_index.xml")));
    let job = crawl(&c).await;
    assert_eq!((at(&job, "status").as_str(), num(&job, "deleted_document_count"), num(&job, "metadata.kept_document_count")), (Some("completed"), 2, 1), "{job:?}");
    assert!(doc(&c, &a.url("/a")).await.is_some() && doc(&c, &a.url("/from-sitemap")).await.is_none() && doc(&c, &a.url("/via-noindex")).await.is_none());
    // Kept through three full crawls in a row, /a is deleted on the third.
    let job = crawl(&c).await;
    assert_eq!(num(&job, "deleted_document_count"), 0, "{job:?}");
    let job = crawl(&c).await;
    assert_eq!((num(&job, "deleted_document_count"), num(&job, "metadata.deleted_after_three_kept_crawls_count")), (1, 1), "{job:?}");
    assert!(doc(&c, &a.url("/a")).await.is_none());

    // An expired login: every page redirects off the crawl. Three crawls in a row would strike
    // every document out; the purge is skipped each time, since most weren't refreshed.
    *a.redirect_all.lock().unwrap() = Some(off.url("/login"));
    for _ in 0..3 {
        let job = crawl(&c).await;
        assert_eq!((at(&job, "status").as_str(), at(&job, "metadata.purge_skipped"), num(&job, "deleted_document_count")), (Some("completed"), &Value::Bool(true), 0), "{job:?}");
    }
    assert!(doc(&c, &a.url("/")).await.is_some() && doc(&c, &a.url("/canon")).await.is_some());
    assert!(off.requests().is_empty());
    // Maintenance: every page redirects to one page of the site.
    a.page("/maintenance", "<p>back soon</p>");
    *a.redirect_all.lock().unwrap() = Some(a.url("/maintenance"));
    let job = crawl(&c).await;
    assert_eq!((at(&job, "metadata.purge_skipped"), num(&job, "deleted_document_count")), (&Value::Bool(true), 0), "{job:?}");
    assert!(doc(&c, &a.url("/")).await.is_some() && doc(&c, &a.url("/nofollow-page")).await.is_some());
    // An unverified domain's daily limit.
    let job = crawl(&c).await;
    assert!(at(&job, "error").as_str().is_some_and(|e| e.contains("at most 8 times")), "{job:?}");
}

/// Without `allow_private_hosts`, a start URL on loopback is refused before any request.
#[tokio::test]
async fn a_private_address_is_never_fetched() {
    let site = Site::start().await;
    site.page("/", "<p>internal</p>");
    let c = cluster(&[("roll.connectors.allow_private_hosts", "false")]).await;
    crawler(&c, &format!(r#"{{"urls":"{}"}}"#, site.url("/")), None).await;
    let job = crawl(&c).await;
    assert_eq!((at(&job, "status").as_str(), num(&job, "indexed_document_count"), num(&job, "metadata.blocked_url_count")), (Some("completed"), 0, 1), "{job:?}");
    assert!(site.requests().is_empty(), "{:?}", site.requests());
}

#[tokio::test]
async fn a_compression_bomb_stops_at_the_response_cap() {
    let site = Site::start().await;
    site.set("/", 200, &[("Content-Type", "text/html"), ("Content-Encoding", "gzip")], gzip(&vec![b' '; 64 << 20]));
    let c = cluster(&[]).await;
    crawler(&c, &format!(r#"{{"urls":"{}"}}"#, site.url("/")), None).await;
    let job = crawl(&c).await;
    assert_eq!((at(&job, "status").as_str(), num(&job, "indexed_document_count"), num(&job, "metadata.error_count.too_large")), (Some("completed"), 0, 1), "{job:?}");
}

#[tokio::test]
async fn a_million_node_page_trips_the_breaker_not_the_node() {
    let site = Site::start().await;
    site.page("/", r#"<p>small</p><a href="/big">big</a>"#);
    site.page("/big", &"<p>".repeat(1_000_000));
    let c = cluster(&[]).await;
    let (s, _, t) = call(&c, "PUT", "/_cluster/settings", Some(r#"{"persistent":{"indices.breaker.request.limit":"16mb"}}"#)).await;
    assert_eq!(s, 200, "{t}");
    crawler(&c, &format!(r#"{{"urls":"{}"}}"#, site.url("/")), None).await;
    let job = crawl(&c).await;
    assert_eq!((at(&job, "status").as_str(), num(&job, "indexed_document_count"), num(&job, "metadata.failed_document_count")), (Some("completed"), 1, 1), "{job:?}");
    assert!(at(&job, "metadata.last_failed_document").as_str().is_some_and(|e| e.contains("Data too large")), "{job:?}");
}

#[tokio::test]
async fn an_endless_calendar_ends_at_the_url_limit() {
    let site = Site::start().await;
    for d in 0..200 {
        site.page(&format!("/cal?d={d}"), &format!(r#"<a href="/cal?d={}">next</a><a href="/cal?d={d}&amp;view=week">week</a>"#, d + 1));
    }
    let c = cluster(&[]).await;
    crawler(&c, &format!(r#"{{"urls":"{}","max_unique_url_count":25,"max_crawl_depth":50}}"#, site.url("/cal?d=0")), None).await;
    let job = crawl(&c).await;
    assert_eq!((at(&job, "status").as_str(), at(&job, "metadata.limit_reached").as_str()), (Some("completed"), Some("max_unique_url_count")), "{job:?}");
    assert_eq!(site.requests().iter().filter(|(p, _)| p.starts_with("/cal")).count(), 25);
}

#[tokio::test]
async fn a_sitemap_declaring_entities_isnt_read() {
    let site = Site::start().await;
    site.set("/robots.txt", 200, &[], format!("Sitemap: {}\n", site.url("/s.xml")));
    let laughs = format!(
        r#"<?xml version="1.0"?><!DOCTYPE urlset [<!ENTITY a "{}"><!ENTITY b "&a;&a;&a;&a;&a;&a;&a;&a;&a;&a;"><!ENTITY c "&b;&b;&b;&b;&b;&b;&b;&b;&b;&b;">]><urlset><url><loc>{}</loc></url><url><loc>{}/&c;</loc></url></urlset>"#,
        "x".repeat(100),
        site.url("/from-entity-sitemap"),
        site.base
    );
    site.set("/s.xml", 200, &[("Content-Type", "application/xml")], laughs);
    site.page("/", "<p>home</p>");
    let c = cluster(&[]).await;
    crawler(&c, &format!(r#"{{"urls":"{}"}}"#, site.url("/")), None).await;
    let job = crawl(&c).await;
    assert_eq!((at(&job, "status").as_str(), num(&job, "indexed_document_count"), num(&job, "metadata.sitemap_error_count")), (Some("completed"), 1, 1), "{job:?}");
    assert_eq!(site.hits("/from-entity-sitemap"), 0);
}
