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
/// Each request's target, `If-None-Match` and `If-Modified-Since`.
type Conditional = Arc<Mutex<Vec<(String, Option<String>, Option<String>)>>>;

/// A site on loopback: routes by request target, and the requests it got.
#[derive(Clone)]
struct Site {
    base: String,
    routes: Arc<Mutex<HashMap<String, Resp>>>,
    log: Log,
    /// Every request but robots.txt and the target itself redirects here (a login, maintenance).
    redirect_all: Arc<Mutex<Option<String>>>,
    conditional: Conditional,
}

impl Site {
    async fn start() -> Site {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let site = Site { base, routes: Arc::default(), log: Arc::default(), redirect_all: Arc::default(), conditional: Arc::default() };
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
                    let header = |name: &str| head.lines().find_map(|l| l.split_once(':').filter(|(k, _)| k.eq_ignore_ascii_case(name)).map(|(_, v)| v.trim().to_string()));
                    let auth = header("authorization");
                    s.log.lock().unwrap().push((target.clone(), auth));
                    let away = s.redirect_all.lock().unwrap().clone().filter(|to| target != "/robots.txt" && !to.ends_with(&target));
                    let mut r = match away {
                        Some(to) => Resp { status: 302, headers: vec![("Location".into(), to)], body: vec![] },
                        None => s.routes.lock().unwrap().get(&target).cloned().unwrap_or(Resp { status: 404, headers: vec![], body: b"no".to_vec() }),
                    };
                    // Conditional requests, as a server with validators answers them.
                    let has = |k: &str, v: &Option<String>| v.is_some() && r.headers.iter().any(|(h, hv)| h.eq_ignore_ascii_case(k) && Some(hv) == v.as_ref());
                    let (inm, ims) = (header("if-none-match"), header("if-modified-since"));
                    s.conditional.lock().unwrap().push((target.clone(), inm.clone(), ims.clone()));
                    if r.status == 200 && (has("etag", &inm) || (inm.is_none() && has("last-modified", &ims))) {
                        r = Resp { status: 304, headers: vec![], body: vec![] };
                    }
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
    let mut cfg = ClusterConfig::new("crawler");
    // Its own slots: the tests' clusters share a name, and the node's pool keys on it.
    cfg.connector_slots = Some(search_es::slots::Slots::new(search_es::xpack::entsearch::sync::SLOTS));
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
    if let Some(snippet) = snippet {
        set_snippet(c, snippet).await;
    }
}

/// Puts `snippet` as the draft advanced snippet, waits for roll to validate it, and activates it.
async fn set_snippet(c: &Cluster, snippet: &str) {
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
    crawl_as(c, "full").await
}

/// Runs a crawl of `job_type` (`full`, `incremental`) to its end; the job.
async fn crawl_as(c: &Cluster, job_type: &str) -> Value {
    let (s, v, t) = call(c, "POST", "/_connector/_sync_job", Some(&format!(r#"{{"id":"site","job_type":"{job_type}"}}"#))).await;
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

    let c = cluster(&[("roll.crawler.unverified.max_crawls_per_day", "8"), ("roll.crawler.strike_min_age", "1ms")]).await;
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

/// A sitemap of 50,000 pages against a frontier of 10: the rest are dropped and counted without
/// being checked one by one.
#[tokio::test]
async fn a_huge_sitemap_stops_at_a_full_frontier() {
    let site = Site::start().await;
    site.set("/robots.txt", 200, &[], format!("Sitemap: {}\n", site.url("/s.xml")));
    let locs: String = (0..50_000).map(|i| format!("<url><loc>{}</loc></url>", site.url(&format!("/p{i}")))).collect();
    site.set("/s.xml", 200, &[("Content-Type", "application/xml")], format!("<urlset>{locs}</urlset>"));
    site.page("/", "<p>home</p>");
    let c = cluster(&[]).await;
    crawler(&c, &format!(r#"{{"urls":"{}","max_unique_url_count":5}}"#, site.url("/")), Some(r#"{"url_queue_size_limit":10}"#)).await;
    let job = crawl(&c).await;
    assert_eq!(at(&job, "status").as_str(), Some("completed"), "{job:?}");
    // The work: once the frontier is full, the rest is counted, never checked against the rules.
    assert!(num(&job, "metadata.dropped_link_count") >= 49_000 && num(&job, "metadata.checked_url_count") < 100, "{job:?}");
}

/// The purge's guards and strikes across two domains: one domain 404 everywhere keeps its
/// documents (per origin); a URL that moved, and an empty page, take strikes and go after three
/// crawls; most of a site failing keeps everything, aliases included; `Disallow: /` is a strike,
/// not proof; and `force_purge` deletes past the guards once, then turns itself off.
#[tokio::test]
async fn purge_guards_and_strikes() {
    let (a, b) = (Site::start().await, Site::start().await);
    let links: String = ["/a1", "/a2", "/a3", "/a4", "/a5", "/a6", "/a7", "/a8", "/old", "/e", "/dup", "/dup2"].iter().map(|l| format!(r#"<a href="{l}">x</a>"#)).collect();
    a.page("/", &format!("<p>home</p>{links}"));
    for i in 1..=8 {
        a.page(&format!("/a{i}"), &format!("<p>page {i}</p>"));
    }
    a.page("/old", "<p>old</p>");
    a.page("/e", "<p>e</p>");
    a.page("/dup", &format!(r#"<link rel="canonical" href="{}"><p>home again</p>"#, a.url("/")));
    a.page("/dup2", &format!(r#"<link rel="canonical" href="{}"><p>two</p>"#, a.url("/c2")));
    a.page("/c2", "<p>two</p>");
    let b_pages = |b: &Site| {
        b.page("/", r#"<p>b</p><a href="/b1">1</a><a href="/b2">2</a>"#);
        b.page("/b1", "<p>b1</p>");
        b.page("/b2", "<p>b2</p>");
    };
    b_pages(&b);
    let c = cluster(&[("roll.crawler.unverified.max_crawls_per_day", "10"), ("roll.crawler.strike_min_age", "1ms")]).await;
    crawler(&c, &format!(r#"{{"urls":"{}"}}"#, a.url("/")), Some(&format!(r#"{{"domains":[{{"url":"{}"}},{{"url":"{}"}}]}}"#, a.base, b.base))).await;

    // 1: canonical URLs. /dup names the home page, indexed first; /dup2 names /c2, fetched after it.
    let job = crawl(&c).await;
    assert_eq!((at(&job, "status").as_str(), num(&job, "metadata.alias_deleted_count")), (Some("completed"), 1), "{job:?}");
    assert!(doc(&c, &a.url("/dup")).await.is_none() && doc(&c, &a.url("/dup2")).await.is_none());
    assert_eq!(at(&doc(&c, &a.url("/c2")).await.unwrap(), "additional_urls.0").as_str(), Some(a.url("/dup2").as_str()));
    assert_eq!(at(&doc(&c, &a.url("/")).await.unwrap(), "additional_urls.0").as_str(), Some(a.url("/dup").as_str()));

    // 2: B is 404 everywhere: its documents stay (per origin). /old moved, /e is empty: strikes.
    for p in ["/", "/b1", "/b2"] {
        b.set(p, 404, &[], "gone");
    }
    a.set("/old", 301, &[("Location", "/new")], "");
    a.page("/new", "<p>new</p>");
    a.page("/e", "<html></html>");
    let job = crawl(&c).await;
    assert_eq!((at(&job, "metadata.purge_skipped"), at(&job, "metadata.purge_skipped_origins.0").as_str()), (&Value::Bool(false), Some(b.base.as_str())), "{job:?}");
    assert!(doc(&c, &b.url("/b1")).await.is_some() && doc(&c, &a.url("/old")).await.is_some() && doc(&c, &a.url("/e")).await.is_some() && doc(&c, &a.url("/new")).await.is_some());

    // 3: most of A fails: nothing is deleted, the alias's own document included.
    b_pages(&b);
    for i in 1..=8 {
        a.set(&format!("/a{i}"), 503, &[], "busy");
    }
    let job = crawl(&c).await;
    assert_eq!((at(&job, "metadata.purge_skipped"), num(&job, "metadata.alias_deleted_count")), (&Value::Bool(true), 0), "{job:?}");
    assert!(doc(&c, &a.url("/dup2")).await.is_some());

    // 4: `Disallow: /` keeps every document of A as a strike, none as gone.
    for i in 1..=8 {
        a.page(&format!("/a{i}"), &format!("<p>page {i}</p>"));
    }
    a.set("/robots.txt", 200, &[], "User-agent: *\nDisallow: /\n");
    let job = crawl(&c).await;
    assert_eq!(num(&job, "metadata.gone_document_count"), 0, "{job:?}");
    assert!(num(&job, "metadata.kept_document_count") >= 13, "{job:?}");
    assert!(doc(&c, &a.url("/a1")).await.is_some());

    // 5: A back: /old and /e were kept by crawls 2 to 5, and go.
    a.set("/robots.txt", 404, &[], "");
    let job = crawl(&c).await;
    assert_eq!(num(&job, "metadata.deleted_after_three_kept_crawls_count"), 2, "{job:?}");
    assert!(doc(&c, &a.url("/old")).await.is_none() && doc(&c, &a.url("/e")).await.is_none() && doc(&c, &a.url("/new")).await.is_some());

    // 6: B gone for good: `force_purge` deletes past the per-origin guard once, then turns off.
    for p in ["/", "/b1", "/b2"] {
        b.set(p, 404, &[], "gone");
    }
    assert_eq!(call(&c, "PUT", "/_connector/site/_configuration", Some(r#"{"values":{"force_purge":true}}"#)).await.0, 200);
    let job = crawl(&c).await;
    assert_eq!(at(&job, "metadata.purge_forced"), &Value::Bool(true), "{job:?}");
    assert!(doc(&c, &b.url("/b1")).await.is_none());
    let (_, conn, _) = call(&c, "GET", "/_connector/site", None).await;
    assert_eq!(at(&conn, "configuration.force_purge.value"), &Value::Bool(false), "{conn:?}");
}

/// A domain taken out of the crawl is purged even when the crawl used its whole budget (its
/// documents are decided as they are listed, fetching nothing), and a page kept through three
/// crawls in a row stays while it is less than a week old (`roll.crawler.strike_min_age`).
#[tokio::test]
async fn removed_domains_go_and_strikes_wait_a_week() {
    let (a, b) = (Site::start().await, Site::start().await);
    a.page("/", r#"<p>a</p><a href="/a1">1</a><a href="/a2">2</a>"#);
    a.page("/a1", "<p>a1</p>");
    a.page("/a2", "<p>a2</p>");
    b.page("/", r#"<p>b</p><a href="/b1">1</a><a href="/b2">2</a>"#);
    b.page("/b1", "<p>b1</p>");
    b.page("/b2", "<p>b2</p>");
    let c = cluster(&[]).await;
    crawler(&c, &format!(r#"{{"urls":"{}"}}"#, a.url("/")), Some(&format!(r#"{{"domains":[{{"url":"{}"}},{{"url":"{}"}}]}}"#, a.base, b.base))).await;
    assert_eq!(num(&crawl(&c).await, "indexed_document_count"), 6);
    a.set("/a1", 503, &[], "busy");
    for _ in 0..2 {
        crawl(&c).await;
    }
    // B leaves the crawl, and the budget (2 URLs) ends the crawl before anything is re-checked.
    set_snippet(&c, &format!(r#"{{"domains":[{{"url":"{}"}}]}}"#, a.base)).await;
    assert_eq!(call(&c, "PUT", "/_connector/site/_configuration", Some(r#"{"values":{"max_unique_url_count":2}}"#)).await.0, 200);
    let job = crawl(&c).await;
    assert_eq!((at(&job, "metadata.limit_reached").as_str(), at(&job, "metadata.purge_skipped")), (Some("max_unique_url_count"), &Value::Bool(false)), "{job:?}");
    assert_eq!((num(&job, "metadata.excluded_document_count"), num(&job, "metadata.deleted_after_three_kept_crawls_count")), (3, 0), "{job:?}");
    assert!(doc(&c, &b.url("/b1")).await.is_none() && doc(&c, &a.url("/a1")).await.is_some());
}

/// A request whose turn would come after the crawl's deadline isn't made: a `Crawl-delay` of 30 s
/// and `max_duration` of 1 s fetch robots.txt and nothing else, and the origin is left at once
/// rather than slept on.
#[tokio::test]
async fn no_request_after_the_deadline() {
    let site = Site::start().await;
    site.set("/robots.txt", 200, &[], "User-agent: *\nCrawl-delay: 30\n");
    site.page("/", "<p>home</p>");
    let c = cluster(&[]).await;
    crawler(&c, &format!(r#"{{"urls":"{}"}}"#, site.url("/")), Some(r#"{"max_duration":1}"#)).await;
    let job = crawl(&c).await;
    assert_eq!((at(&job, "status").as_str(), num(&job, "metadata.skipped_origin_url_count")), (Some("completed"), 1), "{job:?}");
    assert_eq!(site.requests().iter().map(|(p, _)| p.as_str()).collect::<Vec<_>>(), ["/robots.txt"]);
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

/// Extraction rules (Open Crawler's `extraction_rulesets`): on pages a ruleset's URL filter takes,
/// a CSS selector's text as a string or an array, a `set` value when a selector finds anything,
/// and a group of the URL; pages the filter doesn't take get none of it, and no rule replaces the
/// fields every page has.
#[tokio::test]
async fn extraction_rules_fill_fields() {
    let site = Site::start().await;
    site.page("/", r#"<title>Home</title><a href="/blog/first-post">post</a> <a href="/about">about</a>"#);
    let post = r#"<title>First post</title><article><h1>First post</h1><p class="byline">by <span class="author">Ada</span></p>
        <ul class="tags"><li>rust</li><li class="hidden">draft</li><li>search</li></ul><p>Text.</p></article>"#;
    site.page("/blog/first-post", post);
    site.page("/about", r#"<title>About</title><article><p class="byline"><span class="author">Not a post</span></p></article>"#);
    let c = cluster(&[]).await;
    let snippet = format!(
        r#"{{"domains":[{{"url":"{}","extraction_rulesets":[{{"url_filters":[{{"type":"begins","pattern":"/blog"}}],"rules":[
            {{"action":"extract","field_name":"author","selector":"article .byline > span.author","join_as":"string","source":"html"}},
            {{"action":"extract","field_name":"tags","selector":"ul.tags li:not(.hidden)","join_as":"array","source":"html"}},
            {{"action":"set","field_name":"section","selector":"article","value":"blog","join_as":"string","source":"html"}},
            {{"action":"extract","field_name":"slug","selector":"/blog/([a-z-]+)","join_as":"string","source":"url"}}
        ]}}]}}]}}"#,
        site.url("").trim_end_matches('/')
    );
    crawler(&c, &format!(r#"{{"urls":"{}"}}"#, site.url("/")), Some(&snippet)).await;
    let job = crawl(&c).await;
    assert_eq!((at(&job, "status").as_str(), num(&job, "indexed_document_count")), (Some("completed"), 3), "{job:?}");
    let p = doc(&c, &site.url("/blog/first-post")).await.expect("the post is indexed");
    assert_eq!(at(&p, "author").as_str(), Some("Ada"), "{p:?}");
    assert_eq!(at(&p, "tags").as_array().map(|a| a.iter().filter_map(Value::as_str).collect::<Vec<_>>()), Some(vec!["rust", "search"]), "{p:?}");
    assert_eq!((at(&p, "section").as_str(), at(&p, "slug").as_str(), at(&p, "title").as_str()), (Some("blog"), Some("first-post"), Some("First post")), "{p:?}");
    let about = doc(&c, &site.url("/about")).await.expect("about is indexed");
    assert!(about.get("author").is_none() && about.get("section").is_none(), "the filter takes /blog only: {about:?}");
}

/// A page asked for with its validators answers 304: its document stays as it was (nothing
/// extracted or written) and counts as seen, so the purge keeps it. A page that changed behind an
/// unchanged one is still re-read by the purge's re-check, and the links it gained are followed.
/// An incremental crawl asks the same way and deletes nothing.
#[tokio::test]
async fn unchanged_pages_answer_304_and_stay() {
    let site = Site::start().await;
    site.set("/", 200, &[("Content-Type", "text/html"), ("ETag", "\"home-1\"")], r#"<title>Home</title><a href="/a">a</a>"#);
    site.set("/a", 200, &[("Content-Type", "text/html"), ("Last-Modified", "Wed, 25 Nov 2026 10:00:00 GMT")], "<title>A one</title><p>first</p>");
    // Five crawls of one unverified site.
    let c = cluster(&[("roll.crawler.unverified.max_crawls_per_day", "10")]).await;
    crawler(&c, &format!(r#"{{"urls":"{}"}}"#, site.url("/")), None).await;
    let job = crawl(&c).await;
    assert_eq!((at(&job, "status").as_str(), num(&job, "indexed_document_count")), (Some("completed"), 2), "{job:?}");
    let first = doc(&c, &site.url("/a")).await.unwrap();
    assert_eq!(at(&first, "http_last_modified").as_str(), Some("Wed, 25 Nov 2026 10:00:00 GMT"), "{first:?}");

    // Nothing changed: two 304s, no writes, both documents kept as they were.
    let job = crawl(&c).await;
    assert_eq!((num(&job, "metadata.not_modified_count"), num(&job, "indexed_document_count"), num(&job, "deleted_document_count")), (2, 0, 0), "{job:?}");
    assert_eq!(doc(&c, &site.url("/a")).await.unwrap(), first, "untouched");
    let sent: Vec<_> = site.conditional.lock().unwrap().iter().filter(|(t, ..)| t == "/" || t == "/a").cloned().collect();
    assert_eq!(sent.last().map(|(_, inm, ims)| (inm.clone(), ims.clone())), Some((None, Some("Wed, 25 Nov 2026 10:00:00 GMT".into()))), "{sent:?}");
    assert!(sent.iter().any(|(t, inm, _)| t == "/" && inm.as_deref() == Some("\"home-1\"")), "{sent:?}");

    // /a changed behind the unchanged home page, and links a new page.
    site.set("/a", 200, &[("Content-Type", "text/html"), ("Last-Modified", "Thu, 26 Nov 2026 10:00:00 GMT")], r#"<title>A two</title><a href="/b">b</a>"#);
    site.page("/b", "<title>B</title><p>new</p>");
    let job = crawl(&c).await;
    assert_eq!((num(&job, "metadata.not_modified_count"), num(&job, "indexed_document_count")), (1, 2), "{job:?}");
    assert_eq!(at(&doc(&c, &site.url("/a")).await.unwrap(), "title").as_str(), Some("A two"));
    assert!(doc(&c, &site.url("/b")).await.is_some(), "the link the changed page gained was followed");

    // An incremental crawl asks conditionally too, and deletes nothing: /b's 404 waits for a full
    // crawl.
    site.set("/b", 404, &[], "gone");
    let job = crawl_as(&c, "incremental").await;
    assert_eq!((at(&job, "status").as_str(), num(&job, "deleted_document_count")), (Some("completed"), 0), "{job:?}");
    assert!(doc(&c, &site.url("/b")).await.is_some());
    assert!(num(&job, "metadata.not_modified_count") >= 2, "{job:?}");

    // A configuration change reaches unchanged pages: they're read in full and made anew.
    let rules = format!(
        r#"{{"domains":[{{"url":"{}","extraction_rulesets":[{{"rules":[{{"action":"set","field_name":"kind","selector":"title","value":"page","join_as":"string","source":"html"}}]}}]}}]}}"#,
        site.url("").trim_end_matches('/')
    );
    set_snippet(&c, &rules).await;
    let job = crawl(&c).await;
    assert_eq!(num(&job, "metadata.not_modified_count"), 0, "{job:?}");
    assert_eq!(at(&doc(&c, &site.url("/")).await.unwrap(), "kind").as_str(), Some("page"), "re-extracted with the new rules");
}

/// The docs preset: a documentation page becomes one record per section, its hierarchy from the
/// headings, its content from the text under each, at the page's URL plus the heading's anchor;
/// navigation and footers left out. The crawl makes the index's search template, and the records
/// answer it.
#[tokio::test]
async fn the_docs_preset_makes_section_records() {
    let site = Site::start().await;
    let page = r#"<html><head><title>Guide - Example docs</title></head><body>
        <nav><a href="/other">Other</a><p>Navigation text</p></nav>
        <main><div data-docs-lvl0="">Getting started</div>
        <h1>Guide</h1><p>An introduction.</p>
        <h2 id="install">Install</h2><p>Run the installer.</p><ul><li>Step one</li><li><p>Step two</p></li></ul>
        <h3><a id="linux"></a>On Linux</h3><pre>apt install example</pre>
        <h2 id="configure">Configure</h2><p>Edit the file.</p></main>
        <footer><p>Footer text</p></footer></body></html>"#;
    site.page("/guide", page);
    let c = cluster(&[]).await;
    crawler(&c, &format!(r#"{{"urls":"{}","preset":"docs"}}"#, site.url("/guide")), None).await;
    let job = crawl(&c).await;
    assert_eq!((at(&job, "status").as_str(), num(&job, "indexed_document_count")), (Some("completed"), 4), "{job:?}");
    let (_, v, t) = call(&c, "POST", "/content-site/_search", Some(r#"{"size":20,"sort":[{"weight.position":"asc"}]}"#)).await;
    let records: Vec<Value> = at(&v, "hits.hits").as_array().unwrap().iter().map(|h| at(h, "_source").clone()).collect();
    let row = |r: &Value| {
        let s = |p: &str| at(r, p).as_str().unwrap_or("").to_string();
        (s("type"), s("hierarchy.lvl0"), s("hierarchy.lvl1"), s("hierarchy.lvl2"), s("hierarchy.lvl3"), s("content"), s("url").trim_start_matches(&site.base).to_string())
    };
    let rows: Vec<_> = records.iter().map(row).collect();
    let r = |ty: &str, l2: &str, l3: &str, content: &str, url: &str| (ty.to_string(), "Getting started".to_string(), "Guide".to_string(), l2.to_string(), l3.to_string(), content.to_string(), url.to_string());
    assert_eq!(
        rows,
        vec![
            r("lvl1", "", "", "An introduction.", "/guide"),
            r("lvl2", "Install", "", "Run the installer. Step one Step two", "/guide#install"),
            r("lvl3", "Install", "On Linux", "apt install example", "/guide#linux"),
            r("lvl2", "Configure", "", "Edit the file.", "/guide#configure"),
        ],
        "{t}"
    );
    assert!(records.iter().all(|r| at(r, "url_without_anchor").as_str() == Some(site.url("/guide").as_str()) && at(r, "title").as_str() == Some("Guide - Example docs")), "{t}");
    assert!(!t.contains("Navigation text") && !t.contains("Footer text"), "{t}");
    let (s, v, t) = call(&c, "POST", "/content-site/_search/template", Some(r#"{"id":"content-site-docs-search","params":{"query":"configu"}}"#)).await;
    assert_eq!((s, at(&v, "hits.hits.0._source.anchor").as_str()), (200, Some("configure")), "{t}");
    // The query is JSON-escaped into the template, as Elasticsearch's mustache does.
    let (s, v, t) = call(&c, "POST", "/_render/template", Some(r#"{"id":"content-site-docs-search","params":{"query":"a \"b\" \\c"}}"#)).await;
    assert_eq!((s, at(&v, "template_output.query.multi_match.query").as_str()), (200, Some(r#"a "b" \c"#)), "{t}");
}
