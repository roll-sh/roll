//! A small lexer generator: regular expressions over code-point sets compiled to a DFA.
//!
//! Lucene's `StandardTokenizer`, `UAX29URLEmailTokenizer` and `ClassicTokenizer` are JFlex
//! scanners: a list of rules, each a regular expression over Unicode property classes, matched with
//! longest-match-wins and earlier-rule-wins on ties. Reproducing their output exactly means
//! reproducing that matching discipline, so instead of hand-written state machines the grammars
//! are written as [`Re`] trees (see `tokenizer::standard`) and compiled here, once per process:
//!
//! 1. every code-point set used by any rule is collected and the code space is partitioned into
//!    equivalence classes (code points no set distinguishes share a class), so transition tables
//!    are indexed by a small class id instead of a code point;
//! 2. a Thompson NFA is built over class ids and determinized by subset construction; a DFA state
//!    accepts with the lowest rule index among its NFA accept states, which is JFlex's tie-break.
//!
//! Scanning is then one class lookup (a direct table for ASCII, a two-level table otherwise) and
//! one transition load per character, with no allocation.

use std::collections::HashMap;

/// Sorted, non-overlapping, non-adjacent inclusive code-point ranges.
pub(crate) type Ranges = Vec<(u32, u32)>;

const MAX_CP: u32 = 0x10FFFF;

/// Builds a normalized range list.
pub(crate) fn ranges(mut r: Vec<(u32, u32)>) -> Ranges {
    r.sort_unstable();
    let mut out: Ranges = Vec::with_capacity(r.len());
    for (lo, hi) in r {
        if let Some(last) = out.last_mut()
            && lo <= last.1.saturating_add(1)
        {
            last.1 = last.1.max(hi);
            continue;
        }
        out.push((lo, hi));
    }
    out
}

pub(crate) fn union(sets: &[&[(u32, u32)]]) -> Ranges {
    ranges(sets.iter().flat_map(|s| s.iter().copied()).collect())
}

pub(crate) fn negate(a: &[(u32, u32)]) -> Ranges {
    let mut out = Vec::new();
    let mut next = 0u32;
    for &(lo, hi) in a {
        if lo > next {
            out.push((next, lo - 1));
        }
        next = hi + 1;
    }
    if next <= MAX_CP {
        out.push((next, MAX_CP));
    }
    out
}

pub(crate) fn intersect(a: &[(u32, u32)], b: &[(u32, u32)]) -> Ranges {
    let (mut i, mut j) = (0, 0);
    let mut out = Vec::new();
    while i < a.len() && j < b.len() {
        let lo = a[i].0.max(b[j].0);
        let hi = a[i].1.min(b[j].1);
        if lo <= hi {
            out.push((lo, hi));
        }
        if a[i].1 < b[j].1 {
            i += 1;
        } else {
            j += 1;
        }
    }
    out
}

pub(crate) fn difference(a: &[(u32, u32)], b: &[(u32, u32)]) -> Ranges {
    intersect(a, &negate(b))
}

pub(crate) fn chars(s: &str) -> Ranges {
    ranges(s.chars().map(|c| (c as u32, c as u32)).collect())
}

pub(crate) fn contains(set: &[(u32, u32)], cp: u32) -> bool {
    set.binary_search_by(|&(lo, hi)| {
        if hi < cp {
            std::cmp::Ordering::Less
        } else if lo > cp {
            std::cmp::Ordering::Greater
        } else {
            std::cmp::Ordering::Equal
        }
    })
    .is_ok()
}

/// A regular expression over code-point sets.
#[derive(Clone, Debug)]
pub(crate) enum Re {
    Set(Ranges),
    Seq(Vec<Re>),
    Alt(Vec<Re>),
    Star(Box<Re>),
    Plus(Box<Re>),
    Opt(Box<Re>),
    /// `r{min,max}`; `max == None` is unbounded.
    Repeat(Box<Re>, u32, Option<u32>),
}

