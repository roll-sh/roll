//! A noisy tenant can't starve a quiet one: requests run in node-wide slots granted
//! round-robin across clusters, and each tenant's own budgets (in flight, rate, bytes) turn its
//! excess away with 429.

mod common;

use common::{basic, send, start};
use search_server::{ListenOptions, NodeConfig};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::{Duration, Instant};

fn bulk(n: usize, tag: usize) -> Vec<u8> {
    let mut b = String::new();
    for i in 0..n {
        b.push_str(&format!("{{\"index\":{{}}}}\n{{\"title\":\"document {i} of batch {tag} with some words to analyze\",\"n\":{i},\"tags\":[\"a\",\"b\",\"c\"]}}\n"));
    }
    b.into_bytes()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_quiet_tenant_is_served_while_a_noisy_one_saturates_the_node() {
    let t = start(NodeConfig { max_running: 2, ..NodeConfig::default() }, ListenOptions::default()).await;
    let noisy = basic("c1", "pw1");
    let quiet = basic("c2", "pw2");
    let json = ("Content-Type", "application/x-ndjson");
    let body = bulk(400, 0);
    // One heavy request alone.
    let r = send(t.addr, "POST", "/n/_bulk", &[("Authorization", &noisy), json], &body).await;
    assert_eq!(r.status, 200, "{}", r.text());
    let started = Instant::now();
    let r = send(t.addr, "POST", "/n/_bulk", &[("Authorization", &noisy), json], &body).await;
    assert_eq!(r.status, 200);
    let alone = started.elapsed();
    send(t.addr, "GET", "/", &[("Authorization", &quiet)], b"").await;

    // 24 connections of the noisy tenant send heavy requests back to back.
    let stop = Arc::new(AtomicBool::new(false));
    let done = Arc::new(AtomicU64::new(0));
    let mut flood = Vec::new();
    for c in 0..24 {
        let (stop, done, noisy, body) = (stop.clone(), done.clone(), noisy.clone(), body.clone());
        let addr = t.addr;
        flood.push(tokio::spawn(async move {
            let _ = c;
            while !stop.load(Ordering::Relaxed) {
                let r = send(addr, "POST", "/n/_bulk", &[("Authorization", &noisy), ("Content-Type", "application/x-ndjson")], &body).await;
                assert!(r.status == 200 || r.status == 429, "{}", r.status);
                done.fetch_add(1, Ordering::Relaxed);
            }
        }));
    }
    // Let the flood build its queue.
    tokio::time::sleep(alone * 4 + Duration::from_millis(50)).await;
    // Its requests queue for its write pool (holding no running slot) or for a running slot.
    let sched = t.node.scheduler().unwrap();
    let pool = send(t.addr, "GET", "/_cat/thread_pool/write?h=queue", &[("Authorization", &noisy)], b"").await;
    let pool_queue: usize = pool.text().trim().parse().unwrap_or(0);
    assert!(sched.waiting() + pool_queue > 4, "the noisy tenant queues ({} waiting for a slot, {pool_queue} for its pool)", sched.waiting());
    let mut worst = Duration::ZERO;
    for _ in 0..10 {
        let s = Instant::now();
        let r = send(t.addr, "GET", "/_cluster/health", &[("Authorization", &quiet)], b"").await;
        assert_eq!(r.status, 200);
        worst = worst.max(s.elapsed());
    }
    stop.store(true, Ordering::Relaxed);
    for f in flood {
        f.await.unwrap();
    }
    assert!(done.load(Ordering::Relaxed) > 0);
    // A quiet request waits for at most about one running heavy request, not the whole queue.
    let bound = alone * 3 + Duration::from_millis(200);
    assert!(worst < bound, "quiet tenant waited {worst:?} (one heavy request alone: {alone:?})");
}

#[tokio::test]
async fn in_flight_bytes_are_budgeted_per_tenant() {
    let t = start(NodeConfig::default(), ListenOptions::default()).await;
    let mut spec = search_server::ClusterSpec::new("c1").with_password("pw1");
    spec.max_in_flight_bytes = Some(1000);
    t.catalog.insert(spec);
    let auth = basic("c1", "pw1");
    // A stalled upload holds its declared bytes...
    let mut stalled = tokio::net::TcpStream::connect(t.addr).await.unwrap();
    use tokio::io::AsyncWriteExt;
    stalled
        .write_all(format!("POST /i/_doc HTTP/1.1\r\nHost: x\r\nAuthorization: {auth}\r\nContent-Type: application/json\r\nContent-Length: 900\r\n\r\n{{").as_bytes())
        .await
        .unwrap();
    tokio::time::sleep(Duration::from_millis(100)).await;
    // ...so the next large one is rejected like Elasticsearch's indexing pressure,
    let big = format!("{{\"f\":\"{}\"}}", "x".repeat(300));
    let r = send(t.addr, "POST", "/i/_doc", &[("Authorization", &auth), ("Content-Type", "application/json")], big.as_bytes()).await;
    assert_eq!(r.status, 429, "{}", r.text());
    assert!(r.text().contains("rejected execution of coordinating operation"), "{}", r.text());
    // ...while another tenant is unaffected.
    let r = send(t.addr, "POST", "/i/_doc", &[("Authorization", &basic("c2", "pw2")), ("Content-Type", "application/json")], big.as_bytes()).await;
    assert_eq!(r.status, 201, "{}", r.text());
    drop(stalled);
    tokio::time::sleep(Duration::from_millis(100)).await;
    let r = send(t.addr, "POST", "/i/_doc", &[("Authorization", &auth), ("Content-Type", "application/json")], big.as_bytes()).await;
    assert_eq!(r.status, 201, "{}", r.text());
}
