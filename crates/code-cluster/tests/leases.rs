//! Leases: the compare-and-swap, epochs, self-fencing before server-side expiry, takeover,
//! database outages and pinned node ids.

mod common;

use code_cluster::{Cluster, Error, Ownership, Route};
use common::*;
use std::sync::{Arc, Barrier};
use std::time::{Duration, Instant};

/// N nodes race for one key: exactly one wins every round, and the epoch goes up by one each
/// time the holder changes (and not when the holder takes it again).
#[test]
fn exactly_one_racer_wins_and_epochs_rise() {
    let env = Env::new("race");
    let quiet = |id: &str| {
        let mut c = env.config(id);
        // No sweeping or rebalancing: only the racers move the key.
        c.sweep_every = Duration::from_secs(60);
        c.rebalance_every = Duration::from_secs(60);
        env.join_with(c)
    };
    let nodes: Vec<Arc<Cluster>> = (0..8).map(|i| quiet(&format!("n{i}"))).collect();
    let mut alive: Vec<usize> = (0..8).collect();
    let mut last_epoch = 0;
    for round in 0..5 {
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
        assert_eq!(epoch, last_epoch + 1, "round {round}: a new holder bumps the epoch by one");
        for &i in &alive {
            assert_eq!(nodes[i].owns("repo"), i == w, "round {round}: node n{i}");
        }
        let row = env.lease("repo").unwrap();
        assert_eq!((row.node.as_str(), row.epoch), (format!("n{w}").as_str(), epoch));
        // Taking it again, or acquiring what we hold, doesn't change the epoch.
        assert_eq!(nodes[w].acquire("repo").unwrap(), Some(epoch));
        // Losers really lost: their CAS can't take a lease that hasn't expired.
        let loser = *alive.iter().find(|&&i| i != w).unwrap();
        assert_eq!(nodes[loser].acquire("repo").unwrap(), None);
        // The holder leaves, which releases the key for the next round.
        nodes[w].leave();
        assert!(!nodes[w].owns("repo"));
        alive.retain(|&i| i != w);
        last_epoch = epoch;
    }
}

/// The epoch also goes up when the same key goes back to a node that held it before.
#[test]
fn epoch_rises_on_every_change_of_holder_even_back_and_forth() {
    let env = Env::new("epochs");
    let mk = |id: &str| {
        let mut c = env.config(id);
        c.sweep_every = Duration::from_secs(60);
        c.rebalance_every = Duration::from_secs(60);
        env.join_with(c)
    };
    let (a, b) = (mk("a"), mk("b"));
    for (i, expect) in (0..4).zip(1..) {
        let (take, other) = if i % 2 == 0 { (&a, &b) } else { (&b, &a) };
        assert_eq!(take.acquire("k").unwrap(), Some(expect), "turn {i}");
        assert!(other.acquire("k").unwrap().is_none());
        assert!(take.handoff("k"));
        assert!(eventually(Duration::from_secs(3), || !take.owns("k") && env.lease("k").is_some_and(|l| l.expires_at <= env.now())));
    }
}

/// When `node`'s lease ran out: its last state before another node held the key.
fn last_expiry_of(seen: &[(i64, Option<Lease>)], node: &str) -> i64 {
    let states: Vec<&Lease> = seen.iter().filter_map(|(_, l)| l.as_ref()).collect();
    let takeover = states.iter().position(|l| l.node != node).unwrap_or(states.len());
    states[..takeover].iter().rev().find(|l| l.node == node).map(|l| l.expires_at).unwrap()
}

fn first_seen_holder(seen: &[(i64, Option<Lease>)], node: &str) -> Option<Lease> {
    seen.iter().find_map(|(_, l)| l.clone().filter(|l| l.node == node))
}

