//! Placement: rendezvous hashing spreads keys evenly and moves few of them; every node routes
//! a key to the same owner.

mod common;

use code_cluster::{Route, placement};
use common::*;
use std::collections::HashMap;

#[test]
fn rendezvous_spreads_keys_and_moves_about_one_in_n() {
    let keys: Vec<String> = (0..1000).map(|i| format!("repo_{i:04}")).collect();
    let three = ["node-a", "node-b", "node-c"];
    let four = ["node-a", "node-b", "node-c", "node-d"];
    let owner3: Vec<&str> = keys.iter().map(|k| placement::rendezvous(k, three).unwrap()).collect();
    let mut counts: HashMap<&str, usize> = HashMap::new();
    for o in &owner3 {
        *counts.entry(o).or_default() += 1;
    }
    eprintln!("3 nodes: {counts:?}");
    for n in three {
        // 333 expected; ±75 is about 5 standard deviations.
        assert!((258..=408).contains(&counts[n]), "{n} got {} of 1000", counts[n]);
    }

    // A fourth node joins: only keys it now wins move, about 1/4 of them.
    let owner4: Vec<&str> = keys.iter().map(|k| placement::rendezvous(k, four).unwrap()).collect();
    let moved: Vec<usize> = (0..keys.len()).filter(|&i| owner3[i] != owner4[i]).collect();
    eprintln!("a fourth node moves {} keys", moved.len());
    assert!(moved.iter().all(|&i| owner4[i] == "node-d"), "keys only move to the new node");
    assert!((180..=320).contains(&moved.len()), "{} keys moved", moved.len());

    // A node leaves: only its keys move, spread over the others.
    let two = ["node-a", "node-c"];
    for (i, k) in keys.iter().enumerate() {
        let o = placement::rendezvous(k, two).unwrap();
        if owner3[i] != "node-b" {
            assert_eq!(o, owner3[i], "{k} moved though its node stayed");
        }
    }
}

/// Whichever node a request arrives at, it's routed to one owner: the preferred node, which
/// takes the key on first use.
#[test]
fn every_node_routes_a_key_to_the_same_owner() {
    let env = Env::new("route");
    let nodes = [env.join("a"), env.join("b"), env.join("c")];
    converge(&nodes);
    for i in 0..30 {
        let key = format!("repo_{i}");
        let want = placement::rendezvous(&key, ["a", "b", "c"]).unwrap();
        // Ask a node that isn't the owner first: it points at the preferred node.
        let first = nodes.iter().find(|n| n.node_id() != want).unwrap();
        match first.route(&key).unwrap() {
            Route::Remote(n) => assert_eq!(n.id, want),
            r => panic!("{key}: {r:?}"),
        }
        assert_eq!(first.preferred(&key).unwrap().id, want);
        // The owner takes it.
        let owner = nodes.iter().find(|n| n.node_id() == want).unwrap();
        assert!(matches!(owner.route(&key).unwrap(), Route::Local(_)));
        for n in &nodes {
            match n.route(&key).unwrap() {
                Route::Local(g) => {
                    assert_eq!(n.node_id(), want);
                    assert_eq!(g.epoch(), 1);
                }
                Route::Remote(o) => assert_eq!(o.id, want),
                r => panic!("{key}: {r:?}"),
            }
        }
        assert_eq!(env.lease(&key).unwrap().node, want);
    }
    // Nothing moves while membership is stable.
    std::thread::sleep(nodes[0].config().rebalance_every * 2);
    for i in 0..30 {
        let key = format!("repo_{i}");
        let want = placement::rendezvous(&key, ["a", "b", "c"]).unwrap();
        assert_eq!(env.lease(&key).unwrap().epoch, 1, "{key}");
        assert!(nodes.iter().all(|n| n.owns(&key) == (n.node_id() == want)));
    }
}
