//! RediSearch 8.10.1's rune trie (`trie/trie.c`, `trie/trie_node.c`, `trie/rune_util.c`): the
//! radix tree over 16-bit runes behind suggestion keys (FT.SUG*), spelling dictionaries
//! (FT.DICT*), spell-check suggestions, an index's term trie and its suffix trie.
//!
//! Nodes hold a label, flags (terminal / tombstoned), a score, the subtree's maximum score,
//! a document count and an optional payload. Children are kept in the order the module keeps
//! them: by first rune in lexicographic tries, by descending subtree maximum in score tries
//! (except right after a split, which orders the two halves by rune), so every iteration,
//! range and fuzzy walk visits terms in the module's order.

use super::levenshtein::{DfaFilter, MatchMode};
use super::tokenize::{utf8_read, utf8_write};

/// A 16-bit rune: code points above U+FFFF are truncated on conversion.
pub type Rune = u16;

/// `TRIE_INITIAL_STRING_LEN`: keys must be shorter than this many runes.
pub const TRIE_INITIAL_STRING_LEN: usize = 256;
/// `TRIE_MAX_PREFIX`: the longest fuzzy query, in runes.
pub const TRIE_MAX_PREFIX: usize = 100;
/// `MAX_RUNE_STR_LEN`: the most runes converted at once.
pub const MAX_RUNE_STR_LEN: usize = 1024;
/// `SCORE_TRIM_FACTOR`.
pub const SCORE_TRIM_FACTOR: f64 = 10.0;

/// `TrieNode_Add` results.
pub const TRIE_OK_NEW: i32 = 1;
pub const TRIE_OK_UPDATED: i32 = 0;
pub const TRIE_ERR_PAYLOAD_OVERFLOW: i32 = -1;

/// The suggestion dictionaries' module type (`TrieType`).
pub const TYPE_NAME: &str = "trietype0";

/// `TRIE_ENCVER_*`: the `trietype0` RDB versions.
pub const TRIE_ENCVER_CURRENT: i32 = 2;
pub const TRIE_ENCVER_NUMDOCS: i32 = 2;
pub const TRIE_ENCVER_PAYLOADS: i32 = 1;

/// `sizeof(TrieNode)`: the packed header (two lengths, the flag bits, two floats, the document
/// count and the payload pointer).
pub const NODE_HEADER_SIZE: usize = 29;

/// The iterator's stack depth (`TRIE_INITIAL_STRING_LEN - 1` frames); deeper nodes are skipped.
const MAX_STACK: usize = TRIE_INITIAL_STRING_LEN - 1;
/// `TIMEOUT_COUNTER_LIMIT`: the clock is consulted once per this many visited nodes.
const TIMEOUT_COUNTER_LIMIT: u32 = 100;

const TERMINAL: u8 = 0x1;
const DELETED: u8 = 0x2;

/// `TrieSortMode`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SortMode {
    Lex,
    Score,
}

/// `TrieAddOp`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AddOp {
    Replace,
    Incr,
}

/// `TrieDecrResult`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DecrResult {
    NotFound,
    Updated,
    Deleted,
    Unsupported,
}

/// `FilterCode`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FilterCode {
    Continue,
    Stop,
}

/// `StepFilter` plus its `StackPopCallback`: an automaton fed one rune at a time while the
/// iterator descends, rewound by the number of runes a node consumed when it is popped.
pub trait StepFilter {
    /// Feeds rune `b`; sets `matched` when the automaton accepts, may report through
    /// `match_ctx` (the edit distance, for the Levenshtein filter).
    fn step(&mut self, b: Rune, matched: &mut bool, match_ctx: Option<&mut i32>) -> FilterCode;
    /// Rewinds `levels` runes.
    fn pop(&mut self, levels: usize);
}

/// The absent filter of `Trie_IterateAll`.
#[derive(Clone, Copy, Debug, Default)]
pub struct NoFilter;

impl StepFilter for NoFilter {
    fn step(&mut self, _: Rune, _: &mut bool, _: Option<&mut i32>) -> FilterCode {
        FilterCode::Continue
    }
    fn pop(&mut self, _: usize) {}
}

/// `MAX` from `sys/param.h`.
fn fmax(a: f32, b: f32) -> f32 {
    if a > b { a } else { b }
}

// ---------------------------------------------------------------------------------------------
// Runes
// ---------------------------------------------------------------------------------------------

/// `strToRunesN` / `runeBufFill`: UTF-8 (decoded leniently, the lead byte deciding the length)
/// to runes, stopping at a NUL code point; code points are truncated to 16 bits.
pub fn str_to_runes(s: &[u8]) -> Vec<Rune> {
    let mut out = Vec::with_capacity(s.len());
    let mut i = 0;
    while i < s.len() {
        let (cp, n) = utf8_read(&s[i..]);
        i += n.max(1);
        if cp == 0 {
            break;
        }
        out.push(cp as Rune);
    }
    out
}

/// The code points up to the first NUL (`nu_readnstr`).
fn code_points(s: &[u8]) -> Vec<u32> {
    let mut out = Vec::with_capacity(s.len());
    let mut i = 0;
    while i < s.len() {
        let (cp, n) = utf8_read(&s[i..]);
        i += n.max(1);
        if cp == 0 {
            break;
        }
        out.push(cp);
    }
    out
}

/// `strToRunes`: as [`str_to_runes`] but refusing more than [`MAX_RUNE_STR_LEN`] runes.
pub fn str_to_runes_limited(s: &[u8]) -> Option<Vec<Rune>> {
    let r = str_to_runes(s);
    (r.len() <= MAX_RUNE_STR_LEN).then_some(r)
}

/// `runesToStr`: UTF-8 up to the first zero rune; `None` past [`MAX_RUNE_STR_LEN`] runes.
pub fn runes_to_str(r: &[Rune]) -> Option<Vec<u8>> {
    if r.len() > MAX_RUNE_STR_LEN {
        return None;
    }
    let mut out = Vec::with_capacity(r.len());
    for &c in r {
        if c == 0 {
            break;
        }
        utf8_write(c as u32, &mut out);
    }
    Some(out)
}

/// `__fold`: the first code point of the full case folding (libnu's table equals
/// `casefold::fold` for every scalar value), the code point itself without one.
fn fold_cp(cp: u32) -> u32 {
    if cp < 0x80 {
        return (cp as u8).to_ascii_lowercase() as u32;
    }
    match char::from_u32(cp) {
        Some(c) => {
            let mut b = [0u8; 4];
            super::casefold::fold(c.encode_utf8(&mut b)).chars().next().map_or(cp, |f| f as u32)
        }
        None => cp,
    }
}

/// `runeFold`.
pub fn rune_fold(r: Rune) -> Rune {
    fold_cp(r as u32) as Rune
}

/// `strToLowerRunes`: every code point of each lowercase mapping; `None` past
/// [`MAX_RUNE_STR_LEN`] runes.
pub fn str_to_lower_runes(s: &[u8]) -> Option<Vec<Rune>> {
    let mut out = Vec::with_capacity(s.len());
    for cp in code_points(s) {
        if cp < 0x80 {
            out.push((cp as u8).to_ascii_lowercase() as Rune);
            continue;
        }
        match char::from_u32(cp) {
            Some(c) => out.extend(c.to_lowercase().map(|l| l as u32 as Rune)),
            None => out.push(cp as Rune),
        }
    }
    (out.len() <= MAX_RUNE_STR_LEN).then_some(out)
}

/// `strToSingleCodepointFoldedRunes`: each code point folded to the first code point of its
/// folding; `None` past [`MAX_RUNE_STR_LEN`] runes.
pub fn str_to_folded_runes(s: &[u8]) -> Option<Vec<Rune>> {
    let cps = code_points(s);
    if cps.len() > MAX_RUNE_STR_LEN {
        return None;
    }
    Some(cps.into_iter().map(|cp| fold_cp(cp) as Rune).collect())
}

/// `runecmp`.
fn runecmp(a: &[Rune], b: &[Rune]) -> i32 {
    for (x, y) in a.iter().zip(b) {
        let rc = *x as i32 - *y as i32;
        if rc != 0 {
            return rc;
        }
    }
    match a.len().cmp(&b.len()) {
        std::cmp::Ordering::Greater => 1,
        std::cmp::Ordering::Less => -1,
        std::cmp::Ordering::Equal => 0,
    }
}

// ---------------------------------------------------------------------------------------------
// Nodes
// ---------------------------------------------------------------------------------------------

/// `TrieNode`.
#[derive(Clone, Debug, PartialEq)]
pub struct TrieNode {
    label: Vec<Rune>,
    children: Vec<TrieNode>,
    flags: u8,
    score: f32,
    subtree_max: f32,
    num_docs: usize,
    payload: Option<Vec<u8>>,
}

/// A payload as `TrieNode_Add` sees it: the bytes and the length the caller claims (they differ
/// only for a corrupt RDB entry, whose wrapped length overflows).
#[derive(Clone, Copy)]
struct PayloadArg<'p> {
    data: &'p [u8],
    len: usize,
}

impl<'p> PayloadArg<'p> {
    fn of(p: Option<&'p [u8]>) -> Option<Self> {
        p.map(|data| PayloadArg { data, len: data.len() })
    }

    /// Stored only when non-empty.
    fn stored(p: Option<Self>) -> Option<Vec<u8>> {
        p.filter(|p| p.len > 0).map(|p| p.data[..p.len.min(p.data.len())].to_vec())
    }
}

impl TrieNode {
    /// `__newTrieNode`.
    fn new(label: &[Rune], payload: Option<PayloadArg>, score: f32, terminal: bool, num_docs: usize) -> Self {
        TrieNode {
            label: label.to_vec(),
            children: Vec::new(),
            flags: if terminal { TERMINAL } else { 0 },
            score,
            subtree_max: score,
            num_docs,
            payload: PayloadArg::stored(payload),
        }
    }

    /// The node's own runes.
    pub fn label(&self) -> &[Rune] {
        &self.label
    }

    /// `TrieNode_NumChildren`.
    pub fn num_children(&self) -> usize {
        self.children.len()
    }

    /// `TrieNode_Children`.
    pub fn children(&self) -> &[TrieNode] {
        &self.children
    }

    /// `TrieNode_ChildAt`.
    pub fn child_at(&self, i: usize) -> &TrieNode {
        &self.children[i]
    }

    /// `TrieNode_IsTerminal`.
    pub fn is_terminal(&self) -> bool {
        self.flags & TERMINAL != 0
    }

    /// Tombstoned by a delete but kept for its children.
    pub fn is_deleted(&self) -> bool {
        self.flags & DELETED != 0
    }

    /// `TrieNode_NumDocs`.
    pub fn num_docs(&self) -> usize {
        self.num_docs
    }

    pub fn score(&self) -> f32 {
        self.score
    }

    /// `subtreeMaxScore`: the pruning bound of score tries.
    pub fn subtree_max_score(&self) -> f32 {
        self.subtree_max
    }

    /// `TrieNode_GetPayloadData`.
    pub fn payload(&self) -> Option<&[u8]> {
        self.payload.as_deref()
    }

    pub fn payload_mut(&mut self) -> Option<&mut Vec<u8>> {
        self.payload.as_mut()
    }

    /// The rune at `i`, reading the zero slot past the label as the module does.
    fn rune_at(&self, i: usize) -> Rune {
        self.label.get(i).copied().unwrap_or(0)
    }

    fn key(&self) -> Rune {
        self.rune_at(0)
    }

    /// `updateScore`.
    fn update_score(&mut self, v: f32, sort: SortMode) {
        if sort == SortMode::Score {
            self.subtree_max = fmax(self.subtree_max, v);
        }
    }

    /// `__trie_AddChildIdx`: a new terminal child at `idx`.
    fn add_child_idx(&mut self, s: &[Rune], payload: Option<PayloadArg>, score: f32, idx: usize, num_docs: usize) {
        self.children.insert(idx, TrieNode::new(s, payload, score, true, num_docs));
    }

    /// `__trie_SplitNode`: the label's tail from `offset`, the children, score, flags and payload
    /// move to a single new child; this node keeps the head, non-terminal and empty.
    fn split(&mut self, offset: usize, sort: SortMode) {
        let child = TrieNode {
            label: self.label[offset..].to_vec(),
            children: std::mem::take(&mut self.children),
            flags: self.flags,
            score: self.score,
            subtree_max: self.subtree_max,
            num_docs: self.num_docs,
            payload: self.payload.take(),
        };
        self.label.truncate(offset);
        self.score = 0.0;
        self.num_docs = 0;
        self.flags &= !(TERMINAL | DELETED);
        self.update_score(child.score, sort);
        self.children.push(child);
    }

