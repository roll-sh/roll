//! `roll search …` end to end: the real `roll` binary against an in-process search control
//! plane (the `/v1/search` API and the Elasticsearch listener rolld serves). Object storage is a
//! temp directory, or S3 with `ROLL_E2E_S3_BUCKET`.

use search_control::{Control, SearchConfig};
use serde_json::Value;
use std::process::Output;
use std::sync::Arc;

struct Env {
    control: Arc<Control>,
    api: String,
    home: std::path::PathBuf,
}

impl Drop for Env {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.home);
    }
}

async fn env() -> Env {
    let n = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos();
    let home = std::env::temp_dir().join(format!("roll-cli-search-{}-{n}", std::process::id()));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let mut cfg = SearchConfig::local(home.clone());
    cfg.http_port = listener.local_addr().unwrap().port();
    cfg.gc_interval = None;
    let control = Control::new(cfg, search_store::test_objects("cli")).await.unwrap();
    tokio::spawn(search_server::serve(listener, control.node.clone(), search_server::ListenOptions::default()));
    let api_listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let api = format!("http://{}", api_listener.local_addr().unwrap());
    let router = search_control::api::router(control.clone());
    tokio::spawn(async move { axum::serve(api_listener, router).await });
    Env { control, api, home }
}

impl Env {
    /// Runs `roll --api <api> <args>` off the runtime's threads.
    async fn roll_with(&self, args: &[&str], stdin: Option<&str>) -> Output {
        let mut cmd = std::process::Command::new(env!("CARGO_BIN_EXE_roll"));
        cmd.arg("--api").arg(&self.api).args(args).env_remove("ROLL_API_URL");
        let stdin = stdin.map(str::to_string);
        tokio::task::spawn_blocking(move || {
            use std::io::Write;
            use std::process::Stdio;
            cmd.stdin(if stdin.is_some() { Stdio::piped() } else { Stdio::null() }).stdout(Stdio::piped()).stderr(Stdio::piped());
            let mut child = cmd.spawn().expect("run roll");
            if let Some(text) = stdin {
                child.stdin.take().unwrap().write_all(text.as_bytes()).unwrap();
            }
            child.wait_with_output().unwrap()
        })
        .await
        .unwrap()
    }

    /// Runs `roll` and expects success; returns stdout.
    async fn roll(&self, args: &[&str]) -> String {
        let out = self.roll_with(args, None).await;
        let stdout = String::from_utf8_lossy(&out.stdout).into_owned();
        assert!(out.status.success(), "roll {args:?} failed: {stdout}{}", String::from_utf8_lossy(&out.stderr));
        stdout
    }

    async fn roll_json(&self, args: &[&str]) -> Value {
        let mut all = vec!["--json"];
        all.extend_from_slice(args);
        let out = self.roll(&all).await;
        serde_json::from_str(&out).unwrap_or_else(|e| panic!("roll {args:?}: {e}: {out}"))
    }
}

/// The value printed after `label` on its own line (`password  abc   (shown once)`).
fn field(out: &str, label: &str) -> String {
    out.lines()
        .find_map(|l| l.strip_prefix(label).map(|r| r.split_whitespace().next().unwrap_or_default().to_string()))
        .unwrap_or_else(|| panic!("no {label} in {out}"))
}

