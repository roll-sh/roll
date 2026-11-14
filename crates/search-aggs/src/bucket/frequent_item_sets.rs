//! `frequent_item_sets` (x-pack ml): the most frequent combinations of field values that
//! documents share, found with Elasticsearch's Eclat variant.
//!
//! Every collected document is a *transaction*: the list of its items, an item being a
//! `(field, value)` pair (keyword and ip values as bytes, numeric, date and boolean values as
//! longs — doubles truncate, as `longValues()` does). Shard side, each owning bucket keeps a
//! transaction store: item and transaction dictionaries with counts, in first-seen order. The
//! reduce merges stores (partial reduces stop there), then the final reduce prunes items below
//! `minimum_support`, rewrites transactions over the remaining items (most frequent first) and
//! runs the search:
//!
//! - items ordered by count (ties: store order) are walked depth first as a prefix tree
//!   (`ItemSetTraverser`); each node's count comes from scanning the transactions, with a
//!   per-depth skip list of transactions already known not to contain the prefix
//!   (`CountingItemSetTraverser`) and early stops once a count can't reach the bar;
//! - closed sets (no superset with the same count) are offered to a size-bounded priority queue
//!   (`FrequentItemSetCollector`) whose lowest count raises the bar as it fills.
//!
//! Tie handling follows Elasticsearch's data structures (Lucene's `PriorityQueue`, the
//! collector's per-count lists, `ItemSetBitSet.compare`) so results match given the same item
//! order. Elasticsearch numbers pruned items in `BytesRefHash` slot order, which depends on a
//! per-JVM random hash seed: items with equal counts come out in an order Elasticsearch itself
//! doesn't fix; here they keep first-seen order.
//!
//! Quirks kept: documents outside `filter` still count as (empty) transactions, and `support`
//! divides by every transaction.

use std::cmp::Ordering;

use search_index::{ScoreMode, Scorer, SegmentContext, Weight};

use crate::bucket::include::{IncludeExclude, LongFilter, StringFilter};
use crate::collect::{Aggregator, Batch, BindCtx};
use crate::error::{AggError, Result};
use crate::format::DocValueFormat;
use crate::internal::{AggValue, Internal, Prop, ReduceCtx, RenderCtx, java_list};
use crate::json::Json;
use crate::memory::MemTracker;
use crate::metrics::simple::MvsField;
use crate::request::{AggDef, PathElement};
use crate::values::{BoundVs, BytesReader, DvOrds, NumReader, VsType, bind};
use crate::xparser::{Field, Tokens, XParser};

/// `FrequentItemSetsAggregationBuilder` defaults.
pub const DEFAULT_MINIMUM_SUPPORT: f64 = 0.01;
pub const DEFAULT_MINIMUM_SET_SIZE: i32 = 1;
pub const DEFAULT_SIZE: i32 = 10;

/// `EclatMapReducer` constants.
const MAX_BITSET_CACHE_NUMBER_OF_TRANSACTIONS: u64 = 65536;
const BITSET_CACHE_TRAVERSAL_DEPTH: usize = 60;

/// One entry of `fields` (`MultiValuesSourceFieldConfig` with includes/excludes).
#[derive(Clone, Debug)]
pub struct FisField {
    pub vs: MvsField,
    pub include_exclude: Option<IncludeExclude>,
}

/// A parsed `frequent_item_sets`.
#[derive(Clone, Debug)]
pub struct FrequentItemSetsSpec {
    pub fields: Vec<FisField>,
    pub minimum_support: f64,
    pub minimum_set_size: usize,
    pub size: usize,
    pub filter: Option<Json>,
    /// `execution_hint: map` (ordinals are an optimisation only; results don't change).
    pub map_hint: bool,
}

fn parse_field(p: &mut XParser<'_>) -> Result<FisField> {
    let fields = [
        Field::new("field", Tokens::STRING),
        Field::new("missing", Tokens::VALUE),
        Field::new("include", Tokens::OBJECT_ARRAY_OR_STRING),
        Field::new("exclude", Tokens::STRING_ARRAY),
    ];
    let mut vs = MvsField::default();
    let mut ie = None;
    p.object("field_config", &fields, |p, i| {
        match i {
            0 => vs.field = Some(p.text()?),
            1 => vs.missing = p.object_text()?,
            2 => {
                let inc = IncludeExclude::parse_include(p)?;
                ie = IncludeExclude::merge(Some(inc), ie.take())?;
            }
            _ => {
                let exc = IncludeExclude::parse_exclude(p)?;
                ie = IncludeExclude::merge(ie.take(), Some(exc))?;
            }
        }
        Ok(())
    })?;
    vs.check()?;
    Ok(FisField { vs, include_exclude: ie })
}

/// `FrequentItemSetsAggregationBuilder.PARSER` (a `ConstructingObjectParser`: the
/// constructor's checks run once the object is read).
pub fn parse(p: &mut XParser<'_>, name: &str, check_query: &mut dyn FnMut(&XParser<'_>) -> Result<()>) -> Result<FrequentItemSetsSpec> {
    let fields = [
        Field::new("fields", Tokens::OBJECT_ARRAY),
        Field::new("minimum_support", Tokens::FLOAT),
        Field::new("minimum_set_size", Tokens::INT),
        Field::new("size", Tokens::INT),
        Field::new("filter", Tokens::OBJECT),
        Field::new("execution_hint", Tokens::STRING),
    ];
    let (mut list, mut support, mut set_size, mut size, mut filter, mut hint) = (None, None, None, None, None, None);
    p.object("frequent_item_sets", &fields, |p, i| {
        match i {
            0 => list = Some(p.array_items(parse_field)?),
            1 => support = Some(p.double_value()?),
            2 => set_size = Some(p.int_value()?),
            3 => size = Some(p.int_value()?),
            4 => {
                check_query(p)?;
                filter = Some(p.take_value());
            }
            _ => hint = Some(p.text()?),
        }
        Ok(())
    })?;
    let Some(list) = list else { return Err(AggError::illegal_argument("Required [fields]")) };
    // The constructor's exceptions surface wrapped by the `ConstructingObjectParser`.
    let built = |e: AggError| AggError::new(400, "x_content_parse_exception", "Failed to build [frequent_item_sets] after last required field arrived").caused_by(e);
    let support = support.unwrap_or(DEFAULT_MINIMUM_SUPPORT);
    if support <= 0.0 || support > 1.0 || support.is_nan() {
        return Err(built(AggError::illegal_argument(format!(
            "[minimum_support] must be greater than 0 and less or equal to 1. Found [{}] in [{name}]",
            crate::json::java_double(support)
        ))));
    }
    let set_size = set_size.unwrap_or(DEFAULT_MINIMUM_SET_SIZE);
    if set_size <= 0 {
        return Err(built(AggError::illegal_argument(format!("[minimum_set_size] must be greater than 0. Found [{set_size}] in [{name}]"))));
    }
    let size = size.unwrap_or(DEFAULT_SIZE);
    if size <= 0 {
        return Err(built(AggError::illegal_argument(format!("[size] must be greater than 0. Found [{size}] in [{name}]"))));
    }
    if let Some(h) = &hint
        && h != "global_ordinals"
        && h != "map"
    {
        return Err(built(AggError::illegal_argument(format!("[execution_hint] must be one of [global_ordinals,map]. Found [{h}]"))));
    }
    Ok(FrequentItemSetsSpec {
        fields: list,
        minimum_support: support,
        minimum_set_size: set_size as usize,
        size: size as usize,
        filter,
        map_hint: hint.as_deref() == Some("map"),
    })
}

