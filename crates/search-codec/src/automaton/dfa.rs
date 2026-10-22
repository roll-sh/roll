//! Dense byte DFAs built from code point NFAs in two stages, like Lucene.
//!
//! 1. `CpDfa`: subset construction over code point intervals, under Lucene's work limit
//!    (each DFA state costs the NFA states a character leads to; `10 × limit` fails with
//!    [`TooComplex`], whose message matches Lucene's `TooComplexToDeterminizeException`).
//! 2. `Dfa::from_cp`: each state's code point ranges become UTF-8 byte-range sequences in a
//!    trie, deterministic by construction, whose continuation nodes are shared across states
//!    (hash-consed), so a multi-byte `[80-BF]` tail exists once per target. No second subset
//!    construction is needed.
//!
//! The byte alphabet is partitioned into equivalence classes (bytes no transition
//! distinguishes) and the transition table is `states × classes` `u32`s: one class lookup and
//! one load per byte. States that cannot reach an accepting state are [`DEAD`], which is what
//! lets the term dictionary prune; states from which everything is accepted are flagged so
//! intersection can stop checking. Building `elasticsearch~2` takes about 0.3 ms, `*abc*` 8 µs.

use std::collections::HashMap;

use regex_syntax::utf8::Utf8Sequences;

use super::FxBuild;
use super::cp::CpDfa;
use super::nfa::{MAX_CP, Nfa, ascii_case_variants};
use super::{Automaton, DEAD};

/// Lucene's default determinize work limit (`Operations.DEFAULT_DETERMINIZE_WORK_LIMIT`), which
/// Elasticsearch exposes as `max_determinized_states`.
pub const DEFAULT_WORK_LIMIT: u32 = 10_000;

/// The automaton is too complex to determinize within the work limit.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TooComplex {
    pub states: usize,
    pub transitions: usize,
    pub limit: u32,
}

impl std::fmt::Display for TooComplex {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "Determinizing automaton with {} states and {} transitions would require more than {} effort.",
            self.states, self.transitions, self.limit
        )
    }
}

impl std::error::Error for TooComplex {}

/// A deterministic byte automaton.
#[derive(Clone, Debug)]
pub struct Dfa {
    classes: [u8; 256],
    nclasses: u32,
    trans: Vec<u32>,
    accept: Vec<bool>,
    always: Vec<bool>,
    start: u32,
}

struct ByteState {
    accept: bool,
    /// Counts toward the work limit (a code point NFA state, not a UTF-8 intermediate).
    real: bool,
    ranges: Vec<(u8, u8, u32)>,
    eps: Vec<u32>,
}

fn to_bytes_nfa(nfa: &Nfa) -> Vec<ByteState> {
    // Only states a character leads to (and the start) count toward the work limit: Lucene's
    // automata have no epsilon transitions, so the join and fork states of our Thompson
    // construction have no counterpart there and must not inflate the effort.
    let mut entered = vec![false; nfa.states.len()];
    if let Some(e) = entered.get_mut(nfa.start as usize) {
        *e = true;
    }
    for st in &nfa.states {
        for &(_, _, to) in &st.ranges {
            entered[to as usize] = true;
        }
        for &(_, _, to) in &st.bytes {
            entered[to as usize] = true;
        }
    }
    let mut out: Vec<ByteState> = nfa
        .states
        .iter()
        .zip(&entered)
        .map(|(s, &real)| ByteState {
            accept: s.accept,
            real,
            ranges: s.bytes.clone(),
            eps: s.eps.clone(),
        })
        .collect();
    let mut cache: HashMap<(u32, u32, u32), u32> = HashMap::new();
    for (from, s) in nfa.states.iter().enumerate() {
        for &(lo, hi, to) in &s.ranges {
            let entry = match cache.get(&(lo, hi, to)) {
                Some(&e) => e,
                None => {
                    let entry = out.len() as u32;
                    out.push(ByteState {
                        accept: false,
                        real: false,
                        ranges: Vec::new(),
                        eps: Vec::new(),
                    });
                    for (a, b) in split_surrogates(lo, hi) {
                        let (Some(ca), Some(cb)) = (char::from_u32(a), char::from_u32(b)) else {
                            continue;
                        };
                        for seq in Utf8Sequences::new(ca, cb) {
                            let rs = seq.as_slice();
                            let mut cur = entry;
                            for (i, r) in rs.iter().enumerate() {
                                let next = if i + 1 == rs.len() {
                                    to
                                } else {
                                    out.push(ByteState {
                                        accept: false,
                                        real: false,
                                        ranges: Vec::new(),
                                        eps: Vec::new(),
                                    });
                                    (out.len() - 1) as u32
                                };
                                out[cur as usize].ranges.push((r.start, r.end, next));
                                cur = next;
                            }
                        }
                    }
                    cache.insert((lo, hi, to), entry);
                    entry
                }
            };
            out[from].eps.push(entry);
        }
    }
    out
}

