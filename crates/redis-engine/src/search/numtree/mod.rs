//! RediSearch's numeric index (`redisearch_rs/numeric_range_tree`): an adaptive binary tree of
//! value ranges, each holding an entries-tracking numeric inverted index and a HyperLogLog. Leaves
//! split at their median once their estimated cardinality passes a depth-dependent threshold,
//! inner nodes keep their range up to `search-_numeric-ranges-parents` levels, and AVL-like single
//! rotations bound the depth imbalance. Sizes, split points and every `AddResult` delta match the
//! reference, so `inverted_sz`, `num_records` and `FT.DEBUG NUMIDX_SUMMARY` do too.

pub mod hll;

#[cfg(test)]
mod tests;

use std::sync::atomic::{AtomicU32, Ordering};

use super::invidx::{AddOutcome, DocId, Encoding, InvertedIndex, Reader, Record, flags};
use hll::Hll;

/// `size_of::<NumericRangeTree>()` (`NumericRangeTree_BaseSize`): root key (8), node slab (40),
/// stats (40), last doc id (8), revision and unique ids (4 each) and the compression flag, padded.
pub const BASE_SIZE: usize = 112;
/// `size_of::<generational_slab::Entry<NumericRangeNode>>()`: one slot of the node arena.
pub const NODE_SLOT_SIZE: usize = 176;
/// `RawVec`'s first non-zero capacity for elements of this size.
const MIN_ARENA_CAPACITY: usize = 4;

/// `MINIMUM_RANGE_CARDINALITY`: the split threshold at depth 0.
pub const MINIMUM_RANGE_CARDINALITY: usize = 16;
/// `MAXIMUM_RANGE_CARDINALITY`: the threshold's cap.
pub const MAXIMUM_RANGE_CARDINALITY: usize = 2500;
/// `MAXIMUM_RANGE_SIZE`: entries forcing a split of a range with more than one value.
pub const MAXIMUM_RANGE_SIZE: usize = 10000;
/// `MAXIMUM_DEPTH_IMBALANCE`: subtree depth difference that triggers a rotation.
pub const MAXIMUM_DEPTH_IMBALANCE: u32 = 2;
/// `CARDINALITY_GROWTH_FACTOR`: the threshold's growth per level.
pub const CARDINALITY_GROWTH_FACTOR: usize = 4;
/// `LAST_DEPTH_WITHOUT_MAXIMUM_CARDINALITY`: `ilog2(2500 / 16) / ilog2(4)` = 3.
const LAST_DEPTH_WITHOUT_MAXIMUM_CARDINALITY: usize =
    (MAXIMUM_RANGE_CARDINALITY / MINIMUM_RANGE_CARDINALITY).ilog2() as usize / CARDINALITY_GROWTH_FACTOR.ilog2() as usize;

/// `get_split_cardinality`: 16, 64, 256, 1024, then 2500.
pub const fn split_cardinality(depth: usize) -> usize {
    if depth > LAST_DEPTH_WITHOUT_MAXIMUM_CARDINALITY {
        MAXIMUM_RANGE_CARDINALITY
    } else {
        MINIMUM_RANGE_CARDINALITY * CARDINALITY_GROWTH_FACTOR.pow(depth as u32)
    }
}

/// `UNIQUE_ID_COUNTER`: process-wide, starting at 0.
static UNIQUE_ID_COUNTER: AtomicU32 = AtomicU32::new(0);

/// `NumericRange`: value bounds (inverted while empty), the cardinality estimate and the
/// `(doc id, value)` entries.
#[derive(Clone, Debug, PartialEq)]
pub struct NumericRange {
    min_val: f64,
    max_val: f64,
    hll: Hll,
    entries: InvertedIndex,
}

impl Default for NumericRange {
    fn default() -> Self {
        Self::new(false)
    }
}

impl NumericRange {
    /// `NumericRange::new`: an `EntriesTrackingIndex<Numeric>` (or `NumericFloatCompression`)
    /// with `Index_StoreNumeric`.
    pub const fn new(compress_floats: bool) -> Self {
        let encoding = if compress_floats { Encoding::NumericFloatCompression } else { Encoding::Numeric };
        Self {
            min_val: f64::INFINITY,
            max_val: f64::NEG_INFINITY,
            hll: Hll::new(),
            entries: InvertedIndex::with_encoding(encoding, flags::STORE_NUMERIC),
        }
    }