/// An item's value: bytes for keyword-like fields, a long for numeric-like ones.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum ItemValue {
    Bytes(Box<[u8]>),
    Long(i64),
}

/// A result field (`ItemSetMapReduceValueSource.Field`): its name and value format.
#[derive(Clone, Debug)]
pub struct ItemField {
    pub name: String,
    pub format: DocValueFormat,
}

/// One field's values in a segment.
enum Reader {
    /// Plain ordinals: values are cached per ordinal.
    Ords(DvOrds),
    Bytes(BytesReader),
    Num(NumReader),
}

struct Source {
    vs: BoundVs,
    numeric: bool,
    strings: Option<StringFilter>,
    longs: Option<LongFilter>,
    reader: Option<Reader>,
    /// Plain ordinal readers: segment ordinal → global item id (`UNSEEN`, `REJECTED`).
    ord_items: Vec<u32>,
}

const UNSEEN: u32 = u32::MAX;
const REJECTED: u32 = u32::MAX - 1;

/// `HashBasedTransactionStore` of one owning bucket, over the aggregator's global item ids.
#[derive(Default)]
struct BucketStore {
    /// Global item id → local id.
    local: hashbrown::HashMap<u32, u32>,
    items: Vec<u32>,
    item_counts: Vec<u64>,
    total_items: u64,
    tx_ids: hashbrown::HashMap<Box<[u32]>, u32>,
    transactions: Vec<Box<[u32]>>,
    tx_counts: Vec<u64>,
    total_tx: u64,
    filtered: u64,
}

struct FisAgg {
    params: FisParams,
    sources: Vec<Source>,
    fields: Vec<ItemField>,
    filter: Option<Box<dyn Weight>>,
    scorer: Option<Box<dyn Scorer>>,
    last: (u32, bool),
    /// Global items (field index, value) and their ids.
    dict: hashbrown::HashMap<(u32, ItemValue), u32>,
    dict_items: Vec<(u32, ItemValue)>,
    buckets: Vec<Option<BucketStore>>,
    scratch: Vec<u32>,
    term: Vec<u8>,
    bytes: usize,
    tracker: MemTracker,
}

/// `FrequentItemSetsAggregatorFactory.createInternal` + `ItemSetMapReduceAggregator`.
pub fn create(def: &AggDef, spec: &FrequentItemSetsSpec, ctx: &mut BindCtx<'_>) -> Result<Box<dyn Aggregator>> {
    // Every config resolves first (field data errors), then the value sources are built.
    let mut bound = Vec::with_capacity(spec.fields.len());
    for f in &spec.fields {
        bound.push(bind(&f.vs.to_vs(), ctx.lookup, VsType::Keyword, false)?);
    }
    let mut sources = Vec::with_capacity(bound.len());
    let mut fields = Vec::with_capacity(bound.len());
    for (vs, f) in bound.into_iter().zip(&spec.fields) {
        let numeric = match vs.vs_type {
            VsType::Keyword | VsType::Ip => false,
            VsType::Numeric | VsType::Date | VsType::Boolean => true,
            _ if vs.unmapped => false,
            _ => return Err(vs.unsupported("frequent_item_sets")),
        };
        let Some(field) = vs.field.as_ref().filter(|_| !vs.unmapped) else {
            return Err(AggError::illegal_argument("scripts are not supported"));
        };
        fields.push(ItemField { name: field.name.clone(), format: vs.format.clone() });
        let (strings, longs) = match &f.include_exclude {
            None => (None, None),
            Some(ie) if numeric => (None, Some(ie.long_filter(&vs.format)?)),
            Some(ie) => (Some(ie.string_filter(&vs.format)?), None),
        };
        sources.push(Source { vs, numeric, strings, longs, reader: None, ord_items: Vec::new() });
    }
    let filter = match &spec.filter {
        Some(q) => {
            let q = ctx.lookup.query(q)?;
            Some(ctx.searcher.create_weight(&q, ScoreMode::CompleteNoScores)?)
        }
        None => None,
    };
    Ok(Box::new(FisAgg {
        params: FisParams { minimum_support: spec.minimum_support, minimum_set_size: spec.minimum_set_size, size: spec.size },
        sources,
        fields,
        filter,
        scorer: None,
        last: (u32::MAX, false),
        dict: hashbrown::HashMap::new(),
        dict_items: Vec::new(),
        buckets: Vec::new(),
        scratch: Vec::new(),
        term: Vec::new(),
        bytes: 0,
        tracker: ctx.tracker(def),
    }))
}

impl FisAgg {
    fn accepts(&mut self, doc: u32) -> Result<bool> {
        if self.filter.is_none() {
            return Ok(true);
        }
        if self.last.0 == doc {
            return Ok(self.last.1);
        }
        let Some(sc) = self.scorer.as_mut() else { return Ok(false) };
        let mut d = sc.doc();
        if d == u32::MAX || d < doc {
            d = sc.advance(doc);
        }
        let hit = d == doc;
        self.last = (doc, hit);
        Ok(hit)
    }

    fn intern(&mut self, field: u32, value: ItemValue) -> u32 {
        let key = (field, value);
        if let Some(&id) = self.dict.get(&key) {
            return id;
        }
        let id = self.dict_items.len() as u32;
        self.bytes += 48 + match &key.1 {
            ItemValue::Bytes(b) => b.len(),
            ItemValue::Long(_) => 0,
        };
        self.dict.insert(key.clone(), id);
        self.dict_items.push(key);
        id
    }

