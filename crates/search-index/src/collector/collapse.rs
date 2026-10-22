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

use std::cmp::Ordering;
use std::collections::HashMap;

use search_codec::DocValuesReader;

use crate::collector::top_docs::TotalRelation;
use crate::collector::top_field::{
    LeafKeyKind, Native, SortKey, SortValue, build_leaf_keys, cmp_natives, compute_natives, natives_of, output_values,
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

struct Group {
    key: GroupKey,
    doc: u32,
    score: f32,
    values: Vec<SortValue>,
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
    by_key: HashMap<GroupKey, usize>,
    /// Group slots, best first.
    order: Vec<usize>,
    total: u64,
    relation: TotalRelation,
    max_score: f32,
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
            by_key: HashMap::new(),
            order: Vec::new(),
            total: 0,
            relation: TotalRelation::EqualTo,
            max_score: f32::NEG_INFINITY,
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
            .filter_map(|&i| groups[i].take())
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
    ord_terms: HashMap<u64, Vec<u8>>,
}

fn single_valued_error(doc: u32) -> Error {
    Error::illegal_state(format!("failed to collapse {doc}, the collapse field must be single valued"))
}

impl CollapseLeaf<'_> {
    fn group_key(&mut self, doc: u32) -> Result<GroupKey> {
        Ok(match &self.src {
            GroupSrc::None => GroupKey::Null,
            GroupSrc::Sorted(v) => match v.ord(doc) {
                None => GroupKey::Null,
                Some(o) => GroupKey::Bytes(self.term(o)),
            },
            GroupSrc::SortedSet(v) => {
                let mut it = v.ords(doc);
                match it.next() {
                    None => GroupKey::Null,
                    Some(o) => {
                        if it.next().is_some() {
                            return Err(single_valued_error(doc));
                        }
                        GroupKey::Bytes(self.term(o))
                    }
                }
            }
            GroupSrc::Numeric(v) => v.get(doc).map_or(GroupKey::Null, GroupKey::Long),
            GroupSrc::SortedNumeric(v) => {
                let mut it = v.values(doc);
                match it.next() {
                    None => GroupKey::Null,
                    Some(x) => {
                        if it.next().is_some() {
                            return Err(single_valued_error(doc));
                        }
                        GroupKey::Long(x)
                    }
                }
            }
        })
    }

    fn term(&mut self, ord: u64) -> Vec<u8> {
        if let Some(t) = self.ord_terms.get(&ord) {
            return t.clone();
        }
        let mut b = Vec::new();
        match &self.src {
            GroupSrc::Sorted(v) => v.lookup_ord(ord, &mut b),
            GroupSrc::SortedSet(v) => v.lookup_ord(ord, &mut b),
            _ => {}
        }
        self.ord_terms.insert(ord, b.clone());
        b
    }

    /// Orders group slots `a` (as natives) before slot `b`?
    fn cmp_group(&self, natives: &[Native], doc: u32, slot: usize) -> Ordering {
        let g = &self.c.groups[slot];
        let gn = natives_of(&self.keys, &g.values, g.doc, g.score);
        cmp_natives(&self.keys, natives, &gn).then(doc.cmp(&g.doc))
    }

    fn position(&self, natives: &[Native], doc: u32, exclude: Option<usize>) -> usize {
        let order: Vec<usize> = self.c.order.iter().copied().filter(|&s| Some(s) != exclude).collect();
        let (mut lo, mut hi) = (0, order.len());
        while lo < hi {
            let mid = (lo + hi) / 2;
            if self.cmp_group(natives, doc, order[mid]) == Ordering::Less {
                hi = mid;
            } else {
                lo = mid + 1;
            }
        }
        lo
    }

    fn refresh_bottom(&mut self) {
        self.bottom = None;
        if self.c.n > 0
            && self.c.order.len() >= self.c.n
            && let Some(&w) = self.c.order.last()
        {
            let g = &self.c.groups[w];
            self.bottom = Some(natives_of(&self.keys, &g.values, g.doc, g.score));
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
        compute_natives(&mut self.keys, self.base, doc, score, &mut self.natives);
        if let Some(b) = &self.bottom
            && cmp_natives(&self.keys, &self.natives, b) != Ordering::Less
        {
            return Ok(());
        }
        let key = self.group_key(doc)?;
        let gdoc = self.base + doc;
        let natives = std::mem::take(&mut self.natives);
        match self.c.by_key.get(&key).copied() {
            Some(slot) => {
                if self.cmp_group(&natives, gdoc, slot) == Ordering::Less {
                    let pos = self.position(&natives, gdoc, Some(slot));
                    let values = output_values(&self.keys, &natives, score, gdoc);
                    let g = &mut self.c.groups[slot];
                    g.doc = gdoc;
                    g.score = score;
                    g.values = values;
                    self.c.order.retain(|&s| s != slot);
                    self.c.order.insert(pos, slot);
                }
            }
            None => {
                let pos = self.position(&natives, gdoc, None);
                let values = output_values(&self.keys, &natives, score, gdoc);
                let g = Group { key: key.clone(), doc: gdoc, score, values };
                let slot = if self.c.order.len() >= self.c.n {
                    // Evict the worst group and reuse its slot.
                    let w = self.c.order.pop().expect("full queue");
                    let old = std::mem::replace(&mut self.c.groups[w], g);
                    self.c.by_key.remove(&old.key);
                    w
                } else {
                    self.c.groups.push(g);
                    self.c.groups.len() - 1
                };
                self.c.by_key.insert(key, slot);
                self.c.order.insert(pos.min(self.c.order.len()), slot);
            }
        }
        self.natives = natives;
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
        let mut leaf = CollapseLeaf { c: self, base, keys, natives: Vec::new(), bottom: None, src, ord_terms: HashMap::new() };
        leaf.refresh_bottom();
        Ok(Box::new(leaf))
    }
}
