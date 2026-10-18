//! DIALECT 4's query optimizer (`query_optimizer.c`, `optimizer_reader.c` and the trimmed
//! numeric union): what the request's sort and scorer let it skip, decided over the parsed
//! tree, then applied to the iterator tree. A numeric SORTBY reads the query through the
//! OPTIMIZER iterator, a top-k heap over the sort field's ranges limited to what the LIMIT
//! needs (more ranges when the query filters too many out); a lone numeric range sorted by a
//! numeric field keeps just enough of its union's ranges for the LIMIT.

use super::index::Index;
use super::iters::{BoxIt, Counters, DocId, Empty, IKind, Iter, IxResult, K_NUMERIC, K_UNION, PCtx, Pairs, Qcx, Skip, kv, print_leaf, print_union, profile_child};
use super::numtree::NumericFilter;
use super::qast::{Kind, Node, NumFilter};
use super::qeval::{numeric_iter, numeric_ranges};
use redis_proto::Reply;

/// `Q_Optimize_Type` (`Q_OPT_HYBRID` is `Undecided` until the iterators are built).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum OptType {
    /// `Q_OPT_NONE`.
    None,
    /// `Q_OPT_UNDECIDED`.
    Undecided,
    /// `Q_OPT_PARTIAL_RANGE`.
    PartialRange,
    /// `Q_OPT_NO_SORTER`.
    NoSorter,
}

/// `QOptimizer`: the decisions over the parsed tree.
#[derive(Clone, Debug)]
pub struct Plan {
    pub ty: OptType,
    /// The results wanted: LIMIT's offset and count (FT.SEARCH's default page when none).
    pub limit: usize,
    /// `scorerType != SCORER_TYPE_NONE`: the results are scored (and sorted by score).
    pub scorer: bool,
    /// The numeric SORTBY field, by schema position.
    pub field: Option<usize>,
    pub asc: bool,
    /// The sort field's range, taken out of the root's intersection (`sortbyNode`).
    pub nf: Option<NumFilter>,
}

impl Plan {
    pub fn new() -> Self {
        Plan { ty: OptType::Undecided, limit: 0, scorer: false, field: None, asc: false, nf: None }
    }
}

impl Default for Plan {
    fn default() -> Self {
        Self::new()
    }
}

/// `QOptimizer_EstimateLimit`: the documents to read for `limit` results, at the child's
/// estimated share of the index.
pub fn estimate_limit(num_docs: usize, estimate: usize, limit: usize) -> usize {
    if num_docs == 0 || estimate == 0 {
        return 0;
    }
    let ratio = estimate as f64 / num_docs as f64;
    (limit as f64 / ratio + 1.0) as usize
}

/// `QOptimizer_Iterators`: the iterator tree the plan reads the query through.
pub fn apply<'a>(plan: &Plan, root: &Node, it: BoxIt<'a>, ix: &'a Index, q: &'a Qcx<'a>, min_heap: usize) -> BoxIt<'a> {
    match plan.ty {
        OptType::None | OptType::NoSorter => it,
        OptType::PartialRange => {
            let Kind::Numeric(nf) = &root.kind else { return Optimizer::build(plan, "Query partial range", it, ix, q, min_heap) };
            // `TrimUnionIterator`: the numeric union keeps the ranges the limit needs.
            if it.kind() == IKind::Union
                && let Some(fi) = nf.field
            {
                let children = numeric_ranges(ix, q, fi, &nf.filter());
                if children.len() >= 3 {
                    return Box::new(UnionTrimmed::new(children, plan.limit, plan.asc));
                }
            }
            it
        }
        OptType::Undecided if plan.field.is_some() => Optimizer::build(plan, "Hybrid", it, ix, q, min_heap),
        OptType::Undecided => it,
    }
}

// ---------------------------------------------------------------------------------------------
// The OPTIMIZER iterator (`OptimizerIterator`)

/// `OptimizerIterator`: reads the child together with the sort field's ranges, keeps the top
/// `limit` results by the field's value in a heap, rewinding with the next ranges while too few
/// came, then yields the heap's results (the numeric field's records).
pub struct Optimizer<'a> {
    child: BoxIt<'a>,
    numeric: Option<BoxIt<'a>>,
    ix: &'a Index,
    q: &'a Qcx<'a>,
    min_heap: usize,
    field: usize,
    nf: NumericFilter,
    num_docs: usize,
    heap_old: u32,
    num_iterations: u64,
    child_estimate: usize,
    last_limit_estimate: i32,
    /// The first ranges' estimate.
    offset: usize,
    heap: Vec<IxResult>,
    cap: usize,
    asc: bool,
    yielding: bool,
    cur: IxResult,
    mode: &'static str,
}