#[derive(Clone, Copy)]
enum Child {
    Local(usize),
    Final(u32),
}

/// The byte state of trie node `node` (its edges with children resolved bottom-up), shared
/// with every identical node built before; the root's edges are returned instead.
fn resolve(
    node: usize,
    trie: &[Vec<(u8, u8, Child)>],
    edges: &mut Vec<Vec<(u8, u8, u32)>>,
    accept: &mut Vec<bool>,
    shared: &mut HashMap<Vec<(u8, u8, u32)>, u32, FxBuild>,
) -> Vec<(u8, u8, u32)> {
    let mut out: Vec<(u8, u8, u32)> = trie[node]
        .iter()
        .map(|&(a, b, c)| {
            let t = match c {
                Child::Final(t) => t,
                Child::Local(k) => {
                    let e = resolve(k, trie, edges, accept, shared);
                    *shared.entry(e).or_insert_with_key(|e| {
                        edges.push(e.clone());
                        accept.push(false);
                        (edges.len() - 1) as u32
                    })
                }
            };
            (a, b, t)
        })
        .collect();
    out.sort_unstable();
    out
}

fn split_surrogates(lo: u32, hi: u32) -> Vec<(u32, u32)> {
    let hi = hi.min(MAX_CP);
    let mut v = Vec::with_capacity(2);
    if lo < 0xD800 {
        v.push((lo, hi.min(0xD7FF)));
    }
    if hi > 0xDFFF {
        v.push((lo.max(0xE000), hi));
    }
    v.retain(|(a, b)| a <= b);
    v
}

fn closure(states: &[ByteState], set: &mut Vec<u32>, mark: &mut [u32], stamp: u32) {
    let mut i = 0;
    for &s in set.iter() {
        mark[s as usize] = stamp;
    }
    while i < set.len() {
        let s = set[i] as usize;
        for &e in &states[s].eps {
            if mark[e as usize] != stamp {
                mark[e as usize] = stamp;
                set.push(e);
            }
        }
        i += 1;
    }
    set.sort_unstable();
}

impl Dfa {
    /// Determinizes a code point NFA with the given work limit: over code point intervals first
    /// (`CpDfa`, where the work limit applies), then over UTF-8 bytes.
    pub fn from_nfa(nfa: &Nfa, work_limit: u32) -> Result<Dfa, TooComplex> {
        if nfa.states.iter().any(|s| !s.bytes.is_empty()) {
            // Embedded byte automata cannot be determinized per code point.
            return Dfa::from_byte_nfa(nfa, work_limit);
        }
        Ok(Dfa::from_cp(&CpDfa::determinize(nfa, work_limit)?))
    }

