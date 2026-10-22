//! Suggest: weighted completion entries of a field (Elasticsearch's `completion` field, Lucene's
//! `NRTSuggester`), searchable by "keys that start with something an automaton accepts", best
//! weight first.
//!
//! # What is stored
//!
//! Each entry is `(key, weight, doc, payload)`: the key is the analyzed input (with whatever
//! context prefix and separators the indexer encodes), the weight the suggestion's weight, the
//! payload opaque bytes for the caller (the surface form). Entries are sorted by key (then
//! weight descending, doc, payload), so all keys extending a prefix form one contiguous run.
//!
//! ```text
//! u8 version | keys: front-coded blocks of 16 (a TermDict that allows repeats)
//! weights: packed ints | docs: packed ints | payload ends: monotonic + payload bytes
//! max tree: u32 P, then 2P i64 maxima in heap order over blocks of 16 entries (-1 = empty)
//! ```
//!
//! # Completion
//!
//! [`SuggestReader::complete`] works in two steps, like Lucene's `FSTUtil.intersectPrefixPaths`
//! followed by `TopNSearcher`:
//!
//! 1. *Ranges*: walk the sorted keys with the automaton, reusing the states of the bytes shared
//!    with the previous key. At the first depth where the automaton accepts, every key with that
//!    prefix matches (completion is prefix matching), so the run is recorded with the accepted
//!    prefix length and the walk jumps past it; where the automaton dies it jumps to the next
//!    string that could still match. Each key is matched by its shortest accepted prefix, as in
//!    Lucene, and the prefix length is reported (fuzzy completion boosts by it).
//! 2. *Best first*: the runs are decomposed into nodes of a max tree over blocks of 16 entries
//!    plus loose entries at their ends, and a heap ordered by (weight descending, position)
//!    expands nodes lazily. Each result costs O(log n), results come out exactly in descending
//!    weight order (ties in key order), and the caller pulls only as many as it needs (deleted
//!    docs, duplicates and context boosts are the caller's business).

use std::cmp::Ordering;
use std::collections::BinaryHeap;

use crate::automaton::{Automaton, literal_prefix, next_candidate};
use crate::docvalues::{TermDict, TermSeq, write_term_list};
use crate::error::{Result, corrupt};
use crate::io::{In, Out};
use crate::packed::{MonotonicValues, NumericValues, write_monotonic_iter, write_numeric_with};
use crate::slice::Slice;

const VERSION: u8 = 1;
/// Entries per leaf of the max tree (the key dictionary's block size).
const LEAF: u64 = 16;

/// One suggestion as a source hands it to the writer.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SuggestEntry<'a> {
    pub key: &'a [u8],
    /// At most `i64::MAX`.
    pub weight: u64,
    pub doc: u32,
    pub payload: &'a [u8],
}

/// The suggest entries of one field, in any order.
pub trait SuggestSource {
    fn next(&mut self) -> Option<SuggestEntry<'_>>;
}

/// Sorted entries handed to [`write_suggest`] by index.
pub(crate) trait SortedEntries {
    fn len(&self) -> u64;
    fn key(&self, i: u64) -> &[u8];
    fn weight(&self, i: u64) -> u64;
    fn doc(&self, i: u64) -> u32;
    fn payload(&self, i: u64) -> &[u8];
}

struct Keys<'a>(&'a dyn SortedEntries);

impl TermSeq for Keys<'_> {
    fn count(&self) -> u64 {
        self.0.len()
    }
    fn term(&mut self, ord: u64) -> &[u8] {
        self.0.key(ord)
    }
}

/// Writes a suggest section from entries sorted by key.
pub(crate) fn write_suggest<O: Out + ?Sized>(e: &dyn SortedEntries, out: &mut O) {
    let n = e.len();
    out.put_u8(VERSION);
    // Sorted input: the key list cannot fail.
    let _ = write_term_list(&mut Keys(e), false, out);
    write_numeric_with(n as usize, |i| e.weight(i as u64) as i64, out);
    write_numeric_with(n as usize, |i| e.doc(i as u64) as i64, out);
    let ends = || {
        std::iter::once(0u64).chain((0..n).scan(0u64, |at, i| {
            *at += e.payload(i).len() as u64;
            Some(*at)
        }))
    };
    write_monotonic_iter(n as usize + 1, ends, out);
    out.put_vu64(ends().last().unwrap_or(0));
    for i in 0..n {
        out.put_bytes(e.payload(i));
    }
    // Max tree over blocks.
    let blocks = n.div_ceil(LEAF);
    let p = blocks.next_power_of_two().max(1);
    let mut tree = vec![-1i64; 2 * p as usize];
    for b in 0..blocks {
        let m = (b * LEAF..((b + 1) * LEAF).min(n)).map(|i| e.weight(i) as i64).max().unwrap_or(-1);
        tree[(p + b) as usize] = m;
    }
    for node in (1..p as usize).rev() {
        tree[node] = tree[2 * node].max(tree[2 * node + 1]);
    }
    out.put_u32(p as u32);
    for v in &tree {
        out.put_i64(*v);
    }
}

