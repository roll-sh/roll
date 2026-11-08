//! The state machine on the in-process backend (the same on every backend, so tested once):
//! rebalancing to a joining node with a guard holding one key back, the handoff timeout,
//! singletons, retiring and idle keys, leaving, and a short outage costing nothing.

mod common;

use roll_cluster::{Cluster, Ownership, Route, placement};
use common::*;
use parking_lot::Mutex;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

fn rt() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_multi_thread().worker_threads(2).enable_all().build().unwrap()
}

struct Lease {
    node: String,
    epoch: i64,
}

fn rec(n: &Cluster, k: &str) -> Option<Lease> {
    record(n, k).map(|(node, epoch)| Lease { node, epoch })
}

/// A joining node gets the keys it's now preferred for by handoff, a few per round, never
/// owned by two nodes at once; a guard held on one key holds that key back until it's dropped.
#[test]
fn a_joining_node_gets_its_keys_by_handoff_and_a_guard_holds_one_back() {
    let env = Env::new(Kind::Memory, "join").unwrap();
    let cfg = |id: &str| {
        let mut c = env.config(id);
        c.handoff_timeout = Duration::from_secs(60);
        c
    };
    let a = env.join_with(cfg("a"));
    let keys: Vec<String> = (0..40).map(|i| format!("repo_{i:02}")).collect();
    for k in &keys {
        assert!(matches!(a.route(k).unwrap(), Route::Local(_)));
    }
    let (to_b, stay): (Vec<String>, Vec<String>) = keys.iter().cloned().partition(|k| placement::rendezvous(k, ["a", "b"]) == Some("b"));
    assert!(to_b.len() >= 8 && stay.len() >= 8, "{} / {}", to_b.len(), stay.len());
    let held = to_b[0].clone();
    let guard = a.guard(&held).unwrap();
    let ending = guard.ending();
    let mut a_events = a.subscribe_ownership();
    assert_eq!(drain_events(&mut a_events).len(), keys.len(), "subscribing replays what's held");

    let b = env.join_with(cfg("b"));
    let mut b_events = b.subscribe_ownership();
    let rest = &to_b[1..];
    let moved = eventually(Duration::from_secs(20), || {
        for k in &keys {
            // b first: keys only move from a to b here, so both true means a real overlap.
            let on_b = b.owns(k);
            let on_a = a.owns(k);
            assert!(!(on_a && on_b), "{k} owned by a and b at once");
        }
        rest.iter().all(|k| b.owns(k))
    });
    assert!(moved, "not every key moved to b");

    // The guarded key is still a's, being handed off: no new guards, b forwards to a.
    assert!(a.owns(&held));
    assert!(guard.is_ending() && !guard.is_valid());
    assert!(matches!(a.route(&held).unwrap(), Route::Unavailable { .. }));
    assert!(a.guard(&held).is_none());
    assert!(matches!(b.route(&held).unwrap(), Route::Forward(n) if n.id == "a"));
    rt().block_on(async { tokio::time::timeout(Duration::from_secs(1), ending).await }).expect("the guard's ending future resolves");
    assert_eq!(rec(&a, &held).unwrap().node, "a");

    let t = Instant::now();
    drop(guard);
    assert!(eventually(Duration::from_secs(5), || b.owns(&held)), "the key never moved after its guard was dropped");
    eprintln!("moved {:?} after the guard was dropped", t.elapsed());
    assert!(!a.owns(&held));

    for k in &stay {
        assert!(a.owns(k), "{k} should have stayed");
        assert_eq!(rec(&a, k).unwrap().epoch, 1);
    }
    let mut lost: Vec<String> = drain_events(&mut a_events)
        .into_iter()
        .map(|e| match e {
            Ownership::Lost { key } => key,
            e => panic!("a: unexpected {e:?}"),
        })
        .collect();
    lost.sort();
    assert_eq!(lost, to_b);
    let mut got: Vec<(String, i64)> = drain_events(&mut b_events)
        .into_iter()
        .map(|e| match e {
            Ownership::Acquired { key, epoch } => (key, epoch),
            e => panic!("b: unexpected {e:?}"),
        })
        .collect();
    got.sort();
    assert_eq!(got, to_b.iter().map(|k| (k.clone(), 2)).collect::<Vec<_>>());
}