    /// Appends the global item ids of `doc`'s values of every field to `self.scratch`, each
    /// field's values in ascending order (`stream().sorted()`).
    fn doc_items(&mut self, doc: u32, score: f32) -> Result<()> {
        self.scratch.clear();
        for f in 0..self.sources.len() {
            let mut reader = self.sources[f].reader.take();
            let r = (|| -> Result<()> {
                match reader.as_mut() {
                    None => {}
                    Some(Reader::Ords(dv)) => {
                        let start = self.scratch.len();
                        let mut ords: smallvec::SmallVec<[u64; 8]> = smallvec::SmallVec::new();
                        dv.for_each(doc, |o| ords.push(o));
                        for o in ords {
                            let cached = self.sources[f].ord_items.get(o as usize).copied().unwrap_or(UNSEEN);
                            let id = if cached == UNSEEN {
                                dv.lookup(o, &mut self.term);
                                let id = match &self.sources[f].strings {
                                    Some(s) if !s.accept(&self.term) => REJECTED,
                                    _ => {
                                        let v = ItemValue::Bytes(self.term.clone().into_boxed_slice());
                                        self.intern(f as u32, v)
                                    }
                                };
                                let cache = &mut self.sources[f].ord_items;
                                if cache.len() <= o as usize {
                                    cache.resize(o as usize + 1, UNSEEN);
                                }
                                cache[o as usize] = id;
                                id
                            } else {
                                cached
                            };
                            if id != REJECTED {
                                self.scratch.push(id);
                            }
                        }
                        debug_assert!(self.scratch[start..].is_sorted_by_key(|&i| &self.dict_items[i as usize].1));
                    }
                    Some(Reader::Bytes(br)) => {
                        let n = br.load(doc, score)?;
                        let mut vals: smallvec::SmallVec<[Box<[u8]>; 4]> = smallvec::SmallVec::new();
                        for i in 0..n {
                            let v = br.value(i);
                            if self.sources[f].strings.as_ref().is_none_or(|s| s.accept(v)) {
                                vals.push(v.into());
                            }
                        }
                        vals.sort();
                        for v in vals {
                            let id = self.intern(f as u32, ItemValue::Bytes(v));
                            self.scratch.push(id);
                        }
                    }
                    Some(Reader::Num(nr)) => {
                        let vals: smallvec::SmallVec<[i64; 8]> = nr.longs(doc, score)?.iter().copied().collect();
                        let mut vals = vals;
                        vals.sort_unstable();
                        for v in vals {
                            if self.sources[f].longs.as_ref().is_none_or(|l| l.accept(v)) {
                                let id = self.intern(f as u32, ItemValue::Long(v));
                                self.scratch.push(id);
                            }
                        }
                    }
                }
                Ok(())
            })();
            self.sources[f].reader = reader;
            r?;
        }
        Ok(())
    }

    fn store(&mut self, ord: u32) -> &mut BucketStore {
        let o = ord as usize;
        if self.buckets.len() <= o {
            self.buckets.resize_with(o + 1, || None);
        }
        self.buckets[o].get_or_insert_with(BucketStore::default)
    }
}

impl Aggregator for FisAgg {
    fn set_segment(&mut self, seg: &SegmentContext) -> Result<()> {
        for s in &mut self.sources {
            s.ord_items.clear();
            s.reader = Some(if s.numeric {
                Reader::Num(s.vs.numeric_reader(seg)?)
            } else {
                match &s.vs.field {
                    Some(f) if s.vs.script.is_none() && s.vs.missing.is_none() && s.vs.has_ordinals() => Reader::Ords(DvOrds::open(seg, f.number)),
                    _ => Reader::Bytes(s.vs.bytes_reader(seg)?),
                }
            });
        }
        self.scorer = match &self.filter {
            Some(w) => w.scorer(seg)?,
            None => None,
        };
        self.last = (u32::MAX, false);
        Ok(())
    }

    fn collect(&mut self, b: Batch<'_>) -> Result<()> {
        for i in 0..b.len() {
            let (doc, ord) = (b.docs[i], b.ords[i]);
            if !self.accepts(doc)? {
                let st = self.store(ord);
                st.filtered += 1;
                st.total_tx += 1;
                continue;
            }
            self.doc_items(doc, b.score(i))?;
            let items = std::mem::take(&mut self.scratch);
            let mut added = 0usize;
            let st = self.store(ord);
            let mut tx: smallvec::SmallVec<[u32; 16]> = smallvec::SmallVec::with_capacity(items.len());
            for &g in &items {
                let next = st.items.len() as u32;
                let local = *st.local.entry(g).or_insert(next);
                if local == next {
                    st.items.push(g);
                    st.item_counts.push(0);
                    added += 24;
                }
                st.item_counts[local as usize] += 1;
                st.total_items += 1;
                tx.push(local);
            }
            st.total_tx += 1;
            match st.tx_ids.get(tx.as_slice()) {
                Some(&id) => st.tx_counts[id as usize] += 1,
                None => {
                    let id = st.transactions.len() as u32;
                    let key: Box<[u32]> = tx.as_slice().into();
                    added += 48 + key.len() * 8;
                    st.tx_ids.insert(key.clone(), id);
                    st.transactions.push(key);
                    st.tx_counts.push(1);
                }
            }
            self.scratch = items;
            self.bytes += added;
        }
        self.tracker.ensure(self.bytes)
    }

    fn build(&mut self, owning: &[u32]) -> Result<Vec<AggValue>> {
        Ok(owning.iter().map(|&o| AggValue::FrequentItemSets(Box::new(self.result(o as usize)))).collect())
    }

    fn build_empty(&self) -> AggValue {
        AggValue::FrequentItemSets(Box::new(FrequentItemSets { fields: self.fields.clone(), store: None, result: None, params: Some(self.params) }))
    }

    fn needs_scores(&self) -> bool {
        false
    }

    fn validate_sort(&self, name: &str, _key: Option<&str>, rest: &[PathElement]) -> Result<()> {
        let tail = if rest.is_empty() { String::new() } else { format!(" [{name}] is not single-bucket.") };
        Err(AggError::illegal_argument(format!(
            "Buckets can only be sorted on a sub-aggregator path that is built out of zero or more single-bucket aggregations within the path and a final single-bucket or a metrics aggregation at the path end.{tail}"
        )))
    }
}

impl FisAgg {
    /// `mapFinalize`: the bucket's store with item values instead of global ids.
    fn result(&self, o: usize) -> FrequentItemSets {
        let store = self.buckets.get(o).and_then(|b| b.as_ref()).map(|st| TransactionStore {
            items: st.items.iter().map(|&g| self.dict_items[g as usize].clone()).collect(),
            item_counts: st.item_counts.clone(),
            total_items: st.total_items,
            transactions: st.transactions.clone(),
            tx_counts: st.tx_counts.clone(),
            total_tx: st.total_tx,
            filtered: st.filtered,
        });
        FrequentItemSets { fields: self.fields.clone(), store, result: None, params: Some(self.params) }
    }
}

/// A transaction store (`ImmutableTransactionStore`): items in id order with counts,
/// transactions as item id lists with counts.
#[derive(Clone, Debug, Default)]
pub struct TransactionStore {
    pub items: Vec<(u32, ItemValue)>,
    pub item_counts: Vec<u64>,
    pub total_items: u64,
    pub transactions: Vec<Box<[u32]>>,
    pub tx_counts: Vec<u64>,
    pub total_tx: u64,
    pub filtered: u64,
}

impl TransactionStore {
    /// `HashBasedTransactionStore.merge`: `other`'s items, then its transactions (ids
    /// remapped, order kept), in id order.
    fn merge(&mut self, other: &TransactionStore, items: &mut hashbrown::HashMap<(u32, ItemValue), u32>, txs: &mut hashbrown::HashMap<Box<[u32]>, u32>) {
        let mut remap = Vec::with_capacity(other.items.len());
        for (item, &count) in other.items.iter().zip(&other.item_counts) {
            let id = match items.get(item) {
                Some(&id) => id,
                None => {
                    let id = self.items.len() as u32;
                    items.insert(item.clone(), id);
                    self.items.push(item.clone());
                    self.item_counts.push(0);
                    id
                }
            };
            self.item_counts[id as usize] += count;
            remap.push(id);
        }
        for (tx, &count) in other.transactions.iter().zip(&other.tx_counts) {
            let key: Box<[u32]> = tx.iter().map(|&i| remap[i as usize]).collect();
            let id = match txs.get(&key) {
                Some(&id) => id,
                None => {
                    let id = self.transactions.len() as u32;
                    txs.insert(key.clone(), id);
                    self.transactions.push(key);
                    self.tx_counts.push(0);
                    id
                }
            };
            self.tx_counts[id as usize] += count;
        }
        self.total_items += other.total_items;
        self.total_tx += other.total_tx;
        self.filtered += other.filtered;
    }

