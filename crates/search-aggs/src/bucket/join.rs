//! `children` and `parent`: single-bucket aggregations across a `join` field.
//!
//! Elasticsearch's `ParentJoinAggregator`: while collecting, the docs of one side that match
//! the query (`children`: parents; `parent`: children) record the join value — the parent's id —
//! per owning bucket. Sub-aggregations are then fed, at build time, from every live doc of the
//! other side (whatever the query) whose join value was recorded for the bucket, segment by
//! segment, owning buckets in the order they are built, with a score of 1. Bucket doc counts are
//! only known after that replay, so sorting a parent by them at shard level sees zeros, as in
//! Elasticsearch.
//!
//! Both aggregations name the *child* type (`type`); an unknown type (or an index without a join
//! field) makes them unmapped: an empty bucket.

use std::sync::Arc;

use search_index::query::nested::segment_bits;
use search_index::util::bitset::FixedBitSet;
use search_index::{QueryRef, Searcher, SegmentContext};

use crate::bucket::single::{SingleBucket, SingleKind};
use crate::collect::{Aggregator, Batch, BindCtx, Subs, single_bucket_validate_sort};
use crate::error::Result;
use crate::internal::AggValue;
use crate::memory::MemTracker;
use crate::request::{AggDef, PathElement};
use crate::values::{DocCounter, DvOrds};
use crate::xparser::{Token, XParser};

/// A parsed `children` (`parent: false`) or `parent` aggregation.
#[derive(Clone, Debug)]
pub struct JoinSpec {
    pub parent: bool,
    pub child_type: String,
}

/// `ChildrenAggregationBuilder.parse` / `ParentAggregationBuilder.parse`.
pub fn parse(p: &mut XParser<'_>, name: &str, parent: bool) -> Result<JoinSpec> {
    let mut child_type = None;
    let mut cur = String::new();
    while let Some(t) = p.next_token() {
        match t {
            Token::EndObject => break,
            Token::FieldName => cur = p.current_name().unwrap_or("").to_string(),
            Token::String if cur == "type" => child_type = Some(p.text()?),
            Token::String => return Err(p.parsing_error(format!("Unknown key for a {t} in [{name}]: [{cur}]."))),
            t => return Err(p.parsing_error(format!("Unexpected token {t} in [{name}]."))),
        }
    }
    let Some(child_type) = child_type else {
        let which = if parent { "parent" } else { "children" };
        return Err(p.parsing_error(format!("Missing [child_type] field for {which} aggregation [{name}]")));
    };
    Ok(JoinSpec { parent, child_type })
}

/// Binds `children` / `parent`.
pub fn create(def: &AggDef, spec: &JoinSpec, ctx: &mut BindCtx<'_>) -> Result<Box<dyn Aggregator>> {
    let kind = if spec.parent { SingleKind::Parent } else { SingleKind::Children };
    let relation = ctx.lookup.join(Some(&spec.child_type), None);
    let subs = Subs::bind(def, ctx)?;
    let Some(rel) = relation else {
        return Ok(Box::new(JoinAgg { kind, sides: None, subs, searcher: ctx.searcher.clone(), state: State::default(), doc_count_field: ctx.doc_count_field, tracker: ctx.tracker(def) }));
    };
    let (input, output) = if spec.parent { (rel.child_filter, rel.parent_filter) } else { (rel.parent_filter, rel.child_filter) };
    Ok(Box::new(JoinAgg {
        kind,
        sides: Some(Sides { join_field: rel.join_field, input, output }),
        subs,
        searcher: ctx.searcher.clone(),
        state: State::default(),
        doc_count_field: ctx.doc_count_field,
        tracker: ctx.tracker(def),
    }))
}

struct Sides {
    join_field: u32,
    /// Docs whose join values are recorded while collecting.
    input: QueryRef,
    /// Docs replayed into the bucket.
    output: QueryRef,
}

#[derive(Default)]
struct State {
    /// Distinct join values (ids) and their dense numbers.
    ids: hashbrown::HashMap<Vec<u8>, u32>,
    /// `(owning ordinal, id number)` pairs recorded while collecting.
    recorded: hashbrown::HashSet<(u32, u32)>,
    /// Current segment: the input bitset, the join ordinals and a cache of segment ordinal ->
    /// id number.
    seg_bits: Option<Arc<FixedBitSet>>,
    seg_ords: Option<DvOrds>,
    seg_ids: hashbrown::HashMap<u64, u32>,
    term: Vec<u8>,
    /// Doc count per owning ordinal (known after the replay).
    counts: Vec<u64>,
}

struct JoinAgg {
    kind: SingleKind,
    sides: Option<Sides>,
    subs: Subs,
    searcher: Searcher,
    state: State,
    doc_count_field: Option<u32>,
    tracker: MemTracker,
}