    /// The byte DFA of a code point DFA, compiled directly (no subset construction): each code
    /// point state's ranges become UTF-8 byte-range sequences inserted into a trie (sequences of
    /// disjoint code point ranges have equal or disjoint byte ranges at every level, so the trie
    /// is deterministic), and identical continuation nodes are shared across all states, so the
    /// `[80-BF]` tails of multi-byte characters exist once per target.
    pub(crate) fn from_cp(cp: &CpDfa) -> Dfa {
        if cp.is_empty_language() {
            return Dfa::empty();
        }
        let n = cp.accept.len();
        // Byte states: the code point states first, then shared intermediate nodes.
        let mut edges: Vec<Vec<(u8, u8, u32)>> = vec![Vec::new(); n];
        let mut accept = cp.accept.clone();
        let mut shared: HashMap<Vec<(u8, u8, u32)>, u32, FxBuild> = HashMap::default();
        // Per-state trie scratch: nodes of (lo, hi, child) where child is a trie node (local)
        // or a final byte state.
        let mut trie: Vec<Vec<(u8, u8, Child)>> = Vec::new();
        for (s, row) in cp.trans.iter().enumerate() {
            trie.clear();
            trie.push(Vec::new());
            for &(lo, hi, to) in row {
                for (a, b) in split_surrogates(lo, hi) {
                    let (Some(ca), Some(cb)) = (char::from_u32(a), char::from_u32(b)) else {
                        continue;
                    };
                    for seq in Utf8Sequences::new(ca, cb) {
                        let rs = seq.as_slice();
                        let mut node = 0usize;
                        for (i, r) in rs.iter().enumerate() {
                            if i + 1 == rs.len() {
                                trie[node].push((r.start, r.end, Child::Final(to)));
                                break;
                            }
                            let found = trie[node].iter().find_map(|&(x, y, c)| match c {
                                Child::Local(k) if (x, y) == (r.start, r.end) => Some(k),
                                _ => None,
                            });
                            node = match found {
                                Some(k) => k,
                                None => {
                                    trie.push(Vec::new());
                                    let k = trie.len() - 1;
                                    trie[node].push((r.start, r.end, Child::Local(k)));
                                    k
                                }
                            };
                        }
                    }
                }
            }
            let root = resolve(0, &trie, &mut edges, &mut accept, &mut shared);
            edges[s] = root;
        }
        // Byte classes from every edge boundary, then the dense table.
        let mut boundary = [false; 257];
        for row in &edges {
            for &(a, b, _) in row {
                boundary[a as usize] = true;
                boundary[b as usize + 1] = true;
            }
        }
        let mut classes = [0u8; 256];
        let mut c: i32 = -1;
        for (b, class) in classes.iter_mut().enumerate() {
            if b == 0 || boundary[b] {
                c += 1;
            }
            *class = c as u8;
        }
        let nc = (c + 1) as usize;
        let mut trans = vec![DEAD; edges.len() * nc];
        for (s, row) in edges.iter().enumerate() {
            for &(a, b, t) in row {
                let (ca, cb) = (classes[a as usize] as usize, classes[b as usize] as usize);
                trans[s * nc + ca..=s * nc + cb].fill(t);
            }
        }
        let universal = cp.universal();
        let mut always = vec![false; edges.len()];
        always[..n].copy_from_slice(&universal);
        Dfa { classes, nclasses: nc as u32, trans, accept, always, start: 0 }
    }

    /// Subset construction over UTF-8 bytes of an NFA whose code point ranges are expanded to
    /// byte sequences (and which may embed raw byte transitions).
    fn from_byte_nfa(nfa: &Nfa, work_limit: u32) -> Result<Dfa, TooComplex> {
        if nfa.states.is_empty() {
            return Ok(Dfa::empty());
        }
        let states = to_bytes_nfa(nfa);
        // Byte classes from range boundaries.
        let mut boundary = [false; 257];
        for s in &states {
            for &(a, b, _) in &s.ranges {
                boundary[a as usize] = true;
                boundary[b as usize + 1] = true;
            }
        }
        let mut classes = [0u8; 256];
        let mut reps = Vec::new();
        let mut c: i32 = -1;
        for b in 0..256usize {
            if b == 0 || boundary[b] {
                c += 1;
                reps.push(b as u8);
            }
            classes[b] = c as u8;
        }
        let nclasses = reps.len() as u32;

        let too_complex = || TooComplex {
            states: nfa.states.len(),
            transitions: nfa.states.iter().map(|s| s.ranges.len()).sum(),
            limit: work_limit,
        };
        let effort_limit = work_limit as u64 * 10;
        let mut effort = 0u64;

        let mut mark = vec![0u32; states.len()];
        let mut stamp = 1u32;
        let mut ids: HashMap<Vec<u32>, u32, FxBuild> = HashMap::default();
        let mut sets: Vec<Vec<u32>> = Vec::new();
        let mut trans: Vec<u32> = Vec::new();
        let mut accept: Vec<bool> = Vec::new();

        let mut start = vec![nfa.start];
        closure(&states, &mut start, &mut mark, stamp);
        stamp += 1;
        ids.insert(start.clone(), 0);
        sets.push(start);
        let mut next = 0usize;
        let mut targets: Vec<u32> = Vec::new();
        while next < sets.len() {
            let set = sets[next].clone();
            effort += set.iter().filter(|&&s| states[s as usize].real).count() as u64;
            if effort >= effort_limit {
                return Err(too_complex());
            }
            accept.push(set.iter().any(|&s| states[s as usize].accept));
            for (ci, &rep) in reps.iter().enumerate() {
                targets.clear();
                for &s in &set {
                    for &(a, b, to) in &states[s as usize].ranges {
                        if a <= rep && rep <= b && mark[to as usize] != stamp {
                            mark[to as usize] = stamp;
                            targets.push(to);
                        }
                    }
                }
                stamp += 1;
                let id = if targets.is_empty() {
                    DEAD
                } else {
                    let mut t = targets.clone();
                    closure(&states, &mut t, &mut mark, stamp);
                    stamp += 1;
                    match ids.get(&t) {
                        Some(&id) => id,
                        None => {
                            let id = sets.len() as u32;
                            ids.insert(t.clone(), id);
                            sets.push(t);
                            id
                        }
                    }
                };
                let _ = ci;
                trans.push(id);
            }
            next += 1;
        }
        let mut dfa = Dfa {
            classes,
            nclasses,
            trans,
            accept,
            always: Vec::new(),
            start: 0,
        };
        dfa.prune();
        Ok(dfa)
    }