    /// `NumericRange::add`: counts the value in the HLL, then stores it.
    pub fn add(&mut self, doc_id: DocId, value: f64) -> AddOutcome {
        self.hll.add(value);
        self.add_without_cardinality(doc_id, value)
    }

    /// `add_without_cardinality`: bounds and entries only (retained inner ranges, splits).
    pub fn add_without_cardinality(&mut self, doc_id: DocId, value: f64) -> AddOutcome {
        if value < self.min_val {
            self.min_val = value;
        }
        if value > self.max_val {
            self.max_val = value;
        }
        self.entries.add_record(&Record::numeric(doc_id, value))
    }

    /// The HLL estimate of distinct values.
    pub fn cardinality(&self) -> usize {
        self.hll.count()
    }

    /// `contained_in`: `[min_val, max_val]` lies within `[min, max]`.
    pub fn contained_in(&self, min: f64, max: f64) -> bool {
        self.min_val >= min && self.max_val <= max
    }

    /// `overlaps`.
    pub fn overlaps(&self, min: f64, max: f64) -> bool {
        !(min > self.max_val || max < self.min_val)
    }

    pub const fn min_val(&self) -> f64 {
        self.min_val
    }

    pub const fn max_val(&self) -> f64 {
        self.max_val
    }

    /// Records stored, duplicates of a multi-value doc included.
    pub const fn num_entries(&self) -> usize {
        self.entries.number_of_entries()
    }

    /// Distinct doc ids stored.
    pub const fn num_docs(&self) -> u32 {
        self.entries.unique_docs()
    }

    pub fn num_blocks(&self) -> usize {
        self.entries.number_of_blocks()
    }

    /// `memory_usage`: the inverted index's (32 bytes when empty).
    pub fn memory_usage(&self) -> usize {
        self.entries.memory_usage()
    }

    pub const fn entries(&self) -> &InvertedIndex {
        &self.entries
    }

    pub const fn hll(&self) -> &Hll {
        &self.hll
    }

    pub const fn hll_mut(&mut self) -> &mut Hll {
        &mut self.hll
    }

    /// A reader over the entries, in doc id order (supports `seek`).
    pub fn reader(&self) -> Reader<'_> {
        self.entries.reader()
    }

    /// Every `(doc id, value)` in doc id order, values as stored (compressed ones decoded).
    pub fn iter(&self) -> impl Iterator<Item = (DocId, f64)> + '_ {
        self.entries.reader().map(|r| (r.doc_id, r.numeric))
    }
}

/// `NodeIndex`: a slot of the tree's node arena.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct NodeId(u32);

impl NodeId {
    pub const fn position(self) -> u32 {
        self.0
    }
}

/// `InternalNode`: values below `value` go left, the rest right.
#[derive(Clone, Debug, PartialEq)]
pub struct InternalNode {
    pub value: f64,
    /// `max(left, right) + 1`.
    pub max_depth: u32,
    pub left: NodeId,
    pub right: NodeId,
    /// The whole subtree's entries, kept while `max_depth <= max_depth_range`.
    pub range: Option<NumericRange>,
}

/// `NumericRangeNode`.
#[derive(Clone, Debug, PartialEq)]
pub enum Node {
    Leaf(NumericRange),
    Internal(InternalNode),
}

impl Node {
    pub const fn is_leaf(&self) -> bool {
        matches!(self, Self::Leaf(_))
    }

    /// 0 for leaves.
    pub const fn max_depth(&self) -> u32 {
        match self {
            Self::Leaf(_) => 0,
            Self::Internal(n) => n.max_depth,
        }
    }

    pub const fn split_value(&self) -> Option<f64> {
        match self {
            Self::Leaf(_) => None,
            Self::Internal(n) => Some(n.value),
        }
    }

    /// Always `Some` for leaves.
    pub const fn range(&self) -> Option<&NumericRange> {
        match self {
            Self::Leaf(r) => Some(r),
            Self::Internal(n) => n.range.as_ref(),
        }
    }