impl<'a> Optimizer<'a> {
    /// `NewOptimizerIterator`: an empty iterator when the sort field has no ranges.
    fn build(plan: &Plan, mode: &'static str, child: BoxIt<'a>, ix: &'a Index, q: &'a Qcx<'a>, min_heap: usize) -> BoxIt<'a> {
        let Some(field) = plan.field else { return child };
        let asc = plan.asc;
        let num_docs = ix.docs.size;
        let child_estimate = child.num_estimated();
        let mut nf = match &plan.nf {
            Some(n) => NumericFilter { ascending: asc, ..n.filter() },
            None => NumericFilter { min: f64::NEG_INFINITY, max: f64::INFINITY, min_inclusive: true, max_inclusive: true, ascending: asc, limit: 0, offset: 0 },
        };
        nf.limit = estimate_limit(num_docs, child_estimate, plan.limit);
        let last_limit_estimate = nf.limit as i32;
        let Some(numeric) = numeric_iter(ix, q, min_heap, field, &nf) else { return Box::new(Empty) };
        let offset = numeric.num_estimated();
        Box::new(Optimizer {
            child,
            numeric: Some(numeric),
            ix,
            q,
            min_heap,
            field,
            nf,
            num_docs,
            heap_old: 0,
            num_iterations: 0,
            child_estimate,
            last_limit_estimate,
            offset,
            heap: Vec::with_capacity(plan.limit.min(1 << 16)),
            cap: plan.limit,
            asc,
            yielding: false,
            cur: IxResult::default(),
            mode,
        })
    }

    /// `cmpAsc` / `cmpDesc`: by the value, then the doc id.
    fn cmp(&self, a: &IxResult, b: &IxResult) -> i32 {
        let by_value = if a.num > b.num {
            1
        } else if a.num < b.num {
            -1
        } else {
            0
        };
        match by_value {
            0 => {
                if a.doc_id < b.doc_id {
                    -1
                } else {
                    1
                }
            }
            v if self.asc => v,
            v => -v,
        }
    }

    // The heap (`util/heap.c`): its greatest element on top.

    fn push_up(&mut self, mut idx: usize) {
        while idx != 0 {
            let parent = (idx - 1) / 2;
            if self.cmp(&self.heap[idx], &self.heap[parent]) < 0 {
                return;
            }
            self.heap.swap(idx, parent);
            idx = parent;
        }
    }

    fn push_down(&mut self, mut idx: usize) {
        let count = self.heap.len();
        loop {
            let (l, r) = (idx * 2 + 1, idx * 2 + 2);
            let child = if r >= count {
                if l >= count {
                    return;
                }
                l
            } else if self.cmp(&self.heap[l], &self.heap[r]) < 0 {
                r
            } else {
                l
            };
            if self.cmp(&self.heap[idx], &self.heap[child]) < 0 {
                self.heap.swap(idx, child);
                idx = child;
            } else {
                return;
            }
        }
    }

    fn offer(&mut self, r: IxResult) {
        if self.heap.len() < self.cap {
            self.heap.push(r);
            let last = self.heap.len() - 1;
            self.push_up(last);
        } else if let Some(top) = self.heap.first()
            && self.cmp(top, &r) > 0
        {
            self.heap[0] = r;
            self.push_down(0);
        }
    }

    /// `getSuccessRatio`.
    fn success_ratio(&self) -> f64 {
        (self.heap.len() as u32 - self.heap_old) as f64 / self.last_limit_estimate as f64
    }

    /// `OPT_Rewind`: the child from the start, with the ranges after the ones read.
    fn rewind_ranges(&mut self) {
        self.child.rewind();
        if let Some(n) = self.numeric.take() {
            self.nf.offset += n.num_estimated();
        }
        let ratio = self.success_ratio();
        if ratio < 0.01 || self.num_iterations == 3 {
            self.nf.limit = self.num_docs;
        } else {
            let missing = (self.cap as u32).wrapping_sub(self.heap.len() as u32) as i32;
            let estimate = estimate_limit(self.num_docs, self.child_estimate, missing as usize);
            self.nf.limit = (estimate as f64 * ratio) as usize;
            self.last_limit_estimate = self.nf.limit as i32;
        }
        self.numeric = numeric_iter(self.ix, self.q, self.min_heap, self.field, &self.nf);
        self.heap_old = self.heap.len() as u32;
        self.num_iterations += 1;
    }

    /// `OPT_Read`'s collection: the child and the ranges in step, every match offered to the
    /// heap.
    fn collect(&mut self) {
        loop {
            let Some(mut numeric) = self.numeric.take() else { return };
            // The doc ids of the child's and the ranges' current results, once both have one.
            let mut ids: Option<(DocId, DocId)> = None;
            loop {
                match ids {
                    Some((c, n)) if c > n => {
                        if numeric.skip_to(c) == Skip::Eof {
                            break;
                        }
                    }
                    Some((c, n)) if c < n => {
                        if self.child.skip_to(n) == Skip::Eof {
                            break;
                        }
                    }
                    _ => {
                        if !self.child.read() {
                            break;
                        }
                        let target = self.child.last_doc_id();
                        if numeric.skip_to(target) == Skip::Eof {
                            break;
                        }
                    }
                }
                let c = self.child.current().map_or(0, |r| r.doc_id);
                let Some(nr) = numeric.current() else { break };
                let n = nr.doc_id;
                ids = Some((c, n));
                if c != n {
                    continue;
                }
                let mut rec = if nr.kind == K_NUMERIC || nr.children.is_empty() { nr.clone() } else { nr.children[0].clone() };
                rec.doc_id = c;
                // Deleted documents are skipped.
                if self.ix.docs.get(c).is_none_or(|d| d.flags & super::doctable::DOC_DELETED != 0) {
                    continue;
                }
                self.offer(rec);
            }
            self.numeric = Some(numeric);
            // Too few results: read again with more ranges.
            if self.cap > self.heap.len() && self.offset < self.child_estimate && self.success_ratio() < 1.0 {
                self.rewind_ranges();
                if self.numeric.is_some() {
                    self.num_iterations += 1;
                    continue;
                }
            }
            return;
        }
    }
}

impl Iter for Optimizer<'_> {
    fn read(&mut self) -> bool {
        if !self.yielding {
            self.collect();
            self.yielding = true;
        }
        // `OPT_ReadYield`.
        if self.heap.is_empty() {
            return false;
        }
        let top = self.heap.swap_remove(0);
        if self.heap.len() > 1 {
            self.push_down(0);
        }
        self.cur = top;
        true
    }
    fn skip_to(&mut self, id: DocId) -> Skip {
        // The module never skips it (it is always the root).
        while self.read() {
            if self.cur.doc_id >= id {
                return if self.cur.doc_id == id { Skip::Found } else { Skip::NotFound };
            }
        }
        Skip::Eof
    }
    fn current(&mut self) -> Option<&mut IxResult> {
        Some(&mut self.cur)
    }
    fn last_doc_id(&self) -> DocId {
        self.cur.doc_id
    }
    fn at_eof(&self) -> bool {
        self.yielding && self.heap.is_empty()
    }
    fn num_estimated(&self) -> usize {
        let c = self.child.num_estimated();
        self.numeric.as_ref().map_or(c, |n| c.min(n.num_estimated()))
    }
    fn rewind(&mut self) {
        if self.numeric.is_some() {
            self.rewind_ranges();
        }
    }
    fn profile_children(&mut self) {
        profile_child(&mut self.child);
    }
    fn print(&self, pc: &PCtx, c: Option<&Counters>) -> Pairs {
        let mut m = print_leaf("OPTIMIZER", pc, c);
        kv(&mut m, "Optimizer mode", Reply::Status(self.mode.into()));
        kv(&mut m, "Child iterator", Reply::Map(self.child.print(pc, None)));
        m
    }
}

