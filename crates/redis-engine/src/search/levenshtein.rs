//! The Levenshtein automaton RediSearch 8.10.1 walks its rune trie with (`trie/levenshtein.c`,
//! `trie/sparse_vector.c`): a sparse-vector automaton for a query and an edit budget, unrolled
//! into a DFA, and the step filter the trie iterator feeds rune by rune. The filter reports the
//! prefix edit distance through the iterator's match context and, in prefix mode, keeps
//! admitting every rune once some prefix has been accepted.

use std::collections::HashMap;

use super::trie::{FilterCode, Rune, StepFilter, rune_fold};

/// One `sparseVectorEntry`: a column of the edit-distance row and its cost.
type Entry = (i32, i32);

/// `sparseVector`: the non-pruned columns of a row, in column order.
pub type SparseVector = Vec<Entry>;

/// `TrieMatchMode`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MatchMode {
    /// `TRIE_MATCH_EDIT_DISTANCE`: a term is admitted iff its whole distance fits the budget.
    EditDistance,
    /// `TRIE_MATCH_PREFIX`: a term is admitted iff some prefix of it fits the budget.
    Prefix,
}

/// `SparseAutomaton`: the query runes and the edit budget.
#[derive(Clone, Debug)]
pub struct SparseAutomaton {
    string: Vec<Rune>,
    max: i32,
}

impl SparseAutomaton {
    pub fn new(s: &[Rune], max_edits: i32) -> Self {
        SparseAutomaton { string: s.to_vec(), max: max_edits }
    }

    fn len(&self) -> i32 {
        self.string.len() as i32
    }

    /// `SparseAutomaton_Start`: columns 0..=max with their own index as cost (they may run past
    /// the query's end when the budget exceeds its length).
    pub fn start(&self) -> SparseVector {
        (0..self.max + 1).map(|i| (i, i)).collect()
    }

    /// `SparseAutomaton_Step`: the next row after feeding `c`.
    pub fn step(&self, state: &SparseVector, c: Rune) -> SparseVector {
        let mut out: SparseVector = Vec::with_capacity(state.len());
        if let Some(&(idx, val)) = state.first()
            && idx == 0
            && val < self.max
        {
            out.push((0, val + 1));
        }
        for j in 0..state.len() {
            let (idx, cost) = state[j];
            if idx == self.len() {
                break;
            }
            let mut val = cost;
            if self.string[idx as usize] != c {
                val += 1;
            }
            if let Some(&(last_idx, last_val)) = out.last()
                && last_idx == idx
            {
                val = val.min(last_val + 1);
            }
            if j + 1 < state.len() && state[j + 1].0 == idx + 1 {
                val = val.min(state[j + 1].1 + 1);
            }
            if val <= self.max {
                out.push((idx + 1, val));
            }
        }
        out
    }

    /// `SparseAutomaton_IsMatch`: the row reaches the query's last column.
    pub fn is_match(&self, v: &SparseVector) -> bool {
        v.last().is_some_and(|e| e.0 == self.len())
    }

    /// `SparseAutomaton_CanMatch`.
    pub fn can_match(&self, v: &SparseVector) -> bool {
        !v.is_empty()
    }
}

/// `dfaNode`: a DFA state, its cost (the last column's), its edges in creation order and the
/// fallback taken for runes without an edge.
#[derive(Clone, Debug)]
pub struct DfaNode {
    pub distance: i32,
    pub matched: bool,
    pub v: SparseVector,
    pub edges: Vec<(Rune, usize)>,
    pub fallback: Option<usize>,
}

/// The unrolled automaton: every state ever built (the module's state cache), the root first.
#[derive(Clone, Debug)]
pub struct Dfa {
    pub nodes: Vec<DfaNode>,
    lookup: HashMap<SparseVector, usize>,
}