    /// `__trieNode_MergeWithSingleChild`: a non-terminal node with one child absorbs it.
    fn merge_with_single_child(&mut self) {
        if self.is_terminal() || self.children.len() != 1 {
            return;
        }
        let ch = self.children.pop().unwrap();
        self.label.extend_from_slice(&ch.label);
        self.children = ch.children;
        self.score = ch.score;
        self.subtree_max = ch.subtree_max;
        self.num_docs = ch.num_docs;
        self.payload = ch.payload;
        self.flags = ch.flags;
    }

    /// `__trieNode_rotateChildIntoPlace`: move child `idx` left past siblings with a strictly
    /// lower bound.
    fn rotate_child_into_place(&mut self, idx: usize) {
        let bound = self.children[idx].subtree_max;
        let mut dst = idx;
        while dst > 0 && self.children[dst - 1].subtree_max < bound {
            dst -= 1;
        }
        if dst < idx {
            self.children[dst..=idx].rotate_right(1);
        }
    }

    /// `__trieNode_Add`.
    fn add(&mut self, s: &[Rune], payload: Option<PayloadArg>, score: f32, op: AddOp, num_docs: usize, sort: SortMode) -> i32 {
        if s.is_empty() {
            return TRIE_OK_UPDATED;
        }
        let mut offset = 0;
        while offset < s.len() && offset < self.label.len() && s[offset] == self.label[offset] {
            offset += 1;
        }
        if offset < self.label.len() {
            self.split(offset, sort);
            if offset == s.len() {
                // The new key ends exactly at the split point.
                self.score = score;
                self.flags |= TERMINAL;
                self.num_docs += num_docs;
                self.payload = PayloadArg::stored(payload);
                self.update_score(self.score, sort);
            } else {
                // Two halves, ordered by rune in either sort mode.
                let idx = usize::from(s[offset] > self.children[0].key());
                self.add_child_idx(&s[offset..], payload, score, idx, num_docs);
                self.update_score(score, sort);
            }
            return TRIE_OK_NEW;
        }
        if offset == s.len() {
            let was_live = self.is_terminal() && !self.is_deleted();
            match op {
                AddOp::Incr => self.score += score,
                AddOp::Replace => self.score = score,
            }
            self.update_score(self.score, sort);
            self.num_docs += num_docs;
            if let Some(p) = PayloadArg::stored(payload) {
                self.payload = Some(p);
            }
            self.flags |= TERMINAL;
            self.flags &= !DELETED;
            return if was_live { TRIE_OK_UPDATED } else { TRIE_OK_NEW };
        }
        let c = s[offset];
        let rest = &s[offset..];
        match sort {
            SortMode::Lex => {
                let mut idx = 0;
                while idx < self.children.len() {
                    let key = self.children[idx].key();
                    if c == key {
                        let rc = self.children[idx].add(rest, payload, score, op, num_docs, sort);
                        let m = self.children[idx].subtree_max;
                        self.update_score(m, sort);
                        return rc;
                    }
                    if c < key {
                        break;
                    }
                    idx += 1;
                }
                self.add_child_idx(rest, payload, score, idx, num_docs);
                self.update_score(score, sort);
                TRIE_OK_NEW
            }
            SortMode::Score => {
                let mut score_idx = None;
                for idx in 0..self.children.len() {
                    if c == self.children[idx].key() {
                        let rc = self.children[idx].add(rest, payload, score, op, num_docs, sort);
                        let m = self.children[idx].subtree_max;
                        self.update_score(m, sort);
                        self.rotate_child_into_place(idx);
                        return rc;
                    }
                    if self.children[idx].subtree_max < score && score_idx.is_none() {
                        score_idx = Some(idx);
                    }
                }
                let idx = score_idx.unwrap_or(self.children.len());
                self.add_child_idx(rest, payload, score, idx, num_docs);
                self.update_score(score, sort);
                TRIE_OK_NEW
            }
        }
    }

    /// `TrieNode_Add`: the payload's allocation size (`len + 5`) must fit 32 bits.
    fn add_checked(&mut self, s: &[Rune], payload: Option<PayloadArg>, score: f32, op: AddOp, num_docs: usize, sort: SortMode) -> i32 {
        if s.is_empty() {
            return TRIE_OK_UPDATED;
        }
        if let Some(p) = payload
            && p.len.checked_add(5).is_none_or(|n| n > u32::MAX as usize)
        {
            return TRIE_ERR_PAYLOAD_OVERFLOW;
        }
        self.add(s, payload, score, op, num_docs, sort)
    }

    /// `TrieNode_Get`'s walk: the child indexes down to the node and the global offset where it
    /// starts (`offsetOut`). Tombstoned nodes are not found.
    fn find(&self, s: &[Rune], exact: bool) -> Option<(Vec<usize>, usize)> {
        let mut n = self;
        let mut path = Vec::new();
        let mut offset = 0;
        while offset < s.len() {
            let mut local = 0;
            while offset < s.len() && local < n.label.len() && s[offset] == n.label[local] {
                offset += 1;
                local += 1;
            }
            if offset == s.len() {
                if local == n.label.len() || !exact {
                    return (!n.is_deleted()).then_some((path, offset - local));
                }
            } else if local == n.label.len() {
                let i = n.children.iter().position(|ch| ch.key() == s[offset])?;
                path.push(i);
                n = &n.children[i];
            } else {
                return None;
            }
        }
        None
    }

    fn at_path(&self, path: &[usize]) -> &TrieNode {
        path.iter().fold(self, |n, &i| &n.children[i])
    }

    fn at_path_mut(&mut self, path: &[usize]) -> &mut TrieNode {
        path.iter().fold(self, |n, &i| &mut n.children[i])
    }

    /// `TrieNode_Get`.
    pub fn get(&self, s: &[Rune], exact: bool) -> Option<&TrieNode> {
        self.find(s, exact).map(|(p, _)| self.at_path(&p))
    }

    /// `TrieNode_Get` with `offsetOut`.
    pub fn get_with_offset(&self, s: &[Rune], exact: bool) -> Option<(&TrieNode, usize)> {
        self.find(s, exact).map(|(p, off)| (self.at_path(&p), off))
    }

    /// `__trieNode_optimizeChildren`: recompute the bound, drop one childless tombstone (the
    /// siblings after it shift down unmerged), merge single-child children, re-sort score
    /// tries.
    fn optimize_children(&mut self, sort: SortMode) {
        self.subtree_max = self.score;
        let mut i = 0;
        while i < self.children.len() {
            if self.children[i].children.is_empty() && self.children[i].is_deleted() {
                self.children.remove(i);
                for j in i..self.children.len() {
                    let m = self.children[j].subtree_max;
                    self.update_score(m, sort);
                }
                break;
            }
            if self.children[i].children.len() == 1 {
                self.children[i].merge_with_single_child();
            }
            let m = self.children[i].subtree_max;
            self.update_score(m, sort);
            i += 1;
        }
        if sort == SortMode::Score && self.children.len() > 1 {
            self.children.sort_by(|a, b| {
                if a.subtree_max < b.subtree_max {
                    std::cmp::Ordering::Greater
                } else if a.subtree_max > b.subtree_max {
                    std::cmp::Ordering::Less
                } else {
                    runecmp(&a.label, &b.label).cmp(&0)
                }
            });
        }
    }

    /// `TrieNode_Delete`: tombstones the key, then optimizes every node on the path, deepest
    /// first.
    fn delete(&mut self, s: &[Rune], sort: SortMode) -> i32 {
        let mut local = 0;
        while local < s.len() && local < self.label.len() && s[local] == self.label[local] {
            local += 1;
        }
        let rc = if local == s.len() {
            if local == self.label.len() && !self.is_deleted() && self.is_terminal() {
                self.flags |= DELETED;
                self.flags &= !TERMINAL;
                self.score = 0.0;
                self.num_docs = 0;
                1
            } else {
                0
            }
        } else if local == self.label.len() {
            match self.children.iter().position(|ch| ch.key() == s[local]) {
                Some(i) => self.children[i].delete(&s[local..], sort),
                None => 0,
            }
        } else {
            0
        };
        self.optimize_children(sort);
        rc
    }
}

// ---------------------------------------------------------------------------------------------
// Iteration
// ---------------------------------------------------------------------------------------------

#[derive(Clone, Copy, PartialEq, Eq)]
enum State {
    SelfRunes,
    Children,
    Match,
}

struct Frame<'a> {
    state: State,
    n: &'a TrieNode,
    string_offset: usize,
    child_offset: usize,
}

enum Step {
    Stop,
    Cont,
    Match,
}

/// An entry yielded by [`TrieIterator::next_entry`].
#[derive(Clone, Copy, Debug)]
pub struct IterEntry<'s, 'a> {
    pub runes: &'s [Rune],
    pub score: f32,
    pub num_docs: usize,
    pub payload: Option<&'a [u8]>,
}

/// An entry copied out of the iterator: (runes, score, numDocs, payload).
pub type OwnedEntry = (Vec<Rune>, f32, usize, Option<Vec<u8>>);

/// `TrieIterator`: a depth-first walk with an optional step filter. Children whose subtree
/// bound is below `kth_best_score` are skipped (the top-k pruning of `Trie_CollectFuzzy`).
pub struct TrieIterator<'a, F: StepFilter = NoFilter> {
    buf: Vec<Rune>,
    stack: Vec<Frame<'a>>,
    filter: Option<F>,
    pub kth_best_score: f32,
    pub nodes_consumed: usize,
    pub nodes_skipped: usize,
}

impl<'a, F: StepFilter> TrieIterator<'a, F> {
    /// `TrieNode_Iterate`.
    pub fn new(root: &'a TrieNode, filter: Option<F>) -> Self {
        let mut it = TrieIterator {
            buf: Vec::with_capacity(TRIE_INITIAL_STRING_LEN + 1),
            stack: Vec::with_capacity(16),
            filter,
            kth_best_score: i32::MIN as f32,
            nodes_consumed: 0,
            nodes_skipped: 0,
        };
        it.push(root);
        it
    }

    pub fn filter(&self) -> Option<&F> {
        self.filter.as_ref()
    }

    /// `__ti_Push`: silently refused past the stack's depth.
    fn push(&mut self, n: &'a TrieNode) {
        if self.stack.len() < MAX_STACK {
            self.stack.push(Frame { state: State::SelfRunes, n, string_offset: 0, child_offset: 0 });
        }
    }

    /// `__ti_Pop`.
    fn pop(&mut self) {
        if let Some(f) = self.stack.pop() {
            if let Some(filter) = self.filter.as_mut() {
                filter.pop(f.string_offset);
            }
            let l = self.buf.len().saturating_sub(f.string_offset);
            self.buf.truncate(l);
        }
    }

    /// `__ti_step`.
    fn step(&mut self, match_ctx: Option<&mut i32>) -> Step {
        let Some(cur) = self.stack.last_mut() else {
            return Step::Stop;
        };
        if cur.state == State::Match {
            self.pop();
            return Step::Cont;
        }
        if cur.state == State::SelfRunes {
            let n = cur.n;
            if cur.string_offset < n.label.len() {
                let b = n.label[cur.string_offset];
                let mut matched = false;
                if let Some(f) = self.filter.as_mut()
                    && f.step(b, &mut matched, match_ctx) == FilterCode::Stop
                {
                    if matched {
                        self.stack.last_mut().unwrap().state = State::Match;
                        return Step::Match;
                    }
                    self.pop();
                    return Step::Cont;
                }
                self.buf.push(b);
                let cur = self.stack.last_mut().unwrap();
                cur.string_offset += 1;
                if self.filter.is_none() && cur.string_offset == n.label.len() && n.is_terminal() && !n.is_deleted() {
                    matched = true;
                }
                return if matched { Step::Match } else { Step::Cont };
            }
            cur.state = State::Children;
        }
        let cur = self.stack.last_mut().unwrap();
        let n = cur.n;
        if cur.child_offset < n.children.len() {
            let ch = &n.children[cur.child_offset];
            cur.child_offset += 1;
            if ch.subtree_max >= self.kth_best_score {
                self.push(ch);
                self.nodes_consumed += 1;
            } else {
                self.nodes_skipped += 1;
            }
        } else {
            self.pop();
        }
        Step::Cont
    }

    /// `TrieIterator_Next` without a match context.
    pub fn next_entry(&mut self) -> Option<IterEntry<'_, 'a>> {
        self.next_with(None)
    }

    /// `TrieIterator_Next`: the next live terminal the filter accepts; `match_ctx` receives what
    /// the filter reports (and keeps its value when it reports nothing).
    pub fn next_with(&mut self, mut match_ctx: Option<&mut i32>) -> Option<IterEntry<'_, 'a>> {
        loop {
            match self.step(match_ctx.as_deref_mut()) {
                Step::Stop => return None,
                Step::Cont => {}
                Step::Match => {
                    let f = self.stack.last().unwrap();
                    let n = f.n;
                    if n.is_terminal() && n.label.len() == f.string_offset && !n.is_deleted() {
                        return Some(IterEntry { runes: &self.buf, score: n.score, num_docs: n.num_docs, payload: n.payload.as_deref() });
                    }
                }
            }
        }
    }

