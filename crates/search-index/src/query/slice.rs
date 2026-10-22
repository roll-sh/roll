//! Sliced scroll / point-in-time partitions (Elasticsearch's `slice`: `TermsSliceQuery`,
//! `DocValuesSliceQuery`, `DocIdSliceQuery`).
//!
//! A slice is `floorMod(hash, max) == id`, where the hash is the murmur3 hash (seed 7919) of
//! each term of a field (by default `_id`: one term per doc), or the mixed numeric doc values
//! of a field (any value in the slice; docs without one hash as 0); for a point in time
//! without a field the searcher's doc ids are cut into `max` contiguous ranges instead. All
//! three are constant-score filters.

use std::any::Any;
use std::sync::Arc;

use search_codec::{DocValuesReader, NO_MORE_DOCS, PostingsFlags};

use crate::error::{Error, Result};
use crate::query::basic::{SetScorer, explain_const};
use crate::query::function_score::murmur3_32;
use crate::schema::Schema;
use crate::search::{Explanation, Query, Scorable, ScoreMode, Scorer, Searcher, SegmentContext, Weight};
use crate::util::bitset::DocIdSetBuilder;

/// `TermsSliceQuery.SEED`.
const SEED: i32 = 7919;

/// How docs are assigned to slices.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SliceBy {
    /// Hash of each term of the field (`_id` by default); `sliced_id_only` keeps only terms
    /// ending in `#` (a slice-enabled `_id` that indexes extra terms).
    Terms { field: u32, sliced_id_only: bool },
    /// Mixed values of a numeric doc values field.
    DocValues { field: u32 },
    /// Contiguous ranges of the searcher's doc ids (`_doc`, points in time).
    DocId,
}

/// The docs of slice `id` out of `max`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SliceQuery {
    pub by: SliceBy,
    pub id: u32,
    pub max: u32,
}

impl SliceQuery {
    pub fn new(by: SliceBy, id: u32, max: u32) -> Result<SliceQuery> {
        if max == 0 || id >= max {
            return Err(Error::illegal("max must be greater than id"));
        }
        Ok(SliceQuery { by, id, max })
    }

    /// `SliceQuery.contains`: `Math.floorMod(value, max) == id`.
    #[inline]
    fn contains(&self, v: i64) -> bool {
        v.rem_euclid(self.max as i64) == self.id as i64
    }
}

/// HPPC's `BitMixer.mix(long)`: `(int) mix64`.
fn bit_mix(z: i64) -> i32 {
    let mut z = z as u64;
    z = (z ^ (z >> 32)).wrapping_mul(0x4cd6_944c_5cc2_0b6d);
    z = (z ^ (z >> 29)).wrapping_mul(0xfc12_c5b1_9d32_59e9);
    (z ^ (z >> 32)) as i32
}

struct SliceWeight {
    q: SliceQuery,
    score: f32,
    desc: String,
    /// `DocId`: the slice's global doc range.
    range: (u32, u32),
}

/// `DocValuesSliceQuery`'s two-phase scorer.
struct DvSliceScorer {
    values: Option<search_codec::SortedNumericDocValues>,
    q: SliceQuery,
    max_doc: u32,
    doc: u32,
    score: f32,
}

impl Scorable for DvSliceScorer {
    fn score(&mut self) -> f32 {
        self.score
    }
}

impl Scorer for DvSliceScorer {
    fn doc(&self) -> u32 {
        self.doc
    }
    fn next(&mut self) -> u32 {
        loop {
            let d = self.approx_next();
            if d == NO_MORE_DOCS || self.matches() {
                return d;
            }
        }
    }
    fn advance(&mut self, t: u32) -> u32 {
        let d = self.approx_advance(t);
        if d == NO_MORE_DOCS || self.matches() {
            return d;
        }
        self.next()
    }
    fn cost(&self) -> u64 {
        self.max_doc as u64
    }
    fn has_two_phase(&self) -> bool {
        true
    }
    fn approx_next(&mut self) -> u32 {
        let t = if self.doc == u32::MAX { 0 } else { self.doc.saturating_add(1) };
        self.approx_advance(t)
    }
    fn approx_advance(&mut self, t: u32) -> u32 {
        if self.doc != u32::MAX && t <= self.doc {
            return self.doc;
        }
        self.doc = if t < self.max_doc { t } else { NO_MORE_DOCS };
        self.doc
    }
    fn matches(&mut self) -> bool {
        let Some(v) = &self.values else { return self.q.contains(0) };
        let mut any = false;
        let mut empty = true;
        for x in v.values(self.doc) {
            empty = false;
            if self.q.contains(bit_mix(x) as i64) {
                any = true;
                break;
            }
        }
        if empty { self.q.contains(0) } else { any }
    }
    fn match_cost(&self) -> f32 {
        10.0
    }
}