pub(crate) fn set(r: &[(u32, u32)]) -> Re {
    Re::Set(r.to_vec())
}
pub(crate) fn seq(v: Vec<Re>) -> Re {
    Re::Seq(v)
}
pub(crate) fn alt(v: Vec<Re>) -> Re {
    Re::Alt(v)
}
pub(crate) fn star(r: Re) -> Re {
    Re::Star(Box::new(r))
}
pub(crate) fn plus(r: Re) -> Re {
    Re::Plus(Box::new(r))
}
pub(crate) fn opt(r: Re) -> Re {
    Re::Opt(Box::new(r))
}
pub(crate) fn repeat(r: Re, min: u32, max: Option<u32>) -> Re {
    Re::Repeat(Box::new(r), min, max)
}
/// A literal string (case-sensitive).
pub(crate) fn lit(s: &str) -> Re {
    Re::Seq(
        s.chars()
            .map(|c| Re::Set(vec![(c as u32, c as u32)]))
            .collect(),
    )
}
/// A literal string matched ASCII-case-insensitively.
pub(crate) fn lit_ci(s: &str) -> Re {
    Re::Seq(
        s.chars()
            .map(|c| {
                let (l, u) = (c.to_ascii_lowercase() as u32, c.to_ascii_uppercase() as u32);
                Re::Set(ranges(vec![(l, l), (u, u)]))
            })
            .collect(),
    )
}

/// Character-class lookup: code point to class id.
struct ClassMap {
    ascii: [u8; 128],
    index: Vec<u16>,
    blocks: Vec<[u8; 256]>,
}

impl ClassMap {
    #[inline]
    fn get(&self, cp: u32) -> usize {
        if cp < 128 {
            return self.ascii[cp as usize] as usize;
        }
        self.blocks[self.index[(cp >> 8) as usize] as usize][(cp & 0xFF) as usize] as usize
    }
}

/// A compiled scanner.
pub(crate) struct Dfa {
    classes: ClassMap,
    ncls: usize,
    /// Lexical states.
    nlex: usize,
    /// `trans[state * ncls + class]`; state 0 is dead.
    trans: Vec<u32>,
    /// `accept[state * nlex + lexical state]`: 0 = not accepting, otherwise rule index + 1.
    accept: Vec<u16>,
}

/// Hopcroft's algorithm: merges equivalent states (same accepts in every lexical state, and
/// transitions into equivalent states). Returns renumbered tables with the dead state at 0 and
/// the start state at 1.
fn minimize(trans: &[u32], accept: &[u16], ncls: usize, nlex: usize) -> (Vec<u32>, Vec<u16>) {
    let n = accept.len() / nlex;
    // Initial partition by accept signature; the dead state's block first.
    let mut block_of = vec![0u32; n];
    let mut blocks: Vec<Vec<u32>> = Vec::new();
    let mut by_sig: HashMap<&[u16], u32> = HashMap::new();
    for s in 0..n {
        let sig = &accept[s * nlex..(s + 1) * nlex];
        let b = *by_sig.entry(sig).or_insert_with(|| {
            blocks.push(Vec::new());
            (blocks.len() - 1) as u32
        });
        block_of[s] = b;
        blocks[b as usize].push(s as u32);
    }
    // Inverse transitions (CSR per class).
    let mut inv_start = vec![0u32; ncls * (n + 1) + 1];
    for s in 0..n {
        for c in 0..ncls {
            let t = trans[s * ncls + c] as usize;
            inv_start[c * (n + 1) + t + 1] += 1;
        }
    }
    for i in 1..inv_start.len() {
        inv_start[i] += inv_start[i - 1];
    }
    let mut fill = inv_start.clone();
    let mut inv = vec![0u32; n * ncls];
    for s in 0..n {
        for c in 0..ncls {
            let t = trans[s * ncls + c] as usize;
            let k = c * (n + 1) + t;
            inv[fill[k] as usize] = s as u32;
            fill[k] += 1;
        }
    }
    let mut work: Vec<(u32, u16)> = Vec::new();
    // Bitset over (block, class); blocks never exceed n.
    let mut in_work = vec![0u64; (n * ncls).div_ceil(64)];
    let bit = |b: u32, c: u16| b as usize * ncls + c as usize;
    let set = |w: &mut [u64], i: usize| w[i / 64] |= 1 << (i % 64);
    let clear = |w: &mut [u64], i: usize| w[i / 64] &= !(1 << (i % 64));
    let get = |w: &[u64], i: usize| w[i / 64] & (1 << (i % 64)) != 0;
    for b in 0..blocks.len() as u32 {
        for c in 0..ncls as u16 {
            work.push((b, c));
            set(&mut in_work, bit(b, c));
        }
    }
    let mut marked = vec![false; n];
    let mut touched: Vec<u32> = Vec::new();
    let mut members: HashMap<u32, Vec<u32>> = HashMap::new();
    while let Some((b, c)) = work.pop() {
        clear(&mut in_work, bit(b, c));
        // States with a c-transition into block b.
        let c = c as usize;
        for &t in &blocks[b as usize] {
            let k = c * (n + 1) + t as usize;
            for &s in &inv[inv_start[k] as usize..inv_start[k + 1] as usize] {
                if !marked[s as usize] {
                    marked[s as usize] = true;
                    touched.push(s);
                }
            }
        }
        for &s in &touched {
            members.entry(block_of[s as usize]).or_default().push(s);
        }
        for (y, xs) in members.drain() {
            let ylen = blocks[y as usize].len();
            if xs.len() == ylen {
                continue;
            }
            // Split y into (y \ X) and X.
            let new_id = blocks.len() as u32;
            blocks[y as usize].retain(|&s| !marked[s as usize]);
            for &s in &xs {
                block_of[s as usize] = new_id;
            }
            blocks.push(xs);
            for c2 in 0..ncls as u16 {
                if get(&in_work, bit(y, c2)) {
                    work.push((new_id, c2));
                    set(&mut in_work, bit(new_id, c2));
                } else {
                    let smaller = if blocks[y as usize].len() <= blocks[new_id as usize].len() {
                        y
                    } else {
                        new_id
                    };
                    work.push((smaller, c2));
                    set(&mut in_work, bit(smaller, c2));
                }
            }
        }
        for &s in &touched {
            marked[s as usize] = false;
        }
        touched.clear();
    }
    // Renumber: dead's block 0, start's block 1, the rest in order.
    let dead_block = block_of[DEAD as usize];
    let start_block = block_of[START as usize];
    let mut new_id = vec![u32::MAX; blocks.len()];
    new_id[dead_block as usize] = DEAD;
    let mut next = 1u32;
    if start_block != dead_block {
        new_id[start_block as usize] = START;
        next = 2;
    }
    for id in new_id.iter_mut() {
        if *id == u32::MAX {
            *id = next;
            next += 1;
        }
    }
    let m = (next as usize).max(2);
    let mut t2 = vec![0u32; m * ncls];
    let mut a2 = vec![0u16; m * nlex];
    for (b, states) in blocks.iter().enumerate() {
        let Some(&rep) = states.first() else { continue };
        let id = new_id[b] as usize;
        if b as u32 == dead_block {
            continue;
        }
        for c in 0..ncls {
            t2[id * ncls + c] = new_id[block_of[trans[rep as usize * ncls + c] as usize] as usize];
        }
        a2[id * nlex..(id + 1) * nlex]
            .copy_from_slice(&accept[rep as usize * nlex..(rep as usize + 1) * nlex]);
    }
    (t2, a2)
}