    /// The automaton accepting nothing.
    pub fn empty() -> Dfa {
        Dfa {
            classes: [0; 256],
            nclasses: 1,
            trans: Vec::new(),
            accept: Vec::new(),
            always: Vec::new(),
            start: DEAD,
        }
    }

    /// Collapses states that cannot reach acceptance into `DEAD` and computes `always`.
    fn prune(&mut self) {
        let n = self.accept.len();
        let nc = self.nclasses as usize;
        let mut rev: Vec<Vec<u32>> = vec![Vec::new(); n];
        for s in 0..n {
            for c in 0..nc {
                let t = self.trans[s * nc + c];
                if t != DEAD {
                    rev[t as usize].push(s as u32);
                }
            }
        }
        let mut live = self.accept.clone();
        let mut stack: Vec<u32> = (0..n as u32).filter(|&s| live[s as usize]).collect();
        while let Some(s) = stack.pop() {
            for &p in &rev[s as usize] {
                if !live[p as usize] {
                    live[p as usize] = true;
                    stack.push(p);
                }
            }
        }
        for t in self.trans.iter_mut() {
            if *t != DEAD && !live[*t as usize] {
                *t = DEAD;
            }
        }
        if n == 0 || !live[self.start as usize] {
            self.start = DEAD;
        }
        self.always = self.universal_states();
    }

    /// States from which every valid UTF-8 continuation is accepted: the greatest fixpoint over
    /// pairs (DFA state, UTF-8 decoder state) where decoder boundaries require acceptance and
    /// every valid next byte must lead to another good pair.
    fn universal_states(&self) -> Vec<bool> {
        let step = utf8_step;
        let n = self.accept.len();
        let nc = self.nclasses as usize;
        let mut good = vec![true; n * 8];
        for q in 0..n {
            if !self.accept[q] {
                good[q * 8] = false;
            }
        }
        loop {
            let mut changed = false;
            for q in 0..n {
                for v in 0..8 {
                    if !good[q * 8 + v] {
                        continue;
                    }
                    let ok = (0..=255u8).all(|b| match step(v, b) {
                        None => true,
                        Some(v2) => {
                            let t = self.trans[q * nc + self.classes[b as usize] as usize];
                            t != DEAD && good[t as usize * 8 + v2]
                        }
                    });
                    if !ok {
                        good[q * 8 + v] = false;
                        changed = true;
                    }
                }
            }
            if !changed {
                break;
            }
        }
        (0..n).map(|q| good[q * 8]).collect()
    }

    /// Number of states.
    pub fn num_states(&self) -> usize {
        self.accept.len()
    }

    /// Whether the automaton accepts no input at all.
    pub fn is_empty_language(&self) -> bool {
        self.start == DEAD
    }

    /// If the language is exactly one string, that string (lets queries rewrite to a term).
    pub fn single_string(&self) -> Option<Vec<u8>> {
        let mut s = self.start;
        let mut out = Vec::new();
        let nc = self.nclasses as usize;
        loop {
            if s == DEAD {
                return None;
            }
            let row = &self.trans[s as usize * nc..(s as usize + 1) * nc];
            let live: Vec<usize> = (0..nc).filter(|&c| row[c] != DEAD).collect();
            if self.accept[s as usize] {
                return live.is_empty().then_some(out);
            }
            if live.len() != 1 {
                return None;
            }
            let c = live[0];
            let bytes: Vec<u8> = (0..=255u8)
                .filter(|&b| self.classes[b as usize] as usize == c)
                .collect();
            if bytes.len() != 1 || out.len() > 1 << 16 {
                return None;
            }
            out.push(bytes[0]);
            s = row[c];
        }
    }

