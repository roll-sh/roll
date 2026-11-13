//! roll previews on several nodes (in one process, sharing a cluster backend and a previews
//! database): a preview is converged and served by the holder of its lease, and moves when the
//! holder dies.

mod common;

use common::*;
use roll_cluster::{Cluster, ClusterConfig, Memory, MemoryStore};
use roll_previews::engine::{Config, preview_key};
use roll_previews::ports::NoSecrets;
use roll_previews::{Engine, State};
use serde_json::json;
use std::sync::Arc;
use std::time::{Duration, Instant};

fn node(store: &MemoryStore, id: &str) -> Arc<Cluster> {
    Cluster::join(Box::new(Memory::new(store.clone())), ClusterConfig::for_tests(id, "http://127.0.0.1:9", "0123456789abcdef0123")).unwrap()
}

fn eventually(within: Duration, mut f: impl FnMut() -> bool) -> bool {
    let end = Instant::now() + within;
    while Instant::now() < end {
        if f() {
            return true;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    f()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_preview_runs_on_its_leases_holder_and_moves_when_it_dies() {
    let a = env(json!({ "enabled": true }));
    let apps_b = Arc::new(FakeApps::default());
    let b = Engine::new(
        Config { domain: "preview.test".into(), ..Config::default() },
        a.engine.store.clone(),
        a.ws.clone(),
        a.code.clone(),
        vec![a.pg.clone(), a.redis.clone()],
        apps_b.clone(),
        Arc::new(NoSecrets),
        // Node a discovers (the singleton); both converge what they hold.
        Box::new(|| false),
    );
    let backend = MemoryStore::new();
    let (na, nb) = (node(&backend, "a"), node(&backend, "b"));
    assert!(eventually(Duration::from_secs(10), || na.nodes().len() == 2 && nb.nodes().len() == 2));
    let _ = a.engine.cluster.set(na.clone());
    let _ = b.cluster.set(nb.clone());

    a.code.open(change("chg_c", 3));
    for _ in 0..3 {
        a.engine.pass().await.unwrap();
        b.pass().await.unwrap();
    }
    let p = a.engine.store.row_by_change("chg_c").unwrap().unwrap();
    assert_eq!(p.state, State::Ready, "{p:?}");
    let key = preview_key(&p.id);
    let (on_a, on_b) = (!a.apps.deployed.lock().is_empty(), !apps_b.deployed.lock().is_empty());
    assert!(on_a != on_b, "deployed by exactly one node (a: {on_a}, b: {on_b})");
    assert_eq!(on_a, na.owns(&key), "by the holder of the preview's lease");
    assert_eq!(a.pg.made.load(std::sync::atomic::Ordering::SeqCst), 1, "branched once");

    // The holder dies: the other node takes the preview, and the next thing asked of it (a
    // reset, which redeploys its services) happens there.
    let (dead, survivor, survivor_apps) = if on_a { (na.clone(), &b, apps_b.clone()) } else { (nb.clone(), &a.engine, a.apps.clone()) };
    dead.fault_kill();
    survivor_apps.deployed.lock().clear();
    survivor.reset(&p.id, Vec::new()).unwrap();
    let mut moved = false;
    for _ in 0..100 {
        survivor.pass().await.unwrap();
        if !survivor_apps.deployed.lock().is_empty() {
            moved = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    assert!(moved, "the survivor never took the preview over");
    assert_eq!(survivor.store.row(&p.id).unwrap().state, State::Ready);
}