/// A node whose lease thread stalls fences itself (owns → false, `Lost`) at its local
/// deadline, before the lease expires in the database; the preferred live node takes the key
/// only after that expiry.
#[test]
fn a_stalled_node_fences_itself_before_its_lease_expires() {
    let env = Env::new("stall");
    let a = env.join("a");
    let b = env.join("b");
    let key = key_for(&["a", "b"], "a", "repo");
    let mut a_events = a.subscribe();
    let mut b_events = b.subscribe();
    assert!(matches!(a.route(&key).unwrap(), Route::Local(_)));
    assert!(matches!(b.route(&key).unwrap(), Route::Remote(n) if n.id == "a"));
    let monitor = Monitor::start(&env, &key);

    a.fault_freeze(true);
    assert!(eventually(Duration::from_secs(5), || !a.owns(&key)), "a never fenced itself");
    let fenced_at = env.now();
    assert!(!matches!(a.route(&key).unwrap(), Route::Local(_)), "a fenced node never routes locally");
    assert!(a.guard(&key).is_none());
    let mut got = Vec::new();
    assert!(eventually(Duration::from_secs(1), || {
        got.extend(drain_events(&mut a_events));
        got.len() >= 2
    }));
    assert_eq!(got, vec![Ownership::Acquired { key: key.clone(), epoch: 1 }, Ownership::Lost { key: key.clone() }]);

    assert!(eventually(Duration::from_secs(5), || b.owns(&key)), "b never took over");
    let seen = monitor.stop();
    // a's lease as it was last renewed (a renewal may have been in flight when it froze).
    let expiry = last_expiry_of(&seen, "a");
    assert!(fenced_at < expiry, "a fenced at {fenced_at}, after its lease expired at {expiry}");
    let taken = first_seen_holder(&seen, "b").unwrap();
    let taken_at = taken.expires_at - env.ttl();
    assert!(taken_at >= expiry, "b took the lease at {taken_at}, before it expired at {expiry}");
    assert_eq!(taken.epoch, 2);
    assert!(fenced_at < taken_at);
    eprintln!("fenced {} ms before expiry; taken over {} ms after it", expiry - fenced_at, taken_at - expiry);
    assert_eq!(drain_events(&mut b_events), vec![Ownership::Acquired { key: key.clone(), epoch: 2 }]);

    // Back from its stall, a sees it lost the key and doesn't come back as its owner until
    // rebalancing hands the key back.
    a.fault_freeze(false);
    assert!(eventually(Duration::from_secs(5), || a.owns(&key)), "rebalancing never returned the key");
    let row = env.lease(&key).unwrap();
    assert_eq!((row.node.as_str(), row.epoch), ("a", 3));
    assert!(!b.owns(&key));
}

/// A node that dies without cleaning up loses its keys to the preferred live node, after the
/// lease expires.
#[test]
fn a_dead_node_s_keys_move_after_expiry() {
    let env = Env::new("dead");
    let a = env.join("a");
    let b = env.join("b");
    let c = env.join("c");
    let keys = keys_for(&["a", "b", "c"], "a", "repo", 6);
    for k in &keys {
        assert!(matches!(a.route(k).unwrap(), Route::Local(_)));
    }
    let monitors: Vec<Monitor> = keys.iter().map(|k| Monitor::start(&env, k)).collect();
    a.fault_kill();
    // Once a looks dead, but before its lease runs out, nobody serves its keys.
    let unavailable = eventually(Duration::from_secs(3), || matches!(b.route(&keys[0]), Ok(Route::Unavailable { .. })));
    assert!(unavailable || b.owns(&keys[0]) || c.owns(&keys[0]), "b kept routing to a dead node");
    assert!(eventually(Duration::from_secs(6), || keys.iter().all(|k| b.owns(k) || c.owns(k))), "keys never moved");
    for (k, m) in keys.iter().zip(monitors) {
        let seen = m.stop();
        let expiry = last_expiry_of(&seen, "a");
        let new = seen.iter().filter_map(|(_, l)| l.clone()).find(|l| l.node != "a").unwrap();
        assert!(new.expires_at - env.ttl() >= expiry, "{k} taken before a's lease expired at {expiry}: {new:?}");
        let want = code_cluster::placement::rendezvous(k, ["b", "c"]).unwrap();
        assert_eq!(new.node, want, "{k} went to its preferred live node");
        assert_eq!(new.epoch, 2);
    }
}