    pub const fn range_mut(&mut self) -> Option<&mut NumericRange> {
        match self {
            Self::Leaf(r) => Some(r),
            Self::Internal(n) => n.range.as_mut(),
        }
    }

    pub const fn children(&self) -> Option<(NodeId, NodeId)> {
        match self {
            Self::Leaf(_) => None,
            Self::Internal(n) => Some((n.left, n.right)),
        }
    }

    const fn internal(&self) -> &InternalNode {
        match self {
            Self::Internal(n) => n,
            Self::Leaf(_) => panic!("not an internal node"),
        }
    }
}

/// `NodeArena` over `generational_slab::Slab`: nodes by slot, with the capacity the slab's `Vec`
/// would have (it only ever grows through `push`).
#[derive(Clone, Debug, Default)]
struct Arena {
    nodes: Vec<Node>,
    cap: usize,
}

impl Arena {
    fn insert(&mut self, node: Node) -> NodeId {
        if self.nodes.len() == self.cap {
            self.cap = (self.cap * 2).max(self.nodes.len() + 1).max(MIN_ARENA_CAPACITY);
        }
        self.nodes.push(node);
        NodeId((self.nodes.len() - 1) as u32)
    }

    const fn mem_usage(&self) -> usize {
        self.cap * NODE_SLOT_SIZE
    }
}

impl std::ops::Index<NodeId> for Arena {
    type Output = Node;

    fn index(&self, id: NodeId) -> &Node {
        &self.nodes[id.0 as usize]
    }
}

impl std::ops::IndexMut<NodeId> for Arena {
    fn index_mut(&mut self, id: NodeId) -> &mut Node {
        &mut self.nodes[id.0 as usize]
    }
}

/// `AddResult` (`#[repr(C)]`, returned by `_NumericRangeTree_Add`): what one add changed.
/// `numericIndexer` adds `size_delta` to `invertedSize`, `num_records_delta` to `numRecords`
/// and `block_count_delta` to the spec's block count.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct AddResult {
    /// Inverted index bytes (node and struct overhead excluded).
    pub size_delta: i64,
    /// Records written to every range touched (re-adds on splits, removals of dropped ranges).
    pub num_records_delta: i32,
    /// A split or rotation happened (the revision id moved).
    pub changed: bool,
    pub num_ranges_delta: i32,
    pub num_leaves_delta: i32,
    pub block_count_delta: i32,
}

impl AddResult {
    fn absorb_dropped(&mut self, range: &NumericRange) {
        self.size_delta -= range.memory_usage() as i64;
        self.num_records_delta -= range.num_entries() as i32;
        self.num_ranges_delta -= 1;
        self.block_count_delta -= range.num_blocks() as i32;
    }
}

/// `TreeStats`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct TreeStats {
    num_ranges: usize,
    num_leaves: usize,
    num_entries: usize,
    inverted_indexes_size: usize,
    empty_leaves: usize,
}

/// `NumericFilter` as the tree search uses it.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct NumericFilter {
    pub min: f64,
    pub max: f64,
    pub min_inclusive: bool,
    pub max_inclusive: bool,
    /// Visit order: ascending returns ranges low to high.
    pub ascending: bool,
    /// Docs wanted after `offset`; 0 for all.
    pub limit: usize,
    pub offset: usize,
}

impl Default for NumericFilter {
    fn default() -> Self {
        Self { min: 0.0, max: f64::MAX, min_inclusive: true, max_inclusive: true, ascending: true, limit: 0, offset: 0 }
    }
}

impl NumericFilter {
    /// `[min, max]`, both inclusive, ascending, unlimited.
    pub fn range(min: f64, max: f64) -> Self {
        Self { min, max, ..Self::default() }
    }

    /// `value_in_range`.
    pub fn value_in_range(&self, value: f64) -> bool {
        let min_ok = value > self.min || (self.min_inclusive && value == self.min);
        let max_ok = value < self.max || (self.max_inclusive && value == self.max);
        min_ok && max_ok
    }
}

/// A range `find` returned. `contained` is `NumericIteratorVariant::from_tree`'s test for
/// reading it without the filter (both bounds pass `value_in_range`); geo filters always need
/// the per-record check.
#[derive(Clone, Copy, Debug)]
pub struct FoundRange<'a> {
    pub range: &'a NumericRange,
    pub contained: bool,
}