/// A guard that's never dropped holds a handoff back for `handoff_timeout`, not forever.
#[test]
fn a_handoff_goes_ahead_after_its_timeout() {
    let env = Env::new(Kind::Memory, "hotimeout").unwrap();
    let timeout = Duration::from_millis(800) * env.scale();
    let cfg = |id: &str| {
        let mut c = env.config(id);
        c.handoff_timeout = timeout;
        c
    };
    let a = env.join_with(cfg("a"));
    let key = key_for(&["a", "b"], "b", "repo");
    assert!(matches!(a.route(&key).unwrap(), Route::Local(_)));
    let guard = a.guard(&key).unwrap();
    let b = env.join_with(cfg("b"));
    assert!(eventually(Duration::from_secs(5), || guard.is_ending()), "the handoff never started");
    let started = Instant::now();
    assert!(eventually(Duration::from_secs(10), || b.owns(&key)));
    let waited = started.elapsed();
    assert!(waited + Duration::from_millis(100) >= timeout, "moved after {waited:?}, before the timeout");
    assert!(!a.owns(&key) && !guard.is_valid());
    drop(guard);
    assert!(b.owns(&key));
}

/// A singleton has one holder at a time, the preferred node, and moves when it dies.
#[test]
fn a_singleton_has_one_holder_and_moves_when_it_dies() {
    let env = Env::new(Kind::Memory, "singleton").unwrap();
    let ids = ["a", "b", "c"];
    let nodes: Vec<Arc<Cluster>> = ids.iter().map(|id| env.join(id)).collect();
    converge(&nodes);
    // Per node: runs of consecutive `true` answers, as (first, last) sample times.
    type Runs = Vec<Vec<(Instant, Instant)>>;
    let runs: Arc<Mutex<Runs>> = Arc::new(Mutex::new(vec![Vec::new(); 3]));
    let stop = Arc::new(AtomicBool::new(false));
    let threads: Vec<_> = nodes
        .iter()
        .enumerate()
        .map(|(i, n)| {
            let (n, runs, stop) = (n.clone(), runs.clone(), stop.clone());
            std::thread::spawn(move || {
                let mut was = false;
                while !stop.load(Ordering::SeqCst) {
                    let mine = n.singleton("lander");
                    let t = Instant::now();
                    let mut r = runs.lock();
                    match (was, mine) {
                        (false, true) => r[i].push((t, t)),
                        (true, true) => r[i].last_mut().unwrap().1 = t,
                        _ => {}
                    }
                    drop(r);
                    was = mine;
                    std::thread::sleep(Duration::from_millis(5));
                }
            })
        })
        .collect();
    let key = "singleton:lander";
    let holders = || nodes.iter().filter(|n| n.owns(key)).count();
    assert!(eventually(Duration::from_secs(3), || holders() == 1));
    let first = placement::rendezvous(key, ids).unwrap();
    std::thread::sleep(nodes[0].config().rebalance_every * 3);
    assert_eq!(holders(), 1);
    let holder = nodes.iter().position(|n| n.owns(key)).unwrap();
    assert_eq!(ids[holder], first, "the preferred node holds it");

    nodes[holder].fault_kill();
    let rest: Vec<&str> = ids.iter().copied().filter(|id| *id != first).collect();
    let second = placement::rendezvous(key, rest.iter().copied()).unwrap();
    assert!(eventually(Duration::from_secs(8), || nodes.iter().any(|n| n.node_id() == second && n.owns(key))), "it never moved");
    std::thread::sleep(Duration::from_millis(200));
    stop.store(true, Ordering::SeqCst);
    for t in threads {
        t.join().unwrap();
    }
    let runs = runs.lock();
    let all: Vec<(usize, (Instant, Instant))> = runs.iter().enumerate().flat_map(|(i, r)| r.iter().map(move |x| (i, *x))).collect();
    for (i, x) in &all {
        for (j, y) in &all {
            if i != j {
                assert!(x.1 < y.0 || y.1 < x.0, "{} and {} both held it: {x:?} {y:?}", ids[*i], ids[*j]);
            }
        }
    }
    assert!(runs[holder].len() == 1 && runs[ids.iter().position(|id| *id == second).unwrap()].len() == 1, "{runs:?}");
}

/// Retiring a key (its repo was deleted) waits for the guard in flight, emits `Lost` and deletes
/// the lease row: no sweep takes it back, and it's acquired again only when it's asked for.
#[test]
fn a_retired_key_loses_its_lease_row_and_comes_back_only_on_demand() {
    let env = Env::new(Kind::Memory, "retire").unwrap();
    let a = env.join("a");
    let b = env.join("b");
    converge(&[a.clone(), b.clone()]);
    let k = key_for(&["a", "b"], "a", "repo");
    assert!(matches!(a.route(&k).unwrap(), Route::Local(_)));
    let mut events = a.subscribe_ownership();
    let _ = drain_events(&mut events);
    let guard = a.guard(&k).unwrap();
    assert!(a.retire(&k));
    // The guard holds it back; no new guards meanwhile.
    std::thread::sleep(Duration::from_millis(300));
    assert!(rec(&a, &k).is_some() && a.guard(&k).is_none());
    drop(guard);
    assert!(eventually(Duration::from_secs(5), || rec(&a, &k).is_none()), "the row is deleted");
    assert_eq!(drain_events(&mut events), vec![Ownership::Lost { key: k.clone() }]);
    // Several sweeps later nobody has it.
    std::thread::sleep(env.config("x").sweep_every * 5);
    assert!(rec(&a, &k).is_none() && !a.owns(&k) && !b.owns(&k));
    // A request brings it back (on its preferred node).
    assert!(matches!(a.route(&k).unwrap(), Route::Local(_)));
    assert_eq!(rec(&a, &k).unwrap().node, "a");
}