    /// Lucene's Levenshtein automaton: accepts strings within `max_edits` edits of `term`
    /// (insertions, deletions, substitutions and, with `transpositions`, swaps of adjacent
    /// characters), counting code points, where the first `prefix_len` code points must match
    /// exactly.
    pub fn levenshtein(term: &str, max_edits: u32, prefix_len: usize, transpositions: bool) -> Dfa {
        let nfa = levenshtein_nfa(term, max_edits, prefix_len, transpositions);
        // Levenshtein DFAs are linear in the term length for the supported edit distances.
        Dfa::from_nfa(&nfa, u32::MAX / 16).unwrap_or_else(|_| Dfa::empty())
    }

    /// Lucene's `WildcardQuery` syntax: `*` matches any string, `?` any one character, `\`
    /// escapes the next character (a trailing `\` is literal).
    pub fn wildcard(
        pattern: &str,
        case_insensitive: bool,
        work_limit: u32,
    ) -> Result<Dfa, TooComplex> {
        Dfa::from_nfa(&wildcard_nfa(pattern, case_insensitive), work_limit)
    }

    /// Terms starting with `prefix`, compared case-insensitively like Elasticsearch's `prefix`
    /// query with `case_insensitive` (ASCII letters only); case-sensitive prefixes should use
    /// [`super::Prefix`].
    pub fn prefix_case_insensitive(prefix: &str, work_limit: u32) -> Result<Dfa, TooComplex> {
        let mut nfa = case_insensitive_string_nfa(prefix);
        let end = nfa.states.len() as u32 - 1;
        nfa.add_any(end, end);
        Dfa::from_nfa(&nfa, work_limit)
    }

    /// Exactly `term`, compared case-insensitively like Elasticsearch's `term` query with
    /// `case_insensitive` (ASCII letters only).
    pub fn term_case_insensitive(term: &str, work_limit: u32) -> Result<Dfa, TooComplex> {
        Dfa::from_nfa(&case_insensitive_string_nfa(term), work_limit)
    }
}

impl Dfa {
    /// The product automaton accepting inputs both accept.
    pub fn intersection(a: &Dfa, b: &Dfa) -> Dfa {
        if a.start == DEAD || b.start == DEAD {
            return Dfa::empty();
        }
        // Classes of the product: refine both partitions.
        let mut classes = [0u8; 256];
        let mut reps = Vec::new();
        let mut map: HashMap<(u8, u8), u8> = HashMap::new();
        for (byte, class) in classes.iter_mut().enumerate() {
            let key = (a.classes[byte], b.classes[byte]);
            *class = *map.entry(key).or_insert_with(|| {
                reps.push(byte as u8);
                (reps.len() - 1) as u8
            });
        }
        let nc = reps.len();
        let mut ids: HashMap<(u32, u32), u32> = HashMap::new();
        let mut pairs = vec![(a.start, b.start)];
        ids.insert((a.start, b.start), 0);
        let mut trans = Vec::new();
        let mut accept = Vec::new();
        let mut i = 0;
        while i < pairs.len() {
            let (x, y) = pairs[i];
            accept.push(a.is_match(x) && b.is_match(y));
            for &rep in &reps {
                let (nx, ny) = (a.accept(x, rep), b.accept(y, rep));
                let id = if nx == DEAD || ny == DEAD {
                    DEAD
                } else {
                    *ids.entry((nx, ny)).or_insert_with(|| {
                        pairs.push((nx, ny));
                        (pairs.len() - 1) as u32
                    })
                };
                trans.push(id);
            }
            i += 1;
        }
        let mut d = Dfa {
            classes,
            nclasses: nc as u32,
            trans,
            accept,
            always: Vec::new(),
            start: 0,
        };
        d.prune();
        d
    }

    /// The automaton accepting every valid UTF-8 string this one rejects (Lucene's
    /// `Operations.complement` over code points).
    pub fn complement(&self) -> Dfa {
        let nc = self.nclasses as usize;
        let n = self.accept.len();
        let sink = n as u32;
        let mut trans = Vec::with_capacity((n + 1) * nc);
        for s in 0..n {
            for c in 0..nc {
                let t = self.trans[s * nc + c];
                trans.push(if t == DEAD { sink } else { t });
            }
        }
        trans.extend(std::iter::repeat_n(sink, nc));
        let mut accept: Vec<bool> = self.accept.iter().map(|a| !a).collect();
        accept.push(true);
        let start = if self.start == DEAD { sink } else { self.start };
        let flipped = Dfa {
            classes: self.classes,
            nclasses: self.nclasses,
            trans,
            accept,
            always: Vec::new(),
            start,
        };
        Dfa::intersection(&flipped, &Dfa::utf8())
    }