    /// Every remaining entry.
    pub fn collect_all(mut self) -> Vec<OwnedEntry> {
        let mut out = Vec::new();
        while let Some(e) = self.next_entry() {
            out.push((e.runes.to_vec(), e.score, e.num_docs, e.payload.map(<[u8]>::to_vec)));
        }
        out
    }
}

/// `TrieNode_RandomWalk`: at least `min_steps` (and 4) random moves, each up a level or into a
/// child (`rand() % (children + 1) - 1`), until resting on a terminal. Returns the node and its
/// key. `rand` stands for libc's `rand()`.
pub fn random_walk<'a>(root: &'a TrieNode, min_steps: i32, rand: &mut dyn FnMut() -> i32) -> Option<(&'a TrieNode, Vec<Rune>)> {
    if root.children.is_empty() && !root.is_terminal() {
        return None;
    }
    let min_steps = min_steps.max(4);
    let mut stack = vec![root];
    let mut steps = 0;
    while steps < min_steps || !stack.last().unwrap().is_terminal() {
        let n = *stack.last().unwrap();
        let rnd = rand() % (n.children.len() as i32 + 1) - 1;
        if rnd < 0 {
            if stack.len() > 1 {
                steps += 1;
                stack.pop();
            }
            continue;
        }
        stack.push(&n.children[rnd as usize]);
        steps += 1;
    }
    let key = stack.iter().flat_map(|n| n.label.iter().copied()).collect();
    Some((stack.last().unwrap(), key))
}

// ---------------------------------------------------------------------------------------------
// Range, contains and wildcard walks
// ---------------------------------------------------------------------------------------------

/// `TrieRangeCallback`: (key, payload, numDocs) → keep going (`REDISEARCH_OK`).
pub type RangeCallback<'c> = dyn FnMut(&[Rune], Option<&[u8]>, usize) -> bool + 'c;
/// A timeout probe: `None` skips the checks (`skipTimeoutChecks`); otherwise consulted once
/// per [`TIMEOUT_COUNTER_LIMIT`] nodes, `true` meaning timed out.
pub type TimeoutProbe<'t> = Option<&'t mut dyn FnMut() -> bool>;

struct RangeCtx<'c, 't, 'o> {
    buf: Vec<Rune>,
    cb: &'c mut RangeCallback<'c>,
    include_min: bool,
    include_max: bool,
    orig: &'o [Rune],
    prefix: bool,
    contains_stars: bool,
    stop: bool,
    counter: u32,
    timeout: TimeoutProbe<'t>,
}

impl<'c, 't, 'o> RangeCtx<'c, 't, 'o> {
    fn new(cb: &'c mut RangeCallback<'c>, timeout: TimeoutProbe<'t>) -> Self {
        RangeCtx {
            buf: Vec::with_capacity(TRIE_INITIAL_STRING_LEN),
            cb,
            include_min: false,
            include_max: false,
            orig: &[],
            prefix: false,
            contains_stars: false,
            stop: false,
            counter: 0,
            timeout,
        }
    }

    /// `TimedOut_WithCounter`.
    fn timed_out(&mut self) -> bool {
        match self.timeout.as_mut() {
            None => false,
            Some(f) => {
                self.counter += 1;
                if self.counter == TIMEOUT_COUNTER_LIMIT {
                    self.counter = 0;
                    f()
                } else {
                    false
                }
            }
        }
    }

    /// `array_trimm_len`.
    fn trim(&mut self, k: usize) {
        let l = self.buf.len().saturating_sub(k);
        self.buf.truncate(l);
    }

    /// The pattern rune at `i`, its NUL terminator past the end.
    fn orig_at(&self, i: usize) -> Rune {
        self.orig.get(i).copied().unwrap_or(0)
    }
}

/// `rangeIterateSubTree`: every terminal under `n`, in child order; a callback refusal stops
/// the whole walk.
fn range_subtree(n: &TrieNode, r: &mut RangeCtx) -> bool {
    if r.stop {
        return false;
    }
    if r.timed_out() {
        r.stop = true;
        return false;
    }
    r.buf.extend_from_slice(&n.label);
    if n.is_terminal() && !(r.cb)(&r.buf, n.payload.as_deref(), n.num_docs) {
        r.stop = true;
        return false;
    }
    for ch in &n.children {
        if !range_subtree(ch, r) {
            return false;
        }
    }
    r.trim(n.label.len());
    true
}

/// `rsbCompareCommon`: the target against a child's label, over the shorter length in prefix
/// mode.
fn rsb_cmp(term: &[Rune], elem: &TrieNode, prefix: bool) -> i32 {
    if prefix {
        let m = elem.label.len().min(term.len());
        runecmp(&term[..m], &elem.label[..m])
    } else {
        runecmp(term, &elem.label)
    }
}

/// `rsb_gt`: the first child above the target.
fn rsb_gt(arr: &[TrieNode], term: &[Rune], prefix: bool) -> i64 {
    let narr = arr.len() as i64;
    let (mut begin, mut end) = (0i64, narr - 1);
    while begin < end {
        let cur = (begin + end) / 2;
        let rc = rsb_cmp(term, &arr[cur as usize], prefix);
        if rc < 0 {
            end = cur;
        } else {
            begin = cur + 1;
        }
    }
    if begin != narr - 1 {
        return begin;
    }
    if rsb_cmp(term, &arr[begin as usize], prefix) >= 0 {
        begin += 1;
    }
    begin
}

/// `rsb_lt`: the last child below the target, -1 for none.
fn rsb_lt(arr: &[TrieNode], term: &[Rune], prefix: bool) -> i64 {
    let (mut begin, mut end) = (0i64, arr.len() as i64 - 1);
    while begin < end {
        let cur = (begin + end) / 2 + (begin + end) % 2;
        let rc = rsb_cmp(term, &arr[cur as usize], prefix);
        if rc <= 0 {
            end = cur - 1;
        } else {
            begin = cur;
        }
    }
    if begin != 0 {
        return begin;
    }
    if rsb_cmp(term, &arr[0], prefix) <= 0 {
        return -1;
    }
    begin
}

/// `rsb_eq`: a child equal to the target, -1 for none.
fn rsb_eq(arr: &[TrieNode], term: &[Rune], prefix: bool) -> i64 {
    let (mut begin, mut end) = (0i64, arr.len() as i64 - 1);
    while begin < end {
        let cur = (begin + end) / 2 + (begin + end) % 2;
        let rc = rsb_cmp(term, &arr[cur as usize], prefix);
        if rc == 0 {
            return cur;
        } else if rc < 0 {
            end = cur - 1;
        } else {
            begin = cur;
        }
    }
    if rsb_cmp(term, &arr[begin as usize], prefix) == 0 { begin } else { -1 }
}

/// A bound's remainder below a child: `n < 0` clamps to an exhausted (0-length) bound.
fn descend(bound: &[Rune], n: i32, child_len: usize) -> (&[Rune], i32) {
    let next = n - child_len as i32;
    if next < 0 { (&[], 0) } else { (&bound[child_len.min(bound.len())..], next) }
}

/// `rangeIterate`: `nmin`/`nmax` are the bounds' remaining lengths, -1 for unbounded and 0 for
/// a bound consumed exactly (which is where the include flags apply).
fn range_iterate(n: &TrieNode, min: &[Rune], nmin: i32, max: &[Rune], nmax: i32, r: &mut RangeCtx) {
    r.buf.extend_from_slice(&n.label);
    if n.is_terminal() && ((r.include_min && nmin == 0) || (r.include_max && nmax == 0)) {
        (r.cb)(&r.buf, None, n.num_docs);
    }
    let arr = &n.children;
    if !arr.is_empty() {
        let minr = &min[..(nmin.max(0) as usize).min(min.len())];
        let maxr = &max[..(nmax.max(0) as usize).min(max.len())];
        let begin_eq = if nmin > 0 { rsb_eq(arr, minr, true) } else { -1 };
        let end_eq = if nmax > 0 { rsb_eq(arr, maxr, true) } else { -1 };
        if begin_eq == end_eq && end_eq != -1 {
            // Both bounds continue through the same child.
            let child = &arr[begin_eq as usize];
            let (nm, nn) = descend(min, nmin, child.label.len());
            let (xm, xn) = descend(max, nmax, child.label.len());
            range_iterate(child, nm, nn, xm, xn, r);
        } else {
            if begin_eq != -1 {
                let child = &arr[begin_eq as usize];
                let (nm, nn) = descend(min, nmin, child.label.len());
                range_iterate(child, nm, nn, &[], -1, r);
            }
            let mut begin_idx = if nmin > 0 { rsb_gt(arr, minr, false) } else { 0 };
            let mut end_idx = if nmax != 0 { arr.len() as i64 - 1 } else { -1 };
            if nmax > 0 {
                end_idx = rsb_lt(arr, maxr, false);
            }
            if begin_eq != -1 && begin_idx <= begin_eq {
                begin_idx = begin_eq + 1;
            }
            if end_eq != -1 && end_idx >= end_eq {
                end_idx = end_eq - 1;
            }
            let mut i = begin_idx;
            while i <= end_idx {
                range_subtree(&arr[i as usize], r);
                i += 1;
            }
            if end_eq != -1 {
                let child = &arr[end_eq as usize];
                let (xm, xn) = descend(max, nmax, child.label.len());
                range_iterate(child, &[], -1, xm, xn, r);
            }
        }
    }
    r.trim(n.label.len());
}

/// `containsNext`: the node's next rune, or its children once the label is consumed.
fn contains_next(n: &TrieNode, local: usize, global: usize, r: &mut RangeCtx) {
    if n.label.len() == local || n.label.is_empty() {
        for ch in &n.children {
            if r.stop {
                break;
            }
            contains_iterate(ch, 0, global, r);
        }
    } else {
        contains_iterate(n, local, global, r);
    }
}

/// `containsIterate`: matches the pattern rune by rune from every position (contains mode
/// yields a matching node's whole subtree, once per occurrence; suffix mode yields terminals
/// ending right after the pattern).
fn contains_iterate(n: &TrieNode, local: usize, global: usize, r: &mut RangeCtx) {
    if (n.children.is_empty() && r.orig.len() as i64 - global as i64 > n.label.len() as i64) || r.stop {
        return;
    }
    if r.timed_out() {
        r.stop = true;
        return;
    }
    if !n.label.is_empty() {
        r.buf.push(n.label[local]);
    }
    if n.rune_at(local) == r.orig_at(global) {
        if global + 1 == r.orig.len() {
            if r.prefix {
                r.trim(local + 1);
                range_subtree(n, r);
                r.buf.extend_from_slice(&n.label[..local.min(n.label.len())]);
                return;
            }
            if n.is_terminal() && local + 1 == n.label.len() && !(r.cb)(&r.buf, None, n.num_docs) {
                r.stop = true;
            }
            contains_next(n, local + 1, 0, r);
            if !n.label.is_empty() {
                r.trim(1);
            }
            return;
        }
        contains_next(n, local + 1, global + 1, r);
    }
    if global == 0 {
        contains_next(n, local + 1, 0, r);
    }
    if !n.label.is_empty() {
        r.trim(1);
    }
}

/// `match_t`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WildcardMatch {
    Full,
    Partial,
    No,
}

/// `Wildcard_MatchRune`: `*` any run, `?` any rune; `Partial` when the string ran out first
/// (more runes could still match).
pub fn wildcard_match_rune(pattern: &[Rune], s: &[Rune]) -> WildcardMatch {
    const STAR: Rune = b'*' as Rune;
    const QMARK: Rune = b'?' as Rune;
    let (pl, sl) = (pattern.len(), s.len());
    let (mut pi, mut si) = (0usize, 0usize);
    let mut back: Option<(usize, usize)> = None;
    loop {
        if pi != pl {
            let c = pattern[pi];
            if si < sl && (c == s[si] || c == QMARK) {
                si += 1;
                pi += 1;
                continue;
            } else if c == STAR {
                while pi != pl && pattern[pi] == STAR {
                    pi += 1;
                }
                let d = pattern.get(pi).copied().unwrap_or(0);
                while si < sl && !(d == s[si] || d == QMARK) {
                    si += 1;
                }
                back = Some((pi - 1, si + 1));
                continue;
            }
        } else if si >= sl {
            return WildcardMatch::Full;
        }
        if si >= sl {
            return WildcardMatch::Partial;
        }
        match back {
            None => return WildcardMatch::No,
            Some((p, q)) => {
                pi = p;
                si = q;
            }
        }
    }
}