/// The suggest entries of one field. Cheap to clone; `Send + Sync`.
#[derive(Clone, Debug)]
pub struct SuggestReader {
    keys: TermDict,
    weights: NumericValues,
    docs: NumericValues,
    payload_ends: MonotonicValues,
    payloads: Slice,
    /// Leaves of the max tree (a power of two) and the tree.
    p: u64,
    tree: Slice,
}

/// A run of matching entries `[start, end)` whose keys share an accepted prefix of
/// `prefix_len` bytes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SuggestRange {
    pub start: u64,
    pub end: u64,
    pub prefix_len: u32,
}

/// One completion: the entry's index (for [`SuggestReader::key`] and
/// [`SuggestReader::payload`]), weight and doc, and the length of the key prefix the automaton
/// accepted.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Completion {
    pub index: u64,
    pub weight: u64,
    pub doc: u32,
    pub prefix_len: u32,
}

impl SuggestReader {
    pub(crate) fn open(data: Slice) -> Result<SuggestReader> {
        let mut pos = 0usize;
        let v = *data.first().ok_or_else(|| corrupt("empty suggest section"))?;
        if v != VERSION {
            return Err(corrupt(format!("unknown suggest version {v}")));
        }
        pos += 1;
        let keys = TermDict::read(&data, &mut pos)?;
        let weights = NumericValues::read(&data, &mut pos)?;
        let docs = NumericValues::read(&data, &mut pos)?;
        let payload_ends = MonotonicValues::read(&data, &mut pos)?;
        let n = keys.len();
        if weights.len() != n || docs.len() != n || payload_ends.len() != n + 1 {
            return Err(corrupt("suggest entry counts differ"));
        }
        let mut r = In::at(&data, pos);
        let total = r.vu64()?;
        let payloads = data.try_slice(r.pos() as u64, total).ok_or_else(|| corrupt("suggest payloads truncated"))?;
        if payload_ends.get(n) != total {
            return Err(corrupt("suggest payload ends mismatch"));
        }
        r.skip(total as usize)?;
        let p = r.u32()? as u64;
        let blocks = n.div_ceil(LEAF);
        if !p.is_power_of_two() || p < blocks || p > blocks.max(1) * 2 {
            return Err(corrupt("bad suggest tree size"));
        }
        let tree = data.try_slice(r.pos() as u64, 16 * p).ok_or_else(|| corrupt("suggest tree truncated"))?;
        Ok(SuggestReader { keys, weights, docs, payload_ends, payloads, p, tree })
    }

