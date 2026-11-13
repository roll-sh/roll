//! The lease protocol on every backend (Memory, Postgres, DynamoDB), one suite run on each:
//! routing to one owner, two contenders racing, takeover after a node dies, self-fencing when
//! the backend is unreachable, handoff on drain, and a pinned node id waiting for its previous
//! incarnation.

mod common;

use common::*;
use roll_cluster::{Cluster, Error, Ownership, Route, placement};
use std::collections::HashMap;
use std::sync::{Arc, Barrier};
use std::time::{Duration, Instant};

macro_rules! on_every_backend {
    ($($name:ident),* $(,)?) => {
        mod memory { $(#[test] fn $name() { super::$name(super::Kind::Memory) })* }
        mod postgres { $(#[test] fn $name() { super::$name(super::Kind::Postgres) })* }
        mod dynamodb { $(#[test] fn $name() { super::$name(super::Kind::DynamoDb) })* }
    };
}

on_every_backend!(
    every_node_routes_a_key_to_the_same_owner,
    exactly_one_contender_wins,
    a_dead_nodes_keys_move_to_their_preferred_live_node,
    a_node_cut_off_from_the_backend_fences_itself_first_and_recovers,
    draining_hands_everything_over,
    a_pinned_node_id_waits_for_its_previous_incarnation,
);

fn rt() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_multi_thread().worker_threads(2).enable_all().build().unwrap()
}

/// Whichever node a request arrives at, it's routed to one owner: the preferred node, which
/// takes the key on first use; nothing moves while membership is stable.
fn every_node_routes_a_key_to_the_same_owner(kind: Kind) {
    let Some(env) = Env::new(kind, "route") else { return };
    let nodes = [env.join("a"), env.join("b"), env.join("c")];
    converge(&nodes);
    let keys: Vec<String> = (0..12).map(|i| format!("repo_{i}")).collect();
    for key in &keys {
        let want = placement::rendezvous(key, ["a", "b", "c"]).unwrap();
        // A node that isn't the owner points at the preferred node.
        let first = nodes.iter().find(|n| n.node_id() != want).unwrap();
        match first.route(key).unwrap() {
            Route::Forward(n) => assert_eq!(n.id, want),
            r => panic!("{key}: {r:?}"),
        }
        let owner = nodes.iter().find(|n| n.node_id() == want).unwrap();
        assert!(matches!(owner.route(key).unwrap(), Route::Local(_)));
        for n in &nodes {
            match n.route(key).unwrap() {
                Route::Local(g) => assert_eq!((n.node_id(), g.epoch()), (want, 1)),
                Route::Forward(o) => assert_eq!(o.id, want),
                r => panic!("{key}: {r:?}"),
            }
        }
    }
    std::thread::sleep(nodes[0].config().rebalance_every * 2);
    for key in &keys {
        let want = placement::rendezvous(key, ["a", "b", "c"]).unwrap();
        assert_eq!(record(&nodes[0], key), Some((want.to_string(), 1)), "{key}");
        assert!(nodes.iter().all(|n| n.owns(key) == (n.node_id() == want)), "{key}");
    }
}

/// Contenders race for one key: exactly one wins each round, and the epoch goes up by one each
/// time the holder changes (not when the holder takes it again).
fn exactly_one_contender_wins(kind: Kind) {
    let Some(env) = Env::new(kind, "race") else { return };
    let quiet = |id: &str| {
        let mut c = env.config(id);
        // No sweeping or rebalancing: only the racers move the key.
        c.sweep_every = Duration::from_secs(60);
        c.rebalance_every = Duration::from_secs(60);
        env.join_with(c)
    };
    let nodes: Vec<Arc<Cluster>> = (0..4).map(|i| quiet(&format!("n{i}"))).collect();
    let mut alive: Vec<usize> = (0..4).collect();
    for round in 0..3 {
        let barrier = Barrier::new(alive.len());
        let results: Vec<(usize, Option<i64>)> = std::thread::scope(|s| {
            let hs: Vec<_> = alive
                .iter()
                .map(|&i| {
                    let (n, b) = (&nodes[i], &barrier);
                    s.spawn(move || {
                        b.wait();
                        (i, n.acquire("repo").unwrap())
                    })
                })
                .collect();
            hs.into_iter().map(|h| h.join().unwrap()).collect()
        });
        let winners: Vec<&(usize, Option<i64>)> = results.iter().filter(|(_, e)| e.is_some()).collect();
        assert_eq!(winners.len(), 1, "round {round}: {results:?}");
        let (w, epoch) = (winners[0].0, winners[0].1.unwrap());
        assert_eq!(epoch, round + 1, "round {round}: a new holder bumps the epoch by one");
        for &i in &alive {
            assert_eq!(nodes[i].owns("repo"), i == w, "round {round}: node n{i}");
        }
        assert_eq!(record(&nodes[w], "repo"), Some((format!("n{w}"), epoch)));
        assert_eq!(nodes[w].acquire("repo").unwrap(), Some(epoch), "taking it again keeps the epoch");
        let loser = *alive.iter().find(|&&i| i != w).unwrap();
        assert_eq!(nodes[loser].acquire("repo").unwrap(), None, "a held lease can't be taken");
        // The holder leaves, which releases the key for the next round.
        nodes[w].leave();
        alive.retain(|&i| i != w);
    }
}

/// A node that dies without cleaning up loses its keys to their preferred live nodes, with a
/// new epoch, and never while it could still have been acting on them. The nodes that take
/// them count takeovers; the frozen one counts what it fenced.
fn a_dead_nodes_keys_move_to_their_preferred_live_node(kind: Kind) {
    let Some(env) = Env::new(kind, "dead") else { return };
    let (a, b, c) = (env.join("a"), env.join("b"), env.join("c"));
    converge(&[a.clone(), b.clone(), c.clone()]);
    let keys = keys_for(&["a", "b", "c"], "a", "repo", 4);
    for k in &keys {
        assert!(matches!(a.route(k).unwrap(), Route::Local(_)));
    }
    // Let a renewal go through, then freeze a (its threads stop; the watchdog fences it).
    std::thread::sleep(a.config().renew_every * 2);
    let spans = Spans::watch(&[a.clone(), b.clone(), c.clone()], &keys[0]);
    let frozen = Instant::now();
    a.fault_freeze(true);
    let validity = a.config().lease_validity();
    assert!(eventually(validity + Duration::from_secs(1), || !a.owns(&keys[0]) && a.stats().fenced >= 1), "a never fenced itself");
    a.fault_kill();
    assert!(eventually(Duration::from_secs(15), || keys.iter().all(|k| b.owns(k) || c.owns(k))), "keys never moved");
    let spans = spans.stop();
    let taken = spans.iter().filter(|s| s.0 != "a").map(|s| s.1).min().unwrap();
    assert!(taken >= frozen + a.config().lease_margin, "taken {:?} after the freeze", taken - frozen);
    eprintln!("{kind:?}: taken over {:?} after the freeze (lease {:?})", taken - frozen, a.config().lease_ttl);
    // Within a lease and one sweep (plus the heartbeat that first sees the silence, and a second
    // for slow calls), not at some later periodic scan.
    let cfg = a.config();
    let bound = cfg.lease_ttl + cfg.heartbeat + cfg.sweep_every + Duration::from_secs(1);
    assert!(taken - frozen <= bound, "taken {:?} after the freeze; a lease and a sweep is {bound:?}", taken - frozen);
    for k in &keys {
        let want = placement::rendezvous(k, ["b", "c"]).unwrap();
        assert_eq!(record(&b, k), Some((want.to_string(), 2)), "{k} went to its preferred live node");
    }
    assert_eq!(b.stats().takeovers + c.stats().takeovers, keys.len() as u64, "{:?} {:?}", b.stats(), c.stats());
}

/// A node that can't reach the backend stops acting as owner (Lost, unhealthy) before any other
/// node takes its key; when the backend is back, it gets the key back by rebalancing.
fn a_node_cut_off_from_the_backend_fences_itself_first_and_recovers(kind: Kind) {
    let Some(env) = Env::new(kind, "cutoff") else { return };
    let (a, b) = (env.join("a"), env.join("b"));
    converge(&[a.clone(), b.clone()]);
    let key = key_for(&["a", "b"], "a", "repo");
    let mut events = a.subscribe_ownership();
    assert!(matches!(a.route(&key).unwrap(), Route::Local(_)));
    assert!(a.is_healthy());
    let spans = Spans::watch(&[a.clone(), b.clone()], &key);

    a.fault_db(true);
    assert!(eventually(Duration::from_secs(10), || !a.owns(&key)), "a never fenced itself");
    // The lease can lapse at its local deadline a moment before the failing heartbeat marks the
    // node unhealthy (under load either comes first); fencing before b takes over is checked by
    // the spans below.
    assert!(eventually(Duration::from_secs(5), || !a.is_healthy()), "a never went unhealthy");
    match a.route(&key) {
        Ok(Route::Local(_)) => panic!("routed locally without a valid lease"),
        Ok(_) | Err(Error::Fault(_)) => {}
        Err(e) => panic!("{e}"),
    }
    let mut got = Vec::new();
    assert!(eventually(Duration::from_secs(2), || {
        got.extend(drain_events(&mut events));
        got.len() >= 2
    }));
    assert_eq!(got, vec![Ownership::Acquired { key: key.clone(), epoch: 1 }, Ownership::Lost { key: key.clone() }]);
    assert!(eventually(Duration::from_secs(15), || b.owns(&key)), "b never took over");

    a.fault_db(false);
    assert!(eventually(Duration::from_secs(15), || a.owns(&key)), "a never got its key back");
    assert!(eventually(Duration::from_secs(2), || a.is_healthy()));
    assert!(!b.owns(&key));
    spans.stop();
    assert_eq!(drain_events(&mut events), vec![Ownership::Acquired { key: key.clone(), epoch: 3 }]);
}

/// Draining hands every key to the remaining nodes, each taken by its new preferred node soon
/// after it was released (not after a lease's length), and the node then leaves. Handoffs count
/// as handoffs, never as takeovers or failures.
fn draining_hands_everything_over(kind: Kind) {
    let Some(env) = Env::new(kind, "drain") else { return };
    let nodes: HashMap<&str, Arc<Cluster>> = [("a", env.join("a")), ("b", env.join("b")), ("c", env.join("c"))].into_iter().collect();
    converge(&nodes.values().cloned().collect::<Vec<_>>());
    let keys: Vec<String> = (0..24).map(|i| format!("repo_{i:02}")).collect();
    for k in &keys {
        let owner = placement::rendezvous(k, ["a", "b", "c"]).unwrap();
        assert!(matches!(nodes[owner].route(k).unwrap(), Route::Local(_)), "{k}");
    }
    let a = nodes["a"].clone();
    let a_keys: Vec<String> = keys.iter().filter(|k| a.owns(k)).cloned().collect();
    assert!(a_keys.len() >= 4, "{}", a_keys.len());
    let started = Instant::now();
    rt().block_on(a.drain(Duration::from_secs(10)));
    assert!(a.owned().is_empty() && !a.is_healthy());
    let moved = eventually(Duration::from_secs(10), || a_keys.iter().all(|k| nodes["b"].owns(k) || nodes["c"].owns(k)));
    let took = started.elapsed();
    assert!(moved, "keys left unowned");
    eprintln!("{kind:?}: drained and taken over {} keys in {took:?}", a_keys.len());
    assert!(took < a.config().lease_ttl * 2, "released keys are taken at once, not after they'd have expired: {took:?}");
    for k in &a_keys {
        let want = placement::rendezvous(k, ["b", "c"]).unwrap();
        assert_eq!(record(&nodes["b"], k), Some((want.to_string(), 2)), "{k}");
    }
    for k in keys.iter().filter(|k| !a_keys.contains(k)) {
        assert_eq!(record(&nodes["b"], k).unwrap().1, 1, "{k} didn't move");
    }
    let s = a.stats();
    assert_eq!((s.handoffs, s.handoff_failures), (a_keys.len() as u64, 0), "{s:?}");
    assert_eq!(nodes["b"].stats().takeovers + nodes["c"].stats().takeovers, 0);
    assert!(eventually(Duration::from_secs(5), || nodes["b"].nodes().iter().all(|n| n.id != "a")));
}

/// A pinned node id can't run twice: a second process waits for the first to look dead (and
/// fails if it doesn't), takes over its keys as a new holder, and the first, waking up, owns
/// nothing and can't take leases.
fn a_pinned_node_id_waits_for_its_previous_incarnation(kind: Kind) {
    let Some(env) = Env::new(kind, "pinned") else { return };
    let a1 = env.join("a");
    assert_eq!(a1.acquire("k").unwrap(), Some(1));
    let started = Instant::now();
    let err = Cluster::join(env.backend(), env.config("a")).unwrap_err();
    assert!(matches!(err, Error::NodeIdInUse(_)), "{err}");
    assert!(started.elapsed() >= a1.config().lease_ttl, "it waited for the other process first");

    a1.fault_freeze(true);
    let a2 = env.join("a");
    assert!(eventually(Duration::from_secs(15), || a2.owns("k")));
    assert_eq!(a2.epoch("k"), Some(2), "a new incarnation is a new holder");
    assert!(!a1.owns("k"));

    a1.fault_freeze(false);
    assert!(eventually(Duration::from_secs(5), || !a1.is_healthy()));
    assert_eq!(a1.acquire("other").unwrap(), None, "the replaced process can't take leases");
    std::thread::sleep(a2.config().lease_ttl);
    assert!(a2.owns("k") && a2.is_healthy());
    assert_eq!(record(&a2, "k").unwrap().1, 2);
    drop(a1);
    // The old process leaving doesn't remove the new one.
    std::thread::sleep(a2.config().heartbeat * 3);
    assert!(a2.is_healthy() && a2.nodes().iter().any(|n| n.id == "a"));
}
