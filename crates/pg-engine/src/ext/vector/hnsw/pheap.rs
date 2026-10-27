//! PostgreSQL's pairing heap (`src/backend/lib/pairingheap.c`) over search candidates kept in
//! an arena, as `HnswSearchLayer` uses it. Equal distances leave the heap's shape to decide
//! which candidate comes out first, so the merge order here is pairingheap.c's to the letter.
//!
//! A candidate (`HnswSearchCandidate`) has two sets of links: `c` (the candidate queue C) and
//! `w` (the result set W, and later the discarded heap, which reuses `w_node`).

/// No node.
pub(super) const NIL: u32 = u32::MAX;

/// `HnswSearchCandidate`: an element (an index into the caller's elements), its distance to
/// the query, and its links in the two heaps it can be in.
#[derive(Clone, Copy, Debug)]
pub(super) struct Sc {
    pub e: u32,
    pub dist: f64,
    c_child: u32,
    c_sib: u32,
    w_child: u32,
    w_sib: u32,
}

impl Sc {
    pub fn new(e: u32, dist: f64) -> Sc {
        Sc { e, dist, c_child: NIL, c_sib: NIL, w_child: NIL, w_sib: NIL }
    }
}

/// Which links a heap uses, and its order: `NEAREST` puts the smallest distance first
/// (`CompareNearestCandidates`, `CompareNearestDiscardedCandidates`), else the largest
/// (`CompareFurthestCandidates`).
pub(super) struct Heap<const W: bool, const NEAREST: bool> {
    pub root: u32,
}

/// C: nearest first, over `c_node`.
pub(super) type CHeap = Heap<false, true>;
/// W: furthest first, over `w_node`.
pub(super) type WHeap = Heap<true, false>;
/// The discarded candidates: nearest first, over `w_node`.
pub(super) type DHeap = Heap<true, true>;

impl<const W: bool, const NEAREST: bool> Heap<W, NEAREST> {
    pub const fn new() -> Self {
        Heap { root: NIL }
    }

    pub fn is_empty(&self) -> bool {
        self.root == NIL
    }

    /// pairingheap_first.
    pub fn first(&self) -> u32 {
        self.root
    }

    #[inline(always)]
    fn child(a: &[Sc], i: u32) -> u32 {
        if W { a[i as usize].w_child } else { a[i as usize].c_child }
    }

    #[inline(always)]
    fn sib(a: &[Sc], i: u32) -> u32 {
        if W { a[i as usize].w_sib } else { a[i as usize].c_sib }
    }

    #[inline(always)]
    fn set_child(a: &mut [Sc], i: u32, v: u32) {
        if W { a[i as usize].w_child = v } else { a[i as usize].c_child = v }
    }

    #[inline(always)]
    fn set_sib(a: &mut [Sc], i: u32, v: u32) {
        if W { a[i as usize].w_sib = v } else { a[i as usize].c_sib = v }
    }

    /// The heap's comparator is negative: `a` sorts below `b`.
    #[inline(always)]
    fn below(a: &[Sc], x: u32, y: u32) -> bool {
        let (dx, dy) = (a[x as usize].dist, a[y as usize].dist);
        if NEAREST { dx > dy } else { dx < dy }
    }

    /// merge: `b` becomes the first child of `a` (or the other way round when `a` sorts
    /// below `b`).
    #[inline(always)]
    fn merge(a: &mut [Sc], x: u32, y: u32) -> u32 {
        if x == NIL {
            return y;
        }
        if y == NIL {
            return x;
        }
        let (x, y) = if Self::below(a, x, y) { (y, x) } else { (x, y) };
        let first = Self::child(a, x);
        Self::set_sib(a, y, first);
        Self::set_child(a, x, y);
        x
    }

    /// pairingheap_add.
    pub fn add(&mut self, a: &mut [Sc], node: u32) {
        Self::set_child(a, node, NIL);
        self.root = Self::merge(a, self.root, node);
        Self::set_sib(a, self.root, NIL);
    }

    /// pairingheap_remove_first.
    pub fn remove_first(&mut self, a: &mut [Sc]) -> u32 {
        let result = self.root;
        let children = Self::child(a, result);
        self.root = Self::merge_children(a, children);
        if self.root != NIL {
            Self::set_sib(a, self.root, NIL);
        }
        result
    }

    /// merge_children: the two-pass pairing of a node's children.
    fn merge_children(a: &mut [Sc], children: u32) -> u32 {
        if children == NIL || Self::sib(a, children) == NIL {
            return children;
        }
        // Walk the subheaps from left to right, merging in pairs.
        let mut next = children;
        let mut pairs = NIL;
        loop {
            let curr = next;
            if curr == NIL {
                break;
            }
            let s = Self::sib(a, curr);
            if s == NIL {
                // last odd node at the end of list
                Self::set_sib(a, curr, pairs);
                pairs = curr;
                break;
            }
            next = Self::sib(a, s);
            let m = Self::merge(a, curr, s);
            Self::set_sib(a, m, pairs);
            pairs = m;
        }
        // Merge all the pairs together to form a single heap.
        let mut root = pairs;
        let mut next = Self::sib(a, pairs);
        while next != NIL {
            let curr = next;
            next = Self::sib(a, curr);
            root = Self::merge(a, root, curr);
        }
        root
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn orders_like_a_heap() {
        let mut a: Vec<Sc> = [5.0, 1.0, 3.0, 3.0, 9.0, 0.5, 7.0].iter().enumerate().map(|(i, d)| Sc::new(i as u32, *d)).collect();
        let mut c = CHeap::new();
        let mut w = WHeap::new();
        for i in 0..a.len() as u32 {
            c.add(&mut a, i);
            w.add(&mut a, i);
        }
        let mut near = Vec::new();
        while !c.is_empty() {
            let i = c.remove_first(&mut a);
            near.push(a[i as usize].dist);
        }
        let mut far = Vec::new();
        while !w.is_empty() {
            let i = w.remove_first(&mut a);
            far.push(a[i as usize].dist);
        }
        assert_eq!(near, vec![0.5, 1.0, 3.0, 3.0, 5.0, 7.0, 9.0]);
        assert_eq!(far, vec![9.0, 7.0, 5.0, 3.0, 3.0, 1.0, 0.5]);
    }
}