pub(crate) const DEAD: u32 = 0;
pub(crate) const START: u32 = 1;

struct Nfa {
    /// Per state: epsilon edges.
    eps: Vec<Vec<u32>>,
    /// Per state: (set id, target).
    edges: Vec<Vec<(u32, u32)>>,
    /// Per state: accepting rule.
    accept: Vec<Option<u16>>,
}

impl Nfa {
    fn state(&mut self) -> u32 {
        self.eps.push(Vec::new());
        self.edges.push(Vec::new());
        self.accept.push(None);
        (self.eps.len() - 1) as u32
    }

    /// Adds `re` between fresh states, returning (entry, exit).
    fn build(&mut self, re: &Re, set_ids: &HashMap<Ranges, u32>) -> (u32, u32) {
        match re {
            Re::Set(r) => {
                let (a, b) = (self.state(), self.state());
                self.edges[a as usize].push((set_ids[r], b));
                (a, b)
            }
            Re::Seq(v) => {
                let start = self.state();
                let mut cur = start;
                for r in v {
                    let (a, b) = self.build(r, set_ids);
                    self.eps[cur as usize].push(a);
                    cur = b;
                }
                (start, cur)
            }
            Re::Alt(v) => {
                let (start, end) = (self.state(), self.state());
                for r in v {
                    let (a, b) = self.build(r, set_ids);
                    self.eps[start as usize].push(a);
                    self.eps[b as usize].push(end);
                }
                (start, end)
            }
            Re::Star(r) => {
                let (start, end) = (self.state(), self.state());
                let (a, b) = self.build(r, set_ids);
                self.eps[start as usize].push(a);
                self.eps[start as usize].push(end);
                self.eps[b as usize].push(a);
                self.eps[b as usize].push(end);
                (start, end)
            }
            Re::Plus(r) => {
                let (start, end) = (self.state(), self.state());
                let (a, b) = self.build(r, set_ids);
                self.eps[start as usize].push(a);
                self.eps[b as usize].push(a);
                self.eps[b as usize].push(end);
                (start, end)
            }
            Re::Opt(r) => {
                let (start, end) = (self.state(), self.state());
                let (a, b) = self.build(r, set_ids);
                self.eps[start as usize].push(a);
                self.eps[start as usize].push(end);
                self.eps[b as usize].push(end);
                (start, end)
            }
            Re::Repeat(r, min, max) => {
                let mut parts: Vec<Re> = (0..*min).map(|_| (**r).clone()).collect();
                match max {
                    None => parts.push(Re::Star(r.clone())),
                    Some(max) => {
                        for _ in *min..*max {
                            parts.push(Re::Opt(r.clone()));
                        }
                    }
                }
                self.build(&Re::Seq(parts), set_ids)
            }
        }
    }

