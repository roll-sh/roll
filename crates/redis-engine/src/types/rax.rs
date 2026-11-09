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

/// A radix tree's allocations followed through its history (`rax.c` with variable-length keys,
/// as `RedisModuleDict`s use them): `raxInsert` and `raxRemove` ported for the node shapes and
/// the allocator sizes they leave, which is what `RM_MallocSizeDict` reports. Values are only
/// "present or NULL" (a NULL value takes no slot).
#[derive(Clone, Debug)]
pub struct Tree {
    nodes: Vec<TNode>,
    free: Vec<u32>,
    head: u32,
    /// Bytes allocated for the nodes and the `rax` itself (`rax->alloc_size` contributions).
    pub alloc: usize,
    pub numnodes: usize,
    pub numele: usize,
}

#[derive(Clone, Debug, Default)]
struct TNode {
    iskey: bool,
    isnull: bool,
    compr: bool,
    /// Edge bytes (sorted) or the compressed string.
    data: Vec<u8>,
    kids: Vec<u32>,
    /// What the allocator gave this node.
    alloc: usize,
}

/// `raxPadding`.
fn pad(size: usize) -> usize {
    (8 - (size + 4) % 8) & 7
}

impl TNode {
    /// `raxNodeCurrentLength`.
    fn len(&self) -> usize {
        let size = self.data.len();
        4 + size + pad(size) + if self.compr { 8 } else { 8 * size } + if self.iskey && !self.isnull { 8 } else { 0 }
    }
}

impl Default for Tree {
    fn default() -> Self {
        Tree::new()
    }
}

impl Tree {
    /// `raxNewEx`: the rax and an empty head.
    pub fn new() -> Self {
        let mut t = Tree { nodes: Vec::new(), free: Vec::new(), head: 0, alloc: jemalloc_size(RAX), numnodes: 1, numele: 0 };
        t.head = t.new_node(TNode::default(), 4 + pad(0));
        t
    }

    fn new_node(&mut self, mut n: TNode, size: usize) -> u32 {
        n.alloc = jemalloc_size(size);
        self.alloc += n.alloc;
        match self.free.pop() {
            Some(i) => {
                self.nodes[i as usize] = n;
                i
            }
            None => {
                self.nodes.push(n);
                (self.nodes.len() - 1) as u32
            }
        }
    }

    fn free_node(&mut self, i: u32) {
        self.alloc -= self.nodes[i as usize].alloc;
        self.nodes[i as usize] = TNode::default();
        self.free.push(i);
    }

    fn realloc(&mut self, i: u32, size: usize) {
        let n = &mut self.nodes[i as usize];
        self.alloc -= n.alloc;
        n.alloc = jemalloc_size(size);
        self.alloc += n.alloc;
    }

    /// Point whatever held `old` (its parent's slot, or the head) at `new`.
    fn relink(&mut self, parent: Option<u32>, old: u32, new: u32) {
        match parent {
            None => self.head = new,
            Some(p) => {
                for k in self.nodes[p as usize].kids.iter_mut() {
                    if *k == old {
                        *k = new;
                    }
                }
            }
        }
    }

    /// `raxLowWalk`: (stop node, bytes consumed, split position, parents walked).
    fn walk(&self, s: &[u8]) -> (u32, usize, usize, Vec<u32>) {
        let mut h = self.head;
        let (mut i, mut j) = (0, 0);
        let mut stack = Vec::new();
        while !self.nodes[h as usize].data.is_empty() && i < s.len() {
            let n = &self.nodes[h as usize];
            if n.compr {
                j = 0;
                while j < n.data.len() && i < s.len() && n.data[j] == s[i] {
                    j += 1;
                    i += 1;
                }
                if j != n.data.len() {
                    break;
                }
            } else {
                match n.data.iter().position(|&c| c == s[i]) {
                    Some(p) => j = p,
                    None => {
                        j = n.data.len();
                        break;
                    }
                }
                i += 1;
            }
            stack.push(h);
            h = n.kids[if n.compr { 0 } else { j }];
            j = 0;
        }
        let split = if self.nodes[h as usize].compr { j } else { 0 };
        (h, i, split, stack)
    }

    pub fn contains(&self, s: &[u8]) -> bool {
        let (h, i, j, _) = self.walk(s);
        let n = &self.nodes[h as usize];
        i == s.len() && !(n.compr && j != 0) && n.iskey
    }

    /// `raxReallocForData` then `raxSetData`.
    fn set_data(&mut self, h: u32, value: bool) {
        if value {
            let n = &self.nodes[h as usize];
            if !n.iskey || n.isnull {
                let cur = n.len();
                self.realloc(h, cur + 8);
            }
        }
        let n = &mut self.nodes[h as usize];
        n.iskey = true;
        n.isnull = !value;
    }

