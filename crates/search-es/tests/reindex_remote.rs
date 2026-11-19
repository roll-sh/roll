//! Reindex from remote (`crates/search-es/src/api/remote.rs`) as a migration into roll uses it.
//!
//! Always run, against a hostile mock remote on 127.0.0.1 (plain HTTP): a redirect isn't
//! followed, an oversized page is refused before it's held, a private address is refused
//! without the node's flag, and credentials a remote echoes never reach an error or a task.
//!
//! Against a real Elasticsearch 9.5.5 over HTTPS (its generated certificate) when
//! `ROLL_REINDEX_ES` names it: a full reindex with a query, API key auth, a wrong password
//! giving Elasticsearch's own error (compared with what Elasticsearch's reindex answers for the
//! same mistake), and a task whose description, status and stored result hold no credential.
//! Start it as:
//!
//! ```sh
//! docker run -d --name roll-reindex-es -m 3g -p 127.0.0.1:39243:9200 -e discovery.type=single-node \
//!   -e ES_JAVA_OPTS='-Xms1g -Xmx1g' -e ELASTIC_PASSWORD=<pw> -e xpack.ml.enabled=false \
//!   -e cluster.routing.allocation.disk.threshold_enabled=false -e indices.merge.disk.check_interval=0 \
//!   -e 'reindex.remote.whitelist=localhost:*' -e reindex.ssl.verification_mode=none \
//!   docker.elastic.co/elasticsearch/elasticsearch:9.5.5
//! docker cp roll-reindex-es:/usr/share/elasticsearch/config/certs/http_ca.crt /tmp/http_ca.crt
//! ROLL_REINDEX_ES=https://127.0.0.1:39243 ROLL_REINDEX_ES_CA=/tmp/http_ca.crt ROLL_REINDEX_ES_PASSWORD=<pw> \
//!   cargo test -p search-es --test reindex_remote
//! ```
//!
//! (The whitelist and `verification_mode` let Elasticsearch reindex from itself for the error
//! comparison.)

use search_es::json::{Value, parse};
use search_es::{Cluster, ClusterConfig, EsRequest, MemoryStore};
use std::sync::{Arc, Mutex};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

async fn cluster(settings: &[(&str, &str)]) -> Arc<Cluster> {
    let mut cfg = ClusterConfig::new("c");
    for (k, v) in settings {
        cfg.node_settings.push((k.to_string(), v.to_string()));
    }
    let c = Arc::new(Cluster::open(cfg, Arc::new(MemoryStore::new())).await.unwrap());
    c.enable_background_tasks();
    c
}

async fn call(c: &Cluster, method: &str, uri: &str, body: &str) -> (u16, Value, String) {
    let r = if body.is_empty() { EsRequest::new(method, uri) } else { EsRequest::json(method, uri, body.to_string()) };
    let resp = search_es::handle(c, r).await;
    let text = String::from_utf8_lossy(&resp.body).into_owned();
    (resp.status, parse(text.as_bytes()).ok().flatten().unwrap_or(Value::Null), text)
}

fn at<'a>(v: &'a Value, path: &str) -> &'a Value {
    static NULL: Value = Value::Null;
    path.split('.')
        .try_fold(v, |v, k| match k.parse::<usize>() {
            Ok(i) => v.as_array().and_then(|a| a.get(i)),
            Err(_) => v.get(k),
        })
        .unwrap_or(&NULL)
}

/// What a mock remote answers a request: status, extra headers, body.
type Answer = (u16, Vec<(String, String)>, String);