    fn closure(&self, states: &mut Vec<u32>, seen: &mut [bool]) {
        let mut stack: Vec<u32> = states.clone();
        while let Some(s) = stack.pop() {
            for &t in &self.eps[s as usize] {
                if !seen[t as usize] {
                    seen[t as usize] = true;
                    states.push(t);
                    stack.push(t);
                }
            }
        }
        states.sort_unstable();
        for &s in states.iter() {
            seen[s as usize] = false;
        }
    }
}

fn collect_sets(re: &Re, out: &mut Vec<Ranges>) {
    match re {
        Re::Set(r) => out.push(r.clone()),
        Re::Seq(v) | Re::Alt(v) => v.iter().for_each(|r| collect_sets(r, out)),
        Re::Star(r) | Re::Plus(r) | Re::Opt(r) | Re::Repeat(r, _, _) => collect_sets(r, out),
    }
}

/// The class partition of a rule set plus its NFA, with every state's epsilon closure
/// precomputed: what subset construction needs, eager ([`Dfa`]) or on demand ([`LazyDfa`]).
pub(crate) struct Prog {
    classes: ClassMap,
    ncls: usize,
    nlex: usize,
    active: Vec<u32>,
    /// Per NFA state: (set id, target).
    edges: Vec<Vec<(u32, u32)>>,
    /// Per set id: bitmap of the classes it covers.
    set_classes: Vec<[u64; 4]>,
    accept: Vec<Option<u16>>,
    /// Per NFA state: its epsilon closure, restricted to states with a character edge or an
    /// accept (the only ones that distinguish DFA states), sorted.
    closures: Vec<Box<[u32]>>,
    start: Box<[u32]>,
}

