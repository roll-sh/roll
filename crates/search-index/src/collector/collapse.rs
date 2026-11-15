//! Field collapsing (Elasticsearch's `collapse`): the top `n` groups of a single-valued
//! keyword or numeric doc-values field, each represented by its best hit under the search's
//! sort (score by default), as Lucene's first-pass grouping collector keeps them.
//!
//! Every match counts towards the total; docs without a value form one group (`null`). The
//! hit's group value is the hook for inner hits: `search-es` runs each group's inner-hit
//! searches with a filter on that value.
//!
//! Per segment, a doc that can't beat the worst kept group's head is dropped after computing
//! its sort natives only (no group lookup), so non-competitive docs stay cheap.
//!
//! Group heads keep their natives with the segment generation they were computed in (as
//! `TopFieldCollector`'s queue entries do): a head from an earlier segment is translated into
//! the current one once, the first time a comparison needs it, so a competitive doc costs no
//! allocation and no repeated terms-dictionary lookups. Groups are found by borrowed keys (a
//! keyword's bytes from a per-segment ordinal cache), and an evicted group's buffers are reused
//! by the group replacing it.

use std::cmp::Ordering;
use std::collections::HashMap;

use foldhash::fast::RandomState;

use search_codec::DocValuesReader;

use crate::collector::top_docs::TotalRelation;
use crate::collector::top_field::{
    LeafKeyKind, Native, SortKey, SortValue, build_leaf_keys, cmp_natives, compute_against, compute_natives, fill_values, natives_into,
};
use crate::error::{Error, Result};
use crate::search::{Collector, LeafCollector, Scorable, ScoreMode, Searcher, SegmentContext};

/// The collapse field and how its doc values decode.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CollapseField {
    /// Sorted / sorted-set doc values (keyword, ip…): groups by term bytes.
    Keyword(u32),
    /// Numeric / sorted-numeric doc values: groups by the raw long.
    Numeric(u32),
}

/// A group's best hit.
#[derive(Clone, Debug, PartialEq)]
pub struct CollapseHit {
    /// Global doc id.
    pub doc: u32,
    /// The score (`NaN` when not tracked).
    pub score: f32,
    /// Sort values (empty for score-only collapsing).
    pub values: Vec<SortValue>,
    /// The group: `Bytes`, `Long`, or `Null` for docs without a value.
    pub group: SortValue,
}

/// Collapsed results.
#[derive(Clone, Debug, PartialEq)]
pub struct CollapseTopDocs {
    pub total_hits: u64,
    pub relation: TotalRelation,
    pub hits: Vec<CollapseHit>,
    pub max_score: Option<f32>,
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
enum GroupKey {
    Null,
    Bytes(Vec<u8>),
    Long(i64),
}

/// A group key borrowed from the segment (no allocation to find a group).
#[derive(Clone, Copy)]
enum KeyRef<'a> {
    Null,
    Bytes(&'a [u8]),
    Long(i64),
}

struct Group {
    key: GroupKey,
    doc: u32,
    score: f32,
    values: Vec<SortValue>,
    /// `values` in the native space of segment generation `generation`.
    natives: Vec<Native>,
    generation: u32,
}

/// Group slots by key.
#[derive(Default)]
struct Slots {
    null: Option<usize>,
    bytes: HashMap<Vec<u8>, usize, RandomState>,
    longs: HashMap<i64, usize, RandomState>,
}

impl Slots {
    fn get(&self, k: KeyRef<'_>) -> Option<usize> {
        match k {
            KeyRef::Null => self.null,
            KeyRef::Bytes(b) => self.bytes.get(b).copied(),
            KeyRef::Long(v) => self.longs.get(&v).copied(),
        }
    }

    fn remove(&mut self, k: &GroupKey) {
        match k {
            GroupKey::Null => self.null = None,
            GroupKey::Bytes(b) => {
                self.bytes.remove(b.as_slice());
            }
            GroupKey::Long(v) => {
                self.longs.remove(v);
            }
        }
    }