    /// `HashBasedTransactionStore.prune`: drops items whose count isn't above
    /// `(long) (minSupport * totalTransactionCount)`, rewrites transactions over the kept items
    /// sorted by count (descending, then id) and merges the ones that become equal.
    fn prune(&mut self, min_support: f64) {
        let min_count = (min_support * self.total_tx as f64) as i64;
        let mut new_id = vec![u32::MAX; self.items.len()];
        let (mut items, mut counts) = (Vec::new(), Vec::new());
        for (i, (item, &count)) in self.items.iter().zip(&self.item_counts).enumerate() {
            if count as i64 > min_count {
                new_id[i] = items.len() as u32;
                items.push(item.clone());
                counts.push(count);
            }
        }
        let mut tx_ids: hashbrown::HashMap<Box<[u32]>, u32> = hashbrown::HashMap::new();
        let (mut txs, mut tx_counts): (Vec<Box<[u32]>>, Vec<u64>) = (Vec::new(), Vec::new());
        let mut buf: Vec<u32> = Vec::new();
        for (tx, &count) in self.transactions.iter().zip(&self.tx_counts) {
            buf.clear();
            buf.extend(tx.iter().map(|&i| new_id[i as usize]).filter(|&i| i != u32::MAX));
            if buf.is_empty() {
                continue;
            }
            buf.sort_by(|&a, &b| counts[b as usize].cmp(&counts[a as usize]).then(a.cmp(&b)));
            match tx_ids.get(buf.as_slice()) {
                Some(&id) => tx_counts[id as usize] += count,
                None => {
                    let key: Box<[u32]> = buf.as_slice().into();
                    tx_ids.insert(key.clone(), txs.len() as u32);
                    txs.push(key);
                    tx_counts.push(count);
                }
            }
        }
        self.items = items;
        self.item_counts = counts;
        self.transactions = txs;
        self.tx_counts = tx_counts;
    }

    /// Bytes held (memory accounting).
    fn ram_bytes(&self) -> usize {
        let items: usize = self.items.iter().map(|(_, v)| 40 + if let ItemValue::Bytes(b) = v { b.len() } else { 0 }).sum();
        let txs: usize = self.transactions.iter().map(|t| 40 + t.len() * 4).sum();
        items + txs + 64
    }
}

/// One result set (`FrequentItemSet`).
#[derive(Clone, Debug)]
pub struct FrequentItemSet {
    /// Field name → values, in Elasticsearch's `HashMap` iteration order.
    pub key: Vec<(String, Vec<Json>)>,
    pub doc_count: u64,
    pub support: f64,
}

/// What the final reduce needs from the request.
#[derive(Clone, Copy, Debug)]
pub struct FisParams {
    pub minimum_support: f64,
    pub minimum_set_size: usize,
    pub size: usize,
}

/// `InternalItemSetMapReduceAggregation` of the Eclat map-reducer: a transaction store until
/// the final reduce, then the item sets.
#[derive(Clone, Debug)]
pub struct FrequentItemSets {
    pub fields: Vec<ItemField>,
    pub store: Option<TransactionStore>,
    pub result: Option<Vec<FrequentItemSet>>,
    /// Set when bound (shards); the reduce reads it from the leader.
    pub params: Option<FisParams>,
}

impl FrequentItemSets {
    /// `finalizeSampling`: doc counts scale, supports don't.
    pub fn scale(&mut self, f: impl Fn(u64) -> u64) {
        if let Some(r) = &mut self.result {
            for s in r {
                s.doc_count = f(s.doc_count);
            }
        }
    }
}

impl Internal for FrequentItemSets {
    fn type_name(&self) -> &'static str {
        "frequent_item_sets"
    }

    fn class_name(&self) -> &'static str {
        "InternalItemSetMapReduceAggregation"
    }

    fn must_reduce_single(&self) -> bool {
        true
    }

    fn reduce(list: Vec<Self>, ctx: &mut ReduceCtx<'_>) -> Result<Self> {
        let fields = list[0].fields.clone();
        let params = list.iter().find_map(|l| l.params);
        let mut store = TransactionStore::default();
        let (mut items, mut txs) = (hashbrown::HashMap::new(), hashbrown::HashMap::new());
        for part in list.iter().filter_map(|l| l.store.as_ref()) {
            store.merge(part, &mut items, &mut txs);
        }
        drop((items, txs));
        if let Some(b) = &ctx.opts.budget {
            // The merged store is transient: check it fits, don't keep it reserved.
            let bytes = store.ram_bytes() as u64;
            b.reserve(bytes, "<reduce_aggs>")?;
            b.release(bytes);
        }
        if !ctx.is_final() {
            return Ok(FrequentItemSets { fields, store: Some(store), result: None, params });
        }
        let Some(params) = params else {
            return Err(AggError::new(500, "illegal_state_exception", "frequent_item_sets reduced without its parameters"));
        };
        store.prune(params.minimum_support);
        let sets = eclat(&store, &params, &fields);
        Ok(FrequentItemSets { fields, store: None, result: Some(sets), params: Some(params) })
    }

    fn render(&self, out: &mut Json, _rc: &RenderCtx) {
        let Some(sets) = &self.result else { return };
        let buckets = sets
            .iter()
            .map(|s| {
                let mut key = Json::obj();
                for (name, values) in &s.key {
                    key.push(name, Json::Arr(values.clone()));
                }
                Json::obj().with("key", key).with("doc_count", Json::Int(s.doc_count as i64)).with("support", Json::Float(s.support))
            })
            .collect();
        out.push("buckets", Json::Arr(buckets));
    }

    fn property(&self, name: &str, path: &[String]) -> Result<Prop<'_>> {
        Err(AggError::illegal_argument(format!("path not supported for [{name}]: {}", java_list(path))))
    }

    fn ram_bytes(&self) -> usize {
        self.store.as_ref().map(|s| s.ram_bytes()).unwrap_or(64) + self.result.as_ref().map(|r| r.len() * 96).unwrap_or(0)
    }
}

/// `ItemSetBitSet`: a `java.util.BitSet` variant with a cached cardinality.
#[derive(Clone, Debug, Default)]
struct ItemSetBitSet {
    words: Vec<u64>,
    in_use: usize,
    cardinality: usize,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum SetRelation {
    DisjointOrIntersect,
    SubSet,
    SuperSet,
    Equal,
}

impl ItemSetBitSet {
    fn set(&mut self, bit: usize) {
        let w = bit >> 6;
        if self.in_use <= w {
            if self.words.len() <= w {
                self.words.resize(w + 1, 0);
            }
            self.in_use = w + 1;
        }
        let old = self.words[w];
        self.words[w] |= 1u64 << (bit & 63);
        if old != self.words[w] {
            self.cardinality += 1;
        }
    }

