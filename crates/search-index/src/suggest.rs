//! Completion suggestions (Elasticsearch's `completion` suggester: Lucene's `NRTSuggester`
//! driven by `TopSuggestDocsCollector`) over the suggest entries the codec stores per field.
//!
//! The caller owns the key encoding (analyzed input, separators, context prefixes) and builds
//! an automaton over it: a prefix, fuzzy or regex automaton, optionally behind context
//! prefixes. Each entry is matched by the shortest key prefix the automaton accepts (Lucene's
//! `FSTUtil.intersectPrefixPaths`), and that prefix gives the entry's boost (`setNextMatch`:
//! the fuzzy prefix overlap, the context's boost), so the score is Lucene's
//! `CompletionScorer.score(weight, boost)`.
//!
//! Collection follows Lucene step by step, since it decides which suggestions survive:
//!
//! - per segment, candidates come best score first (ties in key order) from one codec
//!   [`Completions`] stream per distinct boost, merged; deleted docs are skipped;
//! - with `skip_duplicates`, a surface form (the payload) seen earlier in the segment is
//!   skipped, each segment fills its own queue of `size`, and the segments' results are sorted
//!   (score descending, surface form, doc) and deduplicated across segments at the end;
//! - without it, one queue of `size` spans the segments (ties: surface form, then doc);
//! - with `one_per_doc` (Elasticsearch's `TopSuggestGroupDocsCollector`), only a doc's first
//!   candidate enters the queue; later ones only add their accepted prefix (where callers
//!   read the matched contexts);
//! - a segment stops after `size` accepted candidates (Lucene's `TopNSearcher`) or at its first
//!   candidate that cannot enter the queue.
//!
//! Surface forms compare as Java strings (UTF-16 code units).

use std::cmp::Ordering;
use std::collections::{BinaryHeap, HashSet};

use search_codec::automaton::Automaton;
use search_codec::suggest::{Completions, SuggestRange, SuggestReader};

use crate::error::{Error, Result};
use crate::search::Searcher;

/// The boost of entries under an accepted key prefix.
pub type BoostFn = dyn Fn(&[u8]) -> f32 + Sync;

/// A completion request on one field.
pub struct CompletionQuery<'a> {
    pub field: u32,
    /// Accepts key prefixes; entries whose key starts with an accepted prefix match.
    pub automaton: &'a dyn Automaton,
    /// The boost of the entries under an accepted key prefix (Lucene's
    /// `CompletionWeight.setNextMatch` then `boost()`); `None`: 0 (score = weight).
    pub boost: Option<&'a BoostFn>,
    /// Suggestions to return (> 0).
    pub size: usize,
    pub skip_duplicates: bool,
    /// At most one suggestion per doc (Elasticsearch).
    pub one_per_doc: bool,
}

/// One suggestion.
#[derive(Clone, Debug, PartialEq)]
pub struct Suggestion {
    /// Global doc id.
    pub doc: u32,
    /// The entry's key (as encoded by the caller).
    pub key: Vec<u8>,
    /// The entry's payload (the surface form).
    pub payload: Vec<u8>,
    pub weight: u64,
    pub score: f32,
    /// The accepted key prefixes of the doc's collected candidates, best first (with
    /// `one_per_doc`; otherwise this suggestion's own).
    pub prefixes: Vec<Vec<u8>>,
}

/// Lucene's `CompletionScorer.score`.
#[inline]
pub fn completion_score(weight: f32, boost: f32) -> f32 {
    if boost == 0.0 {
        weight
    } else if weight == 0.0 {
        boost
    } else {
        weight * boost
    }
}

/// Lucene's `FuzzyCompletionWeight.setNextMatch` boost: the most positions at which the
/// accepted key prefix agrees with one of the query's analyzed forms.
pub fn fuzzy_prefix_boost(path: &[u8], analyzed: &[Vec<u8>]) -> f32 {
    analyzed.iter().map(|a| path.iter().zip(a).filter(|(x, y)| x == y).count()).max().unwrap_or(0) as f32
}