/// `NumericRangeTree`.
#[derive(Clone, Debug)]
pub struct NumericRangeTree {
    root: NodeId,
    nodes: Arena,
    stats: TreeStats,
    last_doc_id: DocId,
    revision_id: u32,
    unique_id: u32,
    compress_floats: bool,
}

impl Default for NumericRangeTree {
    fn default() -> Self {
        Self::new(false)
    }
}

/// `BalanceResult`.
#[derive(Default)]
struct Balance {
    new_depth: u32,
    dropped: AddResult,
}

impl NumericRangeTree {
    /// `NumericRangeTree::new` with the next process-wide unique id.
    pub fn new(compress_floats: bool) -> Self {
        Self::with_unique_id(compress_floats, UNIQUE_ID_COUNTER.fetch_add(1, Ordering::Relaxed))
    }

    /// A tree with an explicit unique id (the reference draws it from a global counter).
    pub fn with_unique_id(compress_floats: bool, unique_id: u32) -> Self {
        let mut nodes = Arena::default();
        let root_node = Node::Leaf(NumericRange::new(compress_floats));
        let size = root_node.range().map_or(0, NumericRange::memory_usage);
        let root = nodes.insert(root_node);
        Self {
            root,
            nodes,
            stats: TreeStats { num_ranges: 1, num_leaves: 1, num_entries: 0, inverted_indexes_size: size, empty_leaves: 1 },
            last_doc_id: 0,
            revision_id: 0,
            unique_id,
            compress_floats,
        }
    }

    /// `NumericRangeTree_BaseSize`.
    pub const fn base_size() -> usize {
        BASE_SIZE
    }

    /// The root's range size, which `open_numeric_or_geo_index` adds to `invertedSize` when it
    /// creates the tree.
    pub fn initial_size(&self) -> usize {
        self.root().range().map_or(0, NumericRange::memory_usage)
    }

    pub fn root(&self) -> &Node {
        &self.nodes[self.root]
    }

    pub const fn root_id(&self) -> NodeId {
        self.root
    }

    pub fn node(&self, id: NodeId) -> &Node {
        &self.nodes[id]
    }

    pub fn node_mut(&mut self, id: NodeId) -> &mut Node {
        &mut self.nodes[id]
    }

    pub const fn num_ranges(&self) -> usize {
        self.stats.num_ranges
    }

    pub const fn num_leaves(&self) -> usize {
        self.stats.num_leaves
    }

    /// Adds accepted (one per value).
    pub const fn num_entries(&self) -> usize {
        self.stats.num_entries
    }

    /// Every range's inverted index bytes.
    pub const fn inverted_indexes_size(&self) -> usize {
        self.stats.inverted_indexes_size
    }

    pub const fn empty_leaves(&self) -> usize {
        self.stats.empty_leaves
    }

    pub const fn last_doc_id(&self) -> DocId {
        self.last_doc_id
    }

    /// Bumped on every structural change; iterators compare it to detect one.
    pub const fn revision_id(&self) -> u32 {
        self.revision_id
    }

    pub const fn increment_revision(&mut self) -> u32 {
        self.revision_id = self.revision_id.wrapping_add(1);
        self.revision_id
    }

    pub const fn unique_id(&self) -> u32 {
        self.unique_id
    }

    pub const fn compress_floats(&self) -> bool {
        self.compress_floats
    }

    /// `NumericRangeTree_MemUsage`: the struct, the inverted indexes and the arena's capacity.
    pub const fn mem_usage(&self) -> usize {
        BASE_SIZE + self.stats.inverted_indexes_size + self.nodes.mem_usage()
    }