// ---------------------------------------------------------------------------------------------
// The trimmed numeric union (`UnionTrimmed`)

/// `UnionTrimmed`: a numeric union cut to the ranges a LIMIT needs (a prefix ascending, a
/// suffix descending, scanning in from the anchor range), drained child by child from the
/// last kept one.
pub struct UnionTrimmed<'a> {
    children: Vec<BoxIt<'a>>,
    start: usize,
    end: usize,
    cursor: usize,
    eof: bool,
    num_estimated: usize,
    res: IxResult,
}

impl<'a> UnionTrimmed<'a> {
    /// At least three children.
    pub fn new(children: Vec<BoxIt<'a>>, limit: usize, asc: bool) -> Self {
        let num = children.len();
        let mut total = 0usize;
        let (start, end) = if asc {
            let mut keep = num;
            for (i, c) in children[1..].iter().enumerate() {
                total += c.num_estimated();
                if total > limit {
                    keep = i + 2;
                    break;
                }
            }
            (0, keep)
        } else {
            let mut skip = 0;
            for (i, c) in children[1..num - 1].iter().enumerate().rev() {
                total += c.num_estimated();
                if total > limit {
                    skip = i + 1;
                    break;
                }
            }
            (skip, num)
        };
        let num_estimated = children[start..end].iter().map(|c| c.num_estimated()).sum();
        UnionTrimmed { children, start, end, cursor: end - 1, eof: false, num_estimated, res: IxResult { kind: K_UNION, weight: 1.0, ..IxResult::default() } }
    }
}