impl Prog {
    pub(crate) fn new(rules: &[Re], active: &[u32]) -> Prog {
        let nlex = active.iter().fold(0u32, |a, &m| a | m).max(1);
        let nlex = 32 - nlex.leading_zeros() as usize;
        // 1. Distinct sets and the class partition.
        let mut sets = Vec::new();
        for r in rules {
            collect_sets(r, &mut sets);
        }
        sets.sort();
        sets.dedup();
        let set_ids: HashMap<Ranges, u32> = sets
            .iter()
            .enumerate()
            .map(|(i, s)| (s.clone(), i as u32))
            .collect();
        let mut bounds = vec![0u32, MAX_CP + 1];
        for s in &sets {
            for &(lo, hi) in s {
                bounds.push(lo);
                bounds.push(hi + 1);
            }
        }
        bounds.sort_unstable();
        bounds.dedup();
        // Class 0 is "in no set".
        let words = sets.len().div_ceil(64).max(1);
        let mut sig_class: HashMap<Vec<u64>, u8> = HashMap::new();
        sig_class.insert(vec![0u64; words], 0);
        let mut class_sets: Vec<Vec<u64>> = vec![vec![0u64; words]];
        let mut intervals: Vec<(u32, u32, u8)> = Vec::new();
        for w in bounds.windows(2) {
            let (lo, hi) = (w[0], w[1] - 1);
            let mut sig = vec![0u64; words];
            for (i, s) in sets.iter().enumerate() {
                if contains(s, lo) {
                    sig[i / 64] |= 1 << (i % 64);
                }
            }
            let next = sig_class.len();
            let cls = *sig_class.entry(sig.clone()).or_insert_with(|| {
                assert!(next < 256, "too many character classes");
                class_sets.push(sig);
                next as u8
            });
            intervals.push((lo, hi, cls));
        }
        let ncls = class_sets.len();
        // Two-level class table built block by block from the sorted intervals.
        let mut ascii = [0u8; 128];
        let mut blocks: Vec<[u8; 256]> = Vec::new();
        let mut block_ids: HashMap<[u8; 256], u16> = HashMap::new();
        let nblocks = (MAX_CP as usize + 1) / 256;
        let mut index = Vec::with_capacity(nblocks);
        let mut k = 0usize;
        for b in 0..nblocks {
            let (blo, bhi) = ((b * 256) as u32, (b * 256 + 255) as u32);
            while intervals[k].1 < blo {
                k += 1;
            }
            let mut block = [0u8; 256];
            let mut j = k;
            while j < intervals.len() && intervals[j].0 <= bhi {
                let (lo, hi, cls) = intervals[j];
                if cls != 0 {
                    let a = lo.max(blo) - blo;
                    let z = hi.min(bhi) - blo;
                    block[a as usize..=z as usize].fill(cls);
                }
                j += 1;
            }
            if b == 0 {
                ascii.copy_from_slice(&block[..128]);
            }
            let id = *block_ids.entry(block).or_insert_with(|| {
                blocks.push(block);
                (blocks.len() - 1) as u16
            });
            index.push(id);
        }
        let set_classes: Vec<[u64; 4]> = (0..sets.len())
            .map(|i| {
                let mut bits = [0u64; 4];
                for (c, sig) in class_sets.iter().enumerate() {
                    if sig[i / 64] & (1 << (i % 64)) != 0 {
                        bits[c / 64] |= 1 << (c % 64);
                    }
                }
                bits
            })
            .collect();

        // 2. NFA.
        let mut nfa = Nfa {
            eps: Vec::new(),
            edges: Vec::new(),
            accept: Vec::new(),
        };
        let start = nfa.state();
        for (i, r) in rules.iter().enumerate() {
            let (a, b) = nfa.build(r, &set_ids);
            nfa.eps[start as usize].push(a);
            let acc = nfa.state();
            nfa.eps[b as usize].push(acc);
            nfa.accept[acc as usize] = Some(i as u16);
        }
        let n = nfa.eps.len();
        let important: Vec<bool> = (0..n)
            .map(|s| !nfa.edges[s].is_empty() || nfa.accept[s].is_some())
            .collect();
        let mut seen = vec![false; n];
        let mut closures = Vec::with_capacity(n);
        for s in 0..n as u32 {
            let mut v = vec![s];
            seen[s as usize] = true;
            nfa.closure(&mut v, &mut seen);
            v.retain(|&x| important[x as usize]);
            closures.push(v.into_boxed_slice());
        }
        let start_set = closures[start as usize].clone();
        Prog {
            classes: ClassMap {
                ascii,
                index,
                blocks,
            },
            ncls,
            nlex,
            active: active.to_vec(),
            edges: nfa.edges,
            set_classes,
            accept: nfa.accept,
            closures,
            start: start_set,
        }
    }

    /// The NFA states reachable from `cur` on class `c` (sorted, deduplicated) into `out`.
    fn step_set(&self, cur: &[u32], c: usize, out: &mut Vec<u32>) {
        out.clear();
        let (w, bit) = (c / 64, 1u64 << (c % 64));
        for &s in cur {
            for &(set, target) in &self.edges[s as usize] {
                if self.set_classes[set as usize][w] & bit != 0 {
                    out.extend_from_slice(&self.closures[target as usize]);
                }
            }
        }
        out.sort_unstable();
        out.dedup();
    }

    /// The rule a set of NFA states accepts in lexical state `lex` (0 = none, else rule + 1).
    fn accept_of(&self, set: &[u32], lex: usize) -> u16 {
        set.iter()
            .filter_map(|&s| self.accept[s as usize])
            .filter(|&r| self.active[r as usize] & (1 << lex) != 0)
            .min()
            .map_or(0, |r| r + 1)
    }
}

impl Dfa {
    /// A scanner with several lexical states (JFlex's `%state`): `active[r]` is the bitmask of
    /// lexical states rule `r` is active in, and [`Dfa::longest_match_in`] only reports the rules
    /// of the given lexical state. (For grammars whose states share few rules, separate
    /// automata are smaller.)
    pub(crate) fn with_states(rules: &[Re], active: &[u32]) -> Dfa {
        Dfa::build(&Prog::new(rules, active), usize::MAX).expect("unbounded DFA construction")
    }

    /// Compiles `rules`; on equal-length matches the earlier rule wins.
    pub(crate) fn new(rules: &[Re]) -> Dfa {
        Dfa::try_new(rules, usize::MAX).expect("unbounded DFA construction")
    }

    /// Like [`Dfa::new`], giving up (None) past `max_states` DFA states.
    pub(crate) fn try_new(rules: &[Re], max_states: usize) -> Option<Dfa> {
        Dfa::build(&Prog::new(rules, &vec![1; rules.len()]), max_states)
    }