    /// `ReversePreOrderDfsIterator`: parent, then right subtree, then left.
    pub fn iter(&self) -> Nodes<'_> {
        Nodes { tree: self, stack: vec![self.root] }
    }

    /// Every range (leaves and retained inner ones) in `iter` order.
    pub fn ranges(&self) -> impl Iterator<Item = &NumericRange> + '_ {
        self.iter().filter_map(Node::range)
    }

    /// `NumericRangeTree::add` (`_NumericRangeTree_Add`): doc ids must not decrease, and repeat
    /// only when `is_multi`; anything else is ignored with an empty result.
    pub fn add(&mut self, doc_id: DocId, value: f64, is_multi: bool, max_depth_range: usize) -> AddResult {
        if doc_id < self.last_doc_id || (doc_id == self.last_doc_id && !is_multi) {
            return AddResult::default();
        }
        self.last_doc_id = doc_id;
        let mut ctx = AddCtx {
            nodes: &mut self.nodes,
            empty_leaves: &mut self.stats.empty_leaves,
            doc_id,
            value,
            max_depth_range: u32::try_from(max_depth_range).unwrap_or(u32::MAX),
            compress_floats: self.compress_floats,
        };
        let rv = ctx.node_add(self.root, 0);
        if rv.changed {
            self.revision_id = self.revision_id.wrapping_add(1);
        }
        let s = &mut self.stats;
        s.num_ranges = s.num_ranges.saturating_add_signed(rv.num_ranges_delta as isize);
        s.num_leaves = s.num_leaves.saturating_add_signed(rv.num_leaves_delta as isize);
        s.num_entries += 1;
        s.inverted_indexes_size = s.inverted_indexes_size.saturating_add_signed(rv.size_delta as isize);
        rv
    }

    /// `NumericRangeTree::find` (`NumericRangeTree_Find`): the non-empty ranges a query needs,
    /// in the filter's order. A range inside `[min, max]` stands for its whole subtree; leaves
    /// that only overlap are returned too. With a limit, the walk stops once the docs counted
    /// reach `offset + limit` (the first partial leaf counts as one doc), and ranges are only
    /// returned once the count passes `offset`.
    pub fn find(&self, filter: &NumericFilter) -> Vec<&NumericRange> {
        let mut out = Vec::with_capacity(8);
        let mut total = 0;
        self.find_in(self.root, filter, &mut total, &mut out);
        out
    }

    /// `find`, with whether each range can be read without the filter.
    pub fn find_ranges(&self, filter: &NumericFilter) -> Vec<FoundRange<'_>> {
        self.find(filter)
            .into_iter()
            .map(|range| FoundRange {
                range,
                contained: filter.value_in_range(range.min_val()) && filter.value_in_range(range.max_val()),
            })
            .collect()
    }

    /// `recursive_find_ranges`.
    fn find_in<'a>(&'a self, id: NodeId, f: &NumericFilter, total: &mut usize, out: &mut Vec<&'a NumericRange>) {
        if f.limit > 0 && *total >= f.offset.saturating_add(f.limit) {
            return;
        }
        let node = &self.nodes[id];
        if let Some(range) = node.range() {
            let num_docs = range.num_docs() as usize;
            if num_docs == 0 {
                return;
            }
            if range.contained_in(f.min, f.max) {
                *total += num_docs;
                if *total > f.offset {
                    out.push(range);
                }
                return;
            }
            if !range.overlaps(f.min, f.max) {
                return;
            }
        }
        match node {
            Node::Internal(n) => {
                let go_left = f.min <= n.value;
                let go_right = f.max >= n.value;
                if f.ascending {
                    if go_left {
                        self.find_in(n.left, f, total, out);
                    }
                    if go_right {
                        self.find_in(n.right, f, total, out);
                    }
                } else {
                    if go_right {
                        self.find_in(n.right, f, total, out);
                    }
                    if go_left {
                        self.find_in(n.left, f, total, out);
                    }
                }
            }
            Node::Leaf(range) => {
                *total += if *total == 0 && f.offset == 0 { 1 } else { range.num_docs() as usize };
                if *total > f.offset {
                    out.push(range);
                }
            }
        }
    }
}

/// The recursive add's state (`node_add`'s arguments).
struct AddCtx<'a> {
    nodes: &'a mut Arena,
    empty_leaves: &'a mut usize,
    doc_id: DocId,
    value: f64,
    max_depth_range: u32,
    compress_floats: bool,
}