    /// `RM_DictSet` (`raxTryInsert`): insert `s` unless it is there. `value` says whether the
    /// value is non-NULL. True when inserted.
    pub fn insert(&mut self, s: &[u8], value: bool) -> bool {
        let (mut h, mut i, j, stack) = self.walk(s);
        let len = s.len();
        let parent = stack.last().copied();
        {
            let n = &self.nodes[h as usize];
            if i == len && (!n.compr || j == 0) {
                if n.iskey {
                    return false;
                }
                self.set_data(h, value);
                self.numele += 1;
                return true;
            }
        }
        if self.nodes[h as usize].compr && i != len {
            // ALGO 1: split the compressed node at `j`.
            let old = self.nodes[h as usize].clone();
            let next = old.kids[0];
            let trimmedlen = j;
            let postfixlen = old.data.len() - j - 1;
            let split_is_key = trimmedlen == 0 && old.iskey && !old.isnull;
            let split = self.new_node(
                TNode { data: vec![old.data[j]], ..Default::default() },
                4 + 1 + pad(1) + 8 + if split_is_key { 8 } else { 0 },
            );
            let trimmed = (trimmedlen > 0).then(|| {
                let size = 4 + j + pad(j) + 8 + if old.iskey && !old.isnull { 8 } else { 0 };
                let t = TNode { iskey: old.iskey, isnull: old.isnull, compr: j > 1, data: old.data[..j].to_vec(), kids: vec![split], alloc: 0 };
                self.new_node(t, size)
            });
            let postfix = if postfixlen > 0 {
                self.numnodes += 1;
                let size = 4 + postfixlen + pad(postfixlen) + 8;
                self.new_node(TNode { compr: postfixlen > 1, data: old.data[j + 1..].to_vec(), kids: vec![next], ..Default::default() }, size)
            } else {
                next
            };
            if j == 0 && old.iskey {
                let n = &mut self.nodes[split as usize];
                n.iskey = true;
                n.isnull = old.isnull;
            }
            self.nodes[split as usize].kids = vec![postfix];
            match trimmed {
                None => self.relink(parent, h, split),
                Some(t) => {
                    self.numnodes += 1;
                    self.relink(parent, h, t);
                }
            }
            self.free_node(h);
            h = split;
        } else if self.nodes[h as usize].compr && i == len {
            // ALGO 2: the key ends inside the compressed node.
            let old = self.nodes[h as usize].clone();
            let next = old.kids[0];
            let postfixlen = old.data.len() - j;
            let psize = 4 + postfixlen + pad(postfixlen) + 8 + if value { 8 } else { 0 };
            let postfix = self.new_node(
                TNode { iskey: true, isnull: !value, compr: postfixlen > 1, data: old.data[j..].to_vec(), kids: vec![next], alloc: 0 },
                psize,
            );
            self.numnodes += 1;
            let tsize = 4 + j + pad(j) + 8 + if old.iskey && !old.isnull { 8 } else { 0 };
            let trimmed = self.new_node(
                TNode { iskey: old.iskey, isnull: old.iskey && old.isnull, compr: j > 1, data: old.data[..j].to_vec(), kids: vec![postfix], alloc: 0 },
                tsize,
            );
            self.relink(parent, h, trimmed);
            self.numele += 1;
            self.free_node(h);
            return true;
        }
        while i < len {
            let n = &self.nodes[h as usize];
            let child;
            if n.data.is_empty() && len - i > 1 {
                // raxCompressNode.
                let l = len - i;
                let valued = n.iskey && !n.isnull;
                child = self.new_node(TNode::default(), 4 + pad(0));
                let size = 4 + l + pad(l) + 8 + if valued { 8 } else { 0 };
                self.realloc(h, size);
                let n = &mut self.nodes[h as usize];
                n.compr = true;
                n.data = s[i..].to_vec();
                n.kids = vec![child];
                i += l;
            } else {
                // raxAddChild.
                let mut grown = n.clone();
                grown.data.push(0);
                grown.kids.push(0);
                let newlen = grown.len();
                child = self.new_node(TNode::default(), 4 + pad(0));
                if self.nodes[h as usize].alloc < newlen {
                    self.realloc(h, newlen);
                }
                let n = &mut self.nodes[h as usize];
                let pos = n.data.iter().position(|&c| c > s[i]).unwrap_or(n.data.len());
                n.data.insert(pos, s[i]);
                n.kids.insert(pos, child);
                i += 1;
            }
            self.numnodes += 1;
            h = child;
        }
        self.set_data(h, value);
        self.numele += 1;
        true
    }

