//! Placement: rendezvous hashing spreads keys evenly and moves few of them (routing to one owner
//! is in the suite, on every backend).

use roll_cluster::placement;
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