impl Dfa {
    /// `NewDFAFilter`'s construction: the root (with cost 0 whatever its row says) and
    /// `dfa_build` from it.
    pub fn build(a: &SparseAutomaton) -> Self {
        let mut dfa = Dfa { nodes: Vec::new(), lookup: HashMap::new() };
        let root = dfa.add_node(0, a.start());
        dfa.build_from(root, a);
        dfa
    }

    fn add_node(&mut self, distance: i32, v: SparseVector) -> usize {
        let id = self.nodes.len();
        self.lookup.insert(v.clone(), id);
        self.nodes.push(DfaNode { distance, matched: false, v, edges: Vec::new(), fallback: None });
        id
    }

    /// `dfaNode_getEdge`.
    pub fn edge(&self, n: usize, r: Rune) -> Option<usize> {
        self.nodes[n].edges.iter().find(|e| e.0 == r).map(|e| e.1)
    }

    /// `dfa_build`: an edge for every query rune the row can still consume (new states built
    /// depth first as they appear), then the fallback, a step with rune 1 standing for "any
    /// other rune".
    fn build_from(&mut self, p: usize, a: &SparseAutomaton) {
        self.nodes[p].matched = a.is_match(&self.nodes[p].v);
        let mut i = 0;
        while i < self.nodes[p].v.len() {
            let idx = self.nodes[p].v[i].0;
            i += 1;
            if idx >= a.len() {
                continue;
            }
            let c = a.string[idx as usize];
            if self.edge(p, c).is_some() {
                continue;
            }
            let nv = a.step(&self.nodes[p].v, c);
            if nv.is_empty() {
                continue;
            }
            match self.lookup.get(&nv) {
                Some(&d) => self.nodes[p].edges.push((c, d)),
                None => {
                    let dist = nv[nv.len() - 1].1;
                    let e = self.add_node(dist, nv);
                    self.nodes[p].edges.push((c, e));
                    self.build_from(e, a);
                }
            }
        }
        let nv = a.step(&self.nodes[p].v, 1);
        if nv.is_empty() {
            return;
        }
        match self.lookup.get(&nv) {
            Some(&d) => self.nodes[p].fallback = Some(d),
            None => {
                let dist = nv[nv.len() - 1].1;
                let f = self.add_node(dist, nv);
                self.nodes[p].fallback = Some(f);
                self.build_from(f, a);
            }
        }
    }
}

/// `runeLower`: the first code point of the rune's lowercase mapping (libnu's table, which
/// matches Rust's for every scalar value).
pub fn rune_lower(r: Rune) -> Rune {
    if r < 0x80 {
        return (r as u8).to_ascii_lowercase() as Rune;
    }
    match char::from_u32(r as u32) {
        Some(c) => c.to_lowercase().next().map_or(r, |l| l as u32 as Rune),
        None => r,
    }
}

/// `DFAFilter`: the DFA plus the state and running-minimum stacks that follow the trie
/// iterator's descent. A `None` state means a prefix already accepted (prefix mode).
#[derive(Clone, Debug)]
pub struct DfaFilter {
    pub dfa: Dfa,
    pub automaton: SparseAutomaton,
    stack: Vec<Option<usize>>,
    dist_stack: Vec<i32>,
    pub mode: MatchMode,
    transform: fn(Rune) -> Rune,
}

impl DfaFilter {
    /// `NewDFAFilter` with `FoldingFilterFunc` (FT.SUGGET): trie runes are case folded.
    pub fn folding(s: &[Rune], max_dist: i32, mode: MatchMode) -> Self {
        Self::with_transform(s, max_dist, mode, rune_fold)
    }

    /// `NewDFAFilter` with `LoweringFilterFunc` (query expansion, spell check): trie runes are
    /// lowercased.
    pub fn lowering(s: &[Rune], max_dist: i32, mode: MatchMode) -> Self {
        Self::with_transform(s, max_dist, mode, rune_lower)
    }