/// A remote that answers with `answer(method, path, headers)`; every request is logged.
async fn mock(answer: impl Fn(&str, &str, &[(String, String)]) -> Answer + Send + Sync + 'static) -> (String, Arc<Mutex<Vec<String>>>) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let seen = Arc::new(Mutex::new(Vec::new()));
    let log = seen.clone();
    let answer = Arc::new(answer);
    tokio::spawn(async move {
        loop {
            let Ok((mut sock, _)) = listener.accept().await else { return };
            let (log, answer) = (log.clone(), answer.clone());
            tokio::spawn(async move {
                let mut buf = Vec::new();
                let mut chunk = [0u8; 8192];
                let (head_end, len) = loop {
                    let n = sock.read(&mut chunk).await.unwrap_or(0);
                    if n == 0 {
                        return;
                    }
                    buf.extend_from_slice(&chunk[..n]);
                    if let Some(i) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
                        let head = String::from_utf8_lossy(&buf[..i]).to_string();
                        let len = head.lines().find_map(|l| l.to_ascii_lowercase().strip_prefix("content-length:").map(|v| v.trim().parse::<usize>().unwrap_or(0))).unwrap_or(0);
                        break (i + 4, len);
                    }
                };
                while buf.len() < head_end + len {
                    let n = sock.read(&mut chunk).await.unwrap_or(0);
                    if n == 0 {
                        break;
                    }
                    buf.extend_from_slice(&chunk[..n]);
                }
                let head = String::from_utf8_lossy(&buf[..head_end]).to_string();
                let mut lines = head.lines();
                let first = lines.next().unwrap_or_default().to_string();
                let mut parts = first.split(' ');
                let (method, path) = (parts.next().unwrap_or_default().to_string(), parts.next().unwrap_or_default().to_string());
                let headers: Vec<(String, String)> = lines.filter_map(|l| l.split_once(':')).map(|(k, v)| (k.trim().to_ascii_lowercase(), v.trim().to_string())).collect();
                log.lock().unwrap().push(format!("{method} {path}"));
                let (status, extra, body) = answer(&method, &path, &headers);
                let mut resp = format!("HTTP/1.1 {status} X\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n", body.len());
                for (k, v) in extra {
                    resp.push_str(&format!("{k}: {v}\r\n"));
                }
                resp.push_str("\r\n");
                let _ = sock.write_all(resp.as_bytes()).await;
                let _ = sock.write_all(body.as_bytes()).await;
            });
        }
    });
    (format!("http://{addr}"), seen)
}

fn root() -> String {
    r#"{"name":"n","cluster_name":"remote","version":{"number":"9.5.5"},"tagline":"You Know, for Search"}"#.to_string()
}

