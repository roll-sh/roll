//! Handoff: rebalancing to a joining node, guards holding a handoff back, the handoff timeout,
//! draining, and singletons.

mod common;

use code_cluster::{Cluster, Ownership, Route, placement};
use common::*;
use parking_lot::Mutex;
use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

fn rt() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_multi_thread().worker_threads(2).enable_all().build().unwrap()
}

/// A joining node gets the keys it's now preferred for by handoff, a few per round, never
/// owned by two nodes at once; a guard held on one key holds that key back until it's dropped.
#[test]
fn a_joining_node_gets_its_keys_by_handoff_and_a_guard_holds_one_back() {
    let env = Env::new("join");
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
    let mut a_events = a.subscribe();
    assert_eq!(drain_events(&mut a_events).len(), keys.len(), "subscribing replays what's held");

    let b = env.join_with(cfg("b"));
    let mut b_events = b.subscribe();
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
    assert!(matches!(b.route(&held).unwrap(), Route::Remote(n) if n.id == "a"));
    rt().block_on(async { tokio::time::timeout(Duration::from_secs(1), ending).await }).expect("the guard's ending future resolves");
    assert_eq!(env.lease(&held).unwrap().node, "a");

    let t = Instant::now();
    drop(guard);
    assert!(eventually(Duration::from_secs(5), || b.owns(&held)), "the key never moved after its guard was dropped");
    eprintln!("moved {:?} after the guard was dropped", t.elapsed());
    assert!(!a.owns(&held));

    for k in &stay {
        assert!(a.owns(k), "{k} should have stayed");
        assert_eq!(env.lease(k).unwrap().epoch, 1);
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
    let env = Env::new("hotimeout");
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

/// Drain hands every key to the remaining nodes; each is taken by its new preferred node
/// within about a sweep of being released, and the drained node's row is gone.
#[test]
fn draining_hands_everything_over_without_gaps() {
    let env = Env::new("drain");
    let nodes: HashMap<&str, Arc<Cluster>> = [("a", env.join("a")), ("b", env.join("b")), ("c", env.join("c"))].into_iter().collect();
    converge(&nodes.values().cloned().collect::<Vec<_>>());
    let keys: Vec<String> = (0..60).map(|i| format!("repo_{i:02}")).collect();
    for k in &keys {
        let owner = placement::rendezvous(k, ["a", "b", "c"]).unwrap();
        assert!(matches!(nodes[owner].route(k).unwrap(), Route::Local(_)), "{k}");
    }
    let a_keys: Vec<String> = keys.iter().filter(|k| nodes["a"].owns(k)).cloned().collect();
    assert!(a_keys.len() >= 10, "{}", a_keys.len());

    // Timestamp a's `Lost` and the others' `Acquired` as they happen.
    let times: Arc<Mutex<HashMap<(String, &'static str), Instant>>> = Arc::default();
    let rt = rt();
    for (id, kind) in [("a", "lost"), ("b", "acquired"), ("c", "acquired")] {
        let mut rx = nodes[id].subscribe();
        let _ = drain_events(&mut rx);
        let times = times.clone();
        rt.spawn(async move {
            while let Some(e) = rx.recv().await {
                let key = match (&e, kind) {
                    (Ownership::Lost { key }, "lost") | (Ownership::Acquired { key, .. }, "acquired") => key.clone(),
                    _ => panic!("unexpected {e:?}"),
                };
                times.lock().insert((key, kind), Instant::now());
            }
        });
    }

    let a = nodes["a"].clone();
    let started = Instant::now();
    rt.block_on(a.drain(Duration::from_secs(10)));
    eprintln!("drained {} keys in {:?}", a_keys.len(), started.elapsed());
    assert!(a.owned().is_empty());
    assert!(!a.is_healthy());
    assert!(!env.node_rows().contains(&"a".to_string()));

    assert!(eventually(Duration::from_secs(5), || a_keys.iter().all(|k| nodes["b"].owns(k) || nodes["c"].owns(k))), "keys left unowned");
    let sweep = nodes["b"].config().sweep_every;
    let slack = Duration::from_millis(400) * env.scale();
    // The recording tasks may still be catching up with the last events.
    let recorded = || a_keys.iter().all(|k| times.lock().contains_key(&(k.clone(), "lost")) && times.lock().contains_key(&(k.clone(), "acquired")));
    assert!(eventually(Duration::from_secs(2), recorded), "events missing");
    let times = times.lock();
    let mut worst = Duration::ZERO;
    for k in &a_keys {
        let want = placement::rendezvous(k, ["b", "c"]).unwrap();
        let row = env.lease(k).unwrap();
        assert_eq!((row.node.as_str(), row.epoch), (want, 2), "{k}");
        let lost = times[&(k.clone(), "lost")];
        let acquired = times[&(k.clone(), "acquired")];
        let gap = acquired.saturating_duration_since(lost);
        worst = worst.max(gap);
        assert!(gap <= sweep + slack, "{k} was unowned for {gap:?}");
    }
    eprintln!("longest gap between a's Lost and the new owner's Acquired: {worst:?} (sweep every {sweep:?})");
    // Keys a never had didn't move.
    for k in keys.iter().filter(|k| !a_keys.contains(k)) {
        assert_eq!(env.lease(k).unwrap().epoch, 1, "{k}");
    }
}

/// A singleton has one holder at a time, the preferred node, and moves when it dies.
#[test]
fn a_singleton_has_one_holder_and_moves_when_it_dies() {
    let env = Env::new("singleton");
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
    let env = Env::new("retire");
    let a = env.join("a");
    let b = env.join("b");
    converge(&[a.clone(), b.clone()]);
    let k = key_for(&["a", "b"], "a", "repo");
    assert!(matches!(a.route(&k).unwrap(), Route::Local(_)));
    let mut events = a.subscribe();
    let _ = drain_events(&mut events);
    let guard = a.guard(&k).unwrap();
    assert!(a.retire(&k));
    // The guard holds it back; no new guards meanwhile.
    std::thread::sleep(Duration::from_millis(300));
    assert!(env.lease(&k).is_some() && a.guard(&k).is_none());
    drop(guard);
    assert!(eventually(Duration::from_secs(5), || env.lease(&k).is_none()), "the row is deleted");
    assert_eq!(drain_events(&mut events), vec![Ownership::Lost { key: k.clone() }]);
    // Several sweeps later nobody has it.
    std::thread::sleep(env.config("x").sweep_every * 5);
    assert!(env.lease(&k).is_none() && !a.owns(&k) && !b.owns(&k));
    // A request brings it back (on its preferred node).
    assert!(matches!(a.route(&k).unwrap(), Route::Local(_)));
    assert_eq!(env.lease(&k).unwrap().node, "a");
}

/// An idle key is given up (row deleted) only once nothing was granted for it for the idle time
/// and the caller's check agrees; a guard in flight or a recent one keeps it, and a key given up
/// is taken again by the next request.
#[test]
fn idle_keys_are_released_and_reacquired_on_demand() {
    let env = Env::new("idle");
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
    assert!(eventually(Duration::from_secs(5), || env.lease(&keys[2]).is_none() && !a.owns(&keys[2])), "given up");
    assert!(env.lease(&keys[0]).is_some() && env.lease(&keys[1]).is_some());
    drop(busy);
    // On demand again, with a new tenure.
    let before = env.lease(&keys[0]).unwrap().epoch;
    assert!(matches!(a.route(&keys[2]).unwrap(), Route::Local(_)));
    assert!(a.owns(&keys[2]) && env.lease(&keys[2]).is_some());
    assert_eq!(env.lease(&keys[0]).unwrap().epoch, before);
}