    pub fn with_transform(s: &[Rune], max_dist: i32, mode: MatchMode, transform: fn(Rune) -> Rune) -> Self {
        let automaton = SparseAutomaton::new(s, max_dist);
        let dfa = Dfa::build(&automaton);
        DfaFilter { dfa, automaton, stack: vec![Some(0)], dist_stack: vec![max_dist + 1], mode, transform }
    }
}

impl StepFilter for DfaFilter {
    /// `FilterFunc`.
    fn step(&mut self, b: Rune, matched: &mut bool, match_ctx: Option<&mut i32>) -> FilterCode {
        let dn = self.stack.last().copied().flatten();
        let min_dist = self.dist_stack.last().copied().unwrap_or(0);
        let Some(dn) = dn else {
            *matched = true;
            self.stack.push(None);
            self.dist_stack.push(min_dist);
            return FilterCode::Continue;
        };
        let mut ctx = match_ctx;
        let node = &self.dfa.nodes[dn];
        *matched = node.matched;
        if *matched && let Some(p) = ctx.as_deref_mut() {
            *p = node.distance.min(min_dist);
        }
        let t = (self.transform)(b);
        let next = self.dfa.edge(dn, t).or(node.fallback);
        if let Some(nx) = next {
            let nn = &self.dfa.nodes[nx];
            if nn.matched {
                *matched = true;
                if let Some(p) = ctx {
                    *p = nn.distance.min(min_dist);
                }
            }
            self.stack.push(Some(nx));
            self.dist_stack.push(nn.distance.min(min_dist));
            return FilterCode::Continue;
        }
        if self.mode == MatchMode::Prefix && *matched {
            self.stack.push(None);
            self.dist_stack.push(min_dist);
            return FilterCode::Continue;
        }
        FilterCode::Stop
    }

    /// `StackPop`.
    fn pop(&mut self, levels: usize) {
        for _ in 0..levels {
            self.stack.pop();
            self.dist_stack.pop();
        }
    }
}

/// The plain Levenshtein distance over runes, for tests and callers that want the reference
/// metric.
pub fn distance(a: &[Rune], b: &[Rune]) -> usize {
    let mut prev: Vec<usize> = (0..=b.len()).collect();
    let mut cur = vec![0; b.len() + 1];
    for (i, &ca) in a.iter().enumerate() {
        cur[0] = i + 1;
        for (j, &cb) in b.iter().enumerate() {
            let sub = prev[j] + usize::from(ca != cb);
            cur[j + 1] = sub.min(prev[j + 1] + 1).min(cur[j] + 1);
        }
        std::mem::swap(&mut prev, &mut cur);
    }
    prev[b.len()]
}

#[cfg(test)]
mod tests {
    use super::*;

    fn r(s: &str) -> Vec<Rune> {
        s.encode_utf16().collect()
    }

    /// Runs a word through the DFA the way the filter does (no trie), returning whether the
    /// final state accepts.
    fn dfa_accepts(dfa: &Dfa, word: &[Rune]) -> bool {
        let mut st = 0;
        for &c in word {
            match dfa.edge(st, c).or(dfa.nodes[st].fallback) {
                Some(n) => st = n,
                None => return false,
            }
        }
        dfa.nodes[st].matched
    }

    #[test]
    fn start_and_step() {
        let a = SparseAutomaton::new(&r("abc"), 1);
        let s = a.start();
        assert_eq!(s, vec![(0, 0), (1, 1)]);
        let s1 = a.step(&s, 'a' as Rune);
        assert_eq!(s1, vec![(0, 1), (1, 0), (2, 1)]);
        let s2 = a.step(&s1, 'b' as Rune);
        assert_eq!(s2, vec![(1, 1), (2, 0), (3, 1)]);
        assert!(a.is_match(&s2));
        let s3 = a.step(&s2, 'x' as Rune);
        assert_eq!(s3, vec![(2, 1), (3, 1)]);
        assert!(a.is_match(&s3));
        assert!(a.can_match(&s3));
        let s4 = a.step(&s3, 'y' as Rune);
        assert!(s4.is_empty());
        assert!(!a.can_match(&s4));
    }