/// `wildcardIterate`.
fn wildcard_iterate(n: &TrieNode, r: &mut RangeCtx) {
    if r.timed_out() {
        r.stop = true;
    }
    if r.stop {
        return;
    }
    r.buf.extend_from_slice(&n.label);
    let m = wildcard_match_rune(r.orig, &r.buf);
    if m == WildcardMatch::Full {
        if r.prefix {
            r.trim(n.label.len());
            range_subtree(n, r);
            return;
        }
        if n.is_terminal() {
            (r.cb)(&r.buf, n.payload.as_deref(), n.num_docs);
        }
    }
    if m != WildcardMatch::No && (r.contains_stars || r.buf.len() < r.orig.len()) {
        for ch in &n.children {
            if r.stop {
                break;
            }
            wildcard_iterate(ch, r);
        }
    }
    r.trim(n.label.len());
}

// ---------------------------------------------------------------------------------------------
// The trie
// ---------------------------------------------------------------------------------------------

/// `TrieSearchResult`.
#[derive(Clone, Debug, PartialEq)]
pub struct TrieSearchResult {
    pub str: Vec<u8>,
    pub score: f32,
    pub payload: Option<Vec<u8>>,
}

/// `Trie`: the root (an empty label), the live key count and the sort mode.
#[derive(Clone, Debug, PartialEq)]
pub struct Trie {
    root: TrieNode,
    size: usize,
    sort: SortMode,
}

impl Trie {
    /// `NewTrie`.
    pub fn new(sort: SortMode) -> Self {
        Trie { root: TrieNode::new(&[], None, 0.0, false, 0), size: 0, sort }
    }

    /// `Trie_Size`.
    pub fn size(&self) -> usize {
        self.size
    }

    pub fn sort_mode(&self) -> SortMode {
        self.sort
    }

    pub fn root(&self) -> &TrieNode {
        &self.root
    }

    /// `Trie_Insert` / `Trie_InsertStringBuffer`: keys over 512 bytes are refused (0).
    pub fn insert(&mut self, s: &[u8], score: f64, incr: bool, payload: Option<&[u8]>, num_docs: usize) -> i32 {
        self.insert_buffer(s, score, incr, PayloadArg::of(payload), num_docs)
    }

    fn insert_buffer(&mut self, s: &[u8], score: f64, incr: bool, payload: Option<PayloadArg>, num_docs: usize) -> i32 {
        if s.len() > TRIE_INITIAL_STRING_LEN * 2 {
            return 0;
        }
        let runes = str_to_runes(s);
        self.insert_runes_arg(&runes, score, incr, payload, num_docs)
    }

    /// `Trie_InsertRune`: empty keys and keys of [`TRIE_INITIAL_STRING_LEN`] runes or more are
    /// refused (0); the size grows on `TRIE_OK_NEW`.
    pub fn insert_runes(&mut self, runes: &[Rune], score: f64, incr: bool, payload: Option<&[u8]>, num_docs: usize) -> i32 {
        self.insert_runes_arg(runes, score, incr, PayloadArg::of(payload), num_docs)
    }

    fn insert_runes_arg(&mut self, runes: &[Rune], score: f64, incr: bool, payload: Option<PayloadArg>, num_docs: usize) -> i32 {
        if runes.is_empty() || runes.len() >= TRIE_INITIAL_STRING_LEN {
            return 0;
        }
        let op = if incr { AddOp::Incr } else { AddOp::Replace };
        let rc = self.root.add_checked(runes, payload, score as f32, op, num_docs, self.sort);
        if rc == TRIE_OK_NEW {
            self.size += 1;
        }
        rc
    }

    /// `Trie_InsertRuneNoSize`: no length guard (beyond the 16-bit length the node API takes)
    /// and no size bookkeeping.
    pub fn insert_runes_no_size(&mut self, runes: &[Rune], score: f64, incr: bool, payload: Option<&[u8]>, num_docs: usize) -> i32 {
        let len = runes.len() as u16 as usize;
        let op = if incr { AddOp::Incr } else { AddOp::Replace };
        self.root.add_checked(&runes[..len], PayloadArg::of(payload), score as f32, op, num_docs, self.sort)
    }

    /// `Trie_Delete`: keys over [`TRIE_INITIAL_STRING_LEN`] runes are not looked up.
    pub fn delete(&mut self, s: &[u8]) -> i32 {
        let runes = str_to_runes(s);
        if runes.len() > TRIE_INITIAL_STRING_LEN {
            return 0;
        }
        self.delete_runes(&runes)
    }

    /// `Trie_DeleteRunes`: 1 when a live key was tombstoned.
    pub fn delete_runes(&mut self, runes: &[Rune]) -> i32 {
        let len = runes.len() as u16 as usize;
        if len == 0 {
            return 0;
        }
        let rc = self.root.delete(&runes[..len], self.sort);
        self.size -= rc as usize;
        rc
    }

    /// `Trie_GetNode`.
    pub fn get_node(&self, runes: &[Rune], exact: bool) -> Option<&TrieNode> {
        self.root.get(&runes[..runes.len() as u16 as usize], exact)
    }

    /// `Trie_GetNode` with `offsetOut`: where the found node starts within `runes`.
    pub fn get_node_with_offset(&self, runes: &[Rune], exact: bool) -> Option<(&TrieNode, usize)> {
        self.root.get_with_offset(&runes[..runes.len() as u16 as usize], exact)
    }

    /// `Trie_GetNode` for callers that edit a node's payload in place (the suffix trie).
    pub fn get_node_mut(&mut self, runes: &[Rune], exact: bool) -> Option<&mut TrieNode> {
        let runes = &runes[..runes.len() as u16 as usize];
        let (path, _) = self.root.find(runes, exact)?;
        Some(self.root.at_path_mut(&path))
    }

    /// `Trie_DecrementNumDocs`: a key reaching zero documents is deleted.
    pub fn decrement_num_docs(&mut self, s: &[u8], delta: usize) -> DecrResult {
        if s.len() > TRIE_INITIAL_STRING_LEN * 2 {
            return DecrResult::Unsupported;
        }
        let runes = str_to_runes(s);
        if runes.is_empty() || runes.len() >= TRIE_INITIAL_STRING_LEN {
            return DecrResult::Unsupported;
        }
        let Some((path, _)) = self.root.find(&runes, true) else {
            return DecrResult::NotFound;
        };
        let node = self.root.at_path_mut(&path);
        if !node.is_terminal() {
            return DecrResult::NotFound;
        }
        node.num_docs = node.num_docs.saturating_sub(delta);
        if node.num_docs == 0 {
            if self.root.delete(&runes, self.sort) == 1 {
                self.size -= 1;
                return DecrResult::Deleted;
            }
            return DecrResult::Updated;
        }
        DecrResult::Updated
    }

    /// `Trie_IterateAll`.
    pub fn iterate_all(&self) -> TrieIterator<'_, NoFilter> {
        TrieIterator::new(&self.root, None)
    }

    /// `Trie_IterateFuzzy`: the query lowercased (`None` past [`TRIE_MAX_PREFIX`] runes), trie
    /// runes lowercased as they are fed.
    pub fn iterate_fuzzy(&self, s: &[u8], max_dist: i32, mode: MatchMode) -> Option<TrieIterator<'_, DfaFilter>> {
        let runes = str_to_lower_runes(s)?;
        if runes.len() > TRIE_MAX_PREFIX {
            return None;
        }
        Some(TrieIterator::new(&self.root, Some(DfaFilter::lowering(&runes, max_dist, mode))))
    }

    /// `Trie_CollectFuzzy` (FT.SUGGET): the best `num` matches by adjusted score (an exact match
    /// scores `INT_MAX`; fuzzy matches lose `exp(-2·dist)`; prefix matches are divided by
    /// `sqrt(1 + |runes - query bytes|)`), kept in the module's binary heap, best first, then
    /// optionally trimmed below a tenth of the best. `None` for queries over 200 bytes or
    /// [`TRIE_MAX_PREFIX`] runes.
    pub fn collect_fuzzy(&self, s: &[u8], num: usize, max_dist: i32, mode: MatchMode, trim: bool, _optimize: bool) -> Option<Vec<TrieSearchResult>> {
        if s.len() > TRIE_MAX_PREFIX * 2 {
            return None;
        }
        let runes = str_to_folded_runes(s)?;
        if runes.len() > TRIE_MAX_PREFIX {
            return None;
        }
        if num == 0 {
            return Some(Vec::new());
        }
        let mut heap = ScoreHeap { items: Vec::with_capacity(num) };
        let mut it = TrieIterator::new(&self.root, Some(DfaFilter::folding(&runes, max_dist, mode)));
        let mut dist = max_dist + 1;
        let mut kth = it.kth_best_score;
        loop {
            it.kth_best_score = kth;
            let Some(e) = it.next_with(Some(&mut dist)) else { break };
            let slen = e.runes.len();
            let mut score = if slen > 0 && slen == runes.len() && bytes_eq_le(&runes, e.runes, slen) { i32::MAX as f32 } else { e.score };
            if max_dist > 0 {
                score = (score as f64 * (-(2 * dist) as f64).exp()) as f32;
            }
            if mode == MatchMode::Prefix {
                score = (score as f64 / ((1 + slen.abs_diff(s.len())) as f64).sqrt()) as f32;
            }
            if heap.items.len() < num {
                heap.offer(result(e.runes, score, e.payload));
                if heap.items.len() == num {
                    kth = heap.items[0].score;
                }
            } else if score > kth {
                heap.poll();
                heap.offer(result(e.runes, score, e.payload));
                let top = heap.items[0].score;
                if top > kth {
                    kth = top;
                }
            }
        }
        let n = heap.items.len().min(num);
        let mut ret: Vec<TrieSearchResult> = (0..n).map(|_| heap.poll().unwrap()).collect();
        ret.reverse();
        if trim {
            let mut max_score = 0f32;
            let mut i = 0;
            while i < n {
                let sc = ret[i].score;
                if max_score != 0.0 && (sc as f64) < max_score as f64 / SCORE_TRIM_FACTOR {
                    break;
                }
                max_score = fmax(max_score, sc);
                i += 1;
            }
            ret.truncate(i);
        }
        Some(ret)
    }

    /// `Trie_IterateRange`: `None` bounds are open; an exhausted bound applies its include flag.
    pub fn iterate_range(&self, min: Option<&[Rune]>, include_min: bool, max: Option<&[Rune]>, include_max: bool, cb: &mut RangeCallback) {
        if let (Some(lo), Some(hi)) = (min, max) {
            let cmp = runecmp(lo, hi);
            if cmp > 0 {
                return;
            }
            if cmp == 0 {
                if (include_min || include_max)
                    && let Some(node) = self.get_node(lo, true)
                    && node.is_terminal()
                {
                    cb(lo, None, node.num_docs);
                }
                return;
            }
        }
        let mut r = RangeCtx::new(cb, None);
        r.include_min = include_min;
        r.include_max = include_max;
        let (lo, nlo) = min.map_or((&[][..], -1), |m| (m, m.len() as i32));
        let (hi, nhi) = max.map_or((&[][..], -1), |m| (m, m.len() as i32));
        range_iterate(&self.root, lo, nlo, hi, nhi, &mut r);
    }

    /// `Trie_IterateContains`: prefix (`prefix && !suffix`), suffix, or contains (both) matching;
    /// an exact lookup when neither.
    pub fn iterate_contains(&self, s: &[Rune], prefix: bool, suffix: bool, cb: &mut RangeCallback, timeout: TimeoutProbe) {
        if !prefix && !suffix {
            if let Some(node) = self.get_node(s, true)
                && node.is_terminal()
            {
                cb(s, None, node.num_docs);
            }
            return;
        }
        let mut r = RangeCtx::new(cb, timeout);
        if prefix && !suffix {
            r.buf.extend_from_slice(s);
            if let Some((node, offset)) = self.get_node_with_offset(s, false) {
                let k = r.buf.len() - offset;
                r.trim(k);
                range_subtree(node, &mut r);
            }
            return;
        }
        r.orig = s;
        r.prefix = prefix;
        contains_iterate(&self.root, 0, 0, &mut r);
    }

    /// `Trie_IterateWildcard`: a trailing `*` yields whole subtrees once matched.
    pub fn iterate_wildcard(&self, s: &[Rune], cb: &mut RangeCallback, timeout: TimeoutProbe) {
        let mut r = RangeCtx::new(cb, timeout);
        r.orig = s;
        r.prefix = s.last() == Some(&(b'*' as Rune));
        r.contains_stars = s.contains(&(b'*' as Rune));
        wildcard_iterate(&self.root, &mut r);
    }

    /// `Trie_RandomKey`: a random walk of `2 + rand() % 8 + logb(1 + size)` steps; the key and
    /// its score.
    pub fn random_key(&self, rand: &mut dyn FnMut() -> i32) -> Option<(Vec<u8>, f64)> {
        if self.size == 0 {
            return None;
        }
        let log = (usize::BITS - 1 - (1 + self.size).leading_zeros()) as i32;
        let steps = 2 + rand() % 8 + log;
        let (n, key) = random_walk(&self.root, steps, rand)?;
        Some((runes_to_str(&key).unwrap_or_default(), n.score as f64))
    }

    /// `TrieType_MemUsage`: a flat per-key estimate.
    pub fn mem_usage(&self) -> usize {
        self.size * (NODE_HEADER_SIZE + 8 + 2 + 2 * 2)
    }

    /// `TrieType_GenericSave`: the size, then per key (in iteration order) the UTF-8 key with its
    /// trailing NUL, the score as a double, optionally the payload (with a NUL; a lone NUL when
    /// absent) and the document count.
    pub fn save(&self, out: &mut dyn RdbSink, save_payloads: bool, save_num_docs: bool) {
        out.save_unsigned(self.size as u64);
        let mut it = self.iterate_all();
        while let Some(e) = it.next_entry() {
            let mut s = runes_to_str(e.runes).unwrap_or_default();
            s.push(0);
            out.save_string_buffer(&s);
            out.save_double(e.score as f64);
            if save_payloads {
                match e.payload {
                    Some(p) if !p.is_empty() => {
                        let mut b = p.to_vec();
                        b.push(0);
                        out.save_string_buffer(&b);
                    }
                    _ => out.save_string_buffer(&[0]),
                }
            }
            if save_num_docs {
                out.save_unsigned(e.num_docs as u64);
            }
        }
    }

    /// `TrieType_GenericLoad`: entries re-inserted (replacing) in saved order; `None` on a
    /// truncated stream or an overflowing payload.
    pub fn load(src: &mut dyn RdbSource, load_payloads: bool, load_num_docs: bool, sort: SortMode) -> Option<Trie> {
        let elements = src.load_unsigned()?;
        let mut t = Trie::new(sort);
        for _ in 0..elements {
            let key = src.load_string_buffer()?;
            let score = src.load_double()?;
            let payload = if load_payloads { Some(src.load_string_buffer()?) } else { None };
            let num_docs = if load_num_docs { src.load_unsigned()? as usize } else { 0 };
            let arg = payload.as_ref().map(|p| PayloadArg { data: p, len: p.len().wrapping_sub(1) }).filter(|p| p.len != 0);
            let rc = match key.len().checked_sub(1) {
                Some(l) => t.insert_buffer(&key[..l], score, false, arg, num_docs),
                None => 0,
            };
            if rc == TRIE_ERR_PAYLOAD_OVERFLOW {
                return None;
            }
        }
        Some(t)
    }

    /// `TrieType_RdbLoad` (`trietype0`, FT.SUG* keys): payloads from version 1, document counts
    /// from version 2, score ordered.
    pub fn rdb_load(src: &mut dyn RdbSource, encver: i32) -> Option<Trie> {
        if encver > TRIE_ENCVER_CURRENT {
            return None;
        }
        Trie::load(src, encver >= TRIE_ENCVER_PAYLOADS, encver >= TRIE_ENCVER_NUMDOCS, SortMode::Score)
    }

    /// `TrieType_RdbSave`.
    pub fn rdb_save(&self, out: &mut dyn RdbSink) {
        self.save(out, true, true);
    }
}