impl Iter for UnionTrimmed<'_> {
    fn read(&mut self) -> bool {
        self.res.reset_aggregate();
        while !self.eof {
            let c = &mut self.children[self.cursor];
            if c.read() {
                if let Some(r) = c.current() {
                    let r = r.clone();
                    self.res.push(&r);
                }
                return true;
            }
            if self.cursor > self.start {
                self.cursor -= 1;
            } else {
                self.eof = true;
            }
        }
        false
    }
    fn skip_to(&mut self, id: DocId) -> Skip {
        // The module never skips it (its documents are not in id order).
        while self.read() {
            if self.res.doc_id >= id {
                return if self.res.doc_id == id { Skip::Found } else { Skip::NotFound };
            }
        }
        Skip::Eof
    }
    fn current(&mut self) -> Option<&mut IxResult> {
        (!self.eof).then_some(&mut self.res)
    }
    fn last_doc_id(&self) -> DocId {
        self.res.doc_id
    }
    fn at_eof(&self) -> bool {
        self.eof
    }
    fn num_estimated(&self) -> usize {
        self.num_estimated
    }
    fn rewind(&mut self) {
        self.res.reset_aggregate();
        self.eof = false;
        self.cursor = self.end - 1;
        self.children[self.start..self.end].iter_mut().for_each(|c| c.rewind());
    }
    fn kind(&self) -> IKind {
        IKind::Union
    }
    fn sort_weight(&self, p: bool) -> f64 {
        let active = if self.eof { 0 } else { self.cursor - self.start + 1 };
        if p { active.max(1) as f64 } else { 1.0 }
    }
    fn profile_children(&mut self) {
        self.children.iter_mut().for_each(profile_child);
    }
    fn print(&self, pc: &PCtx, c: Option<&Counters>) -> Pairs {
        print_union(&("NUMERIC", None), self.children.iter(), pc, c)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::search::iters;

    #[test]
    fn estimates() {
        assert_eq!(estimate_limit(0, 5, 10), 0);
        assert_eq!(estimate_limit(100, 0, 10), 0);
        assert_eq!(estimate_limit(100, 33, 4), 13);
        assert_eq!(estimate_limit(100, 50, 4), 9);
        assert_eq!(estimate_limit(100, 100, 10), 11);
    }

    #[test]
    fn trimmed_windows() {
        let ids = |v: &[u64]| -> BoxIt<'static> { Box::new(iters::IdList::new(v.to_vec(), IxResult { kind: K_NUMERIC, ..IxResult::default() })) };
        let kids = || vec![ids(&[1, 2]), ids(&[3, 4, 5]), ids(&[6, 7, 8, 9]), ids(&[10])];
        let mut asc = UnionTrimmed::new(kids(), 2, true);
        assert_eq!((asc.start, asc.end), (0, 2));
        let mut got = Vec::new();
        while asc.read() {
            got.push(asc.last_doc_id());
        }
        assert_eq!(got, [3, 4, 5, 1, 2]);
        let desc = UnionTrimmed::new(kids(), 3, false);
        assert_eq!((desc.start, desc.end), (2, 4));
        let all = UnionTrimmed::new(kids(), 100, true);
        assert_eq!((all.start, all.end), (0, 4));
    }
}