    /// `raxRemove`. True when `s` was there.
    pub fn remove(&mut self, s: &[u8]) -> bool {
        let (mut h, i, j, mut stack) = self.walk(s);
        {
            let n = &self.nodes[h as usize];
            if i != s.len() || (n.compr && j != 0) || !n.iskey {
                return false;
            }
        }
        self.numele -= 1;
        self.nodes[h as usize].iskey = false;
        let mut trycompress = false;
        if self.nodes[h as usize].data.is_empty() {
            let mut child = None;
            while h != self.head {
                child = Some(h);
                self.free_node(h);
                self.numnodes -= 1;
                h = stack.pop().expect("a parent");
                let n = &self.nodes[h as usize];
                if n.iskey || (!n.compr && n.data.len() != 1) {
                    break;
                }
            }
            if let Some(child) = child {
                // raxRemoveChild.
                let n = &mut self.nodes[h as usize];
                if n.compr {
                    n.compr = false;
                    n.data.clear();
                    n.kids.clear();
                } else {
                    let p = n.kids.iter().position(|&k| k == child).expect("child of parent");
                    n.kids.remove(p);
                    n.data.remove(p);
                    let l = n.len();
                    self.realloc(h, l);
                }
                let n = &self.nodes[h as usize];
                if n.data.len() == 1 && !n.iskey {
                    trycompress = true;
                }
            }
        } else if self.nodes[h as usize].data.len() == 1 {
            trycompress = true;
        }
        if trycompress {
            let parent = loop {
                match stack.pop() {
                    None => break None,
                    Some(p) => {
                        let n = &self.nodes[p as usize];
                        if n.iskey || (!n.compr && n.data.len() != 1) {
                            break Some(p);
                        }
                        h = p;
                    }
                }
            };
            let start = h;
            let mut comprsize = self.nodes[h as usize].data.len();
            let mut nodes = 1;
            while !self.nodes[h as usize].data.is_empty() {
                h = *self.nodes[h as usize].kids.last().expect("a child");
                let n = &self.nodes[h as usize];
                if n.iskey || (!n.compr && n.data.len() != 1) {
                    break;
                }
                nodes += 1;
                comprsize += n.data.len();
            }
            if nodes > 1 {
                let mut data = Vec::with_capacity(comprsize);
                let mut h = start;
                let new = self.new_node(TNode { compr: true, ..Default::default() }, 4 + comprsize + pad(comprsize) + 8);
                self.numnodes += 1;
                while !self.nodes[h as usize].data.is_empty() {
                    data.extend_from_slice(&self.nodes[h as usize].data);
                    let next = *self.nodes[h as usize].kids.last().expect("a child");
                    self.free_node(h);
                    self.numnodes -= 1;
                    h = next;
                    let n = &self.nodes[h as usize];
                    if n.iskey || (!n.compr && n.data.len() != 1) {
                        break;
                    }
                }
                let n = &mut self.nodes[new as usize];
                n.data = data;
                n.kids = vec![h];
                self.relink(parent, start, new);
            }
        }
        true
    }
}

/// `RM_MallocSizeDict` of a dict whose tree is `t`: the dict struct, the rax and its nodes.
pub fn dict_size(t: &Tree) -> usize {
    jemalloc_size(16) + t.alloc
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

    fn tree(keys: &[&[u8]]) -> Tree {
        let mut t = Tree::new();
        for k in keys {
            assert!(t.insert(k, true));
        }
        t
    }

    #[test]
    fn tree_follows_inserts() {
        let canon = |keys: &[&[u8]]| alloc(keys, None) - RAX + jemalloc_size(RAX);
        for keys in [&[b"foo".as_slice(), b"foobar"][..], &[b"ab", b"ac"], &[b"a", b"ab", b"abc"], &[b"x"], &[]] {
            let t = tree(keys);
            assert_eq!(t.alloc, canon(keys), "{keys:?}");
            assert_eq!(t.numele, keys.len());
            assert!(keys.iter().all(|k| t.contains(k)));
        }
        // Splitting a compressed node, then removing back to the start.
        let mut t = tree(&[b"hello"]);
        assert!(!t.insert(b"hello", true));
        assert!(t.insert(b"help", true));
        assert!(t.contains(b"help") && t.contains(b"hello") && !t.contains(b"hel"));
        assert!(t.remove(b"help"));
        assert!(!t.remove(b"help"));
        assert_eq!(t.alloc, canon(&[b"hello"]));
        // The head was compressed: `raxRemoveChild` empties it without shrinking it.
        assert!(t.remove(b"hello"));
        assert_eq!(t.alloc, canon(&[]) - node_alloc(0, false, false) + node_alloc(5, true, false));
        assert_eq!(t.numnodes, 1);
    }
}
