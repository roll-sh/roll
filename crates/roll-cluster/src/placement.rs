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

#[cfg(test)]
mod tests {
    use super::*;

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