    fn insert(&mut self, k: &GroupKey, slot: usize) {
        match k {
            GroupKey::Null => self.null = Some(slot),
            GroupKey::Bytes(b) => {
                self.bytes.insert(b.clone(), slot);
            }
            GroupKey::Long(v) => {
                self.longs.insert(*v, slot);
            }
        }
    }
}

/// `key` as an owned key, reusing `old`'s byte buffer.
fn own_key(key: KeyRef<'_>, old: GroupKey) -> GroupKey {
    match key {
        KeyRef::Null => GroupKey::Null,
        KeyRef::Long(v) => GroupKey::Long(v),
        KeyRef::Bytes(b) => match old {
            GroupKey::Bytes(mut buf) => {
                buf.clear();
                buf.extend_from_slice(b);
                GroupKey::Bytes(buf)
            }
            _ => GroupKey::Bytes(b.to_vec()),
        },
    }
}

/// Brings a head's natives into segment generation `generation` (once per segment).
fn translate(keys: &[LeafKeyKind], generation: u32, g: &mut Group) {
    if g.generation != generation {
        natives_into(keys, &g.values, g.doc, g.score, &mut g.natives);
        g.generation = generation;
    }
}

/// Where a head with `natives` and global `doc` goes in `order` (best first): binary search,
/// translating the probed heads into the current segment.
fn position(keys: &[LeafKeyKind], generation: u32, groups: &mut [Group], order: &[u32], natives: &[Native], doc: u32) -> usize {
    let (mut lo, mut hi) = (0, order.len());
    while lo < hi {
        let mid = (lo + hi) / 2;
        let g = &mut groups[order[mid] as usize];
        translate(keys, generation, g);
        if cmp_natives(keys, natives, &g.natives).then(doc.cmp(&g.doc)) == Ordering::Less {
            hi = mid;
        } else {
            lo = mid + 1;
        }
    }
    lo
}

/// Keeps the top `n` groups by their heads.
pub struct CollapseCollector {
    field: CollapseField,
    keys: Vec<SortKey>,
    n: usize,
    threshold: u64,
    track_scores: bool,
    searcher: Option<Searcher>,
    groups: Vec<Group>,
    by_key: Slots,
    /// Group slots, best first.
    order: Vec<u32>,
    total: u64,
    relation: TotalRelation,
    max_score: f32,
    /// Counts segments: heads whose `generation` differs are translated before comparing.
    generation: u32,
}

impl CollapseCollector {
    /// `keys` empty: by score. `threshold`: hits counted exactly before the total becomes a
    /// lower bound (collapsing never skips docs, so the count stays exact; the relation follows
    /// `track_total_hits`).
    pub fn new(field: CollapseField, keys: Vec<SortKey>, n: usize, threshold: u64, track_scores: bool) -> CollapseCollector {
        let keys = if keys.is_empty() { vec![SortKey::Score { reverse: false }] } else { keys };
        CollapseCollector {
            field,
            keys,
            n,
            threshold,
            track_scores,
            searcher: None,
            groups: Vec::new(),
            by_key: Slots::default(),
            order: Vec::new(),
            total: 0,
            relation: TotalRelation::EqualTo,
            max_score: f32::NEG_INFINITY,
            generation: 0,
        }
    }

    /// Needed for nested sort keys.
    pub fn with_searcher(mut self, s: &Searcher) -> Self {
        self.searcher = Some(s.clone());
        self
    }

    fn needs_scores(&self) -> bool {
        self.track_scores || self.keys.iter().any(|k| matches!(k, SortKey::Score { .. }))
    }