impl JoinAgg {
    /// Feeds the sub-aggregations from the output side (`prepareSubAggs`).
    fn replay(&mut self, owning: &[u32]) -> Result<()> {
        let Some(sides) = &self.sides else { return Ok(()) };
        let st = &mut self.state;
        if st.recorded.is_empty() {
            return Ok(());
        }
        let segs: Vec<SegmentContext> = self.searcher.segments().to_vec();
        for seg in &segs {
            let bits = segment_bits(&self.searcher, seg, &sides.output)?;
            let ords = DvOrds::open(seg, sides.join_field);
            let counter = DocCounter::open(seg, self.doc_count_field);
            self.subs.set_segment(seg)?;
            let mut seg_ids: hashbrown::HashMap<u64, Option<u32>> = hashbrown::HashMap::new();
            for doc in bits.iter() {
                if !seg.is_live(doc) {
                    continue;
                }
                let mut ord = None;
                ords.for_each(doc, |o| {
                    if ord.is_none() {
                        ord = Some(o);
                    }
                });
                let Some(ord) = ord else { continue };
                let id = *seg_ids.entry(ord).or_insert_with(|| {
                    ords.lookup(ord, &mut st.term);
                    st.ids.get(st.term.as_slice()).copied()
                });
                let Some(id) = id else { continue };
                for &o in owning {
                    if st.recorded.contains(&(o, id)) {
                        let i = o as usize;
                        if st.counts.len() <= i {
                            st.counts.resize(i + 1, 0);
                        }
                        st.counts[i] += counter.count(doc);
                        self.subs.push(doc, o, 1.0)?;
                    }
                }
            }
            self.subs.flush()?;
            self.subs.finish_segment()?;
        }
        Ok(())
    }
}

impl Aggregator for JoinAgg {
    fn set_segment(&mut self, seg: &SegmentContext) -> Result<()> {
        let st = &mut self.state;
        st.seg_ids.clear();
        match &self.sides {
            None => {
                st.seg_bits = None;
                st.seg_ords = None;
            }
            Some(s) => {
                st.seg_bits = Some(segment_bits(&self.searcher, seg, &s.input)?);
                st.seg_ords = Some(DvOrds::open(seg, s.join_field));
            }
        }
        Ok(())
    }

    fn collect(&mut self, b: Batch<'_>) -> Result<()> {
        let st = &mut self.state;
        let (Some(bits), Some(ords)) = (st.seg_bits.clone(), st.seg_ords.as_ref()) else { return Ok(()) };
        for i in 0..b.len() {
            let doc = b.docs[i];
            if !bits.get(doc) {
                continue;
            }
            let mut ord = None;
            ords.for_each(doc, |o| {
                if ord.is_none() {
                    ord = Some(o);
                }
            });
            let Some(ord) = ord else { continue };
            let id = match st.seg_ids.get(&ord) {
                Some(&id) => id,
                None => {
                    ords.lookup(ord, &mut st.term);
                    let next = st.ids.len() as u32;
                    let id = *st.ids.entry(st.term.clone()).or_insert(next);
                    st.seg_ids.insert(ord, id);
                    id
                }
            };
            if st.recorded.insert((b.ords[i], id)) && st.recorded.len().is_power_of_two() {
                self.tracker.ensure(st.recorded.capacity() * 12 + st.ids.capacity() * 48)?;
            }
        }
        Ok(())
    }

    fn post_collection(&mut self) -> Result<()> {
        // Delayed until the buckets are built (the replay feeds the sub-aggregations).
        Ok(())
    }

    fn build(&mut self, owning: &[u32]) -> Result<Vec<AggValue>> {
        self.replay(owning)?;
        self.subs.post_collection()?;
        let aggs = self.subs.build(owning)?;
        let kind = self.kind;
        Ok(owning
            .iter()
            .zip(aggs)
            .map(|(&o, aggs)| AggValue::Single(Box::new(SingleBucket { sampler: None, kind, doc_count: self.state.counts.get(o as usize).copied().unwrap_or(0), aggs })))
            .collect())
    }

    fn build_empty(&self) -> AggValue {
        AggValue::Single(Box::new(SingleBucket { sampler: None, kind: self.kind, doc_count: 0, aggs: self.subs.build_empty() }))
    }

    fn needs_scores(&self) -> bool {
        false
    }

    fn validate_sort(&self, name: &str, key: Option<&str>, rest: &[PathElement]) -> Result<()> {
        single_bucket_validate_sort(name, &self.subs, key, rest)
    }

    fn sort_value(&self, _key: Option<&str>, rest: &[PathElement], ord: u32) -> f64 {
        if rest.is_empty() { self.state.counts.get(ord as usize).copied().unwrap_or(0) as f64 } else { self.subs.sort_value(rest, ord) }
    }

    fn sort_is_count(&self, _key: Option<&str>, rest: &[PathElement]) -> bool {
        rest.is_empty() || self.subs.sort_is_count(rest)
    }
}
