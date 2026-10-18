//! Boost's `bgi::rtree<std::pair<box, t_docId>, bgi::quadratic<16>>`: insertion by least
//! content enlargement, the quadratic split, removal with underflowed nodes reinserted, and the
//! nodes' allocations.

use super::algo::{self, content};
use super::{Cs, Rect};

const MAX: usize = 16;
const MIN: usize = 4;
/// `sizeof` a node (a variant of leaf and internal node, each a `varray` of 17 elements).
pub const NODE_BYTES: i64 = 696;

#[derive(Clone)]
enum Node {
    Leaf(Vec<(Rect, u64)>),
    Internal(Vec<(Rect, usize)>),
}

enum Elem {
    Value(Rect, u64),
    Child(Rect, usize),
}

impl Elem {
    fn rect(&self) -> Rect {
        match self {
            Elem::Value(r, _) | Elem::Child(r, _) => *r,
        }
    }
}

#[derive(Default)]
pub struct RTree {
    nodes: Vec<Option<Node>>,
    free: Vec<usize>,
    root: Option<usize>,
    leafs_level: usize,
    len: usize,
}

fn rect_eq(a: &Rect, b: &Rect) -> bool {
    a.min.x.to_bits() == b.min.x.to_bits()
        && a.min.y.to_bits() == b.min.y.to_bits()
        && a.max.x.to_bits() == b.max.x.to_bits()
        && a.max.y.to_bits() == b.max.y.to_bits()
}

impl RTree {
    fn alloc(&mut self, n: Node, bytes: &mut i64) -> usize {
        *bytes += NODE_BYTES;
        if let Some(i) = self.free.pop() {
            self.nodes[i] = Some(n);
            i
        } else {
            self.nodes.push(Some(n));
            self.nodes.len() - 1
        }
    }

    fn dealloc(&mut self, i: usize, bytes: &mut i64) {
        *bytes -= NODE_BYTES;
        self.nodes[i] = None;
        self.free.push(i);
    }

    fn node(&self, i: usize) -> &Node {
        self.nodes[i].as_ref().expect("live node")
    }

    fn node_mut(&mut self, i: usize) -> &mut Node {
        self.nodes[i].as_mut().expect("live node")
    }

    pub fn len(&self) -> usize {
        self.len
    }

    /// Inserts a value; returns the change in allocated bytes.
    pub fn insert(&mut self, cs: Cs, r: Rect, id: u64) -> i64 {
        let mut bytes = 0;
        if self.root.is_none() {
            let root = self.alloc(Node::Leaf(Vec::new()), &mut bytes);
            self.root = Some(root);
            self.leafs_level = 0;
        }
        let level = self.leafs_level;
        self.insert_at(cs, Elem::Value(r, id), level, &mut bytes);
        self.len += 1;
        bytes
    }

    /// The insert visitor: `elem` goes into a node at `level` (the leaf level for values).
    fn insert_at(&mut self, cs: Cs, elem: Elem, level: usize, bytes: &mut i64) {
        let root = self.root.expect("root");
        // The path of (internal node, chosen child index).
        let mut path: Vec<(usize, usize)> = Vec::new();
        let mut cur = root;
        let er = elem.rect();
        for _ in 0..level {
            let Node::Internal(children) = self.node(cur) else { break };
            let mut chosen = 0;
            let mut best_diff = f64::MAX;
            let mut best = f64::MAX;
            for (i, (b, _)) in children.iter().enumerate() {
                let mut exp = *b;
                algo::expand(cs, &mut exp, &er);
                let c = content(&exp);
                let diff = c - content(b);
                if diff < best_diff || (diff == best_diff && c < best) {
                    best_diff = diff;
                    best = c;
                    chosen = i;
                }
            }
            let next = {
                let Node::Internal(children) = self.node_mut(cur) else { unreachable!() };
                algo::expand(cs, &mut children[chosen].0, &er);
                children[chosen].1
            };
            path.push((cur, chosen));
            cur = next;
        }
        match (self.node_mut(cur), elem) {
            (Node::Leaf(v), Elem::Value(r, id)) => v.push((r, id)),
            (Node::Internal(v), Elem::Child(r, c)) => v.push((r, c)),
            _ => unreachable!("element at the wrong level"),
        }
        // `post_traverse` back up: split overflowing nodes.
        let mut node = cur;
        loop {
            let over = match self.node(node) {
                Node::Leaf(v) => v.len() > MAX,
                Node::Internal(v) => v.len() > MAX,
            };
            if over {
                self.split(cs, node, path.last().copied(), bytes);
            }
            match path.pop() {
                Some((parent, _)) => node = parent,
                None => break,
            }
        }
    }