#[tokio::test]
async fn clusters_and_requests() {
    let e = env().await;

    // create: the password is printed once, with a curl line.
    let out = e.roll(&["search", "clusters", "create", "app", "--max-in-flight", "8", "--destructive-requires-name", "false"]).await;
    let pw = field(&out, "password");
    assert_eq!(pw.len(), 32, "{out}");
    assert_eq!(field(&out, "username"), "app");
    assert!(out.contains(&format!("curl -u app:{pw} http://127.0.0.1:")), "{out}");
    assert!(out.contains("(shown once)"));
    let c = e.control.get("app").await.unwrap();
    assert_eq!((c.password.as_str(), c.max_in_flight, c.destructive_requires_name), (pw.as_str(), 8, false));

    // A failing API call exits non-zero with the API's reason.
    let out = e.roll_with(&["search", "clusters", "create", "app"], None).await;
    assert!(!out.status.success());
    assert!(String::from_utf8_lossy(&out.stderr).contains("a search cluster named app already exists"), "{out:?}");
    let out = e.roll_with(&["search", "clusters", "show", "nope"], None).await;
    assert!(!out.status.success());
    assert!(String::from_utf8_lossy(&out.stderr).contains("no search cluster nope"), "{out:?}");

    // list / show (alias `es`, `ls`), the password never reappears.
    let out = e.roll(&["es", "clusters", "ls"]).await;
    assert!(out.starts_with("app "), "{out}");
    assert!(out.contains("asleep") && !out.contains(&pw), "{out}");
    let out = e.roll(&["search", "clusters", "show", "app"]).await;
    assert!(out.contains("limits    8 in flight, 100 MiB bodies"), "{out}");
    assert!(out.contains("idle      300s") && !out.contains(&pw), "{out}");
    let shown = e.roll_json(&["search", "clusters", "show", "app"]).await;
    assert_eq!(shown["name"], "app");
    assert!(shown.get("password").is_none());

    // request: inline JSON, @file NDJSON, stdin; pretty printed by Elasticsearch.
    let out = e.roll(&["search", "request", "app", "PUT", "/books", r#"{"mappings":{"properties":{"title":{"type":"text"}}}}"#]).await;
    assert!(out.contains("\"acknowledged\" : true"), "{out}");
    let bulk = e.home.join("bulk.ndjson");
    std::fs::write(&bulk, "{\"index\":{\"_index\":\"books\",\"_id\":\"1\"}}\n{\"title\":\"dune\"}\n{\"index\":{\"_index\":\"books\",\"_id\":\"2\"}}\n{\"title\":\"emma\"}\n").unwrap();
    let out = e.roll(&["search", "request", "app", "POST", "/_bulk?refresh=true", &format!("@{}", bulk.display())]).await;
    assert!(out.contains("\"errors\" : false"), "{out}");
    // Bodies over axum's 2 MB default go through (the cluster allows 100 MiB).
    let big = e.home.join("big.ndjson");
    let blob = "x".repeat(10_000);
    let lines: String = (0..300).map(|i| format!("{{\"index\":{{\"_index\":\"books\",\"_id\":\"big-{i}\"}}}}\n{{\"blob\":\"{blob}\"}}\n")).collect();
    assert!(lines.len() > 2 << 20);
    std::fs::write(&big, &lines).unwrap();
    let out = e.roll(&["search", "request", "app", "POST", "/_bulk", &format!("@{}", big.display())]).await;
    assert!(out.contains("\"errors\" : false"), "{}", &out[..out.len().min(400)]);
    let out = e.roll_with(&["search", "request", "app", "PUT", "/books/_doc/3", "-"], Some(r#"{"title":"ulysses","author":"joyce"}"#)).await;
    assert!(out.status.success(), "{out:?}");
    assert!(String::from_utf8_lossy(&out.stdout).contains("\"result\" : \"created\""));
    // The body goes out byte for byte: `_source` keeps its key order.
    let out = e.roll(&["search", "request", "app", "GET", "/books/_doc/3?filter_path=_source"]).await;
    let (t, a) = (out.find("\"title\"").unwrap_or_else(|| panic!("{out}")), out.find("\"author\"").unwrap_or_else(|| panic!("{out}")));
    assert!(t < a, "{out}");
    // The leading slash is optional; text APIs print as text.
    let out = e.roll(&["search", "request", "app", "GET", "_cat/indices?h=index,status"]).await;
    assert_eq!(out.trim(), "books open");
    let out = e.roll(&["search", "request", "app", "GET", "/books/_doc/2"]).await;
    assert!(out.contains("\"title\" : \"emma\""), "{out}");
    // Non-2xx: Elasticsearch's error on stdout, the status on stderr, exit 1.
    let out = e.roll_with(&["search", "request", "app", "GET", "/missing"], None).await;
    assert_eq!(out.status.code(), Some(1));
    assert!(String::from_utf8_lossy(&out.stdout).contains("index_not_found_exception"));
    assert_eq!(String::from_utf8_lossy(&out.stderr).trim(), "HTTP 404");
    // --json returns the console envelope untouched.
    let v = e.roll_json(&["search", "request", "app", "HEAD", "/books"]).await;
    assert_eq!((v["status"].as_u64(), v["body"].as_str()), (Some(200), Some("")));

    // indices
    let out = e.roll(&["search", "clusters", "indices", "app"]).await;
    let row = out.lines().find(|l| l.contains("books")).unwrap_or_else(|| panic!("{out}"));
    assert!(row.contains("open"), "{out}");
    assert!(out.starts_with("health"), "{out}");

    // index and docs: the per-index routes, Elasticsearch's answers printed as they come.
    let out = e.roll(&["search", "index", "create", "app", "films", r#"{"mappings":{"properties":{"year":{"type":"integer"}}}}"#]).await;
    assert!(out.contains("\"acknowledged\" : true"), "{out}");
    let out = e.roll(&["search", "docs", "put", "app", "films", "1", r#"{"title":"alien","year":1979}"#, "--refresh"]).await;
    assert!(out.contains("\"result\" : \"created\""), "{out}");
    e.roll(&["search", "index", "bulk", "app", "films", "{\"index\":{\"_id\":\"2\"}}\n{\"title\":\"heat\",\"year\":1995}\n", "--refresh"]).await;
    let v = e.roll_json(&["search", "index", "search", "app", "films", "--q", "year:1995"]).await;
    assert_eq!(v["hits"]["hits"][0]["_id"], "2", "{v}");
    let out = e.roll(&["search", "docs", "get", "app", "films", "1"]).await;
    assert!(out.contains("\"title\" : \"alien\""), "{out}");
    let out = e.roll_with(&["search", "docs", "get", "app", "films", "9"], None).await;
    assert_eq!(out.status.code(), Some(1), "{out:?}");
    assert!(String::from_utf8_lossy(&out.stdout).contains("\"found\" : false"));
    e.roll(&["search", "index", "delete", "app", "films"]).await;

    // url
    let out = e.roll(&["search", "clusters", "url", "app"]).await;
    let url = out.trim().to_string();
    assert!(url.starts_with("http://127.0.0.1:"), "{url}");
    let out = e.roll(&["search", "clusters", "url", "app", "--password", "p@ss"]).await;
    assert_eq!(out.trim(), url.replace("http://", "http://app:p%40ss@"));

    // set
    let out = e.roll(&["search", "clusters", "set", "app", "--idle", "0", "--max-body-mb", "5"]).await;
    assert!(out.starts_with("app "), "{out}");
    let c = e.control.get("app").await.unwrap();
    assert_eq!((c.idle_timeout_s, c.max_body_mb), (0, 5));
    let out = e.roll(&["search", "clusters", "show", "app"]).await;
    assert!(out.contains("idle      always loaded"), "{out}");
    let out = e.roll_with(&["search", "clusters", "set", "app", "--flavor", "solr"], None).await;
    assert!(!out.status.success());
    // Over the cluster's body limit (now 5 MiB): the API's reason, not a JSON parse error.
    std::fs::write(&big, lines.repeat(5)).unwrap();
    let out = e.roll_with(&["search", "request", "app", "POST", "/_bulk", &format!("@{}", big.display())], None).await;
    assert_eq!(out.status.code(), Some(1), "{out:?}");
    assert!(String::from_utf8_lossy(&out.stderr).contains("max_body_mb"), "{out:?}");

    // branch (now and --at), visible in show.
    let out = e.roll(&["search", "clusters", "branch", "app", "app-dev"]).await;
    assert!(out.starts_with("branch app-dev (srch_"), "{out}");
    let bpw = field(&out, "password");
    assert_ne!(bpw, pw);
    assert_eq!(e.control.get("app-dev").await.unwrap().password, bpw);
    let v = e.roll_json(&["search", "clusters", "branch", "app", "app-past", "--at", "0s"]).await;
    assert_eq!(v["parent_name"], "app");
    assert!(v["password"].is_string());
    let out = e.roll_with(&["search", "clusters", "branch", "app", "too-old", "--at", "30d"], None).await;
    assert!(!out.status.success());
    assert!(String::from_utf8_lossy(&out.stderr).contains("outside the history window"), "{out:?}");
    // A duration too long to compute is an error, not a panic.
    let out = e.roll_with(&["search", "clusters", "branch", "app", "too-far", "--at", "999999999999999d"], None).await;
    assert_eq!(out.status.code(), Some(1), "{out:?}");
    assert!(String::from_utf8_lossy(&out.stderr).contains("bad time"), "{out:?}");
    let out = e.roll(&["search", "clusters", "show", "app"]).await;
    assert!(out.contains("branch app-dev") && out.contains("branch app-past"), "{out}");
    let out = e.roll(&["search", "clusters", "list"]).await;
    assert!(out.lines().any(|l| l.starts_with("app-dev ") && l.contains("branch of app")), "{out}");
    let out = e.roll(&["search", "request", "app-dev", "GET", "/_cat/indices?h=index"]).await;
    assert_eq!(out.trim(), "books");

    // password
    let out = e.roll(&["search", "clusters", "password", "app"]).await;
    let pw2 = field(&out, "password");
    assert_ne!(pw2, pw);
    assert_eq!(e.control.get("app").await.unwrap().password, pw2);

    // unload
    let out = e.roll(&["search", "clusters", "unload", "app"]).await;
    assert_eq!(out.trim(), "app is asleep");
    assert!(!e.control.node.loaded().contains(&"app".to_string()));

    // usage
    e.control.sample().await.unwrap();
    let out = e.roll(&["search", "clusters", "usage", "app", "--days", "7"]).await;
    assert!(out.starts_with("app — last 7 days"), "{out}");
    for label in ["loaded", "memory", "requests", "storage", "total", "Elastic Cloud", "Amazon OpenSearch Service", "OpenSearch Serverless"] {
        assert!(out.contains(&format!("  {label} ")), "{label}: {out}");
    }
    let u = e.roll_json(&["search", "clusters", "usage", "app"]).await;
    assert_eq!(u["days"], 30);
    // Every request the node admitted (`Node::take_request_counts`): nine console requests, the
    // indices listing and seven per-index calls. The one over the body limit never reached the
    // cluster.
    assert_eq!(u["requests"], 17, "the node's request counts are metered: {u}");
    assert!(u["loaded_seconds"].as_i64().unwrap() >= 1, "{u}");

    // delete
    let out = e.roll(&["search", "clusters", "delete", "app-past"]).await;
    assert_eq!(out.trim(), "deleted app-past");
    assert!(e.control.get("app-past").await.is_err());
    let names: Vec<String> = e.roll_json(&["search", "clusters", "list"]).await.as_array().unwrap().iter().map(|c| c["name"].as_str().unwrap().to_string()).collect();
    assert_eq!(names.len(), 2, "{names:?}");
    e.control.shutdown().await.unwrap();
}