    fn build(prog: &Prog, max_states: usize) -> Option<Dfa> {
        let (ncls, nlex) = (prog.ncls, prog.nlex);
        let mut dstates: Vec<Box<[u32]>> = vec![Box::new([]), prog.start.clone()];
        let mut ids: HashMap<Box<[u32]>, u32, foldhash::fast::RandomState> =
            HashMap::with_hasher(Default::default());
        ids.insert(Box::new([]), DEAD);
        ids.insert(prog.start.clone(), START);
        let mut trans: Vec<u32> = vec![0; 2 * ncls];
        let mut accept: Vec<u16> = vec![0; 2 * nlex];
        // Successor sets of the current DFA state, one per class, filled in one pass over its
        // NFA states' edges (rather than one pass per class).
        let mut buckets: Vec<Vec<u32>> = vec![Vec::new(); ncls];
        let mut d = START as usize;
        while d < dstates.len() {
            let cur = std::mem::take(&mut dstates[d]);
            for lex in 0..nlex {
                accept[d * nlex + lex] = prog.accept_of(&cur, lex);
            }
            for b in buckets.iter_mut() {
                b.clear();
            }
            for &st in cur.iter() {
                for &(set, target) in &prog.edges[st as usize] {
                    let closure = &prog.closures[target as usize];
                    for (w, &word) in prog.set_classes[set as usize].iter().enumerate() {
                        let mut bits = word;
                        while bits != 0 {
                            let c = w * 64 + bits.trailing_zeros() as usize;
                            bits &= bits - 1;
                            buckets[c].extend_from_slice(closure);
                        }
                    }
                }
            }
            for (c, next) in buckets.iter_mut().enumerate() {
                if next.is_empty() {
                    continue;
                }
                next.sort_unstable();
                next.dedup();
                let id = match ids.get(next.as_slice()) {
                    Some(&id) => id,
                    None => {
                        if dstates.len() >= max_states {
                            return None;
                        }
                        let id = dstates.len() as u32;
                        let key: Box<[u32]> = next.as_slice().into();
                        ids.insert(key.clone(), id);
                        dstates.push(key);
                        trans.extend(std::iter::repeat_n(0, ncls));
                        accept.extend(std::iter::repeat_n(0, nlex));
                        id
                    }
                };
                trans[d * ncls + c] = id;
            }
            dstates[d] = cur;
            d += 1;
        }
        drop(ids);
        drop(dstates);
        let before = accept.len() / nlex;
        let (trans, accept) = minimize(&trans, &accept, ncls, nlex);
        if std::env::var_os("ROLL_DFA_STATS").is_some() {
            eprintln!(
                "dfa: {ncls} classes, {} nfa states, {before} dfa states, {} minimized",
                prog.edges.len(),
                accept.len() / nlex
            );
        }
        let classes = ClassMap {
            ascii: prog.classes.ascii,
            index: prog.classes.index.clone(),
            blocks: prog.classes.blocks.clone(),
        };
        Some(Dfa {
            classes,
            ncls,
            nlex,
            trans,
            accept,
        })
    }

    #[inline]
    pub(crate) fn step(&self, state: u32, c: char) -> u32 {
        let cls = self.classes.get(c as u32);
        self.trans[state as usize * self.ncls + cls]
    }

    /// The rule a state accepts (in lexical state 0), if any.
    #[inline]
    pub(crate) fn accepts(&self, state: u32) -> Option<usize> {
        self.accepts_in(0, state)
    }

    /// The rule a state accepts in lexical state `lex`, if any.
    #[inline]
    pub(crate) fn accepts_in(&self, lex: usize, state: u32) -> Option<usize> {
        match self.accept[state as usize * self.nlex + lex] {
            0 => None,
            r => Some(r as usize - 1),
        }
    }

    /// Longest match starting at byte `start` of `text`, reading at most `limit` UTF-16 code units
    /// (a code point that would cross the limit is not read). Returns (rule, end byte, UTF-16
    /// length).
    #[inline]
    pub(crate) fn longest_match(
        &self,
        text: &str,
        start: usize,
        limit: usize,
    ) -> Option<(usize, usize, usize)> {
        self.longest_match_in(0, text, start, limit)
    }