impl AddCtx<'_> {
    /// `node_add`: descend by split value; a leaf takes the value (with cardinality) and splits
    /// when its estimate reaches `split_cardinality(depth)` or it holds more than
    /// `MAXIMUM_RANGE_SIZE` entries of more than one value. On the way up, retained ranges take
    /// the value (without cardinality), and after a split each ancestor rebalances and drops its
    /// range once deeper than `max_depth_range`.
    fn node_add(&mut self, id: NodeId, depth: usize) -> AddResult {
        let (doc_id, value) = (self.doc_id, self.value);
        if let Node::Internal(n) = &self.nodes[id] {
            let child = if value < n.value { n.left } else { n.right };
            let mut rv = self.node_add(child, depth + 1);
            if let Node::Internal(n) = &mut self.nodes[id]
                && let Some(range) = n.range.as_mut()
            {
                let out = range.add_without_cardinality(doc_id, value);
                rv.size_delta += i64::from(out.mem_growth);
                rv.block_count_delta += out.blocks_added as i32;
                rv.num_records_delta += 1;
            }
            if rv.changed {
                let b = balance_node(self.nodes, id);
                rv.size_delta += b.dropped.size_delta;
                rv.num_records_delta += b.dropped.num_records_delta;
                rv.num_ranges_delta += b.dropped.num_ranges_delta;
                rv.block_count_delta += b.dropped.block_count_delta;
                if let Node::Internal(n) = &mut self.nodes[id] {
                    n.max_depth = b.new_depth;
                }
                if b.new_depth > self.max_depth_range {
                    remove_range(self.nodes, id, &mut rv);
                }
            }
            return rv;
        }

        let Node::Leaf(range) = &mut self.nodes[id] else { unreachable!() };
        if range.num_docs() == 0 {
            *self.empty_leaves = self.empty_leaves.saturating_sub(1);
        }
        let out = range.add(doc_id, value);
        let mut rv = AddResult {
            size_delta: i64::from(out.mem_growth),
            num_records_delta: 1,
            block_count_delta: out.blocks_added as i32,
            ..AddResult::default()
        };
        let card = range.cardinality();
        if card >= split_cardinality(depth) || (range.num_entries() > MAXIMUM_RANGE_SIZE && card > 1) {
            self.split_node(id, &mut rv);
            if self.nodes[id].max_depth() > self.max_depth_range {
                remove_range(self.nodes, id, &mut rv);
            }
        }
        rv
    }

    /// `split_node`: the leaf becomes an internal node at its median (moved up to the next
    /// float when it equals the minimum) keeping its range; two fresh leaves take its entries
    /// as stored, `< split` left.
    fn split_node(&mut self, id: NodeId, rv: &mut AddResult) {
        let parent = match std::mem::replace(&mut self.nodes[id], Node::Leaf(NumericRange::default())) {
            Node::Leaf(range) => range,
            Node::Internal(_) => unreachable!("only leaves split"),
        };
        let median = compute_median(&parent);
        let split = if median == parent.min_val() { median.next_up() } else { median };

        let mut left = NumericRange::new(self.compress_floats);
        let mut right = NumericRange::new(self.compress_floats);
        rv.size_delta += (left.memory_usage() + right.memory_usage()) as i64;
        for (doc_id, v) in parent.iter() {
            let target = if v < split { &mut left } else { &mut right };
            let out = target.add(doc_id, v);
            rv.size_delta += i64::from(out.mem_growth);
            rv.block_count_delta += out.blocks_added as i32;
            rv.num_records_delta += 1;
        }
        // Float compression can collapse every value to one side.
        *self.empty_leaves += usize::from(left.num_docs() == 0) + usize::from(right.num_docs() == 0);

        let left = self.nodes.insert(Node::Leaf(left));
        let right = self.nodes.insert(Node::Leaf(right));
        self.nodes[id] = Node::Internal(InternalNode { value: split, max_depth: 1, left, right, range: Some(parent) });
        rv.changed = true;
        rv.num_ranges_delta += 2;
        rv.num_leaves_delta += 1;
    }
}

/// `compute_median`: the stored value at index `len / 2` in `total_cmp` order (0 when empty).
fn compute_median(range: &NumericRange) -> f64 {
    let mut values: Vec<f64> = range.iter().map(|(_, v)| v).collect();
    if values.is_empty() {
        return 0.0;
    }
    let mid = values.len() / 2;
    *values.select_nth_unstable_by(mid, f64::total_cmp).1
}