    fn clear_bit(&mut self, bit: usize) {
        let w = bit >> 6;
        if w >= self.in_use {
            return;
        }
        let old = self.words[w];
        self.words[w] &= !(1u64 << (bit & 63));
        if old != self.words[w] {
            self.cardinality -= 1;
        }
        while self.in_use > 0 && self.words[self.in_use - 1] == 0 {
            self.in_use -= 1;
        }
    }

    fn clear(&mut self) {
        while self.in_use > 0 {
            self.in_use -= 1;
            self.words[self.in_use] = 0;
        }
        self.cardinality = 0;
    }

    fn reset_from(&mut self, other: &ItemSetBitSet) {
        if self.words.len() < other.in_use {
            self.words.resize(other.in_use, 0);
        }
        self.words[..other.in_use].copy_from_slice(&other.words[..other.in_use]);
        self.cardinality = other.cardinality;
        self.in_use = other.in_use;
    }

    fn next_set_bit(&self, from: usize) -> Option<usize> {
        let mut u = from >> 6;
        if u >= self.in_use {
            return None;
        }
        let mut word = self.words[u] & (u64::MAX << (from & 63));
        loop {
            if word != 0 {
                return Some(u * 64 + word.trailing_zeros() as usize);
            }
            u += 1;
            if u == self.in_use {
                return None;
            }
            word = self.words[u];
        }
    }

    /// `setRelation`, with its asymmetric checks.
    fn relation(&self, set: &ItemSetBitSet) -> SetRelation {
        let covers = |big: &ItemSetBitSet, small: &ItemSetBitSet| (0..small.in_use).rev().all(|i| (small.words[i] & big.words[i]) == small.words[i]);
        if self.in_use > set.in_use || self.cardinality > set.cardinality {
            if covers(self, set) { SetRelation::SuperSet } else { SetRelation::DisjointOrIntersect }
        } else if self.in_use < set.in_use || self.cardinality < set.cardinality {
            if covers(set, self) { SetRelation::SubSet } else { SetRelation::DisjointOrIntersect }
        } else if covers(set, self) {
            SetRelation::Equal
        } else {
            SetRelation::DisjointOrIntersect
        }
    }

    /// `ItemSetBitSet.compare`: larger sets first among equals... (as a signed comparison).
    fn compare(a: &ItemSetBitSet, b: &ItemSetBitSet) -> i32 {
        if a.cardinality != b.cardinality {
            return if a.cardinality > b.cardinality { 1 } else { -1 };
        }
        if a.in_use != b.in_use {
            return if a.in_use < b.in_use { 1 } else { -1 };
        }
        for i in 0..a.in_use {
            if a.words[i] != b.words[i] {
                return if (a.words[i] as i64) < (b.words[i] as i64) { 1 } else { -1 };
            }
        }
        0
    }
}

/// `TransactionsLookupTable`: each top transaction's items as bit positions.
struct LookupTable {
    words: Vec<u64>,
    starts: Vec<usize>,
}

impl LookupTable {
    #[inline]
    fn is_subset_of(&self, row: usize, set: &ItemSetBitSet) -> bool {
        let start = self.starts[row];
        let in_use = self.starts[row + 1] - start;
        if set.in_use > in_use {
            return false;
        }
        (0..set.in_use).rev().all(|i| (self.words[start + i] & set.words[i]) == set.words[i])
    }
}

/// `ItemSetTraverser`: depth-first walk over prefixes of the count-ordered items.
struct ItemSetTraverser {
    top_items: usize,
    /// Per depth, the next index into the top items.
    iterators: Vec<usize>,
    item_ids: Vec<u32>,
    positions: Vec<usize>,
    vector: ItemSetBitSet,
    parent: ItemSetBitSet,
    /// `stackPosition`, -1 when done.
    depth: isize,
}

impl ItemSetTraverser {
    fn new(top_items: usize) -> ItemSetTraverser {
        ItemSetTraverser {
            top_items,
            iterators: vec![0],
            item_ids: Vec::new(),
            positions: Vec::new(),
            vector: ItemSetBitSet::default(),
            parent: ItemSetBitSet::default(),
            depth: 0,
        }
    }

    fn at_leaf(&self) -> bool {
        self.depth >= 0 && self.iterators[self.depth as usize] >= self.top_items
    }

    fn pop_level(&mut self) {
        self.item_ids.pop();
        let p = self.positions.pop().expect("position");
        self.parent.clear_bit(p);
        self.vector.clear_bit(p);
    }

    /// `next()`: the next item set, or `false` when the walk is over.
    fn next(&mut self, top: &[u32]) -> bool {
        if self.depth == -1 {
            return false;
        }
        let item;
        loop {
            let d = self.depth as usize;
            if self.iterators[d] < self.top_items {
                item = top[self.iterators[d]];
                self.iterators[d] += 1;
                break;
            }
            self.depth -= 1;
            if self.depth == -1 {
                return false;
            }
            self.pop_level();
        }
        let d = self.depth as usize;
        let position = self.iterators[d];
        if self.iterators.len() == d + 1 {
            self.iterators.push(position);
        } else {
            self.iterators[d + 1] = position;
        }
        self.item_ids.push(item);
        if let Some(&last) = self.positions.last() {
            self.parent.set(last);
        }
        self.positions.push(position);
        self.vector.set(position);
        self.depth += 1;
        true
    }

    fn item_id(&self) -> u32 {
        *self.item_ids.last().expect("item")
    }

    fn number_of_items(&self) -> usize {
        self.depth.max(0) as usize
    }

    fn prune(&mut self) {
        if self.depth == -1 {
            return;
        }
        self.depth -= 1;
        if self.depth == -1 {
            return;
        }
        self.pop_level();
    }
}

/// `CountingItemSetTraverser`.
struct CountingTraverser<'a> {
    store: &'a TransactionStore,
    top_items: &'a [u32],
    inner: ItemSetTraverser,
    top_tx: Vec<u32>,
    lookup: LookupTable,
    cache_depth: usize,
    cache_tx: usize,
    skip_counts: Vec<u64>,
    skip_list: Vec<u64>,
    occurrences: Vec<u64>,
    visited: Vec<bool>,
}

#[inline]
fn bit_get(bits: &[u64], i: usize) -> bool {
    bits[i >> 6] & (1u64 << (i & 63)) != 0
}

#[inline]
fn bit_set(bits: &mut [u64], i: usize) {
    bits[i >> 6] |= 1u64 << (i & 63);
}