    /// [`Dfa::longest_match`] in lexical state `lex`.
    #[inline]
    pub(crate) fn longest_match_in(
        &self,
        lex: usize,
        text: &str,
        start: usize,
        limit: usize,
    ) -> Option<(usize, usize, usize)> {
        let bytes = text.as_bytes();
        let mut state = START;
        let mut best = None;
        let mut pos = start;
        let mut units = 0usize;
        while pos < bytes.len() {
            let b = bytes[pos];
            let (c, len, ulen) = if b < 0x80 {
                (b as char, 1, 1)
            } else {
                // Decode one scalar value; `text` is valid UTF-8.
                let c = text[pos..].chars().next().unwrap_or('\u{FFFD}');
                (c, c.len_utf8(), c.len_utf16())
            };
            if units + ulen > limit {
                break;
            }
            state = self.step(state, c);
            if state == DEAD {
                break;
            }
            pos += len;
            units += ulen;
            if let Some(rule) = self.accepts_in(lex, state) {
                best = Some((rule, pos, units));
            }
        }
        best
    }

    #[cfg(test)]
    pub(crate) fn states(&self) -> usize {
        self.accept.len() / self.nlex
    }
}

/// A DFA determinized on demand: states are built the first time input reaches them and cached
/// in the instance. For grammars whose full DFA is large (`html_strip`'s tag grammar has tens of
/// thousands of states) but whose inputs only ever visit a few hundred. Each instance belongs
/// to one chain, so the cache needs no locking; it is reset if it grows past a bound.
pub(crate) struct LazyDfa {
    prog: std::sync::Arc<Prog>,
    sets: Vec<Box<[u32]>>,
    ids: HashMap<Box<[u32]>, u32, foldhash::fast::RandomState>,
    /// `trans[state * ncls + class]`; [`UNKNOWN`] until computed.
    trans: Vec<u32>,
    /// `accept[state * nlex + lex]` as in [`Dfa`].
    accept: Vec<u16>,
    scratch: Vec<u32>,
}

const UNKNOWN: u32 = u32::MAX;
/// States kept before the cache is cleared.
const LAZY_MAX_STATES: usize = 20_000;

impl LazyDfa {
    pub(crate) fn new(prog: std::sync::Arc<Prog>) -> LazyDfa {
        let mut d = LazyDfa {
            prog,
            sets: Vec::new(),
            ids: HashMap::with_hasher(Default::default()),
            trans: Vec::new(),
            accept: Vec::new(),
            scratch: Vec::new(),
        };
        d.clear();
        d
    }

    fn clear(&mut self) {
        self.sets.clear();
        self.ids.clear();
        self.trans.clear();
        self.accept.clear();
        let start = self.prog.start.clone();
        self.add(Box::new([]));
        self.add(start);
    }

    fn add(&mut self, set: Box<[u32]>) -> u32 {
        let id = self.sets.len() as u32;
        for lex in 0..self.prog.nlex {
            let a = self.prog.accept_of(&set, lex);
            self.accept.push(a);
        }
        self.trans.extend(std::iter::repeat_n(
            if id == DEAD { DEAD } else { UNKNOWN },
            self.prog.ncls,
        ));
        self.ids.insert(set.clone(), id);
        self.sets.push(set);
        id
    }

    #[inline]
    fn step(&mut self, state: u32, c: char) -> u32 {
        let cls = self.prog.classes.get(c as u32);
        let t = self.trans[state as usize * self.prog.ncls + cls];
        if t != UNKNOWN {
            return t;
        }
        self.compute(state, cls)
    }

    #[cold]
    fn compute(&mut self, state: u32, cls: usize) -> u32 {
        let mut next = std::mem::take(&mut self.scratch);
        self.prog
            .step_set(&self.sets[state as usize], cls, &mut next);
        let id = match self.ids.get(next.as_slice()) {
            Some(&id) => id,
            None if next.is_empty() => DEAD,
            None => self.add(next.as_slice().into()),
        };
        self.trans[state as usize * self.prog.ncls + cls] = id;
        self.scratch = next;
        id
    }