/// `memcmp(runes, rstr, len)`: the first `len` bytes of the little-endian rune arrays (half the
/// runes, as the module compares).
fn bytes_eq_le(a: &[Rune], b: &[Rune], len: usize) -> bool {
    (0..len).all(|i| (a[i / 2] >> (8 * (i % 2))) as u8 == (b[i / 2] >> (8 * (i % 2))) as u8)
}

fn result(runes: &[Rune], score: f32, payload: Option<&[u8]>) -> TrieSearchResult {
    TrieSearchResult { str: runes_to_str(runes).unwrap_or_default(), score, payload: payload.map(<[u8]>::to_vec) }
}

/// `heap_t` with `cmpEntries`: the lowest score at the root; equal scores still swap upward.
struct ScoreHeap {
    items: Vec<TrieSearchResult>,
}

impl ScoreHeap {
    /// `cmpEntries`: lower scores compare greater.
    fn cmp(a: &TrieSearchResult, b: &TrieSearchResult) -> i32 {
        if a.score < b.score {
            1
        } else if a.score > b.score {
            -1
        } else {
            0
        }
    }

    /// `heap_offerx` / `__pushup`.
    fn offer(&mut self, item: TrieSearchResult) {
        self.items.push(item);
        let mut idx = self.items.len() - 1;
        while idx != 0 {
            let parent = (idx - 1) / 2;
            if Self::cmp(&self.items[idx], &self.items[parent]) < 0 {
                return;
            }
            self.items.swap(idx, parent);
            idx = parent;
        }
    }

    /// `heap_poll` / `__pushdown`.
    fn poll(&mut self) -> Option<TrieSearchResult> {
        if self.items.is_empty() {
            return None;
        }
        let item = self.items.swap_remove(0);
        let count = self.items.len();
        if count > 1 {
            let mut idx = 0;
            loop {
                let (l, r) = (2 * idx + 1, 2 * idx + 2);
                let child = if r >= count {
                    if l >= count {
                        break;
                    }
                    l
                } else if Self::cmp(&self.items[l], &self.items[r]) < 0 {
                    r
                } else {
                    l
                };
                if Self::cmp(&self.items[idx], &self.items[child]) < 0 {
                    self.items.swap(idx, child);
                    idx = child;
                } else {
                    break;
                }
            }
        }
        Some(item)
    }
}

// ---------------------------------------------------------------------------------------------
// RDB primitives
// ---------------------------------------------------------------------------------------------

/// The `RedisModule_Save*` calls the trie makes.
pub trait RdbSink {
    fn save_unsigned(&mut self, v: u64);
    fn save_string_buffer(&mut self, s: &[u8]);
    fn save_double(&mut self, v: f64);
}

/// The RDB layer's module writer as a sink.
pub struct ModuleSink<'a, M: crate::bloom::ModuleOut>(pub &'a mut M);

impl<M: crate::bloom::ModuleOut> RdbSink for ModuleSink<'_, M> {
    fn save_unsigned(&mut self, v: u64) {
        self.0.uint(v);
    }
    fn save_string_buffer(&mut self, s: &[u8]) {
        self.0.buffer(s);
    }
    fn save_double(&mut self, v: f64) {
        self.0.double(v);
    }
}

/// The RDB layer's module reader as a source.
pub struct ModuleSource<'a, M: crate::bloom::ModuleIn>(pub &'a mut M);

impl<M: crate::bloom::ModuleIn> RdbSource for ModuleSource<'_, M> {
    fn load_unsigned(&mut self) -> Option<u64> {
        self.0.uint()
    }
    fn load_string_buffer(&mut self) -> Option<Vec<u8>> {
        self.0.buffer()
    }
    fn load_double(&mut self) -> Option<f64> {
        self.0.double()
    }
}

/// The `RedisModule_Load*` calls the trie makes; `None` is an IO error.
pub trait RdbSource {
    fn load_unsigned(&mut self) -> Option<u64>;
    fn load_string_buffer(&mut self) -> Option<Vec<u8>>;
    fn load_double(&mut self) -> Option<f64>;
}

/// One saved primitive.
#[derive(Clone, Debug, PartialEq)]
pub enum RdbItem {
    Unsigned(u64),
    StringBuffer(Vec<u8>),
    Double(f64),
}

impl RdbSink for Vec<RdbItem> {
    fn save_unsigned(&mut self, v: u64) {
        self.push(RdbItem::Unsigned(v));
    }
    fn save_string_buffer(&mut self, s: &[u8]) {
        self.push(RdbItem::StringBuffer(s.to_vec()));
    }
    fn save_double(&mut self, v: f64) {
        self.push(RdbItem::Double(v));
    }
}

/// Reads [`RdbItem`]s back in order; a type mismatch or the end is an IO error.
pub struct RdbItems<'a> {
    items: &'a [RdbItem],
    pos: usize,
}

impl<'a> RdbItems<'a> {
    pub fn new(items: &'a [RdbItem]) -> Self {
        RdbItems { items, pos: 0 }
    }

    fn next(&mut self) -> Option<&'a RdbItem> {
        let it = self.items.get(self.pos)?;
        self.pos += 1;
        Some(it)
    }
}