    fn split(&mut self, cs: Cs, n: usize, parent: Option<(usize, usize)>, bytes: &mut i64) {
        let taken = std::mem::replace(self.node_mut(n), Node::Leaf(Vec::new()));
        let (first, second, b1, b2) = match taken {
            Node::Leaf(v) => {
                let (a, b, b1, b2) = redistribute(cs, v, |e| e.0);
                (Node::Leaf(a), Node::Leaf(b), b1, b2)
            }
            Node::Internal(v) => {
                let (a, b, b1, b2) = redistribute(cs, v, |e| e.0);
                (Node::Internal(a), Node::Internal(b), b1, b2)
            }
        };
        *self.node_mut(n) = first;
        let n2 = self.alloc(second, bytes);
        match parent {
            Some((p, idx)) => {
                let Node::Internal(children) = self.node_mut(p) else { unreachable!() };
                children[idx].0 = b1;
                children.push((b2, n2));
            }
            None => {
                let root = self.alloc(Node::Internal(vec![(b1, n), (b2, n2)]), bytes);
                self.root = Some(root);
                self.leafs_level += 1;
            }
        }
    }

    /// Removes the value; returns the change in allocated bytes, or `None` when absent.
    pub fn remove(&mut self, cs: Cs, r: Rect, id: u64) -> Option<i64> {
        let root = self.root?;
        let mut bytes = 0;
        let mut st = RemoveState { removed: false, underflow: false, underflowed: Vec::new() };
        self.remove_visit(cs, root, &r, id, 0, None, &mut st, &mut bytes);
        if !st.removed {
            return None;
        }
        self.len -= 1;
        Some(bytes)
    }

    #[allow(clippy::too_many_arguments)]
    fn remove_visit(&mut self, cs: Cs, n: usize, r: &Rect, id: u64, level: usize, parent: Option<(usize, usize)>, st: &mut RemoveState, bytes: &mut i64) {
        match self.node(n) {
            Node::Leaf(_) => {
                let Node::Leaf(v) = self.node_mut(n) else { unreachable!() };
                if let Some(pos) = v.iter().position(|(b, d)| *d == id && rect_eq(b, r)) {
                    v.swap_remove(pos);
                    st.removed = true;
                    st.underflow = v.len() < MIN;
                    let bx = values_box(cs, v.iter().map(|e| e.0));
                    if let Some((p, idx)) = parent {
                        let Node::Internal(children) = self.node_mut(p) else { unreachable!() };
                        children[idx].0 = bx;
                    }
                }
            }
            Node::Internal(children) => {
                let count = children.len();
                let mut idx = 0;
                while idx < count {
                    let (bx, child) = {
                        let Node::Internal(children) = self.node(n) else { unreachable!() };
                        children[idx]
                    };
                    if algo::box_covered_by(cs, r, &bx) {
                        self.remove_visit(cs, child, r, id, level + 1, Some((n, idx)), st, bytes);
                        if st.removed {
                            break;
                        }
                    }
                    idx += 1;
                }
                if !st.removed {
                    return;
                }
                if st.underflow {
                    let relative = self.leafs_level - level;
                    let Node::Internal(children) = self.node_mut(n) else { unreachable!() };
                    let (_, child) = children.swap_remove(idx);
                    st.underflowed.push((relative, child));
                    st.underflow = children.len() < MIN;
                }
                if let Some((p, pidx)) = parent {
                    let Node::Internal(children) = self.node(n) else { unreachable!() };
                    let bx = values_box(cs, children.iter().map(|e| e.0));
                    let Node::Internal(pc) = self.node_mut(p) else { unreachable!() };
                    pc[pidx].0 = bx;
                } else {
                    // The root: reinsert the underflowed nodes' elements, last stored first.
                    let stored = std::mem::take(&mut st.underflowed);
                    for (relative, node) in stored.into_iter().rev() {
                        let elems: Vec<Elem> = match self.node(node) {
                            Node::Leaf(v) => v.iter().map(|&(b, d)| Elem::Value(b, d)).collect(),
                            Node::Internal(v) => v.iter().map(|&(b, c)| Elem::Child(b, c)).collect(),
                        };
                        for e in elems {
                            let lvl = self.leafs_level - (relative - 1);
                            self.insert_at(cs, e, lvl, bytes);
                        }
                        self.dealloc(node, bytes);
                    }
                    // The visited node's size decides, the current root is the one freed.
                    let Node::Internal(children) = self.node(n) else { return };
                    if children.len() <= 1 {
                        let next = children.first().map(|c| c.1);
                        let destroy = self.root.expect("root");
                        self.root = next;
                        self.leafs_level = self.leafs_level.saturating_sub(1);
                        self.dealloc(destroy, bytes);
                    }
                }
            }
        }
    }

    /// Every value in the tree's iteration order (`begin()`: depth first, children in order).
    pub fn values(&self) -> Vec<(Rect, u64)> {
        let mut out = Vec::new();
        let Some(root) = self.root else { return out };
        let mut stack = vec![root];
        while let Some(n) = stack.pop() {
            match self.node(n) {
                Node::Leaf(v) => out.extend(v.iter().copied()),
                Node::Internal(v) => {
                    for (_, c) in v.iter().rev() {
                        stack.push(*c);
                    }
                }
            }
        }
        out
    }