/// `remove_range`: an inner node stops retaining its range.
fn remove_range(nodes: &mut Arena, id: NodeId, rv: &mut AddResult) {
    if let Node::Internal(n) = &mut nodes[id]
        && let Some(range) = n.range.take()
    {
        rv.absorb_dropped(&range);
    }
}

/// `balance_node`: a single rotation when one side is more than `MAXIMUM_DEPTH_IMBALANCE`
/// deeper; ranges of the two rotated nodes are dropped.
fn balance_node(nodes: &mut Arena, id: NodeId) -> Balance {
    let Node::Internal(n) = &nodes[id] else {
        return Balance { new_depth: nodes[id].max_depth(), ..Balance::default() };
    };
    let left_depth = nodes[n.left].max_depth();
    let right_depth = nodes[n.right].max_depth();
    let dropped = if right_depth > left_depth + MAXIMUM_DEPTH_IMBALANCE {
        rotate_left(nodes, id)
    } else if left_depth > right_depth + MAXIMUM_DEPTH_IMBALANCE {
        rotate_right(nodes, id)
    } else {
        return Balance { new_depth: left_depth.max(right_depth) + 1, ..Balance::default() };
    };
    let mut b = Balance { new_depth: nodes[id].max_depth(), ..Balance::default() };
    for range in dropped.into_iter().flatten().flatten() {
        b.dropped.absorb_dropped(&range);
    }
    b
}

/// Takes an inner node out of its slot (left as an empty leaf until rewritten).
fn take_internal(nodes: &mut Arena, id: NodeId) -> InternalNode {
    match std::mem::replace(&mut nodes[id], Node::Leaf(NumericRange::default())) {
        Node::Internal(n) => n,
        Node::Leaf(_) => unreachable!(),
    }
}

/// `InternalNode::rotate_left`: the right child B is promoted into A's slot, A is rebuilt in
/// B's slot over A.left and B.left; both lose their ranges. `None` when B is a leaf.
fn rotate_left(nodes: &mut Arena, id: NodeId) -> Option<[Option<NumericRange>; 2]> {
    let b_id = nodes[id].internal().right;
    if nodes[b_id].is_leaf() {
        return None;
    }
    let b = take_internal(nodes, b_id);
    let a = take_internal(nodes, id);
    let demoted_depth = nodes[a.left].max_depth().max(nodes[b.left].max_depth()) + 1;
    nodes[b_id] = Node::Internal(InternalNode { value: a.value, max_depth: demoted_depth, left: a.left, right: b.left, range: None });
    let promoted_depth = demoted_depth.max(nodes[b.right].max_depth()) + 1;
    nodes[id] = Node::Internal(InternalNode { value: b.value, max_depth: promoted_depth, left: b_id, right: b.right, range: None });
    Some([b.range, a.range])
}

/// `InternalNode::rotate_right`: the mirror image.
fn rotate_right(nodes: &mut Arena, id: NodeId) -> Option<[Option<NumericRange>; 2]> {
    let b_id = nodes[id].internal().left;
    if nodes[b_id].is_leaf() {
        return None;
    }
    let b = take_internal(nodes, b_id);
    let a = take_internal(nodes, id);
    let demoted_depth = nodes[b.right].max_depth().max(nodes[a.right].max_depth()) + 1;
    nodes[b_id] = Node::Internal(InternalNode { value: a.value, max_depth: demoted_depth, left: b.right, right: a.right, range: None });
    let promoted_depth = nodes[b.left].max_depth().max(demoted_depth) + 1;
    nodes[id] = Node::Internal(InternalNode { value: b.value, max_depth: promoted_depth, left: b.left, right: b_id, range: None });
    Some([b.range, a.range])
}

/// `ReversePreOrderDfsIterator`.
#[derive(Clone, Debug)]
pub struct Nodes<'a> {
    tree: &'a NumericRangeTree,
    stack: Vec<NodeId>,
}

impl<'a> Iterator for Nodes<'a> {
    type Item = &'a Node;

    fn next(&mut self) -> Option<&'a Node> {
        let id = self.stack.pop()?;
        let node = &self.tree.nodes[id];
        if let Node::Internal(n) = node {
            self.stack.push(n.left);
            self.stack.push(n.right);
        }
        Some(node)
    }
}