    #[test]
    fn budget_beyond_query_length() {
        // Columns past the query's end keep the root from accepting.
        let a = SparseAutomaton::new(&r("a"), 2);
        assert_eq!(a.start(), vec![(0, 0), (1, 1), (2, 2)]);
        assert!(!a.is_match(&a.start()));
        let a = SparseAutomaton::new(&r("ab"), 2);
        assert!(a.is_match(&a.start()));
    }

    #[test]
    fn dfa_matches_levenshtein() {
        let words = ["", "a", "ab", "abc", "abd", "acb", "bac", "abcd", "xbc", "abcde", "hello", "help", "yellow", "ab ", "cab"];
        for q in ["abc", "ab", "hello", "a"] {
            for k in 0..3 {
                let a = SparseAutomaton::new(&r(q), k);
                let dfa = Dfa::build(&a);
                assert_eq!(dfa.nodes[0].distance, 0);
                for w in words {
                    // The start row's columns past the query's end keep the root from
                    // accepting the empty word when the budget exceeds the query's length.
                    let want = distance(&r(q), &r(w)) <= k as usize && !(w.is_empty() && k as usize > q.len());
                    assert_eq!(dfa_accepts(&dfa, &r(w)), want, "q={q} k={k} w={w}");
                }
            }
        }
    }

    #[test]
    fn dfa_states_are_unique() {
        let a = SparseAutomaton::new(&r("abcabc"), 2);
        let dfa = Dfa::build(&a);
        let mut seen = std::collections::HashSet::new();
        for n in &dfa.nodes {
            assert!(seen.insert(n.v.clone()));
            assert_eq!(n.matched, a.is_match(&n.v));
        }
    }

    #[test]
    fn filter_prefix_mode_carries_distance() {
        // "abc" vs "abzzz" in prefix mode: the "ab" accept (cost 1) is carried across.
        let mut f = DfaFilter::folding(&r("abc"), 2, MatchMode::Prefix);
        let mut dist = 99;
        let mut matched = false;
        for c in r("abzzz") {
            assert_eq!(f.step(c, &mut matched, Some(&mut dist)), FilterCode::Continue);
        }
        assert!(matched);
        assert_eq!(dist, 1);
        f.pop(5);
        assert_eq!(f.stack, vec![Some(0)]);
        assert_eq!(f.dist_stack, vec![3]);
    }

    #[test]
    fn filter_edit_mode_stops() {
        let mut f = DfaFilter::lowering(&r("ab"), 0, MatchMode::EditDistance);
        let mut matched = false;
        let mut dist = 7;
        assert_eq!(f.step('A' as Rune, &mut matched, Some(&mut dist)), FilterCode::Continue);
        assert!(!matched);
        assert_eq!(f.step('B' as Rune, &mut matched, Some(&mut dist)), FilterCode::Continue);
        assert!(matched);
        assert_eq!(dist, 0);
        // The state before the rune accepts, so the stop still reports a match.
        assert_eq!(f.step('c' as Rune, &mut matched, Some(&mut dist)), FilterCode::Stop);
        assert!(matched);
    }

    #[test]
    fn rune_lowering() {
        assert_eq!(rune_lower('A' as Rune), 'a' as Rune);
        assert_eq!(rune_lower(0x130), 'i' as Rune); // İ lowers to "i̇", first code point kept
        assert_eq!(rune_lower(0x3A3), 0x3C3);
        assert_eq!(rune_lower(0xD800), 0xD800);
        assert_eq!(rune_lower('1' as Rune), '1' as Rune);
    }

    #[test]
    fn plain_distance() {
        assert_eq!(distance(&r("kitten"), &r("sitting")), 3);
        assert_eq!(distance(&r(""), &r("abc")), 3);
        assert_eq!(distance(&r("abc"), &r("abc")), 0);
    }
}