    pub fn top_docs(self) -> CollapseTopDocs {
        let max_score = (self.needs_scores() && self.total > 0).then_some(self.max_score);
        let score_only = matches!(self.keys.as_slice(), [SortKey::Score { reverse: false }]);
        let mut groups: Vec<Option<Group>> = self.groups.into_iter().map(Some).collect();
        let hits = self
            .order
            .iter()
            .filter_map(|&i| groups[i as usize].take())
            .map(|g| CollapseHit {
                doc: g.doc,
                score: g.score,
                values: if score_only { Vec::new() } else { g.values },
                group: match g.key {
                    GroupKey::Null => SortValue::Null,
                    GroupKey::Bytes(b) => SortValue::Bytes(b),
                    GroupKey::Long(v) => SortValue::Long(v),
                },
            })
            .collect();
        CollapseTopDocs { total_hits: self.total, relation: self.relation, hits, max_score }
    }
}

enum GroupSrc {
    None,
    Sorted(search_codec::SortedDocValues),
    SortedSet(search_codec::SortedSetDocValues),
    Numeric(search_codec::NumericDocValues),
    SortedNumeric(search_codec::SortedNumericDocValues),
}

struct CollapseLeaf<'a> {
    c: &'a mut CollapseCollector,
    base: u32,
    keys: Vec<LeafKeyKind>,
    natives: Vec<Native>,
    bottom: Option<Vec<Native>>,
    src: GroupSrc,
    /// Term bytes per ordinal seen in this segment.
    ord_terms: HashMap<u64, Vec<u8>, RandomState>,
    /// A head's natives before its update.
    scratch: Vec<Native>,
}

fn single_valued_error(doc: u32) -> Error {
    Error::illegal_state(format!("failed to collapse {doc}, the collapse field must be single valued"))
}

/// A doc's group: `Ok(Err(ord))` for a keyword ordinal (resolved through the term cache).
fn group_of(src: &GroupSrc, doc: u32) -> Result<std::result::Result<KeyRef<'static>, u64>> {
    Ok(match src {
        GroupSrc::None => Ok(KeyRef::Null),
        GroupSrc::Sorted(v) => v.ord(doc).map_or(Ok(KeyRef::Null), Err),
        GroupSrc::SortedSet(v) => {
            let mut it = v.ords(doc);
            match it.next() {
                None => Ok(KeyRef::Null),
                Some(o) => {
                    if it.next().is_some() {
                        return Err(single_valued_error(doc));
                    }
                    Err(o)
                }
            }
        }
        GroupSrc::Numeric(v) => Ok(v.get(doc).map_or(KeyRef::Null, KeyRef::Long)),
        GroupSrc::SortedNumeric(v) => {
            let mut it = v.values(doc);
            match it.next() {
                None => Ok(KeyRef::Null),
                Some(x) => {
                    if it.next().is_some() {
                        return Err(single_valued_error(doc));
                    }
                    Ok(KeyRef::Long(x))
                }
            }
        }
    })
}

impl CollapseLeaf<'_> {
    fn refresh_bottom(&mut self) {
        let c = &mut *self.c;
        if c.n > 0
            && c.order.len() >= c.n
            && let Some(&w) = c.order.last()
        {
            let g = &mut c.groups[w as usize];
            translate(&self.keys, c.generation, g);
            match &mut self.bottom {
                Some(b) => b.clone_from(&g.natives),
                None => self.bottom = Some(g.natives.clone()),
            }
        } else {
            self.bottom = None;
        }
    }
}