    /// The values whose boxes satisfy `pred`.
    pub fn query(&self, mut pred: impl FnMut(&Rect) -> bool) -> Vec<u64> {
        let mut out = Vec::new();
        let Some(root) = self.root else { return out };
        let mut stack = vec![root];
        while let Some(n) = stack.pop() {
            match self.node(n) {
                Node::Leaf(v) => {
                    for (b, id) in v {
                        if pred(b) {
                            out.push(*id);
                        }
                    }
                }
                Node::Internal(v) => {
                    for (_, c) in v.iter().rev() {
                        stack.push(*c);
                    }
                }
            }
        }
        out
    }
}

struct RemoveState {
    removed: bool,
    underflow: bool,
    underflowed: Vec<(usize, usize)>,
}

fn values_box(cs: Cs, mut it: impl Iterator<Item = Rect>) -> Rect {
    let Some(mut b) = it.next() else {
        // `assign_inverse`.
        return Rect { min: super::Pt { x: f64::MAX, y: f64::MAX }, max: super::Pt { x: f64::MIN, y: f64::MIN } };
    };
    for r in it {
        algo::expand(cs, &mut b, &r);
    }
    b
}

/// `move_from_back` then `pop_back`.
fn take_at<T: Copy>(v: &mut Vec<T>, i: usize) -> T {
    let e = v[i];
    let last = v.len() - 1;
    v[i] = v[last];
    v.pop();
    e
}

/// The quadratic split of 17 elements into two groups of at least 4.
fn redistribute<T: Copy>(cs: Cs, elems: Vec<T>, rect: impl Fn(&T) -> Rect) -> (Vec<T>, Vec<T>, Rect, Rect) {
    let count = elems.len();
    // `pick_seeds`: the pair wasting the most content.
    let (mut seed1, mut seed2) = (0, 1);
    let mut greatest = 0.0;
    for i in 0..count - 1 {
        for j in i + 1..count {
            let (a, b) = (rect(&elems[i]), rect(&elems[j]));
            let mut e = a;
            algo::expand(cs, &mut e, &b);
            let free = (content(&e) - content(&a)) - content(&b);
            if greatest < free {
                greatest = free;
                seed1 = i;
                seed2 = j;
            }
        }
    }
    let mut copy = elems;
    let mut g1 = vec![copy[seed1]];
    let mut g2 = vec![copy[seed2]];
    let mut box1 = rect(&copy[seed1]);
    let mut box2 = rect(&copy[seed2]);
    if seed1 < seed2 {
        take_at(&mut copy, seed2);
        take_at(&mut copy, seed1);
    } else {
        take_at(&mut copy, seed1);
        take_at(&mut copy, seed2);
    }
    let mut c1 = content(&box1);
    let mut c2 = content(&box2);
    let mut remaining = copy.len();
    while !copy.is_empty() {
        let mut pick = copy.len() - 1;
        let into1 = if g1.len() + remaining <= MIN {
            true
        } else if g2.len() + remaining <= MIN {
            false
        } else {
            // `pick_next` from the back: the greatest difference of the content increases.
            let mut greatest_diff = 0.0;
            let (mut inc1, mut inc2) = (0.0, 0.0);
            for k in (0..copy.len()).rev() {
                let r = rect(&copy[k]);
                let (mut e1, mut e2) = (box1, box2);
                algo::expand(cs, &mut e1, &r);
                algo::expand(cs, &mut e2, &r);
                let i1 = content(&e1) - c1;
                let i2 = content(&e2) - c2;
                let diff = if i1 < i2 { i2 - i1 } else { i1 - i2 };
                if greatest_diff < diff {
                    greatest_diff = diff;
                    pick = k;
                    inc1 = i1;
                    inc2 = i2;
                }
            }
            inc1 < inc2 || (inc1 == inc2 && (c1 < c2 || (c1 == c2 && g1.len() <= g2.len())))
        };
        let e = take_at(&mut copy, pick);
        let r = rect(&e);
        if into1 {
            g1.push(e);
            algo::expand(cs, &mut box1, &r);
            c1 = content(&box1);
        } else {
            g2.push(e);
            algo::expand(cs, &mut box2, &r);
            c2 = content(&box2);
        }
        remaining -= 1;
    }
    (g1, g2, box1, box2)
}

#[cfg(test)]
mod tests {
    use super::super::Pt;
    use super::*;

    fn pt(x: f64, y: f64) -> Rect {
        algo::mbr(Cs::Flat, &super::super::Geom::Point(Pt { x, y }))
    }

    #[test]
    fn splits_and_removes() {
        let mut t = RTree::default();
        let mut bytes = 0;
        let mut steps = Vec::new();
        for i in 1..=40u64 {
            let d = t.insert(Cs::Flat, pt(i as f64, (i * 7 % 13) as f64), i);
            bytes += d;
            if d != 0 {
                steps.push((i, d));
            }
        }
        assert_eq!(steps[0], (1, NODE_BYTES));
        assert_eq!(steps[1], (17, 2 * NODE_BYTES));
        for i in 1..=40u64 {
            bytes += t.remove(Cs::Flat, pt(i as f64, (i * 7 % 13) as f64), i).unwrap();
        }
        assert_eq!(t.len(), 0);
        assert_eq!(bytes, NODE_BYTES);
        let mut ids = t.query(|_| true);
        ids.sort();
        assert!(ids.is_empty());
    }
}