    /// The automaton accepting exactly the valid UTF-8 strings.
    pub fn utf8() -> Dfa {
        // States: 0 boundary (accepting), 1..=3 need that many continuation bytes, 4 after E0,
        // 5 after ED, 6 after F0, 7 after F4.
        let mut classes = [0u8; 256];
        let mut sigs: Vec<[u32; 8]> = Vec::new();
        let mut cols: Vec<[u32; 8]> = Vec::new();
        for (b, class) in classes.iter_mut().enumerate() {
            let sig: [u32; 8] = std::array::from_fn(|v| utf8_step(v, b as u8).map_or(DEAD, |x| x as u32));
            let c = match sigs.iter().position(|s| *s == sig) {
                Some(c) => c,
                None => {
                    sigs.push(sig);
                    cols.push(sig);
                    sigs.len() - 1
                }
            };
            *class = c as u8;
        }
        let nc = cols.len();
        let mut trans = vec![DEAD; 8 * nc];
        for v in 0..8 {
            for (c, col) in cols.iter().enumerate() {
                trans[v * nc + c] = col[v];
            }
        }
        let mut accept = vec![false; 8];
        accept[0] = true;
        let mut d = Dfa { classes, nclasses: nc as u32, trans, accept, always: Vec::new(), start: 0 };
        d.prune();
        d
    }

}

/// One step of a strict UTF-8 decoder (no overlongs, surrogates or values past U+10FFFF):
/// 0 is the boundary state, 1..=3 need that many continuation bytes, 4 follows E0, 5 ED, 6 F0
/// and 7 F4.
fn utf8_step(v: usize, b: u8) -> Option<usize> {
    match v {
        0 => match b {
            0x00..=0x7F => Some(0),
            0xC2..=0xDF => Some(1),
            0xE0 => Some(4),
            0xE1..=0xEC | 0xEE..=0xEF => Some(2),
            0xED => Some(5),
            0xF0 => Some(6),
            0xF1..=0xF3 => Some(3),
            0xF4 => Some(7),
            _ => None,
        },
        1..=3 => (0x80..=0xBF).contains(&b).then_some(v - 1),
        4 => (0xA0..=0xBF).contains(&b).then_some(1),
        5 => (0x80..=0x9F).contains(&b).then_some(1),
        6 => (0x90..=0xBF).contains(&b).then_some(2),
        7 => (0x80..=0x8F).contains(&b).then_some(2),
        _ => None,
    }
}

impl Automaton for Dfa {
    #[inline]
    fn start(&self) -> u32 {
        self.start
    }
    #[inline]
    fn is_match(&self, s: u32) -> bool {
        s != DEAD && self.accept[s as usize]
    }
    #[inline]
    fn can_match(&self, s: u32) -> bool {
        s != DEAD
    }
    #[inline]
    fn will_always_match(&self, s: u32) -> bool {
        s != DEAD && self.always[s as usize]
    }
    #[inline]
    fn accept(&self, s: u32, b: u8) -> u32 {
        if s == DEAD {
            return DEAD;
        }
        self.trans[s as usize * self.nclasses as usize + self.classes[b as usize] as usize]
    }
    #[inline]
    fn next_live_byte(&self, s: u32, from: u8) -> Option<u8> {
        if s == DEAD {
            return None;
        }
        let row = &self.trans[s as usize * self.nclasses as usize..][..self.nclasses as usize];
        (from..=255).find(|&b| row[self.classes[b as usize] as usize] != DEAD)
    }
}

/// The Levenshtein NFA (see [`Dfa::levenshtein`]).
pub fn levenshtein_nfa(term: &str, max_edits: u32, prefix_len: usize, transpositions: bool) -> Nfa {
    let chars: Vec<u32> = term.chars().map(|c| c as u32).collect();
    let p = prefix_len.min(chars.len());
    let (prefix, rest) = chars.split_at(p);
    let mut nfa = Nfa::new();
    let start = nfa.add_state();
    nfa.set_start(start);
    let mut cur = start;
    for &c in prefix {
        let n = nfa.add_state();
        nfa.add_char(cur, c, n);
        cur = n;
    }
    let n = rest.len();
    let k = max_edits as usize;
    let base = nfa.len() as u32;
    let id = |i: usize, e: usize| base + (i * (k + 1) + e) as u32;
    for _ in 0..(n + 1) * (k + 1) {
        nfa.add_state();
    }
    nfa.add_epsilon(cur, id(0, 0));
    for i in 0..=n {
        for e in 0..=k {
            let s = id(i, e);
            if i == n {
                nfa.set_accept(s, true);
            }
            if i < n {
                nfa.add_char(s, rest[i], id(i + 1, e));
            }
            if e < k {
                nfa.add_any(s, id(i, e + 1));
                if i < n {
                    nfa.add_any(s, id(i + 1, e + 1));
                    nfa.add_epsilon(s, id(i + 1, e + 1));
                }
                if transpositions && i + 1 < n && rest[i] != rest[i + 1] {
                    let t = nfa.add_state();
                    nfa.add_char(s, rest[i + 1], t);
                    nfa.add_char(t, rest[i], id(i + 2, e + 1));
                }
            }
        }
    }
    nfa
}