fn reindex(host: &str, extra: &str) -> String {
    format!(r#"{{"source":{{"remote":{{"host":"{host}"{extra}}},"index":"src"}},"dest":{{"index":"dst"}}}}"#)
}

#[tokio::test]
async fn a_private_address_needs_the_nodes_flag() {
    let c = cluster(&[]).await;
    for host in ["http://127.0.0.1:9200", "http://localhost:9200", "https://10.0.0.8:9243", "http://[::1]:9200", "http://169.254.169.254:80"] {
        let (s, v, _) = call(&c, "POST", "/_reindex", &reindex(host, "")).await;
        assert_eq!(s, 400, "{host}: {v:?}");
        let reason = at(&v, "error.reason").as_str().unwrap_or_default();
        assert!(reason.ends_with("is a private address, which reindex from remote may not connect to unless [roll.reindex.remote.allow_private_hosts] is set"), "{reason}");
    }
    // A node whitelist narrows further, in Elasticsearch's words.
    let c = cluster(&[("reindex.remote.whitelist", "search.example.com:443")]).await;
    let (_, v, _) = call(&c, "POST", "/_reindex", &reindex("https://other.example.com:443", "")).await;
    assert_eq!(at(&v, "error.reason").as_str(), Some("[other.example.com:443] not whitelisted in reindex.remote.whitelist"));
}

#[tokio::test]
async fn a_redirect_is_not_followed() {
    let (target, target_seen) = mock(|_, _, _| (200, vec![], root())).await;
    let (remote, _) = mock(move |_, _, _| (302, vec![("location".into(), format!("{target}/"))], String::new())).await;
    let c = cluster(&[("roll.reindex.remote.allow_private_hosts", "true")]).await;
    let (s, v, _) = call(&c, "POST", "/_reindex", &reindex(&remote, "")).await;
    assert_eq!((s, at(&v, "error.type").as_str()), (302, Some("status_exception")), "{v:?}");
    assert!(at(&v, "error.caused_by.reason").as_str().unwrap_or_default().contains("status line [HTTP/1.1 302 Found]"), "{v:?}");
    assert!(target_seen.lock().unwrap().is_empty(), "the redirect's target was called");
}

#[tokio::test]
async fn an_oversized_page_is_refused() {
    let big = format!(r#"{{"pit_id":"p","hits":{{"total":{{"value":1,"relation":"eq"}},"hits":[{{"_index":"src","_id":"1","_source":{{"x":"{}"}},"sort":[1]}}]}}}}"#, "a".repeat(200_000));
    let (remote, seen) = mock(move |m, p, _| match (m, p) {
        ("GET", "/") => (200, vec![], root()),
        (_, p) if p.contains("_pit") => (200, vec![], r#"{"id":"p"}"#.into()),
        _ => (200, vec![], big.clone()),
    })
    .await;
    let c = cluster(&[("roll.reindex.remote.allow_private_hosts", "true"), ("roll.reindex.remote.buffer_limit", "64kb")]).await;
    let (s, v, _) = call(&c, "POST", "/_reindex", &reindex(&remote, "")).await;
    assert_eq!(s, 400, "{v:?}");
    assert_eq!(at(&v, "error.reason").as_str(), Some("Remote responded with a chunk that was too large. Use a smaller batch size."));
    assert!(at(&v, "error.caused_by.reason").as_str().unwrap_or_default().ends_with("for the configured buffer limit [65536]"), "{v:?}");
    let (_, idx, _) = call(&c, "GET", "/dst/_count", "").await;
    assert_eq!(at(&idx, "count").as_i64().unwrap_or(0), 0, "nothing written: {idx:?}");
    // The point in time is released however the run ends.
    assert!(seen.lock().unwrap().iter().any(|r| r == "DELETE /_pit"), "{:?}", seen.lock().unwrap());
}

/// A remote that echoes whatever credentials it got into its errors.
#[tokio::test]
async fn echoed_credentials_never_reach_an_error_or_a_task() {
    let (remote, _) = mock(|_, _, headers| {
        let auth = headers.iter().find(|(k, _)| k == "authorization").map(|(_, v)| v.clone()).unwrap_or_default();
        (401, vec![], format!(r#"{{"error":"bad credentials [{auth}] [hunter2-secret] [c2VjcmV0LWtleS12YWx1ZQ==]"}}"#))
    })
    .await;
    let c = cluster(&[("roll.reindex.remote.allow_private_hosts", "true")]).await;
    for creds in [r#","username":"elastic","password":"hunter2-secret""#, r#","headers":{"Authorization":"ApiKey c2VjcmV0LWtleS12YWx1ZQ=="}"#] {
        let (s, v, text) = call(&c, "POST", "/_reindex", &reindex(&remote, creds)).await;
        assert_eq!((s, at(&v, "error.type").as_str()), (401, Some("status_exception")), "{text}");
        assert!(!text.contains("hunter2") && !text.contains("c2VjcmV0") && !text.contains("ZWxhc3RpYzpo"), "{text}");
        assert!(text.contains("::es_redacted::"), "{text}");
        // The same as a task: its stored result, its description and the task list.
        let (s, v, _) = call(&c, "POST", "/_reindex?wait_for_completion=false", &reindex(&remote, creds)).await;
        assert_eq!(s, 200, "{v:?}");
        let task = at(&v, "task").as_str().unwrap().to_string();
        let (_, done, text) = call(&c, "GET", &format!("/_tasks/{task}?wait_for_completion=true&timeout=10s"), "").await;
        assert_eq!(at(&done, "completed").as_bool(), Some(true), "{text}");
        assert_eq!(at(&done, "task.description").as_str().map(|d| d.starts_with("reindex from [host=127.0.0.1 port=")), Some(true), "{text}");
        assert!(!text.contains("hunter2") && !text.contains("c2VjcmV0") && !text.contains("ZWxhc3RpYzpo"), "{text}");
    }
}

// --- a real Elasticsearch 9.5.5 over HTTPS -----------------------------------------------------

struct Es {
    url: String,
    ca: String,
    password: String,
    http: reqwest::Client,
}

fn real_es() -> Option<Es> {
    let url = std::env::var("ROLL_REINDEX_ES").ok()?;
    let ca = std::env::var("ROLL_REINDEX_ES_CA").expect("ROLL_REINDEX_ES_CA: the remote's CA certificate");
    let password = std::env::var("ROLL_REINDEX_ES_PASSWORD").expect("ROLL_REINDEX_ES_PASSWORD: elastic's password");
    let pem = std::fs::read(&ca).unwrap();
    let http = reqwest::Client::builder().tls_certs_merge(reqwest::Certificate::from_pem_bundle(&pem).unwrap()).build().unwrap();
    Some(Es { url, ca, password, http })
}

impl Es {
    async fn call(&self, method: reqwest::Method, path: &str, body: Option<&str>) -> (u16, Value) {
        let mut r = self.http.request(method, format!("{}{path}", self.url)).basic_auth("elastic", Some(&self.password));
        if let Some(b) = body {
            r = r.header("content-type", "application/json").body(b.to_string());
        }
        let resp = r.send().await.unwrap();
        let status = resp.status().as_u16();
        let bytes = resp.bytes().await.unwrap();
        (status, parse(&bytes).ok().flatten().unwrap_or(Value::Null))
    }
}

/// `v` with `from` replaced by `to` wherever a string holds it.
fn replace(v: &Value, from: &str, to: &str) -> Value {
    parse(v.to_json().replace(from, to).as_bytes()).unwrap().unwrap()
}

#[tokio::test]
async fn from_a_real_elasticsearch_over_https() {
    let Some(es) = real_es() else {
        eprintln!("skipped: set ROLL_REINDEX_ES (see the module docs)");
        return;
    };
    use reqwest::Method;
    let _ = es.call(Method::DELETE, "/roll-src", None).await;
    let (s, v) = es.call(Method::PUT, "/roll-src", Some(r#"{"settings":{"number_of_shards":2},"mappings":{"properties":{"n":{"type":"integer"},"tag":{"type":"keyword"}}}}"#)).await;
    assert_eq!(s, 200, "{v:?}");
    let mut bulk = String::new();
    for i in 0..250 {
        bulk.push_str(&format!("{{\"index\":{{\"_id\":\"{i}\"{}}}}}\n{{\"n\":{i},\"tag\":\"{}\"}}\n", if i % 50 == 0 { ",\"routing\":\"r\"" } else { "" }, if i % 2 == 0 { "even" } else { "odd" }));
    }
    let resp = es.http.post(format!("{}/roll-src/_bulk?refresh=true", es.url)).basic_auth("elastic", Some(&es.password)).header("content-type", "application/x-ndjson").body(bulk).send().await.unwrap();
    assert_eq!(resp.status().as_u16(), 200);
    let (_, key) = es.call(Method::POST, "/_security/api_key", Some(r#"{"name":"roll-migrate","role_descriptors":{"r":{"cluster":["monitor"],"indices":[{"names":["roll-src"],"privileges":["read","view_index_metadata"]}]}}}"#)).await;
    let api_key = at(&key, "encoded").as_str().unwrap().to_string();

    let c = cluster(&[("roll.reindex.remote.allow_private_hosts", "true"), ("reindex.ssl.certificate_authorities", &es.ca)]).await;
    let remote = |creds: &str, index: &str, dest: &str, extra: &str| format!(r#"{{"source":{{"remote":{{"host":"{}"{creds}}},"index":"{index}","size":40{extra}}},"dest":{{"index":"{dest}"}}}}"#, es.url);
    let basic = format!(r#","username":"elastic","password":"{}""#, es.password);

    // A full copy with a query, page by page; routing kept.
    let (s, v, text) = call(&c, "POST", "/_reindex?refresh=true", &remote(&basic, "roll-src", "evens", r#","query":{"term":{"tag":"even"}}"#)).await;
    assert_eq!((s, at(&v, "created").as_i64(), at(&v, "total").as_i64(), at(&v, "batches").as_i64()), (200, Some(125), Some(125), Some(4)), "{text}");
    let (_, d, _) = call(&c, "GET", "/evens/_doc/50?routing=r", "").await;
    assert_eq!(at(&d, "_source.n").as_i64(), Some(50), "{d:?}");

    // An API key in `headers`.
    let (s, v, text) = call(&c, "POST", "/_reindex?refresh=true", &remote(&format!(r#","headers":{{"Authorization":"ApiKey {api_key}"}}"#), "roll-src", "all", "")).await;
    assert_eq!((s, at(&v, "created").as_i64()), (200, Some(250)), "{text}");
    assert!(!text.contains(&api_key));

    // A wrong password: Elasticsearch's own answer for the same mistake (its reindex from itself),
    // the hosts aside.
    let wrong = r#","username":"elastic","password":"wrong-password-1""#;
    let (s, mine, text) = call(&c, "POST", "/_reindex", &remote(wrong, "roll-src", "x", "")).await;
    assert!(!text.contains("wrong-password-1"), "{text}");
    let (es_status, theirs) = es.call(Method::POST, "/_reindex", Some(&remote(wrong, "roll-src", "x", "").replace(&es.url, "https://localhost:9200"))).await;
    assert_eq!((s, at(&mine, "error.type").as_str()), (401, Some("status_exception")), "{text}");
    assert_eq!(s, es_status);
    assert_eq!(replace(&mine, &es.url, "HOST"), replace(&theirs, "https://localhost:9200", "HOST"));

    // A task: its description, status and stored result never hold a credential; cancel stops it.
    let (_, v, _) = call(&c, "POST", "/_reindex?wait_for_completion=false&requests_per_second=1", &remote(&basic, "roll-src", "slow", "")).await;
    let task = at(&v, "task").as_str().unwrap().to_string();
    let mut listed = String::new();
    for _ in 0..50 {
        let (_, l, text) = call(&c, "GET", "/_tasks?detailed=true&actions=*reindex", "").await;
        listed = text;
        if l.to_json().contains("\"created\":40") {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }
    let port = es.url.rsplit(':').next().unwrap();
    assert!(listed.contains(&format!("reindex from [scheme=https host=127.0.0.1 port={port}][roll-src] to [slow]")), "{listed}");
    call(&c, "POST", &format!("/_tasks/{task}/_cancel"), "").await;
    let (_, done, text) = call(&c, "GET", &format!("/_tasks/{task}?wait_for_completion=true&timeout=10s"), "").await;
    assert_eq!((at(&done, "completed").as_bool(), at(&done, "response.canceled").as_str()), (Some(true), Some("by user request")), "{text}");
    for t in [&listed, &text] {
        assert!(!t.contains(&es.password) && !t.contains("ZWxhc3RpYz"), "{t}");
    }
    let (_, open) = es.call(Method::GET, "/_nodes/stats/indices/search", None).await;
    let contexts: i64 = open.get("nodes").and_then(Value::as_object).map(|n| n.values().filter_map(|x| at(x, "indices.search.open_contexts").as_i64()).sum()).unwrap_or(0);
    assert_eq!(contexts, 0, "every point in time released: {open:?}");
    let _ = es.call(Method::DELETE, "/roll-src", None).await;
}