impl RdbSource for RdbItems<'_> {
    fn load_unsigned(&mut self) -> Option<u64> {
        match self.next()? {
            RdbItem::Unsigned(v) => Some(*v),
            _ => None,
        }
    }
    fn load_string_buffer(&mut self) -> Option<Vec<u8>> {
        match self.next()? {
            RdbItem::StringBuffer(s) => Some(s.clone()),
            _ => None,
        }
    }
    fn load_double(&mut self) -> Option<f64> {
        match self.next()? {
            RdbItem::Double(v) => Some(*v),
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn r(s: &str) -> Vec<Rune> {
        str_to_runes(s.as_bytes())
    }

    fn s(runes: &[Rune]) -> String {
        String::from_utf8(runes_to_str(runes).unwrap()).unwrap()
    }

    fn keys(t: &Trie) -> Vec<String> {
        t.iterate_all().collect_all().into_iter().map(|e| s(&e.0)).collect()
    }

    fn labels(n: &TrieNode) -> Vec<String> {
        n.children().iter().map(|c| s(c.label())).collect()
    }

    fn sugget(t: &Trie, q: &str, num: usize, fuzzy: bool) -> Vec<(String, f64)> {
        t.collect_fuzzy(q.as_bytes(), num, i32::from(fuzzy), MatchMode::Prefix, false, false)
            .unwrap()
            .into_iter()
            .map(|e| (String::from_utf8(e.str).unwrap(), e.score as f64))
            .collect()
    }

    fn sug(pairs: &[(&str, f64)]) -> Vec<(String, f64)> {
        pairs.iter().map(|(k, v)| (k.to_string(), *v)).collect()
    }

    fn collect_range(t: &Trie, min: Option<&str>, imin: bool, max: Option<&str>, imax: bool) -> Vec<String> {
        let (lo, hi) = (min.map(r), max.map(r));
        let mut out = Vec::new();
        t.iterate_range(lo.as_deref(), imin, hi.as_deref(), imax, &mut |k, _, _| {
            out.push(s(k));
            true
        });
        out
    }

    fn collect_contains(t: &Trie, q: &str, prefix: bool, suffix: bool) -> Vec<String> {
        let mut out = Vec::new();
        t.iterate_contains(&r(q), prefix, suffix, &mut |k, _, _| {
            out.push(s(k));
            true
        }, None);
        out
    }

    fn collect_wildcard(t: &Trie, q: &str) -> Vec<String> {
        let mut out = Vec::new();
        t.iterate_wildcard(&r(q), &mut |k, _, _| {
            out.push(s(k));
            true
        }, None);
        out
    }

    fn lex(words: &[&str]) -> Trie {
        let mut t = Trie::new(SortMode::Lex);
        for w in words {
            t.insert(w.as_bytes(), 1.0, true, None, 1);
        }
        t
    }

    /// The FT.SUGADD sequence the reference was probed with.
    fn suggestions() -> Trie {
        let mut t = Trie::new(SortMode::Score);
        for (k, v) in [("hello", 1.0), ("help", 1.0), ("helium", 2.0), ("hero", 1.0), ("her", 3.0), ("hi", 1.0), ("world", 5.0), ("hel", 1.0), ("Hellish", 1.5), ("héllo", 2.0)] {
            assert_eq!(t.insert(k.as_bytes(), v, false, None, 0), TRIE_OK_NEW);
        }
        assert_eq!(t.size(), 10);
        t
    }

    #[test]
    fn rune_conversion() {
        assert_eq!(r("abc"), vec![97, 98, 99]);
        assert_eq!(str_to_runes("é日".as_bytes()), vec![0xE9, 0x65E5]);
        // Above the BMP: truncated to 16 bits (U+1F600 -> U+F600, U+10000 -> 0).
        assert_eq!(str_to_runes("😀x".as_bytes()), vec![0xF600, b'x' as Rune]);
        assert_eq!(str_to_runes("\u{10000}a".as_bytes()), vec![0, b'a' as Rune]);
        // A NUL ends the conversion.
        assert_eq!(str_to_runes(b"ab\0cd"), vec![97, 98]);
        // Lenient decoding: the lead byte decides the length.
        assert_eq!(str_to_runes(b"\xc3"), vec![0xC0]);
        assert_eq!(str_to_runes(b"\x80a"), vec![0x21]);
        assert_eq!(runes_to_str(&[0xF600, b'x' as Rune]).unwrap(), b"\xef\x98\x80x");
        assert_eq!(runes_to_str(&[b'a' as Rune, 0, b'b' as Rune]).unwrap(), b"a");
        assert_eq!(runes_to_str(&vec![b'a' as Rune; 1024]).unwrap().len(), 1024);
        assert!(runes_to_str(&vec![b'a' as Rune; 1025]).is_none());
        assert_eq!(str_to_runes_limited(&[b'a'; 1024]).unwrap().len(), 1024);
        assert!(str_to_runes_limited(&[b'a'; 1025]).is_none());
        assert_eq!(s(&r("ünïcode")), "ünïcode");
    }

    #[test]
    fn case_mapping() {
        assert_eq!(rune_fold(b'A' as Rune), b'a' as Rune);
        assert_eq!(rune_fold(0xDF), 0x73); // ß folds to "ss"
        assert_eq!(rune_fold(0x3A3), 0x3C3);
        assert_eq!(rune_fold(0x3C2), 0x3C3); // final sigma folds
        assert_eq!(rune_fold(0xD800), 0xD800);
        assert_eq!(str_to_lower_runes("HÉLLO".as_bytes()).unwrap(), r("héllo"));
        // Multi-code-point lowercase mappings are kept whole.
        assert_eq!(str_to_lower_runes("İx".as_bytes()).unwrap(), vec![b'i' as Rune, 0x307, b'x' as Rune]);
        assert_eq!(str_to_lower_runes(b"AB\0CD").unwrap(), r("ab"));
        assert!(str_to_lower_runes(&[b'A'; 1025]).is_none());
        assert_eq!(str_to_folded_runes("Straße".as_bytes()).unwrap(), r("strase"));
        assert_eq!(str_to_folded_runes("😀".as_bytes()).unwrap(), vec![0xF600]);
        assert!(str_to_folded_runes(&[b'a'; 1025]).is_none());
    }

    #[test]
    fn insert_replace_incr() {
        let mut t = Trie::new(SortMode::Lex);
        assert_eq!(t.insert(b"foo", 1.0, false, None, 2), TRIE_OK_NEW);
        assert_eq!(t.insert(b"foo", 2.0, false, Some(b"pl"), 3), TRIE_OK_UPDATED);
        assert_eq!(t.size(), 1);
        let n = t.get_node(&r("foo"), true).unwrap();
        assert_eq!((n.score(), n.num_docs(), n.payload()), (2.0, 5, Some(&b"pl"[..])));
        assert_eq!(t.insert(b"foo", 0.5, true, None, 0), TRIE_OK_UPDATED);
        let n = t.get_node(&r("foo"), true).unwrap();
        // INCR adds; a missing (or empty) payload keeps the stored one.
        assert_eq!((n.score(), n.payload()), (2.5, Some(&b"pl"[..])));
        t.insert(b"foo", 7.0, false, Some(b""), 0);
        let n = t.get_node(&r("foo"), true).unwrap();
        assert_eq!((n.score(), n.payload()), (7.0, Some(&b"pl"[..])));
        t.insert(b"foo", 7.0, false, Some(b"new"), 0);
        assert_eq!(t.get_node(&r("foo"), true).unwrap().payload(), Some(&b"new"[..]));
        // An empty payload is never stored.
        t.insert(b"bar", 1.0, false, Some(b""), 0);
        assert_eq!(t.get_node(&r("bar"), true).unwrap().payload(), None);
        // Empty keys are refused.
        assert_eq!(t.insert(b"", 1.0, false, None, 0), 0);
        assert_eq!(t.insert(b"\0abc", 1.0, false, None, 0), 0);
        assert_eq!(t.size(), 2);
        // Scores are stored as floats.
        t.insert(b"pi", std::f64::consts::PI, false, None, 0);
        assert_eq!(t.get_node(&r("pi"), true).unwrap().score(), std::f32::consts::PI);
    }

    #[test]
    fn length_limits() {
        let mut t = Trie::new(SortMode::Lex);
        // 255 runes fit, 256 do not.
        assert_eq!(t.insert(&[b'a'; 255], 1.0, false, None, 0), TRIE_OK_NEW);
        assert_eq!(t.insert(&[b'b'; 256], 1.0, false, None, 0), 0);
        // Over 512 bytes is refused before decoding, even when it would be few runes.
        let wide = "é".repeat(256); // 512 bytes, 256 runes
        assert_eq!(t.insert(wide.as_bytes(), 1.0, false, None, 0), 0);
        let wide = "é".repeat(255); // 510 bytes, 255 runes
        assert_eq!(t.insert(wide.as_bytes(), 1.0, false, None, 0), TRIE_OK_NEW);
        let mut over = "é".repeat(255).into_bytes();
        over.extend_from_slice(b"xxx"); // 513 bytes
        assert_eq!(t.insert(&over, 1.0, false, None, 0), 0);
        assert_eq!(t.size(), 2);
        assert_eq!(t.insert_runes(&[b'c' as Rune; 256], 1.0, false, None, 0), 0);
        assert_eq!(t.insert_runes(&[], 1.0, false, None, 0), 0);
        // The suffix trie's full-word insert takes any length and leaves the size alone.
        assert_eq!(t.insert_runes_no_size(&[b'c' as Rune; 300], 1.0, false, None, 0), TRIE_OK_NEW);
        assert_eq!(t.size(), 2);
        assert!(t.get_node(&[b'c' as Rune; 300], true).is_some());
        // Delete looks up at most 256 runes.
        assert_eq!(t.delete(&[b'c'; 300]), 0);
        assert_eq!(t.delete_runes(&[b'c' as Rune; 300]), 1);
        assert_eq!(t.mem_usage(), t.size().wrapping_mul(43));
    }

    #[test]
    fn splits() {
        let mut t = lex(&["hello", "help"]);
        assert_eq!(labels(t.root()), ["hel"]);
        assert_eq!(labels(&t.root().children()[0]), ["lo", "p"]);
        assert!(!t.root().children()[0].is_terminal());
        // A key ending at the split point turns the head terminal.
        t.insert(b"he", 1.0, false, None, 4);
        let he = &t.root().children()[0];
        assert_eq!((s(he.label()), he.is_terminal(), he.num_docs()), ("he".into(), true, 4));
        assert_eq!(labels(he), ["l"]);
        assert_eq!(labels(&he.children()[0]), ["lo", "p"]);
        // A divergence orders the two halves by rune.
        t.insert(b"hat", 1.0, false, None, 0);
        assert_eq!(labels(t.root()), ["h"]);
        assert_eq!(labels(&t.root().children()[0]), ["at", "e"]);
        assert_eq!(keys(&t), ["hat", "he", "hello", "help"]);
        assert_eq!(t.size(), 4);
    }

    #[test]
    fn delete_merges() {
        let mut t = lex(&["hello", "help", "helium"]);
        assert_eq!(t.delete(b"help"), 1);
        assert_eq!(t.delete(b"help"), 0);
        assert_eq!(t.delete(b"hel"), 0);
        assert_eq!(t.delete(b"nope"), 0);
        assert_eq!(t.size(), 2);
        assert_eq!(labels(&t.root().children()[0]), ["ium", "lo"]);
        assert_eq!(t.delete(b"helium"), 1);
        // "hel" is left with one child and merges with it.
        assert_eq!(labels(t.root()), ["hello"]);
        assert!(t.root().children()[0].children().is_empty());
        assert_eq!(keys(&t), ["hello"]);
        // A deleted node with two children stays as a tombstone.
        let mut t = lex(&["ab", "abc", "abd"]);
        assert_eq!(t.delete(b"ab"), 1);
        let ab = &t.root().children()[0];
        assert!(ab.is_deleted() && !ab.is_terminal());
        assert!(t.get_node(&r("ab"), true).is_none());
        // Prefix lookups through a tombstone find nothing either.
        assert!(t.get_node(&r("a"), false).is_none());
        assert!(t.get_node(&r("ab"), false).is_none());
        assert!(t.get_node(&r("abc"), false).is_some());
        assert_eq!(collect_contains(&t, "ab", true, false), Vec::<String>::new());
        assert_eq!(keys(&t), ["abc", "abd"]);
        // Re-adding the tombstone revives it as new.
        assert_eq!(t.insert(b"ab", 1.0, false, None, 0), TRIE_OK_NEW);
        assert_eq!(t.size(), 3);
        // Deleting one child of a tombstone merges the survivor into it.
        let mut t = lex(&["ab", "abc", "abd"]);
        t.delete(b"ab");
        t.delete(b"abc");
        assert_eq!(labels(t.root()), ["abd"]);
        assert_eq!(keys(&t), ["abd"]);
    }

    #[test]
    fn delete_removes_one_tombstone_per_pass() {
        // Optimizing removes the first childless tombstone; the siblings shifted down after it
        // are not merged in that pass.
        let mut t = lex(&["a", "b", "cx", "cy"]);
        // Tombstone "a" without optimizing the root, then delete "cx" so the root's pass sees
        // both.
        t.root.children[0].flags = DELETED;
        t.size -= 1;
        assert_eq!(t.delete(b"cx"), 1);
        assert_eq!(labels(t.root()), ["b", "c"]);
        assert_eq!(labels(&t.root().children()[1]), ["y"]);
    }

    #[test]
    fn get_offsets() {
        let t = lex(&["hello", "help"]);
        let (n, off) = t.get_node_with_offset(&r("hell"), false).unwrap();
        assert_eq!((s(n.label()), off), ("lo".into(), 3));
        let (n, off) = t.get_node_with_offset(&r("he"), false).unwrap();
        assert_eq!((s(n.label()), off), ("hel".into(), 0));
        assert!(t.get_node(&r("he"), true).is_none());
        assert!(t.get_node(&r("hex"), false).is_none());
        assert!(t.get_node(&r("hello!"), false).is_none());
        assert!(t.get_node(&[], false).is_none());
        // Exact lookups of internal nodes return them (callers check terminality).
        assert!(!t.get_node(&r("hel"), true).unwrap().is_terminal());
    }

    #[test]
    fn lex_iteration_matches_dictdump() {
        // FT.DICTADD zztrd banana apple App Äpfel zebra über apple ápple a ab abc abd b 日本 日本語 😀x
        let words = ["banana", "apple", "App", "Äpfel", "zebra", "über", "apple", "ápple", "a", "ab", "abc", "abd", "b", "日本", "日本語", "😀x"];
        let mut t = Trie::new(SortMode::Lex);
        let added: i32 = words.iter().map(|w| t.insert(w.as_bytes(), 1.0, true, None, 0)).sum();
        assert_eq!(added, 15);
        let want = ["App", "a", "ab", "abc", "abd", "apple", "b", "banana", "zebra", "Äpfel", "ápple", "über", "日本", "日本語", "\u{f600}x"];
        assert_eq!(keys(&t), want);
        assert_eq!(t.delete(b"ab") + t.delete(b"abc") + t.delete(b"zzz"), 2);
        let want = ["App", "a", "abd", "apple", "b", "banana", "zebra", "Äpfel", "ápple", "über", "日本", "日本語", "\u{f600}x"];
        assert_eq!(keys(&t), want);
        // Dictionaries count every add as a score increment.
        assert_eq!(t.get_node(&r("apple"), true).unwrap().score(), 2.0);
    }

    #[test]
    fn score_mode_child_order() {
        let mut t = Trie::new(SortMode::Score);
        t.insert(b"a", 1.0, false, None, 0);
        t.insert(b"b", 3.0, false, None, 0);
        t.insert(b"c", 2.0, false, None, 0);
        t.insert(b"d", 2.0, false, None, 0);
        // Descending bound; equal bounds keep insertion order.
        assert_eq!(labels(t.root()), ["b", "c", "d", "a"]);
        // Raising a child's bound rotates it forward past strictly lower siblings.
        t.insert(b"d", 3.0, false, None, 0);
        assert_eq!(labels(t.root()), ["b", "d", "c", "a"]);
        t.insert(b"aa", 10.0, false, None, 0);
        assert_eq!(labels(t.root()), ["a", "b", "d", "c"]);
        assert_eq!(t.root().subtree_max_score(), 10.0);
        // A lowered score leaves the bound over-estimated until a delete recomputes it.
        t.insert(b"aa", 0.5, false, None, 0);
        assert_eq!(t.root().children()[0].subtree_max_score(), 10.0);
        // Deleting "c" re-sorts the root; "a" is off the path and keeps its stale bound.
        assert_eq!(t.delete(b"c"), 1);
        assert_eq!(labels(t.root()), ["a", "b", "d"]);
        assert_eq!(t.root().children()[0].subtree_max_score(), 10.0);
        // Deleting "aa" recomputes the bounds along its path.
        assert_eq!(t.delete(b"aa"), 1);
        assert_eq!(labels(t.root()), ["b", "d", "a"]);
        assert_eq!(t.root().children()[2].subtree_max_score(), 1.0);
        // After a split the halves are in rune order, whatever their scores.
        let mut t = Trie::new(SortMode::Score);
        t.insert(b"ab", 1.0, false, None, 0);
        t.insert(b"aa", 5.0, false, None, 0);
        assert_eq!(labels(&t.root().children()[0]), ["a", "b"]);
        t.insert(b"ac", 3.0, false, None, 0);
        assert_eq!(labels(&t.root().children()[0]), ["a", "c", "b"]);
        assert_eq!(keys(&t), ["aa", "ac", "ab"]);
        // A delete re-sorts by bound, then label.
        let mut t = Trie::new(SortMode::Score);
        t.insert(b"xb", 1.0, false, None, 0);
        t.insert(b"xa", 1.0, false, None, 0);
        t.insert(b"xc", 1.0, false, None, 0);
        assert_eq!(labels(&t.root().children()[0]), ["a", "b", "c"]);
        t.insert(b"xd", 1.0, false, None, 0);
        t.delete(b"xd");
        assert_eq!(labels(&t.root().children()[0]), ["a", "b", "c"]);
    }

    #[test]
    fn sugget_matches_reference() {
        let mut t = suggestions();
        let he = sug(&[
            ("her", 2.1213202476501465),
            ("helium", 0.8944271802902222),
            ("hel", 0.7071067690849304),
            ("Hellish", 0.6123724579811096),
            ("help", 0.5773502588272095),
            ("hero", 0.5773502588272095),
            ("hello", 0.5),
        ]);
        assert_eq!(sugget(&t, "he", 20, false), he);
        assert_eq!(sugget(&t, "HE", 20, false), he);
        // MAX 5: the heap keeps "hero" over "help".
        let top5: Vec<String> = sugget(&t, "he", 5, false).into_iter().map(|e| e.0).collect();
        assert_eq!(top5, ["her", "helium", "hel", "Hellish", "hero"]);
        // "hi" passes as exact: only the first slen bytes of the runes are compared.
        assert_eq!(
            sugget(&t, "he", 20, true),
            sug(&[
                ("hi", 290630304.0),
                ("her", 2.1213202476501465),
                ("helium", 0.8944271802902222),
                ("hel", 0.7071067690849304),
                ("Hellish", 0.6123724579811096),
                ("help", 0.5773502588272095),
                ("hero", 0.5773502588272095),
                ("hello", 0.5),
                ("héllo", 0.1353352814912796),
            ])
        );
        assert_eq!(sugget(&t, "h", 3, false), sug(&[("her", 1.7320507764816284), ("héllo", 0.8944271802902222), ("helium", 0.8164966106414795)]));
        assert_eq!(
            sugget(&t, "h", 20, false),
            sug(&[
                ("her", 1.7320507764816284),
                ("héllo", 0.8944271802902222),
                ("helium", 0.8164966106414795),
                ("hi", 0.7071067690849304),
                ("hel", 0.5773502588272095),
                ("Hellish", 0.5669466853141785),
                ("help", 0.5),
                ("hero", 0.5),
                ("hello", 0.4472135901451111),
            ])
        );
        assert_eq!(
            sugget(&t, "hel", 20, false),
            sug(&[("hel", 2147483648.0), ("helium", 1.0), ("help", 0.7071067690849304), ("Hellish", 0.670820415019989), ("hello", 0.5773502588272095)])
        );
        assert_eq!(sugget(&t, "hello", 20, true), sug(&[("hello", 2147483648.0), ("héllo", 0.2706705629825592), ("Hellish", 0.11720379441976547)]));

        assert_eq!((t.delete(b"her"), t.delete(b"helium"), t.delete(b"nothere")), (1, 1, 0));
        assert_eq!(t.size(), 8);
        assert_eq!(
            sugget(&t, "h", 20, false),
            sug(&[
                ("héllo", 0.8944271802902222),
                ("hi", 0.7071067690849304),
                ("hel", 0.5773502588272095),
                ("Hellish", 0.5669466853141785),
                ("help", 0.5),
                ("hero", 0.5),
                ("hello", 0.4472135901451111),
            ])
        );
        t.insert(b"help", 4.0, true, None, 0);
        t.insert(b"hero", 2.0, true, Some(b"pp"), 0);
        t.insert(b"hero", 2.0, true, None, 0);
        assert_eq!(t.size(), 8);
        let got: Vec<(String, f64, Option<Vec<u8>>)> = t
            .collect_fuzzy(b"h", 20, 0, MatchMode::Prefix, false, false)
            .unwrap()
            .into_iter()
            .map(|e| (String::from_utf8(e.str).unwrap(), e.score as f64, e.payload))
            .collect();
        let want = vec![
            ("help".to_string(), 2.5, None),
            ("hero".to_string(), 2.5, Some(b"pp".to_vec())),
            ("héllo".to_string(), 0.8944271802902222, None),
            ("hi".to_string(), 0.7071067690849304, None),
            ("hel".to_string(), 0.5773502588272095, None),
            ("Hellish".to_string(), 0.5669466853141785, None),
            ("hello".to_string(), 0.4472135901451111, None),
        ];
        assert_eq!(got, want);
        t.insert(b"help", 0.25, false, None, 0);
        assert_eq!(sugget(&t, "h", 4, false), sug(&[("hero", 2.5), ("héllo", 0.8944271802902222), ("hi", 0.7071067690849304), ("hel", 0.5773502588272095)]));
        assert_eq!(sugget(&t, "HÉ", 20, false), sug(&[("héllo", 1.154700517654419)]));
        assert_eq!(
            sugget(&t, "hÉl", 20, true),
            sug(&[
                ("héllo", 1.4142135381698608),
                ("Hellish", 0.10150146484375),
                ("hello", 0.09569649398326874),
                ("hel", 0.09569649398326874),
                ("help", 0.0338338203728199),
            ])
        );
    }

    #[test]
    fn collect_fuzzy_limits_and_trim() {
        let mut t = Trie::new(SortMode::Score);
        t.insert(b"abc", 100.0, false, None, 0);
        t.insert(b"abd", 50.0, false, None, 0);
        t.insert(b"abe", 5.0, false, None, 0);
        t.insert(b"abf", 9.0, false, None, 0);
        let trimmed: Vec<String> = t.collect_fuzzy(b"ab", 10, 0, MatchMode::Prefix, true, false).unwrap().into_iter().map(|e| String::from_utf8(e.str).unwrap()).collect();
        assert_eq!(trimmed, ["abc", "abd"]);
        assert_eq!(t.collect_fuzzy(b"ab", 10, 0, MatchMode::Prefix, false, false).unwrap().len(), 4);
        assert!(t.collect_fuzzy(&[b'a'; 201], 5, 0, MatchMode::Prefix, false, false).is_none());
        assert!(t.collect_fuzzy(&[b'a'; 101], 5, 0, MatchMode::Prefix, false, false).is_none());
        assert_eq!(t.collect_fuzzy(&[b'a'; 100], 5, 0, MatchMode::Prefix, false, false).unwrap().len(), 0);
        assert_eq!(t.collect_fuzzy(b"ab", 0, 0, MatchMode::Prefix, false, false).unwrap().len(), 0);
        // Edit-distance mode: whole-key distance, the exact key first.
        let got: Vec<String> = t.collect_fuzzy(b"abd", 10, 1, MatchMode::EditDistance, false, false).unwrap().into_iter().map(|e| String::from_utf8(e.str).unwrap()).collect();
        assert_eq!(got, ["abd", "abc", "abf", "abe"]);
    }

    #[test]
    fn fuzzy_iteration() {
        let words = ["hello", "help", "hel", "hero", "helo", "yellow", "hallo", "he", "h", "helloo", "Hello"];
        let t = lex(&words);
        for (q, k) in [("helo", 1), ("hello", 1), ("he", 1), ("hallo", 2), ("xyz", 1), ("HELO", 0)] {
            let mut it = t.iterate_fuzzy(q.as_bytes(), k, MatchMode::EditDistance).unwrap();
            let mut got = Vec::new();
            let mut dist = -1;
            while let Some(e) = it.next_with(Some(&mut dist)) {
                got.push(s(e.runes));
            }
            let ql = str_to_lower_runes(q.as_bytes()).unwrap();
            let want: Vec<String> = keys(&t)
                .into_iter()
                .filter(|w| super::super::levenshtein::distance(&ql, &str_to_lower_runes(w.as_bytes()).unwrap()) <= k as usize)
                .collect();
            assert_eq!(got, want, "q={q} k={k}");
        }
        // The reported distance is the prefix edit distance.
        let t = lex(&["abzzz", "abc"]);
        let mut it = t.iterate_fuzzy(b"abc", 2, MatchMode::Prefix).unwrap();
        let mut dist = -1;
        let mut got = Vec::new();
        while let Some(e) = it.next_with(Some(&mut dist)) {
            got.push((s(e.runes), dist));
        }
        assert_eq!(got, [("abc".to_string(), 0), ("abzzz".to_string(), 1)]);
        // Prefix mode admits every continuation of an accepted prefix.
        let t = lex(&["he", "hello", "help", "world", "hxllo"]);
        let got: Vec<String> = t.iterate_fuzzy(b"hel", 0, MatchMode::Prefix).unwrap().collect_all().into_iter().map(|e| s(&e.0)).collect();
        assert_eq!(got, ["hello", "help"]);
        let got: Vec<String> = t.iterate_fuzzy(b"hel", 1, MatchMode::Prefix).unwrap().collect_all().into_iter().map(|e| s(&e.0)).collect();
        assert_eq!(got, ["he", "hello", "help", "hxllo"]);
        assert!(t.iterate_fuzzy(&[b'a'; 101], 1, MatchMode::Prefix).is_none());
        assert!(t.iterate_fuzzy(&[b'a'; 100], 1, MatchMode::Prefix).is_some());
    }

    #[test]
    fn iteration_payloads_and_docs() {
        let mut t = Trie::new(SortMode::Lex);
        t.insert(b"b", 2.0, false, Some(b"pb"), 7);
        t.insert(b"a", 1.0, false, None, 3);
        let all = t.iterate_all().collect_all();
        assert_eq!(all, vec![(r("a"), 1.0, 3, None), (r("b"), 2.0, 7, Some(b"pb".to_vec()))]);
        let empty = Trie::new(SortMode::Score);
        assert!(empty.iterate_all().next_entry().is_none());
    }

    #[test]
    fn iterator_depth_limit() {
        // Keys "a", "aa", ... nest one node per rune; the 255-rune key sits at depth 256 and
        // is past the iterator's stack.
        let mut t = Trie::new(SortMode::Lex);
        for n in 1..=255 {
            assert_eq!(t.insert(&vec![b'a'; n], 1.0, false, None, 0), TRIE_OK_NEW);
        }
        assert_eq!(t.size(), 255);
        assert_eq!(t.iterate_all().collect_all().len(), 254);
    }

    #[test]
    fn subtree_pruning() {
        let mut t = Trie::new(SortMode::Score);
        t.insert(b"a", 1.0, false, None, 0);
        t.insert(b"b", 5.0, false, None, 0);
        let mut it = t.iterate_all();
        it.kth_best_score = 2.0;
        let got: Vec<Vec<Rune>> = it.collect_all().into_iter().map(|e| e.0).collect();
        assert_eq!(got, vec![r("b")]);
    }

    #[test]
    fn decrement_num_docs() {
        let mut t = lex(&["foo", "food"]);
        t.insert(b"foo", 1.0, true, None, 2);
        assert_eq!(t.get_node(&r("foo"), true).unwrap().num_docs(), 3);
        assert_eq!(t.decrement_num_docs(b"foo", 1), DecrResult::Updated);
        assert_eq!(t.get_node(&r("foo"), true).unwrap().num_docs(), 2);
        assert_eq!(t.decrement_num_docs(b"fo", 1), DecrResult::NotFound);
        assert_eq!(t.decrement_num_docs(b"bar", 1), DecrResult::NotFound);
        assert_eq!(t.decrement_num_docs(b"", 1), DecrResult::Unsupported);
        assert_eq!(t.decrement_num_docs(&[b'x'; 256], 1), DecrResult::Unsupported);
        assert_eq!(t.decrement_num_docs(&[b'x'; 513], 1), DecrResult::Unsupported);
        assert_eq!(t.decrement_num_docs(b"foo", 10), DecrResult::Deleted);
        assert_eq!(t.size(), 1);
        assert_eq!(t.decrement_num_docs(b"foo", 1), DecrResult::NotFound);
        assert_eq!(keys(&t), ["food"]);
        assert_eq!(labels(t.root()), ["food"]);
    }

    /// Lexicographic order over runes, the reference for well-formed ranges.
    fn brute_range(words: &[&str], min: Option<&str>, imin: bool, max: Option<&str>, imax: bool) -> Vec<String> {
        let mut ws: Vec<Vec<Rune>> = words.iter().map(|w| r(w)).collect();
        ws.sort();
        ws.dedup();
        ws.into_iter()
            .filter(|w| match min.map(r) {
                None => true,
                Some(m) => w > &m || (imin && *w == m),
            })
            .filter(|w| match max.map(r) {
                None => true,
                Some(m) => w < &m || (imax && *w == m),
            })
            .map(|w| s(&w))
            .collect()
    }

    #[test]
    fn ranges() {
        let words = ["apple", "banana", "band", "bandana", "can", "cane", "candy", "dog", "door", "zoo", "b", "ba"];
        let t = lex(&words);
        let cases: &[(Option<&str>, bool, Option<&str>, bool)] = &[
            (None, true, None, true),
            (Some("b"), true, Some("c"), false),
            (Some("bz"), true, Some("d"), true),
            (Some("a"), true, None, false),
            (Some("dog"), true, Some("door"), true),
            (Some("dog"), false, Some("door"), false),
            (Some("e"), true, Some("y"), true),
            (Some("0"), true, Some("~"), true),
            (Some(""), true, Some("az"), false),
            (Some("apple"), false, Some("c"), false),
        ];
        for &(lo, il, hi, ih) in cases {
            assert_eq!(collect_range(&t, lo, il, hi, ih), brute_range(&words, lo, il, hi, ih), "{lo:?} {il} {hi:?} {ih}");
        }
        // A child reached through a bound's prefix emits its own key only where that bound is
        // consumed exactly: an upper bound running past "can" / "b" loses them, one ending
        // inside "can" ("c") gains it.
        assert_eq!(collect_range(&t, Some("b"), false, Some("c"), true), ["ba", "banana", "band", "bandana", "can"]);
        assert_eq!(collect_range(&t, Some("ban"), true, Some("cane"), true), ["banana", "band", "bandana", "candy", "cane"]);
        assert_eq!(collect_range(&t, None, false, Some("candy"), false), ["apple", "b", "ba", "banana", "band", "bandana"]);
        assert_eq!(collect_range(&t, None, false, Some("candy"), true), ["apple", "b", "ba", "banana", "band", "bandana", "candy"]);
        assert_eq!(collect_range(&t, Some(""), true, Some("bb"), false), ["apple", "ba", "banana", "band", "bandana"]);
        assert_eq!(collect_range(&t, Some("apple"), false, Some("bz"), true), ["ba", "banana", "band", "bandana"]);
        // Equal bounds: an existence check when either side is inclusive.
        assert_eq!(collect_range(&t, Some("band"), true, Some("band"), false), ["band"]);
        assert_eq!(collect_range(&t, Some("band"), false, Some("band"), false), Vec::<String>::new());
        assert_eq!(collect_range(&t, Some("bandx"), true, Some("bandx"), true), Vec::<String>::new());
        // Reversed bounds yield nothing.
        assert_eq!(collect_range(&t, Some("z"), true, Some("a"), true), Vec::<String>::new());
        // A bound that is a proper prefix of a child's label counts as consumed there: the
        // child's key takes the include flag of the exhausted bound.
        let t = lex(&["ab", "abc"]);
        assert_eq!(collect_range(&t, Some("a"), true, Some("ab"), false), ["ab"]);
        assert_eq!(collect_range(&t, Some("a"), false, Some("ab"), true), ["ab"]);
        assert_eq!(collect_range(&t, Some("a"), false, Some("ab"), false), Vec::<String>::new());
        // The callback's answer does not stop direct emissions, only whole-subtree walks.
        let t = lex(&["a", "b", "c"]);
        let mut n = 0;
        t.iterate_range(None, true, None, true, &mut |_, _, _| {
            n += 1;
            false
        });
        assert_eq!(n, 1);
    }

    #[test]
    fn contains_prefix_suffix() {
        let words = ["hello", "help", "yellow", "shell", "hell", "lo", "low", "abab", "ab", "el", "cello"];
        let t = lex(&words);
        assert_eq!(collect_contains(&t, "hel", true, false), ["hell", "hello", "help"]);
        assert_eq!(collect_contains(&t, "hello", true, false), ["hello"]);
        assert_eq!(collect_contains(&t, "x", true, false), Vec::<String>::new());
        assert_eq!(collect_contains(&t, "lo", false, true), ["cello", "hello", "lo"]);
        assert_eq!(collect_contains(&t, "ll", false, true), ["hell", "shell"]);
        // Contains: every occurrence yields the node's subtree again.
        assert_eq!(collect_contains(&t, "ab", true, true), ["ab", "abab", "abab"]);
        assert_eq!(collect_contains(&t, "ell", true, true), ["cello", "hell", "hello", "shell", "yellow"]);
        assert_eq!(collect_contains(&t, "el", true, true), ["cello", "el", "hell", "hello", "help", "shell", "yellow"]);
        // Neither flag: an exact lookup.
        assert_eq!(collect_contains(&t, "hell", false, false), ["hell"]);
        assert_eq!(collect_contains(&t, "hel", false, false), Vec::<String>::new());
        // A refusal stops the walk.
        let mut got = Vec::new();
        t.iterate_contains(&r("el"), true, true, &mut |k, _, _| {
            got.push(s(k));
            got.len() < 2
        }, None);
        assert_eq!(got, ["cello", "el"]);
    }

    #[test]
    fn contains_timeout() {
        let mut t = Trie::new(SortMode::Lex);
        for i in 0..500 {
            t.insert(format!("w{i:03}x").as_bytes(), 1.0, false, None, 0);
        }
        let mut probes = 0;
        let mut probe = || {
            probes += 1;
            true
        };
        let mut got = 0;
        t.iterate_contains(&r("x"), false, true, &mut |_, _, _| {
            got += 1;
            true
        }, Some(&mut probe));
        assert_eq!(probes, 1);
        assert!(got > 0 && got < 500);
        let mut got = 0;
        t.iterate_contains(&r("x"), false, true, &mut |_, _, _| {
            got += 1;
            true
        }, Some(&mut || false));
        assert_eq!(got, 500);
    }

    #[test]
    fn wildcard_matching() {
        use WildcardMatch::*;
        let m = |p: &str, s: &str| wildcard_match_rune(&r(p), &r(s));
        assert_eq!(m("he*", "hello"), Full);
        assert_eq!(m("he*", "h"), Partial);
        assert_eq!(m("h?llo", "hallo"), Full);
        assert_eq!(m("h?llo", "hal"), Partial);
        assert_eq!(m("h?llo", "hxlo"), No);
        assert_eq!(m("*lo", "hello"), Full);
        assert_eq!(m("*lo", "hellx"), Partial);
        assert_eq!(m("a*b*c", "axxbyyc"), Full);
        assert_eq!(m("a*b", "abab"), Full);
        assert_eq!(m("abc", "abcd"), No);
        assert_eq!(m("", ""), Full);
        assert_eq!(m("**", "x"), Full);

        let words = ["hello", "help", "hallo", "yellow", "hell", "h", "he", "shell", "cello"];
        let t = lex(&words);
        assert_eq!(collect_wildcard(&t, "he*"), ["he", "hell", "hello", "help"]);
        assert_eq!(collect_wildcard(&t, "h?ll*"), ["hallo", "hell", "hello"]);
        assert_eq!(collect_wildcard(&t, "*llo"), ["cello", "hallo", "hello"]);
        assert_eq!(collect_wildcard(&t, "h?llo"), ["hallo", "hello"]);
        assert_eq!(collect_wildcard(&t, "*ell*"), ["cello", "hell", "hello", "shell", "yellow"]);
        assert_eq!(collect_wildcard(&t, "he"), ["he"]);
        assert_eq!(collect_wildcard(&t, "*").len(), words.len());
    }

    #[test]
    fn random_walks() {
        let mut t = Trie::new(SortMode::Score);
        let mut seed = 12345u32;
        let mut rand = move || {
            seed = seed.wrapping_mul(1103515245).wrapping_add(12345);
            ((seed >> 1) & 0x7fff_ffff) as i32
        };
        assert!(t.random_key(&mut rand).is_none());
        for (k, v) in [("alpha", 1.0), ("beta", 2.0), ("gamma", 3.0), ("alps", 4.0)] {
            t.insert(k.as_bytes(), v, false, None, 0);
        }
        for _ in 0..50 {
            let (k, score) = t.random_key(&mut rand).unwrap();
            let n = t.get_node(&str_to_runes(&k), true).unwrap();
            assert!(n.is_terminal());
            assert_eq!(n.score() as f64, score);
        }
        // The root is [alp, gamma, beta] ("alps" lifted the split node), "alp" is [ha, s].
        assert_eq!(labels(t.root()), ["alp", "gamma", "beta"]);
        // A scripted walk: into "alp", into "s", back up, into "s" again; four steps on a
        // terminal end it.
        let mut script = [1, 2, 0, 2, 1].into_iter();
        let mut next = move || script.next().unwrap_or(0);
        let (n, key) = random_walk(t.root(), 4, &mut next).unwrap();
        assert!(n.is_terminal());
        assert_eq!(s(&key), "alps");
    }

    #[test]
    fn save_load_round_trip() {
        let mut t = Trie::new(SortMode::Score);
        t.insert(b"hello", 2.0, false, Some(b"pay"), 3);
        t.insert(b"h\xc3\xa9", 1.5, false, None, 0);
        t.insert(b"world", 0.25, false, None, 9);
        let mut items: Vec<RdbItem> = Vec::new();
        t.rdb_save(&mut items);
        use RdbItem::*;
        assert_eq!(
            items,
            vec![
                Unsigned(3),
                StringBuffer(b"hello\0".to_vec()),
                Double(2.0),
                StringBuffer(b"pay\0".to_vec()),
                Unsigned(3),
                StringBuffer(b"h\xc3\xa9\0".to_vec()),
                Double(1.5),
                StringBuffer(b"\0".to_vec()),
                Unsigned(0),
                StringBuffer(b"world\0".to_vec()),
                Double(0.25),
                StringBuffer(b"\0".to_vec()),
                Unsigned(9),
            ]
        );
        let back = Trie::rdb_load(&mut RdbItems::new(&items), TRIE_ENCVER_CURRENT).unwrap();
        assert_eq!(back.size(), 3);
        assert_eq!(back.iterate_all().collect_all(), t.iterate_all().collect_all());
        assert!(Trie::rdb_load(&mut RdbItems::new(&items), 3).is_none());
        // Truncated streams fail.
        assert!(Trie::rdb_load(&mut RdbItems::new(&items[..6]), 2).is_none());
        // Without payloads and document counts (dictionaries, old versions).
        let mut plain: Vec<RdbItem> = Vec::new();
        t.save(&mut plain, false, false);
        assert_eq!(plain.len(), 1 + 2 * 3);
        let back = Trie::load(&mut RdbItems::new(&plain), false, false, SortMode::Lex).unwrap();
        assert_eq!(keys(&back), ["hello", "hé", "world"]);
        assert_eq!(back.get_node(&r("hello"), true).unwrap().num_docs(), 0);
        let v1 = vec![Unsigned(1), StringBuffer(b"ab\0".to_vec()), Double(1.0), StringBuffer(b"p\0".to_vec())];
        let back = Trie::rdb_load(&mut RdbItems::new(&v1), 1).unwrap();
        assert_eq!(back.get_node(&r("ab"), true).unwrap().payload(), Some(&b"p"[..]));
        // An empty payload buffer wraps its length around and overflows.
        let bad = vec![Unsigned(1), StringBuffer(b"ab\0".to_vec()), Double(1.0), StringBuffer(Vec::new()), Unsigned(0)];
        assert!(Trie::rdb_load(&mut RdbItems::new(&bad), 2).is_none());
        // An empty key buffer is skipped.
        let odd = vec![Unsigned(1), StringBuffer(Vec::new()), Double(1.0), StringBuffer(b"\0".to_vec()), Unsigned(0)];
        assert_eq!(Trie::rdb_load(&mut RdbItems::new(&odd), 2).unwrap().size(), 0);
        assert_eq!(t.mem_usage(), 3 * 43);
    }

    #[test]
    fn payload_overflow_guard() {
        let mut n = TrieNode::new(&[], None, 0.0, false, 0);
        let arg = Some(PayloadArg { data: b"x", len: u32::MAX as usize - 5 });
        assert_eq!(n.add_checked(&r("a"), arg, 1.0, AddOp::Replace, 0, SortMode::Lex), TRIE_OK_NEW);
        let arg = Some(PayloadArg { data: b"x", len: u32::MAX as usize - 4 });
        assert_eq!(n.add_checked(&r("b"), arg, 1.0, AddOp::Replace, 0, SortMode::Lex), TRIE_ERR_PAYLOAD_OVERFLOW);
    }
}