/// A chain accepting `s` with ASCII letters in either case; its last state accepts.
fn case_insensitive_string_nfa(s: &str) -> Nfa {
    let mut nfa = Nfa::new();
    let mut cur = nfa.add_state();
    nfa.set_start(cur);
    for c in s.chars() {
        let n = nfa.add_state();
        for v in ascii_case_variants(c as u32) {
            nfa.add_char(cur, v, n);
        }
        cur = n;
    }
    nfa.set_accept(cur, true);
    nfa
}

/// The wildcard NFA (see [`Dfa::wildcard`]).
pub fn wildcard_nfa(pattern: &str, case_insensitive: bool) -> Nfa {
    let mut nfa = Nfa::new();
    let mut cur = nfa.add_state();
    nfa.set_start(cur);
    let mut chars = pattern.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '*' => {
                nfa.add_any(cur, cur);
            }
            '?' => {
                let n = nfa.add_state();
                nfa.add_any(cur, n);
                cur = n;
            }
            _ => {
                // An escaped character matches exactly, even case-insensitively; a trailing
                // `\` is a literal backslash (Elasticsearch's lenient parsing).
                let (lit, escaped) = match (c, chars.peek()) {
                    ('\\', Some(_)) => (chars.next().unwrap_or('\\'), true),
                    _ => (c, false),
                };
                let n = nfa.add_state();
                if case_insensitive && !escaped {
                    for v in ascii_case_variants(lit as u32) {
                        nfa.add_char(cur, v, n);
                    }
                } else {
                    nfa.add_char(cur, lit as u32, n);
                }
                cur = n;
            }
        }
    }
    nfa.set_accept(cur, true);
    nfa
}

#[cfg(test)]
mod tests {
    use super::super::edit_distance;
    use super::*;
    use rand::{Rng, SeedableRng, rngs::StdRng};

    #[test]
    fn case_insensitive_prefix_escapes_wildcards() {
        let d = Dfa::prefix_case_insensitive("Ab*?\\", DEFAULT_WORK_LIMIT).unwrap();
        assert!(d.matches("ab*?\\".as_bytes()));
        assert!(d.matches("AB*?\\xyz".as_bytes()));
        assert!(!d.matches(b"abc?\\"));
        assert!(!d.matches(b"ab*?"));
        let all = Dfa::prefix_case_insensitive("", DEFAULT_WORK_LIMIT).unwrap();
        assert!(all.matches(b"") && all.matches(b"anything"));
    }

    fn random_word(rng: &mut StdRng, alphabet: &[char], max: usize) -> String {
        let n = rng.gen_range(0..=max);
        (0..n)
            .map(|_| alphabet[rng.gen_range(0..alphabet.len())])
            .collect()
    }

    #[test]
    fn levenshtein_matches_brute_force() {
        let mut rng = StdRng::seed_from_u64(1);
        let alphabet = ['a', 'b', 'c', 'é', '日'];
        for _ in 0..300 {
            let term = random_word(&mut rng, &alphabet, 6);
            let k = rng.gen_range(0..=2);
            let trans = rng.gen_bool(0.5);
            let prefix = rng.gen_range(0..=2);
            let dfa = Dfa::levenshtein(&term, k, prefix, trans);
            let tp: String = term.chars().take(prefix).collect();
            for _ in 0..100 {
                let cand = random_word(&mut rng, &alphabet, 8);
                let cp: String = cand.chars().take(tp.chars().count()).collect();
                let want = cp == tp
                    && edit_distance(
                        &term.chars().skip(prefix).collect::<String>(),
                        &cand.chars().skip(tp.chars().count()).collect::<String>(),
                        trans,
                    ) <= k;
                assert_eq!(
                    dfa.matches(cand.as_bytes()),
                    want,
                    "term {term:?} k {k} t {trans} p {prefix} cand {cand:?}"
                );
            }
        }
    }

