//! The first codebase-index build of a big repo: streamed in batches, bounded memory, and the
//! index answers afterwards. `CODE_INDEX_SCALE_N` files (default 2,000; the measured run in
//! docs/code/test-coverage.md used 50,000 under a 2 GB memory limit).

mod common;

use common::{app, app_with};
use serde_json::{Map, Value, json};
use std::time::Instant;

fn rss_kb(field: &str) -> u64 {
    std::fs::read_to_string("/proc/self/status")
        .ok()
        .and_then(|s| s.lines().find(|l| l.starts_with(field)).and_then(|l| l.split_whitespace().nth(1)).and_then(|v| v.parse().ok()))
        .unwrap_or(0)
}

fn source(n: usize) -> String {
    let mut s = format!("//! Module {n}.\n\nuse crate::shared::Shared{};\n\n", n % 97);
    for f in 0..8 {
        s.push_str(&format!(
            "/// Handles case {f} of module {n}.\npub fn handle_{n}_{f}(input: &str) -> usize {{\n    let parsed = parse_shared(input);\n    if parsed > {f} {{\n        return helper_{n}(parsed);\n    }}\n    parsed + {f}\n}}\n\n"
        ));
    }
    s.push_str(&format!("pub struct Thing{n} {{\n    pub id: u64,\n    pub name: String,\n}}\n\nfn helper_{n}(x: usize) -> usize {{\n    x * 2\n}}\n"));
    s
}

#[tokio::test]
async fn a_big_first_build_is_streamed_and_bounded() {
    let n: usize = std::env::var("CODE_INDEX_SCALE_N").ok().and_then(|v| v.parse().ok()).unwrap_or(2_000);
    let a = app(None);
    a.repo("acme", "big").await;
    // Land the files in chunks of 5,000 (one changeset each).
    let mut landed = 0;
    while landed < n {
        let mut files = Map::new();
        for i in landed..(landed + 5_000).min(n) {
            files.insert(format!("src/m{}/file_{i}.rs", i % 50), json!({ "content": source(i) }));
        }
        landed += files.len();
        let (s, v) = a.req("POST", "/v1/code/repos/acme/big/land", Some(json!({ "message": format!("files to {landed}"), "files": Value::Object(files) })), None).await;
        assert!(s.is_success(), "{s} {v}");
    }
    // Reset the peak (VmHWM) so it measures the build, not the landings before it.
    let _ = std::fs::write("/proc/self/clear_refs", "5");
    let before = rss_kb("VmRSS:");
    let (_, repo) = a.req("GET", "/v1/code/repos/acme/big", None, None).await;
    let r = a.code.repo_by_id(repo["id"].as_str().unwrap()).unwrap();
    let t = Instant::now();
    a.code.index_repo(&r).await.unwrap();
    let took = t.elapsed();
    let peak = rss_kb("VmHWM:");
    let stats = a.code.ai.index.as_ref().unwrap().repo(&r.id).unwrap().stats().unwrap();
    eprintln!(
        "index build: {n} files in {:.1}s ({:.0} files/s); RSS before {} MB, peak {} MB; {} symbols, {} trigram postings",
        took.as_secs_f64(),
        n as f64 / took.as_secs_f64(),
        before / 1024,
        peak / 1024,
        stats.symbols,
        stats.trigrams
    );
    assert_eq!(stats.files, n as u64);
    // The index answers: a definition, references, and a search.
    let defs = a.code.ai.index.as_ref().unwrap().repo(&r.id).unwrap().definitions(&format!("helper_{}", n - 1), 5).unwrap();
    assert_eq!(defs.len(), 1);
    let (s, v) = a.req("GET", &format!("/v1/code/repos/acme/big/search?q=handle_{}_7&limit=5", n / 2), None, None).await;
    assert!(s.is_success(), "{v}");
    assert_eq!(v["matches"].as_array().unwrap().len(), 1, "{v}");
    // Bounded: the build's growth stays well under the repo's source size times a small factor
    // (it never holds every file at once).
    let source_mb = (n * source(0).len()) as u64 / (1 << 20);
    let grew_mb = peak.saturating_sub(before) / 1024;
    assert!(grew_mb < 256 + source_mb / 4, "build grew RSS by {grew_mb} MB for {source_mb} MB of source");
}

#[tokio::test]
async fn searches_answer_503_while_a_first_build_runs_in_the_background() {
    // No waiting: the first search kicks off the build and answers 503 at once.
    let a = app_with(|c| c.search_index_wait_ms = 0);
    a.repo("acme", "big").await;
    let mut files = Map::new();
    for i in 0..3_000 {
        files.insert(format!("src/file_{i}.rs"), json!({ "content": source(i) }));
    }
    let (s, _) = a.req("POST", "/v1/code/repos/acme/big/land", Some(json!({ "message": "files", "files": Value::Object(files) })), None).await;
    assert!(s.is_success());
    let (status, bytes) = a.raw("GET", "/v1/code/repos/acme/big/search?q=helper_7", None, None).await;
    assert_eq!(status, axum::http::StatusCode::SERVICE_UNAVAILABLE);
    let v: Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(v["indexing"], true);
    assert!(v["error"].as_str().unwrap().contains("still being built"));
    // The build goes on in the background; once it's done, searches answer.
    let mut last = Value::Null;
    for _ in 0..600 {
        let (s, v) = a.req("GET", "/v1/code/repos/acme/big/search?q=helper_7(", None, None).await;
        if s.is_success() {
            last = v;
            break;
        }
        assert_eq!(s, axum::http::StatusCode::SERVICE_UNAVAILABLE);
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }
    assert!(!last["matches"].as_array().unwrap().is_empty(), "{last}");
    // Later landings update it in place (no 503: it was built before).
    let (s, _) = a.req("POST", "/v1/code/repos/acme/big/land", Some(json!({ "message": "one more", "files": { "src/new.rs": { "content": "pub fn brand_new_symbol() {}\n" } } })), None).await;
    assert!(s.is_success());
    let (s, v) = a.req("GET", "/v1/code/repos/acme/big/search?q=brand_new_symbol", None, None).await;
    assert!(s.is_success(), "{v}");
    assert_eq!(v["matches"].as_array().unwrap().len(), 1);
}
