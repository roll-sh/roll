//! What Redis' radix trees (`rax.c`) allocate, for MEMORY USAGE of the structures built on them
//! (stream nodes, consumer groups, pending entries).
//!
//! A node is a 4 byte header, its edge bytes (or compressed string) padded to a pointer, one child
//! pointer per edge (one for a compressed node) and a value pointer when it is a key. Nodes are
//! allocated at that length and reallocated when it changes, so each takes what the allocator gives
//! for its current length. Trees with fixed-length keys (stream IDs) inline the values into the
//! last node's child slots instead of allocating leaves.

use crate::client::jemalloc_size;

/// `sizeof(rax)`.
pub const RAX: usize = 40;

/// `raxNodeCurrentLength` of a node with `size` edges (or a compressed string of `size` bytes),
/// as the allocator sizes it.
pub fn node_alloc(size: usize, compressed: bool, value: bool) -> usize {
    let pad = (8 - (size + 4) % 8) % 8;
    let ptrs = if compressed { 1 } else { size };
    jemalloc_size(4 + size + pad + 8 * ptrs + if value { 8 } else { 0 })
}

/// A rax holding `keys` (sorted, distinct) in the shape insertions give it: every chain of
/// single-child nodes that aren't keys is one compressed node. `fixed` keys all have that length
/// and keep their values in the parents' slots; otherwise every key node holds a value pointer.
pub fn alloc(keys: &[&[u8]], fixed: Option<usize>) -> usize {
    RAX + nodes(keys, 0, fixed)
}

/// The node for the prefix of length `depth` that `keys` share, and everything below it.
fn nodes(keys: &[&[u8]], depth: usize, fixed: Option<usize>) -> usize {
    let is_key = keys.first().is_some_and(|k| k.len() == depth);
    let rest = if is_key { &keys[1..] } else { keys };
    let value = is_key && fixed.is_none();
    let (Some(first), Some(last)) = (rest.first(), rest.last()) else {
        return node_alloc(0, false, value);
    };
    // A child reached at the fixed length is a value slot, not a node.
    let below = |group: &[&[u8]], at: usize| if fixed == Some(at) { 0 } else { nodes(group, at, fixed) };
    if first[depth] == last[depth] {
        // One edge: compress it with the following ones while they stay single and keyless (the
        // shortest key, sorted first, may end the run).
        let lcp = first[depth..].iter().zip(&last[depth..]).take_while(|(a, b)| a == b).count();
        return node_alloc(lcp, lcp > 1, value) + below(rest, depth + lcp);
    }
    let mut total = 0;
    let mut edges = 0;
    let mut i = 0;
    while i < rest.len() {
        let c = rest[i][depth];
        let j = i + rest[i..].iter().take_while(|k| k[depth] == c).count();
        total += below(&rest[i..j], depth + 1);
        edges += 1;
        i = j;
    }
    node_alloc(edges, false, value) + total
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn node_lengths() {
        // An empty node: header and padding.
        assert_eq!(node_alloc(0, false, false), 8);
        // A leaf holding a value.
        assert_eq!(node_alloc(0, false, true), 16);
        // 16 compressed bytes and the child pointer: 4 + 16 + 4 + 8.
        assert_eq!(node_alloc(16, true, false), 32);
        // Two edges: 4 + 2 + 2 + 16.
        assert_eq!(node_alloc(2, false, false), 24);
    }

    #[test]
    fn canonical_shapes() {
        // Empty: the head alone.
        assert_eq!(alloc(&[], Some(16)), RAX + 8);
        // One fixed key: a compressed head whose slot holds the value.
        let a = [0u8; 16];
        assert_eq!(alloc(&[&a], Some(16)), RAX + 32);
        // Variable keys "foo" and "foobar": compressed "foo", the key node compressing "bar" with
        // a value, then the leaf with its value.
        let keys: [&[u8]; 2] = [b"foo", b"foobar"];
        assert_eq!(alloc(&keys, None), RAX + node_alloc(3, true, false) + node_alloc(3, true, true) + node_alloc(0, false, true));
        // "ab" and "ac": "a", then two edges, then two leaves.
        let keys: [&[u8]; 2] = [b"ab", b"ac"];
        assert_eq!(alloc(&keys, None), RAX + node_alloc(1, false, false) + node_alloc(2, false, false) + 2 * node_alloc(0, false, true));
    }
}