impl<'a> CountingTraverser<'a> {
    fn new(store: &'a TransactionStore, top_items: &'a [u32], cache_depth: usize, cache_tx: usize) -> CountingTraverser<'a> {
        // `getTopTransactionIds`: by count, descending (a stable sort).
        let mut top_tx: Vec<u32> = (0..store.transactions.len() as u32).filter(|&t| store.tx_counts[t as usize] > 0).collect();
        top_tx.sort_by(|&a, &b| store.tx_counts[b as usize].cmp(&store.tx_counts[a as usize]));
        // `createLookupTableByTopTransactions`: bit `1 + position` per item.
        let mut positions = vec![0usize; store.items.len()];
        for (i, &id) in top_items.iter().enumerate() {
            positions[id as usize] = i;
        }
        let mut lookup = LookupTable { words: Vec::new(), starts: vec![0] };
        let mut bits = ItemSetBitSet::default();
        for &t in &top_tx {
            bits.clear();
            for &item in store.transactions[t as usize].iter() {
                bits.set(1 + positions[item as usize]);
            }
            lookup.words.extend_from_slice(&bits.words[..bits.in_use]);
            lookup.starts.push(lookup.words.len());
        }
        let skip_bits = (cache_depth - 1) * cache_tx;
        CountingTraverser {
            store,
            top_items,
            inner: ItemSetTraverser::new(top_items.len()),
            top_tx,
            lookup,
            cache_depth,
            cache_tx,
            skip_counts: vec![0; cache_depth - 1],
            skip_list: vec![0; skip_bits.div_ceil(64)],
            occurrences: vec![0; 10],
            visited: Vec::new(),
        }
    }

    /// `BitSet.clear(from, to)`, a word at a time.
    fn clear_range(&mut self, from: usize, to: usize) {
        if from >= to {
            return;
        }
        let (first, last) = (from >> 6, (to - 1) >> 6);
        let head = u64::MAX << (from & 63);
        let tail = u64::MAX >> (63 - ((to - 1) & 63));
        if first == last {
            self.skip_list[first] &= !(head & tail);
            return;
        }
        self.skip_list[first] &= !head;
        self.skip_list[first + 1..last].fill(0);
        self.skip_list[last] &= !tail;
    }

    fn next(&mut self, early_stop_min: u64) -> bool {
        if !self.inner.next(self.top_items) {
            return false;
        }
        let total = self.store.total_tx;
        let depth = self.inner.number_of_items();
        let single = self.store.item_counts[self.inner.item_id() as usize];
        if depth == 1 {
            self.occurrences[0] = single;
            return true;
        }
        if single < early_stop_min {
            self.remember(depth, single);
            return true;
        }
        let cache = self.cache_tx;
        if depth < self.cache_depth {
            let mut skip = self.skip_counts[depth - 2];
            let mut reachable = total as i64 - skip as i64;
            self.clear_range((depth - 1) * cache, depth * cache);
            let mut pos = 0usize;
            let mut occurrences = 0u64;
            while pos < self.top_tx.len() {
                if pos < cache && bit_get(&self.skip_list, cache * (depth - 2) + pos) {
                    bit_set(&mut self.skip_list, cache * (depth - 1) + pos);
                    pos += 1;
                    continue;
                }
                let count = self.store.tx_counts[self.top_tx[pos] as usize];
                if self.lookup.is_subset_of(pos, &self.inner.vector) {
                    occurrences += count;
                } else if pos < cache {
                    skip += count;
                    bit_set(&mut self.skip_list, cache * (depth - 1) + pos);
                }
                reachable -= count as i64;
                if reachable + (occurrences as i64) < early_stop_min as i64 {
                    break;
                }
                pos += 1;
            }
            self.skip_counts[depth - 1] = skip;
            self.remember(depth, occurrences);
            return true;
        }
        let skip = self.skip_counts[self.cache_depth - 2];
        let mut reachable = total as i64 - skip as i64;
        let mut occurrences = 0u64;
        for (n, &t) in self.top_tx.iter().enumerate() {
            if n < cache && bit_get(&self.skip_list, cache * (self.cache_depth - 2) + n) {
                continue;
            }
            let count = self.store.tx_counts[t as usize];
            if self.lookup.is_subset_of(n, &self.inner.vector) {
                occurrences += count;
            }
            reachable -= count as i64;
            if reachable + (occurrences as i64) < early_stop_min as i64 {
                break;
            }
        }
        self.remember(depth, occurrences);
        true
    }

    fn remember(&mut self, index: usize, occurrences: u64) {
        if self.occurrences.len() < index {
            self.occurrences.resize(index + 10, 0);
        }
        self.occurrences[index - 1] = occurrences;
        if self.visited.len() >= index {
            self.visited[index - 1] = false;
        }
    }

    fn count(&self) -> u64 {
        let n = self.inner.number_of_items();
        if n > 0 { self.occurrences[n - 1] } else { 0 }
    }

    fn parent_count(&self) -> u64 {
        let n = self.inner.number_of_items();
        if n > 1 { self.occurrences[n - 2] } else { 0 }
    }

    fn visited(&self, i: usize) -> bool {
        self.visited.get(i).copied().unwrap_or(false)
    }

    fn has_been_visited(&self) -> bool {
        let n = self.inner.number_of_items();
        if n > 0 { self.visited(n - 1) } else { true }
    }

    fn has_parent_been_visited(&self) -> bool {
        let n = self.inner.number_of_items();
        if n > 1 { self.visited(n - 2) } else { true }
    }

    fn set_parent_visited(&mut self) {
        let n = self.inner.number_of_items();
        if n > 1 {
            if self.visited.len() < n - 1 {
                self.visited.resize(n - 1, false);
            }
            self.visited[n - 2] = true;
        }
    }

    fn prune_to_next_main_branch(&mut self) {
        let this = self.count();
        while self.inner.number_of_items() > 1 && self.count() == this {
            self.inner.prune();
        }
    }
}

/// A candidate of the collector (`FrequentItemSetCandidate`), addressed by arena index (its
/// Java identity).
struct Candidate {
    items: ItemSetBitSet,
    doc_count: u64,
}

/// `FrequentItemSetCollector` over a Lucene `PriorityQueue` (1-based heap, smallest on top).
struct Collector {
    arena: Vec<Candidate>,
    heap: Vec<usize>,
    size: usize,
    min: u64,
    /// `frequentItemsByCount` (lists in insertion order; removal by identity).
    by_count: hashbrown::HashMap<u64, Vec<usize>>,
    spare: usize,
}

impl Collector {
    fn new(size: usize, min: u64) -> Collector {
        Collector {
            arena: vec![Candidate { items: ItemSetBitSet::default(), doc_count: 0 }],
            heap: vec![usize::MAX],
            size,
            min,
            by_count: hashbrown::HashMap::new(),
            spare: 0,
        }
    }

    fn len(&self) -> usize {
        self.heap.len() - 1
    }

    fn top(&self) -> Option<usize> {
        self.heap.get(1).copied()
    }

    fn less_than(&self, a: usize, b: usize) -> bool {
        let (x, y) = (&self.arena[a], &self.arena[b]);
        if x.doc_count == y.doc_count { ItemSetBitSet::compare(&x.items, &y.items) < 0 } else { x.doc_count < y.doc_count }
    }

    fn up_heap(&mut self, orig: usize) -> bool {
        let mut i = orig;
        let node = self.heap[i];
        let mut j = i >> 1;
        while j > 0 && self.less_than(node, self.heap[j]) {
            self.heap[i] = self.heap[j];
            i = j;
            j >>= 1;
        }
        self.heap[i] = node;
        i != orig
    }