/// When one node can't reach the database, it stops routing locally and emits `Lost` before
/// any other node can take its keys; when the database comes back it recovers them.
#[test]
fn a_node_cut_off_from_the_database_fences_first_and_recovers() {
    let env = Env::new("cutoff");
    let a = env.join("a");
    let b = env.join("b");
    let key = key_for(&["a", "b"], "a", "repo");
    let mut events = a.subscribe();
    assert!(matches!(a.route(&key).unwrap(), Route::Local(_)));
    assert!(a.is_healthy());
    let monitor = Monitor::start(&env, &key);

    a.fault_db(true);
    assert!(eventually(Duration::from_secs(5), || !a.owns(&key)), "a never fenced itself");
    let fenced_at = env.now();
    assert!(!a.is_healthy());
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

    assert!(eventually(Duration::from_secs(5), || b.owns(&key)), "b never took over");
    let seen = monitor.stop();
    let expiry = last_expiry_of(&seen, "a");
    assert!(fenced_at < expiry, "fenced at {fenced_at}, lease expired at {expiry}");
    let taken = first_seen_holder(&seen, "b").unwrap_or_else(|| panic!("{seen:?}"));
    assert!(taken.expires_at - env.ttl() >= expiry, "b took the lease before it expired");

    a.fault_db(false);
    assert!(eventually(Duration::from_secs(6), || a.owns(&key)), "a never got its key back");
    assert!(eventually(Duration::from_secs(1), || a.is_healthy()));
    assert!(!b.owns(&key));
    let evs = drain_events(&mut events);
    assert_eq!(evs, vec![Ownership::Acquired { key: key.clone(), epoch: 3 }], "{evs:?}");
}

/// A short outage (shorter than the margin) costs nothing: no `Lost`, the key stays.
#[test]
fn a_blip_shorter_than_the_lease_margin_loses_nothing() {
    let env = Env::new("blip");
    let a = env.join("a");
    let mut events = a.subscribe();
    assert!(a.acquire("k").unwrap().is_some());
    // Let a renewal go through first (the acquisition may have been slow to open a connection).
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

/// A pinned node id can't run twice: a second process waits for the first to look dead, and
/// a first process that wakes up after being replaced owns nothing.
#[test]
fn a_pinned_node_id_is_never_used_by_two_live_processes() {
    let env = Env::new("pinned");
    let a1 = env.join("a");
    assert_eq!(a1.acquire("k").unwrap(), Some(1));
    let started = Instant::now();
    let err = Cluster::join(env.node_db(), env.config("a")).unwrap_err();
    assert!(matches!(err, Error::NodeIdInUse(_)), "{err}");
    assert!(started.elapsed() >= Duration::from_secs(2), "it waited for the other process first");

    a1.fault_freeze(true);
    let a2 = env.join("a");
    // a1's lease had expired (the join waited for that); the new process takes it as a new holder.
    assert!(eventually(Duration::from_secs(6), || a2.owns("k")));
    assert_eq!(a2.epoch("k"), Some(2));
    assert!(!a1.owns("k"));

    a1.fault_freeze(false);
    assert!(eventually(Duration::from_secs(2), || !a1.is_healthy()));
    assert_eq!(a1.acquire("other").unwrap(), None, "the replaced process can't take leases");
    std::thread::sleep(a2.config().lease_ttl / 3);
    assert!(a2.owns("k") && a2.is_healthy());
    assert_eq!(env.lease("k").unwrap().epoch, 2);
    drop(a1);
    // The old process leaving doesn't remove the new one's row.
    assert_eq!(env.node_rows(), vec!["a".to_string()]);
}

/// Leaving gives everything up at once: `Lost`, released leases, no node row.
#[test]
fn leaving_releases_everything() {
    let env = Env::new("leave");
    let a = env.join("a");
    let b = env.join("b");
    let mut events = a.subscribe();
    let keys: Vec<String> = keys_for(&["a", "b"], "a", "repo", 5);
    for k in &keys {
        assert!(matches!(a.route(k).unwrap(), Route::Local(_)));
    }
    let _ = drain_events(&mut events);
    drop(a);
    let lost: Vec<Ownership> = drain_events(&mut events);
    assert_eq!(lost.len(), keys.len());
    assert!(lost.iter().all(|e| matches!(e, Ownership::Lost { .. })));
    assert_eq!(env.node_rows(), vec!["b".to_string()]);
    // b takes them within a sweep or two of noticing (they were released, not left to expire).
    let t = Instant::now();
    assert!(eventually(Duration::from_secs(3), || keys.iter().all(|k| b.owns(k))));
    assert!(t.elapsed() < Duration::from_millis(env.ttl() as u64), "released keys were taken before they'd have expired: {:?}", t.elapsed());
}