    #[test]
    fn levenshtein_any_char_is_any_code_point() {
        let dfa = Dfa::levenshtein("abc", 1, 0, true);
        assert!(dfa.matches("abXc".as_bytes()));
        assert!(dfa.matches("ab😀c".as_bytes()));
        assert!(dfa.matches("ab😀".as_bytes()));
        assert!(dfa.matches("bac".as_bytes()));
        assert!(!dfa.matches("ab😀😀".as_bytes()));
        assert!(!dfa.matches(&[b'a', b'b', 0xff]));
    }

    #[test]
    fn wildcards() {
        let d = Dfa::wildcard("te?t*", false, DEFAULT_WORK_LIMIT).unwrap();
        assert!(d.matches(b"test"));
        assert!(d.matches("tést".replace('é', "e").as_bytes()));
        assert!(d.matches("te日t".as_bytes()));
        assert!(d.matches(b"testing"));
        assert!(!d.matches(b"tet"));
        let d = Dfa::wildcard(r"a\*b\", false, DEFAULT_WORK_LIMIT).unwrap();
        assert!(d.matches(br"a*b\"));
        assert!(!d.matches(b"axb"));
        let d = Dfa::wildcard("*", false, DEFAULT_WORK_LIMIT).unwrap();
        assert!(d.will_always_match(d.start()));
        let d = Dfa::wildcard("FoO*", true, DEFAULT_WORK_LIMIT).unwrap();
        assert!(d.matches(b"foobar"));
        assert!(d.matches(b"FOO"));
        assert!(!d.matches(b"fo"));
        assert_eq!(
            Dfa::wildcard("abc", false, DEFAULT_WORK_LIMIT)
                .unwrap()
                .single_string(),
            Some(b"abc".to_vec())
        );
        assert_eq!(
            Dfa::wildcard("ab*", false, DEFAULT_WORK_LIMIT)
                .unwrap()
                .single_string(),
            None
        );
    }

    #[test]
    fn work_limit() {
        let e = Dfa::wildcard("*a*b*c*d*e*f*g*h*i*j*k*l*m*n*o*p*", false, 10).unwrap_err();
        assert!(e.to_string().starts_with("Determinizing automaton with"));
    }

    /// `cargo test --release -p search-codec --lib bench_automaton_build -- --ignored --nocapture`
    #[test]
    #[ignore]
    fn bench_automaton_build() {
        let time = |name: &str, f: &dyn Fn() -> usize| {
            let rounds = 200;
            let t = std::time::Instant::now();
            let mut n = 0;
            for _ in 0..rounds {
                n += f();
            }
            println!("{name}: {:?}/build ({} states)", t.elapsed() / rounds, n / rounds as usize);
        };
        {
            let nfa = levenshtein_nfa("elasticsearch", 2, 0, true);
            let rounds = 200;
            let t = std::time::Instant::now();
            let mut cp = CpDfa::default();
            for _ in 0..rounds {
                cp = CpDfa::determinize(&nfa, u32::MAX / 16).unwrap();
            }
            println!("  cp stage: {:?} ({} cp states)", t.elapsed() / rounds, cp.accept.len());
            let t = std::time::Instant::now();
            let mut d = Dfa::empty();
            for _ in 0..rounds {
                d = Dfa::from_cp(&cp);
            }
            println!("  byte stage: {:?} ({} byte states, {} classes)", t.elapsed() / rounds, d.num_states(), d.nclasses);
        }
        time("fuzzy 'elasticsearch'~2 transpositions", &|| Dfa::levenshtein("elasticsearch", 2, 0, true).trans.len());
        time("fuzzy 'kitten'~1", &|| Dfa::levenshtein("kitten", 1, 0, true).trans.len());
        time("fuzzy 'internationalization'~2", &|| Dfa::levenshtein("internationalization", 2, 1, true).trans.len());
        time("wildcard '*abc*'", &|| Dfa::wildcard("*abc*", false, DEFAULT_WORK_LIMIT).unwrap().trans.len());
        time("wildcard ci 'Foo*Bar?baz*'", &|| Dfa::wildcard("Foo*Bar?baz*", true, DEFAULT_WORK_LIMIT).unwrap().trans.len());
        time("regexp '[a-z]+ing'", &|| crate::automaton::regexp::compile("[a-z]+ing", 0xff, false, DEFAULT_WORK_LIMIT).unwrap().trans.len());
        time("regexp '.*a.{12}' (near limit)", &|| crate::automaton::regexp::compile(".*a.{12}", 0xff, false, DEFAULT_WORK_LIMIT).unwrap().trans.len());
    }
}