    fn down_heap(&mut self, mut i: usize) {
        let size = self.len();
        let node = self.heap[i];
        let mut j = i << 1;
        let mut k = j + 1;
        if k <= size && self.less_than(self.heap[k], self.heap[j]) {
            j = k;
        }
        while j <= size && self.less_than(self.heap[j], node) {
            self.heap[i] = self.heap[j];
            i = j;
            j = i << 1;
            k = j + 1;
            if k <= size && self.less_than(self.heap[k], self.heap[j]) {
                j = k;
            }
        }
        self.heap[i] = node;
    }

    /// `insertWithOverflow`: the evicted (or rejected) candidate.
    fn insert_with_overflow(&mut self, e: usize) -> Option<usize> {
        if self.len() < self.size {
            self.heap.push(e);
            let n = self.len();
            self.up_heap(n);
            None
        } else if self.len() > 0 && !self.less_than(e, self.heap[1]) {
            let ret = self.heap[1];
            self.heap[1] = e;
            self.down_heap(1);
            Some(ret)
        } else {
            Some(e)
        }
    }

    /// `PriorityQueue.remove(element)` (identity).
    fn remove(&mut self, e: usize) {
        let size = self.len();
        let Some(i) = (1..=size).find(|&i| self.heap[i] == e) else { return };
        self.heap[i] = self.heap[size];
        self.heap.pop();
        if i < self.heap.len() && !self.up_heap(i) {
            self.down_heap(i);
        }
    }

    fn pop(&mut self) -> Option<usize> {
        if self.len() == 0 {
            return None;
        }
        let result = self.heap[1];
        let last = self.heap.pop().expect("non-empty");
        if self.len() > 0 {
            self.heap[1] = last;
            self.down_heap(1);
        }
        Some(result)
    }

    fn remove_from_by_count(&mut self, c: usize) {
        let dc = self.arena[c].doc_count;
        if let Some(list) = self.by_count.get_mut(&dc) {
            if list.len() == 1 {
                self.by_count.remove(&dc);
            } else if let Some(p) = list.iter().position(|&x| x == c) {
                list.remove(p);
            }
        }
    }

    fn bar(&self) -> u64 {
        if self.len() < self.size { self.min } else { self.arena[self.heap[1]].doc_count }
    }

    /// `add(itemSet, docCount)`: the new minimum count.
    fn add(&mut self, items: &ItemSetBitSet, doc_count: u64) -> u64 {
        if let Some(t) = self.top()
            && self.len() == self.size
        {
            let top = &self.arena[t];
            if doc_count < top.doc_count || (doc_count == top.doc_count && items.cardinality <= top.items.cardinality) {
                return top.doc_count;
            }
        }
        if let Some(list) = self.by_count.get(&doc_count).cloned() {
            for other in list {
                match items.relation(&self.arena[other].items) {
                    SetRelation::SuperSet => {
                        self.remove_from_by_count(other);
                        self.remove(other);
                        break;
                    }
                    SetRelation::SubSet => return self.bar(),
                    _ => {}
                }
            }
        }
        let spare = self.spare;
        self.arena[spare].items.reset_from(items);
        self.arena[spare].doc_count = doc_count;
        match self.insert_with_overflow(spare) {
            Some(removed) => {
                self.remove_from_by_count(removed);
                self.spare = removed;
            }
            None => {
                self.arena.push(Candidate { items: ItemSetBitSet::default(), doc_count: 0 });
                self.spare = self.arena.len() - 1;
            }
        }
        self.by_count.entry(doc_count).or_default().push(spare);
        self.bar()
    }
}

/// `EclatMapReducer.eclat` over a pruned store.
fn eclat(store: &TransactionStore, params: &FisParams, fields: &[ItemField]) -> Vec<FrequentItemSet> {
    let total = store.total_tx;
    let mut min_count = (total as f64 * params.minimum_support).ceil() as u64;
    // `getTopItemIds`: items with a count, by count descending then id.
    let mut top: Vec<u32> = (0..store.items.len() as u32).filter(|&i| store.item_counts[i as usize] > 0).collect();
    top.sort_by(|&a, &b| store.item_counts[b as usize].cmp(&store.item_counts[a as usize]).then(a.cmp(&b)));
    let cache_tx = MAX_BITSET_CACHE_NUMBER_OF_TRANSACTIONS.min(total) as usize;
    let mut t = CountingTraverser::new(store, &top, BITSET_CACHE_TRAVERSAL_DEPTH, cache_tx);
    let mut collector = Collector::new(params.size, min_count);
    let min_set = params.minimum_set_size;
    while t.next(min_count) {
        if t.count() < min_count {
            t.inner.prune();
            if t.inner.at_leaf() && !t.has_been_visited() && t.count() >= min_count && t.inner.vector.cardinality >= min_set {
                min_count = collector.add(&t.inner.vector, t.count());
            }
            if t.inner.at_leaf() {
                t.prune_to_next_main_branch();
            }
            continue;
        }
        if !t.has_parent_been_visited() && t.inner.vector.cardinality > min_set && t.count() < t.parent_count() {
            let (parent, count) = (t.inner.parent.clone(), t.parent_count());
            min_count = collector.add(&parent, count);
        }
        t.set_parent_visited();
        if t.inner.at_leaf() && t.inner.vector.cardinality >= min_set {
            let (v, count) = (t.inner.vector.clone(), t.count());
            min_count = collector.add(&v, count);
        }
        if t.inner.at_leaf() {
            t.inner.prune();
            t.prune_to_next_main_branch();
        }
    }
    // `finalizeAndGetResults`: popped smallest first, reported largest first.
    let mut popped = Vec::with_capacity(collector.len());
    while let Some(c) = collector.pop() {
        popped.push(c);
    }
    popped.reverse();
    popped.into_iter().map(|c| to_item_set(&collector.arena[c], store, &top, fields)).collect()
}

/// `FrequentItemSetCandidate.toFrequentItemSet`.
fn to_item_set(c: &Candidate, store: &TransactionStore, top: &[u32], fields: &[ItemField]) -> FrequentItemSet {
    let mut names: Vec<&str> = Vec::new();
    let mut values: Vec<Vec<Json>> = Vec::new();
    let mut pos = c.items.next_set_bit(0);
    while let Some(p) = pos.filter(|&p| p > 0) {
        let (field, value) = &store.items[top[p - 1] as usize];
        let f = &fields[*field as usize];
        let formatted = match value {
            ItemValue::Bytes(b) => f.format.format_bytes(b),
            ItemValue::Long(v) => f.format.format_long(*v),
        };
        let k = match names.iter().position(|n| *n == f.name) {
            Some(k) => k,
            None => {
                names.push(&f.name);
                values.push(Vec::new());
                names.len() - 1
            }
        };
        values[k].push(formatted);
        pos = c.items.next_set_bit(p + 1);
    }
    // `new HashMap<>()` filled with `put` (new keys append to their bin), in bit order.
    let mut map = crate::util::JavaHashMapModel::new(None);
    for (k, name) in names.iter().enumerate() {
        map.put_new(k, crate::util::JavaHashMapModel::spread(crate::util::java_string_hash_code(name)));
    }
    let order = map.order();
    let key = order.into_iter().map(|k| (names[k].to_string(), std::mem::take(&mut values[k]))).collect();
    FrequentItemSet { key, doc_count: c.doc_count, support: c.doc_count as f64 / store.total_tx as f64 }
}

