//! PostgreSQL's pairing heap (`lib/pairingheap.c`) over a fixed set of nodes, as ivfflat's
//! scan keeps its nearest lists in one: a max-heap by the comparator, whose order among equal
//! keys is the heap's own shape, so it is ported as it links its nodes.

use std::cmp::Ordering;

#[derive(Clone, Copy, Default)]
struct Node {
    first_child: Option<usize>,
    next_sibling: Option<usize>,
    prev_or_parent: Option<usize>,
}

/// A pairing heap over nodes `0..n` (the caller keeps their payloads by index).
pub(super) struct PairingHeap {
    nodes: Vec<Node>,
    root: Option<usize>,
}

impl PairingHeap {
    /// `pairingheap_allocate`, with room for nodes `0..n`.
    pub(super) fn new(n: usize) -> PairingHeap {
        PairingHeap { nodes: vec![Node::default(); n], root: None }
    }

    /// `merge`: `b` becomes a child of `a`, the greater one (`cmp(a, b)`: a against b).
    fn merge(&mut self, a: Option<usize>, b: Option<usize>, cmp: &mut impl FnMut(usize, usize) -> Ordering) -> Option<usize> {
        let (Some(mut a), Some(mut b)) = (a, b) else {
            return a.or(b);
        };
        // swap 'a' and 'b' so that 'a' is the one with larger value
        if cmp(a, b) == Ordering::Less {
            std::mem::swap(&mut a, &mut b);
        }
        // and put 'b' as a child of 'a'
        if let Some(fc) = self.nodes[a].first_child {
            self.nodes[fc].prev_or_parent = Some(b);
        }
        self.nodes[b].prev_or_parent = Some(a);
        self.nodes[b].next_sibling = self.nodes[a].first_child;
        self.nodes[a].first_child = Some(b);
        Some(a)
    }

    /// `pairingheap_add`.
    pub(super) fn add(&mut self, node: usize, cmp: &mut impl FnMut(usize, usize) -> Ordering) {
        self.nodes[node].first_child = None;
        // Link the new node as a new tree
        self.root = self.merge(self.root, Some(node), cmp);
        let r = self.root.expect("non-empty");
        self.nodes[r].prev_or_parent = None;
        self.nodes[r].next_sibling = None;
    }

    /// `pairingheap_first`.
    pub(super) fn first(&self) -> usize {
        self.root.expect("pairing heap is empty")
    }

    pub(super) fn is_empty(&self) -> bool {
        self.root.is_none()
    }

    /// `pairingheap_remove_first`.
    pub(super) fn remove_first(&mut self, cmp: &mut impl FnMut(usize, usize) -> Ordering) -> usize {
        // Remove the root, and form a new heap of its children.
        let result = self.first();
        let children = self.nodes[result].first_child;
        self.root = self.merge_children(children, cmp);
        if let Some(r) = self.root {
            self.nodes[r].prev_or_parent = None;
            self.nodes[r].next_sibling = None;
        }
        result
    }

    /// `merge_children`: the two-pass pairing of a list of subheaps.
    fn merge_children(&mut self, children: Option<usize>, cmp: &mut impl FnMut(usize, usize) -> Ordering) -> Option<usize> {
        let c = children?;
        if self.nodes[c].next_sibling.is_none() {
            return children;
        }
        // Walk the subheaps from left to right, merging in pairs
        let mut next = children;
        let mut pairs: Option<usize> = None;
        while let Some(curr) = next {
            let Some(sib) = self.nodes[curr].next_sibling else {
                // last odd node at the end of list
                self.nodes[curr].next_sibling = pairs;
                pairs = Some(curr);
                break;
            };
            next = self.nodes[sib].next_sibling;
            // merge this and the next subheap, and add to 'pairs' list.
            let m = self.merge(Some(curr), Some(sib), cmp).expect("non-empty");
            self.nodes[m].next_sibling = pairs;
            pairs = Some(m);
        }
        // Merge all the pairs together to form a single heap.
        let mut newroot = pairs;
        let mut next = pairs.and_then(|p| self.nodes[p].next_sibling);
        while let Some(curr) = next {
            next = self.nodes[curr].next_sibling;
            newroot = self.merge(newroot, Some(curr), cmp);
        }
        newroot
    }
}
