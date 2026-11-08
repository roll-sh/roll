//! Placement: which node a key belongs on, by rendezvous (highest random weight) hashing.
//!
//! Every node computes the same answer from the same set of nodes, with no coordination: each
//! (key, node) pair gets a pseudo-random weight and the node with the highest weight wins. When
//! a node joins, only the keys it now wins move (about 1/N of them); when one leaves, only its
//! keys move, spread over the others.
//!
//! The weight must be identical on every node and every build, across rolling upgrades, so it's
//! SHA-256 (not `std`'s `RandomState`/`DefaultHasher`, whose algorithm may change between Rust
//! releases).

use sha2::{Digest, Sha256};

/// The weight of `node` for `key`. Stable across processes, machines and releases.
pub fn weight(key: &str, node: &str) -> u64 {
    let mut h = Sha256::new();
    // Length-prefixed, so ("ab", "c") and ("a", "bc") don't collide.
    h.update((key.len() as u64).to_be_bytes());
    h.update(key.as_bytes());
    h.update(node.as_bytes());
    let d = h.finalize();
    u64::from_be_bytes(d[..8].try_into().expect("8 bytes"))
}

/// The node `key` belongs on among `nodes` (ids), or `None` if there are none. Ties (which
/// need a 64-bit collision) go to the larger id, so the answer never depends on the order.
pub fn rendezvous<'a, I>(key: &str, nodes: I) -> Option<&'a str>
where
    I: IntoIterator<Item = &'a str>,
{
    nodes.into_iter().map(|n| (weight(key, n), n)).max().map(|(_, n)| n)
}

/// The node `key` belongs on among `nodes`, whose zones `zone` tells: [`rendezvous`], except
/// that a key `<anchor>#<name>` (a replica of `<anchor>`, say) goes where a standby of `<anchor>`
/// goes ([`standby`]), so it never shares a node with `<anchor>` while another node is live.
pub fn place<'a>(key: &str, nodes: &'a [String], zone: impl Fn(&str) -> Option<String>) -> Option<&'a str> {
    match key.split_once('#') {
        Some((anchor, _)) => standby(anchor, key, nodes, zone).or_else(|| rendezvous(key, nodes.iter().map(String::as_str))),
        None => rendezvous(key, nodes.iter().map(String::as_str)),
    }
}

/// Where `key`, a standby of `anchor` (itself, for a standby proper), goes: not on `anchor`'s
/// node, in another zone than it when one is live, the rest by rendezvous weight of `key` (so the
/// replicas of one key spread out). `None` when `anchor`'s node is the only one.
pub fn standby<'a>(anchor: &str, key: &str, nodes: &'a [String], zone: impl Fn(&str) -> Option<String>) -> Option<&'a str> {
    let first = rendezvous(anchor, nodes.iter().map(String::as_str))?;
    let home = zone(first);
    let mut rest: Vec<&'a String> = nodes.iter().filter(|id| id.as_str() != first).collect();
    rest.sort_by_key(|id| std::cmp::Reverse((weight(key, id), id.as_str())));
    rest.iter().find(|id| home.is_some() && zone(id) != home).or(rest.first()).map(|id| id.as_str())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_replica_key_never_shares_its_anchors_node() {
        let nodes: Vec<String> = ["n1", "n2", "n3"].map(String::from).to_vec();
        let zone = |id: &str| Some(if id == "n3" { "b" } else { "a" }.to_string());
        for i in 0..50 {
            let anchor = format!("db{i}");
            let primary = place(&anchor, &nodes, zone).unwrap();
            let replica = place(&format!("{anchor}#r"), &nodes, zone).unwrap();
            assert_ne!(primary, replica);
            assert_ne!(zone(primary), zone(replica), "another zone while one is live");
        }
        assert_eq!(place("db#r", &nodes[..1], zone), Some("n1"), "alone, it shares");
    }

    #[test]
    fn weights_are_stable() {
        // Pinned: changing the hash would move every repo in a mixed-version cluster.
        assert_eq!(weight("repo_1", "node-a"), weight("repo_1", "node-a"));
        assert_ne!(weight("repo_1", "node-a"), weight("repo_1", "node-b"));
        assert_ne!(weight("ab", "c"), weight("a", "bc"));
        let pinned = weight("k", "n");
        assert_eq!(pinned, 0x6173_35c0_4ca5_f63e, "{pinned:#x}");
    }

    #[test]
    fn order_does_not_matter() {
        let a = ["n1", "n2", "n3", "n4"];
        let b = ["n4", "n2", "n1", "n3"];
        for i in 0..200 {
            let k = format!("key{i}");
            assert_eq!(rendezvous(&k, a), rendezvous(&k, b));
        }
        assert_eq!(rendezvous("k", std::iter::empty()), None);
    }
}