impl Ord for ItemValue {
    fn cmp(&self, other: &Self) -> Ordering {
        match (self, other) {
            (ItemValue::Bytes(a), ItemValue::Bytes(b)) => a.cmp(b),
            (ItemValue::Long(a), ItemValue::Long(b)) => a.cmp(b),
            (ItemValue::Bytes(_), ItemValue::Long(_)) => Ordering::Less,
            (ItemValue::Long(_), ItemValue::Bytes(_)) => Ordering::Greater,
        }
    }
}

impl PartialOrd for ItemValue {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn bits(list: &[usize]) -> ItemSetBitSet {
        let mut b = ItemSetBitSet::default();
        for &i in list {
            b.set(i);
        }
        b
    }

    #[test]
    fn skip_list_range_clear() {
        let store = TransactionStore::default();
        let top: [u32; 0] = [];
        let mut t = CountingTraverser::new(&store, &top, 4, 100);
        for (from, to) in [(0, 0), (3, 4), (0, 64), (5, 130), (64, 128), (63, 65), (100, 300)] {
            t.skip_list.fill(u64::MAX);
            t.clear_range(from, to);
            for i in 0..t.skip_list.len() * 64 {
                assert_eq!(bit_get(&t.skip_list, i), !(from..to).contains(&i), "{from}..{to} bit {i}");
            }
        }
    }

    #[test]
    fn bitset_relations_and_order() {
        assert_eq!(bits(&[1, 2, 3]).relation(&bits(&[1, 3])), SetRelation::SuperSet);
        assert_eq!(bits(&[1, 3]).relation(&bits(&[1, 2, 3])), SetRelation::SubSet);
        assert_eq!(bits(&[1, 3]).relation(&bits(&[1, 3])), SetRelation::Equal);
        assert_eq!(bits(&[1, 4]).relation(&bits(&[1, 3])), SetRelation::DisjointOrIntersect);
        assert_eq!(bits(&[70]).relation(&bits(&[1, 2])), SetRelation::DisjointOrIntersect);
        // More items compare greater; among equal sizes, the lower words win.
        assert_eq!(ItemSetBitSet::compare(&bits(&[1, 2]), &bits(&[1])), 1);
        assert_eq!(ItemSetBitSet::compare(&bits(&[1, 2]), &bits(&[1, 3])), 1);
        assert_eq!(ItemSetBitSet::compare(&bits(&[63]), &bits(&[1])), 1);
        let mut b = bits(&[1, 65]);
        b.clear_bit(65);
        assert_eq!((b.in_use, b.cardinality, b.next_set_bit(0)), (1, 1, Some(1)));
    }

    fn store(txs: &[&[u32]], items: u32) -> TransactionStore {
        let mut s = TransactionStore { items: (0..items).map(|i| (0, ItemValue::Long(i as i64))).collect(), item_counts: vec![0; items as usize], ..Default::default() };
        let mut ids: hashbrown::HashMap<Box<[u32]>, u32> = hashbrown::HashMap::new();
        for t in txs {
            for &i in t.iter() {
                s.item_counts[i as usize] += 1;
                s.total_items += 1;
            }
            s.total_tx += 1;
            let key: Box<[u32]> = (*t).into();
            match ids.get(&key) {
                Some(&id) => s.tx_counts[id as usize] += 1,
                None => {
                    ids.insert(key.clone(), s.transactions.len() as u32);
                    s.transactions.push(key);
                    s.tx_counts.push(1);
                }
            }
        }
        s
    }

    /// Random transaction sets against Elasticsearch 9.5.5's answers
    /// (`tests/fixtures/aggs-fis-rounds.json`, one long field, `minimum_support` 0.1, `size`
    /// 1000). Its Eclat isn't a complete closed-set miner, so this pins its exact output.
    #[test]
    fn eclat_matches_elasticsearch() {
        let path = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/aggs-fis-rounds.json");
        let want: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap();
        let mut rng = 0x2545F4914F6CDD1Du64;
        let mut next = || {
            rng ^= rng << 13;
            rng ^= rng >> 7;
            rng ^= rng << 17;
            rng
        };
        let mut compared = 0;
        for round in 0..40u64 {
            let n_items = 3 + (round % 5) as u32;
            let mut txs: Vec<Vec<u32>> = Vec::new();
            for _ in 0..(20 + next() % 60) {
                txs.push((0..n_items).filter(|i| next() % 100 < 70 - (*i as u64) * 9).collect());
            }
            // Items numbered in order of appearance, as a shard's store does.
            let mut ids = vec![u32::MAX; n_items as usize];
            let mut values = Vec::new();
            let mapped: Vec<Vec<u32>> = txs
                .iter()
                .map(|t| {
                    t.iter()
                        .map(|&v| {
                            if ids[v as usize] == u32::MAX {
                                ids[v as usize] = values.len() as u32;
                                values.push(v);
                            }
                            ids[v as usize]
                        })
                        .collect()
                })
                .collect();
            let refs: Vec<&[u32]> = mapped.iter().map(|t| t.as_slice()).collect();
            let mut s = store(&refs, values.len() as u32);
            for (i, &v) in values.iter().enumerate() {
                s.items[i].1 = ItemValue::Long(v as i64);
            }
            let min_set = 1 + (round % 2) as usize;
            let fields = vec![ItemField { name: "f".into(), format: DocValueFormat::Raw }];
            s.prune(0.1);
            let got = eclat(&s, &FisParams { minimum_support: 0.1, minimum_set_size: min_set, size: 1000 }, &fields);
            assert!(got.windows(2).all(|w| w[0].doc_count >= w[1].doc_count));
            // Equal item counts are ordered by Elasticsearch's hash slots (a per-JVM random
            // seed), which changes the sets found: only rounds without ties are comparable.
            let mut counts = s.item_counts.clone();
            counts.sort_unstable();
            if counts.windows(2).any(|w| w[0] == w[1]) {
                continue;
            }
            compared += 1;
            let mut got_sets: Vec<(Vec<i64>, u64)> = got
                .iter()
                .map(|g| {
                    let mut v: Vec<i64> = g.key[0].1.iter().map(|j| j.as_i64().unwrap()).collect();
                    v.sort();
                    (v, g.doc_count)
                })
                .collect();
            got_sets.sort();
            let want_sets: Vec<(Vec<i64>, u64)> = want[round as usize]
                .as_array()
                .unwrap()
                .iter()
                .map(|e| (e[0].as_array().unwrap().iter().map(|v| v.as_i64().unwrap()).collect(), e[1].as_u64().unwrap()))
                .collect();
            assert_eq!(got_sets, want_sets, "round {round}");
        }
        assert!(compared >= 20, "only {compared} rounds without ties");
    }
}