impl Weight for SliceWeight {
    fn scorer(&self, seg: &SegmentContext) -> Result<Option<Box<dyn Scorer>>> {
        let md = seg.reader.max_doc();
        match self.q.by {
            SliceBy::Terms { field, sliced_id_only } => {
                let Some(tr) = seg.reader.terms(field) else { return Ok(None) };
                seg.ensure_postings(field)?;
                let mut b = DocIdSetBuilder::new(md);
                let mut it = tr.iter();
                let mut cursor = None;
                while let Some(t) = it.next() {
                    if sliced_id_only && t.last() != Some(&b'#') {
                        continue;
                    }
                    if !self.q.contains(murmur3_32(t, SEED) as i64) {
                        continue;
                    }
                    let st = it.state();
                    let c = match cursor.as_mut() {
                        Some(c) => {
                            tr.postings_reset(&st, PostingsFlags::DOCS, c);
                            c
                        }
                        None => cursor.insert(tr.postings(&st, PostingsFlags::DOCS)),
                    };
                    loop {
                        let d = c.next();
                        if d == NO_MORE_DOCS {
                            break;
                        }
                        b.add(d);
                    }
                }
                Ok(Some(Box::new(SetScorer::new(Arc::new(b.build()), self.score))))
            }
            SliceBy::DocValues { field } => {
                let values = match seg.reader.doc_values(field) {
                    Some(DocValuesReader::SortedNumeric(v)) => Some(v.clone()),
                    _ => None,
                };
                Ok(Some(Box::new(DvSliceScorer { values, q: self.q.clone(), max_doc: md, doc: u32::MAX, score: self.score })))
            }
            SliceBy::DocId => {
                let (lo, hi) = self.range;
                let (start, end) = (seg.base, seg.base + md);
                if end <= lo || start >= hi {
                    return Ok(None);
                }
                let (a, b) = (lo.max(start) - seg.base, hi.min(end) - seg.base);
                let mut set = DocIdSetBuilder::new(md);
                set.add_range(a, b);
                Ok(Some(Box::new(SetScorer::new(Arc::new(set.build()), self.score))))
            }
        }
    }

    fn explain(&self, seg: &SegmentContext, doc: u32) -> Result<Explanation> {
        explain_const(self, seg, doc, &self.desc, self.score)
    }
}

impl Query for SliceQuery {
    fn weight(&self, s: &Searcher, mode: ScoreMode, boost: f32) -> Result<Box<dyn Weight>> {
        // `DocIdSliceQuery`: `max` ranges over the searcher's doc ids, the first
        // `max_doc % max` one doc longer.
        let max_doc = s.max_doc();
        let (rem, quo) = (max_doc % self.max, max_doc / self.max);
        let range = if self.id < rem {
            ((quo + 1) * self.id, (quo + 1) * self.id + quo + 1)
        } else {
            let start = rem * (quo + 1) + (self.id - rem) * quo;
            (start, start + quo)
        };
        let score = if mode.needs_scores() { boost } else { 0.0 };
        Ok(Box::new(SliceWeight { q: self.clone(), score, desc: self.lucene_string(s.schema()), range }))
    }

    fn lucene_string(&self, schema: &Schema) -> String {
        let (class, field) = match self.by {
            SliceBy::Terms { field, .. } => ("TermsSliceQuery", schema.name(field)),
            SliceBy::DocValues { field } => ("DocValuesSliceQuery", schema.name(field)),
            SliceBy::DocId => ("DocIdSliceQuery", "_doc".to_string()),
        };
        format!("{class}[field={field}, id={}, max={}]", self.id, self.max)
    }

    fn as_any(&self) -> &dyn Any {
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bit_mixer_matches_hppc() {
        // HPPC BitMixer.mix(long), computed independently.
        assert_eq!(bit_mix(0), 0);
        assert_eq!(bit_mix(1), 1939593575);
        assert_eq!(bit_mix(42), -271839007);
        assert_eq!(bit_mix(-7), 1960491678);
        let q = SliceQuery { by: SliceBy::DocId, id: 2, max: 3 };
        assert!(q.contains(-1));
        assert!(q.contains(5));
        assert!(!q.contains(4));
    }
}