/// Java's `CharSequence` order (UTF-16 code units) of two payloads (raw bytes when not UTF-8).
fn surface_cmp(a: &[u8], b: &[u8]) -> Ordering {
    match (std::str::from_utf8(a), std::str::from_utf8(b)) {
        (Ok(x), Ok(y)) => x.encode_utf16().cmp(y.encode_utf16()),
        _ => a.cmp(b),
    }
}

/// A collected suggestion (segment-local doc until the end).
struct Hit {
    doc: u32,
    index: u64,
    seg: usize,
    payload: Vec<u8>,
    weight: u64,
    score: f32,
    /// The accepted key prefix.
    prefix: Vec<u8>,
}

/// Lucene's `SuggestScoreDocPriorityQueue.lessThan`: lower score, then the larger surface
/// form, then the larger doc is "less".
fn less(a: &Hit, b: &Hit) -> bool {
    if a.score == b.score {
        match surface_cmp(&a.payload, &b.payload) {
            Ordering::Equal => a.doc > b.doc,
            o => o == Ordering::Greater,
        }
    } else {
        a.score < b.score
    }
}

/// Min-queue of the best `cap` hits (worst on top), with `insertWithOverflow`.
struct Queue {
    cap: usize,
    heap: BinaryHeap<Worst>,
}

struct Worst(Hit);

impl PartialEq for Worst {
    fn eq(&self, o: &Self) -> bool {
        self.cmp(o) == Ordering::Equal
    }
}
impl Eq for Worst {}
impl PartialOrd for Worst {
    fn partial_cmp(&self, o: &Self) -> Option<Ordering> {
        Some(self.cmp(o))
    }
}
impl Ord for Worst {
    // The max-heap's top is the "least" hit.
    fn cmp(&self, o: &Self) -> Ordering {
        if less(&self.0, &o.0) {
            Ordering::Greater
        } else if less(&o.0, &self.0) {
            Ordering::Less
        } else {
            Ordering::Equal
        }
    }
}

impl Queue {
    fn new(cap: usize) -> Queue {
        Queue { cap, heap: BinaryHeap::with_capacity(cap.min(1024) + 1) }
    }

    /// False when `h` overflowed (the queue is full and `h` is worse than its worst).
    fn insert(&mut self, h: Hit) -> bool {
        if self.heap.len() < self.cap {
            self.heap.push(Worst(h));
            return true;
        }
        let Some(top) = self.heap.peek() else { return false };
        if less(&h, &top.0) {
            return false;
        }
        self.heap.pop();
        self.heap.push(Worst(h));
        true
    }

    /// Best first.
    fn drain_sorted(&mut self) -> Vec<Hit> {
        let mut v: Vec<Hit> = std::mem::take(&mut self.heap).into_sorted_vec().into_iter().map(|w| w.0).collect();
        // `into_sorted_vec` is ascending by `Worst` order, i.e. best first.
        v.truncate(self.cap);
        v
    }
}

/// One segment's candidates, best score first: a stream per distinct boost, merged.
struct Candidates<'a> {
    streams: Vec<(f32, Completions<'a>)>,
    /// Heads: (score, entry index, stream).
    heads: Vec<Option<(f32, search_codec::suggest::Completion)>>,
}