    /// Number of entries.
    pub fn len(&self) -> u64 {
        self.keys.len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// The keys, in entry order.
    pub(crate) fn keys(&self) -> &TermDict {
        &self.keys
    }

    /// The key of entry `index` into `out`.
    pub fn key(&self, index: u64, out: &mut Vec<u8>) {
        self.keys.term(index, out)
    }

    pub fn weight(&self, index: u64) -> u64 {
        self.weights.get(index) as u64
    }

    pub fn doc(&self, index: u64) -> u32 {
        self.docs.get(index) as u32
    }

    /// The payload of entry `index` (empty when out of range).
    pub fn payload(&self, index: u64) -> &[u8] {
        if index >= self.len() {
            return &[];
        }
        let (s, e) = (self.payload_ends.get(index) as usize, self.payload_ends.get(index + 1) as usize);
        self.payloads.get(s..e.max(s)).unwrap_or(&[])
    }

    /// The largest weight of any entry (`None` when empty).
    pub fn max_weight(&self) -> Option<u64> {
        let m = self.node(1);
        (m >= 0).then_some(m as u64)
    }

    #[inline]
    fn node(&self, i: u64) -> i64 {
        let at = i as usize * 8;
        self.tree.get(at..at + 8).map_or(-1, |b| i64::from_le_bytes(b.try_into().unwrap()))
    }

    /// The runs of entries whose keys start with a prefix `aut` accepts (each key matched by
    /// its shortest accepted prefix), in key order.
    pub fn ranges(&self, aut: &dyn Automaton) -> Vec<SuggestRange> {
        let n = self.len();
        let mut out = Vec::new();
        let start = aut.start();
        if n == 0 || !aut.can_match(start) {
            return out;
        }
        if aut.is_match(start) {
            out.push(SuggestRange { start: 0, end: n, prefix_len: 0 });
            return out;
        }
        let lit = literal_prefix(aut);
        let mut it = self.keys.iter_from(self.keys.lower_bound(&lit));
        let mut states = vec![start];
        let mut skip = Vec::new();
        let mut dead_runs = 0u32;
        while it.next().is_some() {
            states.truncate(it.shared().min(states.len() - 1) + 1);
            let key = it.term();
            let mut dead = None;
            let mut accepted = None;
            loop {
                let depth = states.len() - 1;
                let s = states[depth];
                if !aut.can_match(s) {
                    dead = Some(depth);
                    break;
                }
                if aut.is_match(s) {
                    accepted = Some(depth);
                    break;
                }
                if depth == key.len() {
                    break;
                }
                states.push(aut.accept(s, key[depth]));
            }
            if let Some(k) = accepted {
                dead_runs = 0;
                let ord = it.ord();
                let end = match successor(&key[..k]) {
                    Some(next) => self.keys.lower_bound(&next).max(ord + 1),
                    None => n,
                };
                out.push(SuggestRange { start: ord, end, prefix_len: k as u32 });
                if end >= n {
                    break;
                }
                it = self.keys.iter_from(end);
                states.truncate(1);
                continue;
            }
            let Some(dead) = dead else { continue };
            if dead == 0 {
                break;
            }
            let cur = it.ord();
            dead_runs += 1;
            if dead_runs >= 4 {
                dead_runs = 0;
                if !next_candidate(aut, &states[..dead], key, dead - 1, &mut skip) {
                    break;
                }
                let ord = self.keys.lower_bound(&skip);
                if ord > cur + 1 {
                    it = self.keys.iter_from(ord);
                    states.truncate(1);
                }
            }
        }
        out
    }

    /// Entries whose keys start with a prefix `aut` accepts, highest weight first (ties in key
    /// order).
    pub fn complete(&self, aut: &dyn Automaton) -> Completions<'_> {
        self.complete_ranges(self.ranges(aut))
    }

    /// Best-first iteration over given runs (for callers that build or filter them).
    pub fn complete_ranges(&self, ranges: Vec<SuggestRange>) -> Completions<'_> {
        let mut c = Completions { reader: self, heap: BinaryHeap::new(), ranges };
        for r in 0..c.ranges.len() {
            let SuggestRange { start, end, .. } = c.ranges[r];
            let end = end.min(self.len());
            if start >= end {
                continue;
            }
            // Loose entries before the first and after the last whole block; whole blocks as
            // tree nodes (the classic bottom-up segment tree decomposition).
            let first_block = start.div_ceil(LEAF);
            let last_block = end / LEAF;
            if first_block >= last_block {
                for i in start..end {
                    c.push_entry(i, r as u32);
                }
                continue;
            }
            for i in start..first_block * LEAF {
                c.push_entry(i, r as u32);
            }
            for i in last_block * LEAF..end {
                c.push_entry(i, r as u32);
            }
            let (mut lo, mut hi) = (first_block + self.p, last_block + self.p);
            while lo < hi {
                if lo & 1 == 1 {
                    c.push_node(lo, r as u32);
                    lo += 1;
                }
                if hi & 1 == 1 {
                    hi -= 1;
                    c.push_node(hi, r as u32);
                }
                lo >>= 1;
                hi >>= 1;
            }
        }
        c
    }
}

/// The smallest string greater than every string starting with `prefix` (`None` if there is
/// none: the prefix is all `0xFF`).
fn successor(prefix: &[u8]) -> Option<Vec<u8>> {
    let mut v = prefix.to_vec();
    while let Some(&last) = v.last() {
        if last < 0xFF {
            *v.last_mut().unwrap() += 1;
            return Some(v);
        }
        v.pop();
    }
    None
}

#[derive(Clone, Copy, PartialEq, Eq)]
struct Item {
    weight: i64,
    /// First entry the item can produce (ties go to the earlier key).
    first: u64,
    /// Tree node (`>= 1`) or 0 for a single entry at `first`.
    node: u64,
    range: u32,
}

impl Ord for Item {
    fn cmp(&self, o: &Item) -> Ordering {
        self.weight
            .cmp(&o.weight)
            .then_with(|| o.first.cmp(&self.first))
            .then_with(|| (o.node != 0).cmp(&(self.node != 0)))
    }
}