impl LeafCollector for CollapseLeaf<'_> {
    fn collect(&mut self, doc: u32, scorer: &mut dyn Scorable) -> Result<()> {
        self.c.total += 1;
        if self.c.total > self.c.threshold {
            self.c.relation = TotalRelation::GreaterThanOrEqualTo;
        }
        let needs = self.c.needs_scores();
        let score = if needs { scorer.score() } else { f32::NAN };
        if needs && score > self.c.max_score {
            self.c.max_score = score;
        }
        if self.c.n == 0 {
            return Ok(());
        }
        match &self.bottom {
            Some(b) => {
                if compute_against(&mut self.keys, self.base, doc, score, b, &mut self.natives) != Ordering::Less {
                    return Ok(());
                }
            }
            None => compute_natives(&mut self.keys, self.base, doc, score, &mut self.natives),
        }
        let key = match group_of(&self.src, doc)? {
            Ok(k) => k,
            Err(ord) => {
                let src = &self.src;
                KeyRef::Bytes(self.ord_terms.entry(ord).or_insert_with(|| {
                    let mut b = Vec::new();
                    match src {
                        GroupSrc::Sorted(v) => v.lookup_ord(ord, &mut b),
                        GroupSrc::SortedSet(v) => v.lookup_ord(ord, &mut b),
                        _ => {}
                    }
                    b
                }))
            }
        };
        let gdoc = self.base + doc;
        let c = &mut *self.c;
        let generation = c.generation;
        match c.by_key.get(key) {
            Some(slot) => {
                let g = &mut c.groups[slot];
                translate(&self.keys, generation, g);
                if cmp_natives(&self.keys, &self.natives, &g.natives).then(gdoc.cmp(&g.doc)) != Ordering::Less {
                    return Ok(());
                }
                // The head only moves up: find its place by its old natives, and the new one
                // among those before it, then shift just that range. Heads from earlier
                // segments whose keywords fall in one gap of this segment's ordinals tie as
                // natives, so the search may miss the head: then it is looked up by slot.
                self.scratch.clone_from(&g.natives);
                let old_doc = g.doc;
                let at = position(&self.keys, generation, &mut c.groups, &c.order, &self.scratch, old_doc)
                    .checked_sub(1)
                    .filter(|&a| c.order[a] as usize == slot)
                    .unwrap_or_else(|| c.order.iter().position(|&s| s as usize == slot).expect("queued group"));
                let pos = position(&self.keys, generation, &mut c.groups, &c.order[..at], &self.natives, gdoc);
                let g = &mut c.groups[slot];
                fill_values(&self.keys, &self.natives, score, gdoc, &mut g.values);
                g.natives.clone_from(&self.natives);
                (g.doc, g.score, g.generation) = (gdoc, score, generation);
                c.order[pos..=at].rotate_right(1);
            }
            None => {
                let slot = if c.order.len() >= c.n {
                    // Evict the worst group; the new one takes its slot and buffers.
                    let w = c.order.pop().expect("full queue") as usize;
                    let g = &mut c.groups[w];
                    let old = std::mem::replace(&mut g.key, GroupKey::Null);
                    c.by_key.remove(&old);
                    g.key = own_key(key, old);
                    w
                } else {
                    c.groups.push(Group {
                        key: own_key(key, GroupKey::Null),
                        doc: 0,
                        score: 0.0,
                        values: Vec::with_capacity(self.keys.len()),
                        natives: Vec::new(),
                        generation,
                    });
                    c.groups.len() - 1
                };
                let pos = position(&self.keys, generation, &mut c.groups, &c.order, &self.natives, gdoc);
                let g = &mut c.groups[slot];
                fill_values(&self.keys, &self.natives, score, gdoc, &mut g.values);
                g.natives.clone_from(&self.natives);
                (g.doc, g.score, g.generation) = (gdoc, score, generation);
                c.by_key.insert(&g.key, slot);
                c.order.insert(pos, slot as u32);
            }
        }
        self.refresh_bottom();
        Ok(())
    }
}

impl Collector for CollapseCollector {
    fn score_mode(&self) -> ScoreMode {
        if self.needs_scores() { ScoreMode::Complete } else { ScoreMode::CompleteNoScores }
    }

    fn leaf<'a>(&'a mut self, seg: &SegmentContext) -> Result<Box<dyn LeafCollector + 'a>> {
        let keys = build_leaf_keys(&self.keys, seg, self.searcher.as_ref())?;
        let (field, keyword) = match self.field {
            CollapseField::Keyword(f) => (f, true),
            CollapseField::Numeric(f) => (f, false),
        };
        let src = match (seg.reader.doc_values(field), keyword) {
            (None, _) => GroupSrc::None,
            (Some(DocValuesReader::Sorted(v)), true) => GroupSrc::Sorted(v.clone()),
            (Some(DocValuesReader::SortedSet(v)), true) => GroupSrc::SortedSet(v.clone()),
            (Some(DocValuesReader::Numeric(v)), false) => GroupSrc::Numeric(v.clone()),
            (Some(DocValuesReader::SortedNumeric(v)), false) => GroupSrc::SortedNumeric(v.clone()),
            _ => return Err(Error::illegal(format!("unexpected doc values type of the collapse field [{field}]"))),
        };
        let base = seg.base;
        // Heads' natives belong to the previous segment.
        self.generation = self.generation.wrapping_add(1);
        let mut leaf = CollapseLeaf { c: self, base, keys, natives: Vec::new(), bottom: None, src, ord_terms: HashMap::default(), scratch: Vec::new() };
        leaf.refresh_bottom();
        Ok(Box::new(leaf))
    }
}
