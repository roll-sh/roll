//! Rough per-request cost of the REST layer (run with `--release -- --ignored --nocapture`).

use search_es::{Cluster, EsRequest};
use std::time::Instant;

#[tokio::test]
#[ignore]
async fn rest_path_cost() {
    let c = Cluster::in_memory("bench").await;
    search_es::handle(&c, EsRequest::json("PUT", "/idx", r#"{"mappings":{"properties":{"a":{"type":"keyword"},"b":{"type":"text"}}}}"#)).await;
    for (label, method, uri) in [
        ("GET /", "GET", "/"),
        ("health", "GET", "/_cluster/health"),
        ("get mapping", "GET", "/idx/_mapping"),
        ("unknown param", "GET", "/_cluster/health?bogus=1"),
        ("cat indices", "GET", "/_cat/indices?v"),
        ("no handler", "GET", "/_nope/a/b"),
    ] {
        let n = 20_000;
        let t = Instant::now();
        for _ in 0..n {
            std::hint::black_box(search_es::handle(&c, EsRequest::new(method, uri)).await);
        }
        println!("{label:>14}: {:>6} ns/request", t.elapsed().as_nanos() / n);
    }
}