impl<'a> Candidates<'a> {
    fn new(reader: &'a SuggestReader, ranges: Vec<SuggestRange>, boost: Option<&BoostFn>) -> Candidates<'a> {
        let mut groups: Vec<(f32, Vec<SuggestRange>)> = Vec::new();
        let mut key = Vec::new();
        for r in ranges {
            let b = match boost {
                Some(f) => {
                    reader.key(r.start, &mut key);
                    f(&key[..(r.prefix_len as usize).min(key.len())])
                }
                None => 0.0,
            };
            match groups.iter_mut().find(|(x, _)| x.to_bits() == b.to_bits()) {
                Some((_, v)) => v.push(r),
                None => groups.push((b, vec![r])),
            }
        }
        let mut streams: Vec<(f32, Completions<'a>)> = groups.into_iter().map(|(b, rs)| (b, reader.complete_ranges(rs))).collect();
        let heads = streams.iter_mut().map(|(b, c)| c.next().map(|x| (completion_score(x.weight as f32, *b), x))).collect();
        Candidates { streams, heads }
    }

    fn next(&mut self) -> Option<(f32, search_codec::suggest::Completion)> {
        let mut best: Option<usize> = None;
        for (i, h) in self.heads.iter().enumerate() {
            let Some((s, c)) = h else { continue };
            best = match best {
                Some(j) => {
                    let (bs, bc) = self.heads[j].as_ref().unwrap();
                    if *s > *bs || (*s == *bs && c.index < bc.index) { Some(i) } else { Some(j) }
                }
                None => Some(i),
            };
        }
        let i = best?;
        let out = self.heads[i].take();
        let (b, c) = &mut self.streams[i];
        self.heads[i] = c.next().map(|x| (completion_score(x.weight as f32, *b), x));
        out
    }
}

impl Searcher {
    /// The top completions of `q` (see the module docs for Lucene's collection rules).
    pub fn complete(&self, q: &CompletionQuery<'_>) -> Result<Vec<Suggestion>> {
        if q.size == 0 {
            return Err(Error::illegal("'num' must be > 0"));
        }
        let mut shared = Queue::new(q.size);
        let mut pending: Vec<Hit> = Vec::new();
        let mut prefixes: std::collections::HashMap<u32, Vec<Vec<u8>>> = std::collections::HashMap::new();
        for (si, seg) in self.segments().iter().enumerate() {
            let Some(reader) = seg.reader.suggest(q.field) else { continue };
            if reader.is_empty() {
                continue;
            }
            let ranges = reader.ranges(q.automaton);
            if ranges.is_empty() {
                continue;
            }
            let mut cands = Candidates::new(reader, ranges, q.boost);
            let mut own = Queue::new(q.size);
            let queue = if q.skip_duplicates { &mut own } else { &mut shared };
            let mut seen: HashSet<Vec<u8>> = HashSet::new();
            let mut accepted = 0usize;
            let mut key = Vec::new();
            while accepted < q.size
                && let Some((score, c)) = cands.next()
            {
                if !seg.is_live(c.doc) {
                    continue;
                }
                let payload = reader.payload(c.index);
                if q.skip_duplicates && !seen.insert(payload.to_vec()) {
                    continue;
                }
                accepted += 1;
                let doc = seg.base + c.doc;
                reader.key(c.index, &mut key);
                let prefix = key[..(c.prefix_len as usize).min(key.len())].to_vec();
                if q.one_per_doc {
                    match prefixes.entry(doc) {
                        std::collections::hash_map::Entry::Occupied(mut e) => {
                            e.get_mut().push(prefix);
                            continue;
                        }
                        std::collections::hash_map::Entry::Vacant(e) => {
                            e.insert(vec![prefix.clone()]);
                        }
                    }
                }
                let h = Hit { doc, index: c.index, seg: si, payload: payload.to_vec(), weight: c.weight, score, prefix };
                if !queue.insert(h) {
                    break;
                }
            }
            if q.skip_duplicates {
                pending.extend(own.drain_sorted());
            }
        }
        let hits = if q.skip_duplicates {
            pending.sort_by(|a, b| {
                b.score.partial_cmp(&a.score).unwrap_or(Ordering::Equal).then_with(|| surface_cmp(&a.payload, &b.payload)).then(a.doc.cmp(&b.doc))
            });
            let mut seen: HashSet<Vec<u8>> = HashSet::new();
            let mut out = Vec::new();
            for h in pending {
                if seen.insert(h.payload.clone()) {
                    out.push(h);
                    if out.len() == q.size {
                        break;
                    }
                }
            }
            out
        } else {
            shared.drain_sorted()
        };
        let segs = self.segments();
        Ok(hits
            .into_iter()
            .map(|h| {
                let mut key = Vec::new();
                if let Some(r) = segs[h.seg].reader.suggest(q.field) {
                    r.key(h.index, &mut key);
                }
                let prefixes = if q.one_per_doc { prefixes.remove(&h.doc).unwrap_or_default() } else { vec![h.prefix] };
                Suggestion { doc: h.doc, key, payload: h.payload, weight: h.weight, score: h.score, prefixes }
            })
            .collect())
    }
}