    /// [`Dfa::longest_match_in`].
    pub(crate) fn longest_match_in(
        &mut self,
        lex: usize,
        text: &str,
        start: usize,
        limit: usize,
    ) -> Option<(usize, usize, usize)> {
        if self.sets.len() > LAZY_MAX_STATES {
            self.clear();
        }
        let nlex = self.prog.nlex;
        let mut state = START;
        let mut best = None;
        let mut pos = start;
        let mut units = 0usize;
        let bytes = text.as_bytes();
        while pos < bytes.len() {
            let b = bytes[pos];
            let (c, len, ulen) = if b < 0x80 {
                (b as char, 1, 1)
            } else {
                let c = text[pos..].chars().next().unwrap_or('\u{FFFD}');
                (c, c.len_utf8(), c.len_utf16())
            };
            if units + ulen > limit {
                break;
            }
            state = self.step(state, c);
            if state == DEAD {
                break;
            }
            pos += len;
            units += ulen;
            match self.accept[state as usize * nlex + lex] {
                0 => {}
                r => best = Some((r as usize - 1, pos, units)),
            }
        }
        best
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn set_ops() {
        let a = ranges(vec![(10, 20), (30, 40)]);
        let b = ranges(vec![(15, 35)]);
        assert_eq!(intersect(&a, &b), vec![(15, 20), (30, 35)]);
        assert_eq!(difference(&a, &b), vec![(10, 14), (36, 40)]);
        assert_eq!(union(&[&a, &b]), vec![(10, 40)]);
        assert_eq!(negate(&[(0, 5)]), vec![(6, MAX_CP)]);
        assert_eq!(ranges(vec![(5, 6), (1, 2), (3, 4)]), vec![(1, 6)]);
        assert!(contains(&a, 30) && !contains(&a, 25));
    }

    #[test]
    fn longest_match_and_priority() {
        let digit = chars("0123456789");
        let letter = ranges(vec![('a' as u32, 'z' as u32)]);
        let dfa = Dfa::new(&[
            plus(set(&digit)),                                         // 0: number
            plus(alt(vec![set(&letter), set(&digit)])),                // 1: word
            lit("if"), // 2: never wins against 1 (same length, later)
            seq(vec![plus(set(&digit)), lit("."), plus(set(&digit))]), // 3: decimal
        ]);
        assert_eq!(dfa.longest_match("123 ", 0, 255), Some((0, 3, 3)));
        assert_eq!(dfa.longest_match("12ab", 0, 255), Some((1, 4, 4)));
        assert_eq!(dfa.longest_match("if", 0, 255), Some((1, 2, 2)));
        assert_eq!(dfa.longest_match("1.5x", 0, 255), Some((3, 3, 3)));
        // Backs off to the last accepting position.
        assert_eq!(dfa.longest_match("1.x", 0, 255), Some((0, 1, 1)));
        assert_eq!(dfa.longest_match("12345", 0, 3), Some((0, 3, 3)));
        assert_eq!(dfa.longest_match("!", 0, 255), None);
        assert!(dfa.states() > 2);
    }

    #[test]
    fn repeat_bounds() {
        let a = chars("a");
        let dfa = Dfa::new(&[repeat(set(&a), 2, Some(3))]);
        assert_eq!(dfa.longest_match("a", 0, 99), None);
        assert_eq!(dfa.longest_match("aa", 0, 99), Some((0, 2, 2)));
        assert_eq!(dfa.longest_match("aaaa", 0, 99), Some((0, 3, 3)));
        let dfa = Dfa::new(&[repeat(set(&a), 2, None)]);
        assert_eq!(dfa.longest_match("aaaaa", 0, 99), Some((0, 5, 5)));
    }

    #[test]
    fn lazy_matches_eager() {
        let digit = chars("0123456789");
        let letter = ranges(vec![('a' as u32, 'z' as u32)]);
        let rules = [
            plus(set(&digit)),
            plus(alt(vec![set(&letter), set(&digit)])),
            seq(vec![plus(set(&digit)), lit("."), plus(set(&digit))]),
        ];
        let dfa = Dfa::new(&rules);
        let mut lazy = LazyDfa::new(std::sync::Arc::new(Prog::new(&rules, &[1, 1, 1])));
        for text in ["123 ", "12ab", "1.5x", "1.x", "!", "x9.9", "😀1"] {
            for start in 0..text.len() {
                if text.is_char_boundary(start) {
                    assert_eq!(
                        lazy.longest_match_in(0, text, start, 99),
                        dfa.longest_match(text, start, 99),
                        "{text:?} at {start}"
                    );
                }
            }
        }
    }

    #[test]
    fn non_ascii_and_surrogate_limit() {
        let any = ranges(vec![(0, MAX_CP)]);
        let dfa = Dfa::new(&[plus(set(&any))]);
        // 😀 is two UTF-16 units: with a limit of 3 only "a😀" fits.
        assert_eq!(dfa.longest_match("a😀😀", 0, 3), Some((0, 5, 3)));
        assert_eq!(dfa.longest_match("a😀😀", 0, 4), Some((0, 5, 3)));
        assert_eq!(dfa.longest_match("a😀😀", 0, 5), Some((0, 9, 5)));
    }
}
