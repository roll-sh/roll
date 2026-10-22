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

/// Server-side bulk indexing cost: 50k generated documents in 1k-document `_bulk` requests,
/// bodies built before the clock starts (run with `--release -- --ignored --nocapture`).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore]
async fn bulk_cost() {
    let c = Cluster::in_memory("bench").await;
    search_es::handle(
        &c,
        EsRequest::json(
            "PUT",
            "/idx",
            r#"{"settings":{"number_of_shards":1,"refresh_interval":"-1"},"mappings":{"properties":{"title":{"type":"text"},"body":{"type":"text"},"cat":{"type":"keyword"},"n":{"type":"long"},"price":{"type":"double"},"at":{"type":"date"}}}}"#,
        ),
    )
    .await;
    // A small deterministic generator (xorshift) instead of a dependency.
    let mut x: u64 = 0x9E37_79B9_7F4A_7C15;
    let mut next = move || {
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        x
    };
    let n = 50_000;
    let mut bodies = Vec::new();
    let mut body = String::new();
    for i in 0..n {
        let words = |k: usize, next: &mut dyn FnMut() -> u64| (0..k).map(|_| format!("w{}", next() % 5000)).collect::<Vec<_>>().join(" ");
        let title = words(5, &mut next);
        let text = words(30, &mut next);
        body.push_str(&format!(
            "{{\"index\":{{\"_id\":\"{i}\"}}}}\n{{\"title\":\"{title}\",\"body\":\"{text}\",\"cat\":\"c{}\",\"n\":{},\"price\":{}.{:02},\"at\":{}}}\n",
            next() % 50,
            next() % 100_000,
            next() % 1000,
            next() % 100,
            1_700_000_000_000u64 + next() % (86_400_000 * 365)
        ));
        if (i + 1) % 1000 == 0 {
            bodies.push(std::mem::take(&mut body));
        }
    }
    let clients: usize = std::env::var("BULK_CLIENTS").ok().and_then(|v| v.parse().ok()).unwrap_or(1);
    let c = std::sync::Arc::new(c);
    let t = Instant::now();
    let mut lanes: Vec<Vec<String>> = vec![Vec::new(); clients];
    for (i, b) in bodies.into_iter().enumerate() {
        lanes[i % clients].push(b);
    }
    let tasks: Vec<_> = lanes
        .into_iter()
        .map(|lane| {
            let c = c.clone();
            tokio::spawn(async move {
                for b in lane {
                    let mut r = EsRequest::json("POST", "/idx/_bulk", b);
                    r.headers = vec![("content-type".into(), "application/x-ndjson".into())];
                    let resp = search_es::handle(&c, r).await;
                    assert_eq!(resp.status, 200);
                }
            })
        })
        .collect();
    for t in tasks {
        t.await.unwrap();
    }
    let bulk = t.elapsed();
    search_es::handle(&c, EsRequest::new("POST", "/idx/_refresh")).await;
    println!("bulk {n} docs: {:?} ({:.0} docs/s), refresh {:?}", bulk, n as f64 / bulk.as_secs_f64(), t.elapsed() - bulk);
    let stats = search_es::handle(&c, EsRequest::new("GET", "/idx/_stats/indexing?filter_path=_all.primaries.indexing.index_time_in_millis")).await;
    println!("shard indexing time: {}", String::from_utf8_lossy(&stats.body));
}