/// An idle key is given up (row deleted) only once nothing was granted for it for the idle time
/// and the caller's check agrees; a guard in flight or a recent one keeps it, and a key given up
/// is taken again by the next request.
#[test]
fn idle_keys_are_released_and_reacquired_on_demand() {
    let env = Env::new(Kind::Memory, "idle").unwrap();
    let a = env.join("a");
    let keys = keys_for(&["a"], "a", "repo", 3);
    for k in &keys {
        assert!(matches!(a.route(k).unwrap(), Route::Local(_)));
    }
    let idle = Duration::from_millis(400);
    assert!(a.idle(idle).is_empty(), "just used");
    let busy = a.guard(&keys[0]).unwrap();
    std::thread::sleep(idle + Duration::from_millis(100));
    // keys[0] has a guard in flight: not idle. keys[1]: the check says there's work left.
    assert_eq!(a.idle(idle), vec![keys[1].clone(), keys[2].clone()]);
    assert!(!a.release_if_idle(&keys[0], idle, || true));
    let mut no_guard = false;
    assert!(!a.release_if_idle(&keys[1], idle, || {
        // No guard while it looks: nothing new can start.
        no_guard = a.guard(&keys[1]).is_none();
        false
    }));
    assert!(no_guard && a.guard(&keys[1]).is_some(), "no guard while it looked; kept, and usable again");
    // The guard just granted and dropped counts as use.
    assert!(!a.release_if_idle(&keys[1], idle, || true));
    assert!(a.release_if_idle(&keys[2], idle, || true));
    assert!(eventually(Duration::from_secs(5), || rec(&a, &keys[2]).is_none() && !a.owns(&keys[2])), "given up");
    assert!(rec(&a, &keys[0]).is_some() && rec(&a, &keys[1]).is_some());
    drop(busy);
    // On demand again, with a new tenure.
    let before = rec(&a, &keys[0]).unwrap().epoch;
    assert!(matches!(a.route(&keys[2]).unwrap(), Route::Local(_)));
    assert!(a.owns(&keys[2]) && rec(&a, &keys[2]).is_some());
    assert_eq!(rec(&a, &keys[0]).unwrap().epoch, before);
}

/// A short outage (shorter than the margin) costs nothing: no `Lost`, the key stays.
#[test]
fn a_blip_shorter_than_the_lease_margin_loses_nothing() {
    let env = Env::new(Kind::Memory, "blip").unwrap();
    let a = env.join("a");
    let mut events = a.subscribe_ownership();
    assert!(a.acquire("k").unwrap().is_some());
    std::thread::sleep(a.config().renew_every * 2);
    let _ = drain_events(&mut events);
    a.fault_db(true);
    std::thread::sleep(Duration::from_millis(300));
    a.fault_db(false);
    let until = Instant::now() + a.config().lease_ttl;
    while Instant::now() < until {
        assert!(a.owns("k"));
        std::thread::sleep(Duration::from_millis(20));
    }
    assert!(drain_events(&mut events).is_empty());
}

/// Leaving gives everything up at once: `Lost`, and released leases the others take within a
/// sweep or two rather than after they'd have expired.
#[test]
fn leaving_releases_everything() {
    let env = Env::new(Kind::Memory, "leave").unwrap();
    let a = env.join("a");
    let b = env.join("b");
    converge(&[a.clone(), b.clone()]);
    let mut events = a.subscribe_ownership();
    let keys: Vec<String> = keys_for(&["a", "b"], "a", "repo", 5);
    for k in &keys {
        assert!(matches!(a.route(k).unwrap(), Route::Local(_)));
    }
    let _ = drain_events(&mut events);
    drop(a);
    let lost: Vec<Ownership> = drain_events(&mut events);
    assert_eq!(lost.len(), keys.len());
    assert!(lost.iter().all(|e| matches!(e, Ownership::Lost { .. })));
    let t = Instant::now();
    assert!(eventually(Duration::from_secs(3), || keys.iter().all(|k| b.owns(k))));
    assert!(t.elapsed() < b.config().lease_ttl, "released keys were taken before they'd have expired: {:?}", t.elapsed());
    assert_eq!(b.nodes().len(), 1);
}