impl PartialOrd for Item {
    fn partial_cmp(&self, o: &Item) -> Option<Ordering> {
        Some(self.cmp(o))
    }
}

/// Iterator returned by [`SuggestReader::complete`].
pub struct Completions<'a> {
    reader: &'a SuggestReader,
    heap: BinaryHeap<Item>,
    ranges: Vec<SuggestRange>,
}

impl Completions<'_> {
    fn push_entry(&mut self, i: u64, range: u32) {
        let w = self.reader.weight(i) as i64;
        self.heap.push(Item { weight: w, first: i, node: 0, range });
    }

    fn push_node(&mut self, node: u64, range: u32) {
        let w = self.reader.node(node);
        if w < 0 {
            return;
        }
        // The first entry under the node.
        let mut first = node;
        while first < self.reader.p {
            first *= 2;
        }
        self.heap.push(Item { weight: w, first: (first - self.reader.p) * LEAF, node, range });
    }

    /// The next completion in descending weight order.
    #[allow(clippy::should_implement_trait)]
    pub fn next(&mut self) -> Option<Completion> {
        while let Some(it) = self.heap.pop() {
            if it.node == 0 {
                let r = self.reader;
                return Some(Completion {
                    index: it.first,
                    weight: it.weight as u64,
                    doc: r.doc(it.first),
                    prefix_len: self.ranges[it.range as usize].prefix_len,
                });
            }
            if it.node >= self.reader.p {
                let b = it.node - self.reader.p;
                for i in b * LEAF..((b + 1) * LEAF).min(self.reader.len()) {
                    self.push_entry(i, it.range);
                }
            } else {
                self.push_node(2 * it.node, it.range);
                self.push_node(2 * it.node + 1, it.range);
            }
        }
        None
    }

    /// An upper bound of the weight of every completion still to come (`None` when done):
    /// callers that scale weights by a per-range boost stop once it cannot beat their top N.
    pub fn max_remaining_weight(&self) -> Option<u64> {
        self.heap.peek().map(|i| i.weight as u64)
    }

    /// The matched runs.
    pub fn ranges(&self) -> &[SuggestRange] {
        &self.ranges
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::automaton::{AlwaysMatch, Dfa, Prefix, regexp};
    use rand::{Rng, SeedableRng, rngs::StdRng};

    type Entry = (Vec<u8>, u64, u32, Vec<u8>);

    struct Vecs(Vec<Entry>);

    impl SortedEntries for Vecs {
        fn len(&self) -> u64 {
            self.0.len() as u64
        }
        fn key(&self, i: u64) -> &[u8] {
            &self.0[i as usize].0
        }
        fn weight(&self, i: u64) -> u64 {
            self.0[i as usize].1
        }
        fn doc(&self, i: u64) -> u32 {
            self.0[i as usize].2
        }
        fn payload(&self, i: u64) -> &[u8] {
            &self.0[i as usize].3
        }
    }

    fn build(mut entries: Vec<Entry>) -> (SuggestReader, Vec<Entry>) {
        entries.sort_by(|a, b| a.0.cmp(&b.0).then(b.1.cmp(&a.1)).then(a.2.cmp(&b.2)).then(a.3.cmp(&b.3)));
        let mut out = Vec::new();
        let v = Vecs(entries);
        write_suggest(&v, &mut out);
        (SuggestReader::open(Slice::from(out)).unwrap(), v.0)
    }

    /// Shortest prefix of `key` the automaton accepts.
    fn accepted_prefix(aut: &dyn Automaton, key: &[u8]) -> Option<usize> {
        let mut s = aut.start();
        for d in 0..=key.len() {
            if !aut.can_match(s) {
                return None;
            }
            if aut.is_match(s) {
                return Some(d);
            }
            if d < key.len() {
                s = aut.accept(s, key[d]);
            }
        }
        None
    }

    fn check(r: &SuggestReader, entries: &[Entry], aut: &dyn Automaton) {
        let mut want: Vec<(u64, u64, u32, u32)> = entries
            .iter()
            .enumerate()
            .filter_map(|(i, e)| accepted_prefix(aut, &e.0).map(|k| (i as u64, e.1, e.2, k as u32)))
            .collect();
        want.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0)));
        let mut c = r.complete(aut);
        let mut got = Vec::new();
        let mut last = u64::MAX;
        while let Some(x) = c.next() {
            assert!(x.weight <= last);
            last = x.weight;
            assert!(c.max_remaining_weight().is_none_or(|m| m <= x.weight));
            got.push((x.index, x.weight, x.doc, x.prefix_len));
        }
        assert_eq!(got, want);
        let mut key = Vec::new();
        for &(i, ..) in got.iter().take(20) {
            r.key(i, &mut key);
            assert_eq!(key, entries[i as usize].0);
            assert_eq!(r.payload(i), &entries[i as usize].3[..]);
        }
    }

    #[test]
    fn completions_match_brute_force() {
        let mut rng = StdRng::seed_from_u64(7);
        let alphabet = [b'a', b'b', b'c', b' ', 0xC3, 0xA9, 0x1F];
        for n in [0usize, 1, 15, 16, 17, 33, 300, 5000] {
            let entries: Vec<_> = (0..n)
                .map(|i| {
                    let key: Vec<u8> = (0..rng.gen_range(0..8)).map(|_| alphabet[rng.gen_range(0..alphabet.len())]).collect();
                    let weight = if rng.gen_bool(0.3) { 5 } else { rng.gen_range(0..1000) };
                    (key, weight, i as u32, format!("surface {i}").into_bytes())
                })
                .collect();
            let (r, entries) = build(entries);
            assert_eq!(r.len() as usize, n);
            assert_eq!(r.max_weight(), entries.iter().map(|e| e.1).max());
            let auts: Vec<Box<dyn Automaton>> = vec![
                Box::new(AlwaysMatch),
                Box::new(Prefix::new(&b""[..])),
                Box::new(Prefix::new(&b"a"[..])),
                Box::new(Prefix::new(&b"ab"[..])),
                Box::new(Prefix::new(&b"cc c"[..])),
                Box::new(Prefix::new(&[0xFFu8][..])),
                Box::new(Dfa::levenshtein("abc", 1, 0, true)),
                Box::new(Dfa::levenshtein("ba", 2, 1, false)),
                Box::new(regexp::compile("[ab]c", regexp::ALL, false, 10_000).unwrap()),
                Box::new(regexp::compile("é|c", regexp::ALL, false, 10_000).unwrap()),
            ];
            for aut in &auts {
                check(&r, &entries, aut.as_ref());
            }
        }
    }

    /// `cargo test --release -p search-codec --lib bench_suggest -- --ignored --nocapture`
    #[test]
    #[ignore]
    fn bench_suggest() {
        let mut rng = StdRng::seed_from_u64(3);
        let words = ["apple", "banana", "cherry", "date", "elder", "fig", "grape", "honeydew", "kiwi", "lemon"];
        let n = 1_000_000;
        let entries: Vec<_> = (0..n)
            .map(|i| {
                let k = format!("{} {} {}", words[rng.gen_range(0..10)], words[rng.gen_range(0..10)], rng.gen_range(0..100_000));
                (k.into_bytes(), rng.gen_range(0..1_000_000u64), i as u32, Vec::new())
            })
            .collect();
        let t = std::time::Instant::now();
        let (r, _) = build(entries);
        println!("build {n}: {:?}", t.elapsed());
        for (name, aut) in [
            ("prefix 'b'", Box::new(Prefix::new("b")) as Box<dyn Automaton>),
            ("prefix 'cherry fig 1'", Box::new(Prefix::new("cherry fig 1"))),
            ("fuzzy 'cherri' ~1", Box::new(Dfa::levenshtein("cherri", 1, 1, true))),
        ] {
            let rounds = 200;
            let t = std::time::Instant::now();
            let mut sum = 0u64;
            for _ in 0..rounds {
                let mut c = r.complete(aut.as_ref());
                for _ in 0..10 {
                    sum += c.next().map_or(0, |x| x.weight);
                }
            }
            println!("{name}: top 10 in {:?} ({sum})", t.elapsed() / rounds);
        }
    }

    #[test]
    fn huge_weights_and_corruption() {
        let entries = vec![(b"x".to_vec(), i64::MAX as u64, 3, Vec::new()), (b"x".to_vec(), 0, 1, b"p".to_vec())];
        let (r, _) = build(entries);
        let mut c = r.complete(&Prefix::new("x"));
        assert_eq!(c.next().map(|x| (x.weight, x.doc)), Some((i64::MAX as u64, 3)));
        assert_eq!(c.next().map(|x| (x.weight, x.doc)), Some((0, 1)));
        assert_eq!(c.next(), None);
        assert_eq!(r.payload(1), b"p");
        assert_eq!(r.payload(9), b"");
        let mut out = Vec::new();
        write_suggest(&Vecs(vec![(b"a".to_vec(), 1, 0, b"q".to_vec())]), &mut out);
        for n in 0..out.len() {
            assert!(SuggestReader::open(Slice::from(out[..n].to_vec())).is_err(), "truncated at {n}");
        }
    }
}
