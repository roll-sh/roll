//! `filters` (one bucket per filter, plus an optional "other" bucket) and `adjacency_matrix`
//! (one bucket per filter and per non-empty pairwise intersection).
//!
//! Each filter is a query bound to the shard once; per segment its scorer advances along the
//! ascending docs it is asked about, and the answers of one doc are reused when the doc comes
//! again (multi-valued parents hand the same doc with several ordinals). Bucket ordinals are
//! `owning * slots + slot`.

use std::sync::Arc;

use search_index::{ScoreMode, Scorer, SegmentContext, Weight};

use crate::collect::{Aggregator, Batch, BindCtx, Subs, grow};
use crate::error::{AggError, Result};
use crate::internal::{AggValue, Aggs, Internal, MultiBuckets, Prop, ReduceCtx, RenderCtx, multi_bucket_property};
use crate::json::Json;
use crate::request::AggDef;
use crate::values::DocCounter;
use crate::xparser::{Field, Token, Tokens, XParser};

/// A parsed `filters`.
#[derive(Clone, Debug)]
pub struct FiltersSpec {
    /// `(key, query)`; keyed filters are sorted by key.
    pub filters: Vec<(String, Json)>,
    /// Filters given as an object (named buckets).
    pub keyed: bool,
    /// `keyed` in the request: render named buckets as an object (true) or a list.
    pub keyed_bucket: bool,
    pub other_bucket: bool,
    pub other_bucket_key: String,
}

/// `FiltersAggregationBuilder.parse`.
pub fn parse_filters(p: &mut XParser<'_>, name: &str, check_query: &mut dyn FnMut(&XParser<'_>) -> Result<()>) -> Result<FiltersSpec> {
    let mut filters: Vec<(String, Json)> = Vec::new();
    let mut cur = String::new();
    let mut other_bucket_key: Option<String> = None;
    let mut other_bucket: Option<bool> = None;
    let mut keyed_bucket: Option<bool> = None;
    let mut keyed = false;
    while let Some(t) = p.next_token() {
        match t {
            Token::EndObject => break,
            Token::FieldName => cur = p.current_name().unwrap_or("").to_string(),
            Token::Boolean => match cur.as_str() {
                "other_bucket" => other_bucket = Some(p.boolean_value()?),
                "keyed" => keyed_bucket = Some(p.boolean_value()?),
                _ => return Err(p.parsing_error(format!("Unknown key for a VALUE_BOOLEAN in [{name}]: [{cur}]."))),
            },
            Token::String => match cur.as_str() {
                "other_bucket_key" => other_bucket_key = Some(p.text()?),
                _ => return Err(p.parsing_error(format!("Unknown key for a VALUE_STRING in [{name}]: [{cur}]."))),
            },
            Token::StartObject => {
                if cur != "filters" {
                    return Err(p.parsing_error(format!("Unknown key for a START_OBJECT in [{name}]: [{cur}].")));
                }
                let mut key = String::new();
                loop {
                    match p.next_token() {
                        Some(Token::EndObject) | None => break,
                        Some(Token::FieldName) => key = p.current_name().unwrap_or("").to_string(),
                        Some(_) => {
                            check_query(p)?;
                            filters.push((key.clone(), p.take_value()));
                        }
                    }
                }
                keyed = true;
            }
            Token::StartArray => {
                if cur != "filters" {
                    return Err(p.parsing_error(format!("Unknown key for a START_ARRAY in [{name}]: [{cur}].")));
                }
                let mut list = Vec::new();
                loop {
                    match p.next_token() {
                        Some(Token::EndArray) | None => break,
                        Some(_) => {
                            check_query(p)?;
                            list.push(p.take_value());
                        }
                    }
                }
                for (i, q) in list.into_iter().enumerate() {
                    filters.push((i.to_string(), q));
                }
            }
            t => return Err(p.parsing_error(format!("Unknown key for a {t} in [{name}]: [{cur}]."))),
        }
    }
    if filters.is_empty() {
        return Err(AggError::illegal_argument("[filters] cannot be empty."));
    }
    if keyed {
        // A stable sort by key, as `List.sort(Comparator.comparing(KeyedFilter::key))`
        // (UTF-16 order).
        filters.sort_by(|a, b| a.0.encode_utf16().cmp(b.0.encode_utf16()));
    }
    if other_bucket.is_none() && other_bucket_key.is_some() {
        other_bucket = Some(true);
    }
    Ok(FiltersSpec {
        filters,
        keyed,
        keyed_bucket: if keyed { keyed_bucket.unwrap_or(true) } else { true },
        other_bucket: other_bucket.unwrap_or(false),
        other_bucket_key: other_bucket_key.unwrap_or_else(|| "_other_".into()),
    })
}

/// A parsed `adjacency_matrix`.
#[derive(Clone, Debug)]
pub struct AdjacencySpec {
    pub filters: Vec<(String, Json)>,
    pub separator: String,
}

pub fn parse_adjacency(p: &mut XParser<'_>, check_query: &mut dyn FnMut(&XParser<'_>) -> Result<()>) -> Result<AdjacencySpec> {
    let fields = [Field::new("separator", Tokens::STRING), Field::new("filters", Tokens::OBJECT_ARRAY)];
    let mut filters: Vec<(String, Json)> = Vec::new();
    let mut separator = "&".to_string();
    p.object("adjacency_matrix", &fields, |p, i| {
        if i == 0 {
            separator = p.text()?;
            return Ok(());
        }
        if p.token() != Token::StartObject {
            return Err(AggError::illegal_argument("[adjacency_matrix] filters doesn't support values of type: START_ARRAY"));
        }
        loop {
            match p.next_token() {
                Some(Token::EndObject) | None => break,
                Some(Token::FieldName) => {
                    let key = p.current_name().unwrap_or("").to_string();
                    p.next_token();
                    check_query(p)?;
                    filters.push((key, p.take_value()));
                }
                Some(_) => {}
            }
        }
        Ok(())
    })?;
    filters.sort_by(|a, b| a.0.encode_utf16().cmp(b.0.encode_utf16()));
    Ok(AdjacencySpec { filters, separator })
}

struct Filter {
    weight: Box<dyn Weight>,
    scorer: Option<Box<dyn Scorer>>,
}

impl Filter {
    #[inline]
    fn matches(&mut self, doc: u32) -> bool {
        let Some(s) = self.scorer.as_mut() else { return false };
        let mut d = s.doc();
        if d == u32::MAX || d < doc {
            d = s.advance(doc);
        }
        d == doc
    }
}

fn bind_filters(list: &[(String, Json)], ctx: &BindCtx<'_>) -> Result<Vec<Filter>> {
    let mut out = Vec::with_capacity(list.len());
    for (_, q) in list {
        let query = ctx.lookup.query(q)?;
        out.push(Filter { weight: ctx.searcher.create_weight(&query, ScoreMode::CompleteNoScores)?, scorer: None });
    }
    Ok(out)
}

/// `filters` / `adjacency_matrix` aggregator.
struct FiltersAgg {
    keys: Vec<String>,
    filters: Vec<Filter>,
    /// `filters`: Some(other key) with an other bucket; adjacency: None.
    spec: Option<Arc<FiltersSpec>>,
    separator: Option<String>,
    slots: u32,
    counts: Vec<u64>,
    subs: Subs,
    doc_counter: DocCounter,
    doc_count_field: Option<u32>,
    last_doc: u32,
    hits: Vec<bool>,
}

pub fn create_filters(def: &AggDef, spec: &FiltersSpec, ctx: &mut BindCtx<'_>) -> Result<Box<dyn Aggregator>> {
    let filters = bind_filters(&spec.filters, ctx)?;
    let subs = Subs::bind(def, ctx)?;
    let n = spec.filters.len() as u32;
    Ok(Box::new(FiltersAgg {
        keys: spec.filters.iter().map(|f| f.0.clone()).collect(),
        filters,
        spec: Some(Arc::new(spec.clone())),
        separator: None,
        slots: n + u32::from(spec.other_bucket),
        counts: Vec::new(),
        subs,
        doc_counter: DocCounter::none(),
        doc_count_field: ctx.doc_count_field,
        last_doc: u32::MAX,
        hits: vec![false; n as usize],
    }))
}

pub fn create_adjacency(def: &AggDef, spec: &AdjacencySpec, ctx: &mut BindCtx<'_>) -> Result<Box<dyn Aggregator>> {
    let max = ctx.searcher.limits().max_clause_count;
    if spec.filters.len() > max {
        return Err(AggError::illegal_argument(format!(
            "Number of filters is too large, must be less than or equal to: [{max}] but was [{}].  You can increase this limit by scaling up your java heap",
            spec.filters.len()
        )));
    }
    let filters = bind_filters(&spec.filters, ctx)?;
    let subs = Subs::bind(def, ctx)?;
    let n = spec.filters.len() as u32;
    Ok(Box::new(FiltersAgg {
        keys: spec.filters.iter().map(|f| f.0.clone()).collect(),
        filters,
        spec: None,
        separator: Some(spec.separator.clone()),
        slots: n + n * n.saturating_sub(1) / 2,
        counts: Vec::new(),
        subs,
        doc_counter: DocCounter::none(),
        doc_count_field: ctx.doc_count_field,
        last_doc: u32::MAX,
        hits: vec![false; n as usize],
    }))
}

impl FiltersAgg {
    #[inline]
    fn hit(&mut self, owning: u32, slot: u32, doc: u32, score: f32) -> Result<()> {
        let ord = owning * self.slots + slot;
        if self.counts.len() <= ord as usize {
            grow(&mut self.counts, ord, 0);
        }
        self.counts[ord as usize] += self.doc_counter.count(doc);
        if !self.subs.is_empty() {
            self.subs.push(doc, ord, score)?;
        }
        Ok(())
    }
}

impl Aggregator for FiltersAgg {
    fn set_segment(&mut self, seg: &SegmentContext) -> Result<()> {
        self.doc_counter = DocCounter::open(seg, self.doc_count_field);
        for f in &mut self.filters {
            f.scorer = f.weight.scorer(seg)?;
        }
        self.last_doc = u32::MAX;
        self.subs.set_segment(seg)
    }

    fn collect(&mut self, b: Batch<'_>) -> Result<()> {
        let n = self.filters.len();
        for i in 0..b.len() {
            let doc = b.docs[i];
            if doc != self.last_doc {
                for k in 0..n {
                    self.hits[k] = self.filters[k].matches(doc);
                }
                self.last_doc = doc;
            }
            let owning = b.ords[i];
            let score = b.score(i);
            let mut any = false;
            for k in 0..n {
                if self.hits[k] {
                    any = true;
                    self.hit(owning, k as u32, doc, score)?;
                }
            }
            if self.separator.is_some() {
                let mut pos = n as u32;
                for x in 0..n {
                    for y in x + 1..n {
                        if self.hits[x] && self.hits[y] {
                            self.hit(owning, pos, doc, score)?;
                        }
                        pos += 1;
                    }
                }
            } else if !any && self.slots > n as u32 {
                self.hit(owning, n as u32, doc, score)?;
            }
        }
        Ok(())
    }

    fn finish_segment(&mut self) -> Result<()> {
        self.subs.finish_segment()
    }

    fn post_collection(&mut self) -> Result<()> {
        self.subs.post_collection()
    }

    fn build(&mut self, owning: &[u32]) -> Result<Vec<AggValue>> {
        let n = self.filters.len() as u32;
        let slots = self.slots;
        match &self.spec {
            Some(spec) => {
                let all: Vec<u32> = owning.iter().flat_map(|&o| (0..slots).map(move |s| o * slots + s)).collect();
                let mut aggs = self.subs.build(&all)?.into_iter();
                let mut out = Vec::new();
                for &o in owning {
                    let mut buckets = Vec::new();
                    for s in 0..self.slots {
                        let key = if s < n { self.keys[s as usize].clone() } else { spec.other_bucket_key.clone() };
                        buckets.push(FilterBucket { key, doc_count: self.counts.get((o * self.slots + s) as usize).copied().unwrap_or(0), aggs: aggs.next().unwrap_or_default() });
                    }
                    out.push(AggValue::Filters(Box::new(Filters { buckets, keyed: spec.keyed, keyed_bucket: spec.keyed_bucket })));
                }
                Ok(out)
            }
            None => {
                let sep = self.separator.clone().unwrap_or_default();
                let mut ords = Vec::new();
                let mut keys_per: Vec<Vec<(String, u32)>> = Vec::new();
                for &o in owning {
                    let mut ks = Vec::new();
                    for s in 0..n {
                        let ord = o * self.slots + s;
                        if self.counts.get(ord as usize).copied().unwrap_or(0) > 0 {
                            ks.push((self.keys[s as usize].clone(), ord));
                        }
                    }
                    let mut pos = n;
                    for x in 0..n as usize {
                        for y in x + 1..n as usize {
                            let ord = o * self.slots + pos;
                            if self.counts.get(ord as usize).copied().unwrap_or(0) > 0 {
                                ks.push((format!("{}{sep}{}", self.keys[x], self.keys[y]), ord));
                            }
                            pos += 1;
                        }
                    }
                    ords.extend(ks.iter().map(|k| k.1));
                    keys_per.push(ks);
                }
                let mut aggs = self.subs.build(&ords)?.into_iter();
                Ok(keys_per
                    .into_iter()
                    .map(|ks| {
                        let buckets = ks
                            .into_iter()
                            .map(|(key, ord)| FilterBucket { key, doc_count: self.counts[ord as usize], aggs: aggs.next().unwrap_or_default() })
                            .collect();
                        AggValue::AdjacencyMatrix(Box::new(AdjacencyMatrix { buckets }))
                    })
                    .collect())
            }
        }
    }

    fn build_empty(&self) -> AggValue {
        match &self.spec {
            Some(spec) => {
                let empty = self.subs.build_empty();
                let n = self.filters.len() as u32;
                let buckets = (0..self.slots)
                    .map(|s| FilterBucket {
                        key: if s < n { self.keys[s as usize].clone() } else { spec.other_bucket_key.clone() },
                        doc_count: 0,
                        aggs: empty.clone(),
                    })
                    .collect();
                AggValue::Filters(Box::new(Filters { buckets, keyed: spec.keyed, keyed_bucket: spec.keyed_bucket }))
            }
            None => AggValue::AdjacencyMatrix(Box::new(AdjacencyMatrix { buckets: Vec::new() })),
        }
    }

    fn needs_scores(&self) -> bool {
        self.subs.needs_scores()
    }
}

/// A `filters` / `adjacency_matrix` bucket.
#[derive(Clone, Debug)]
pub struct FilterBucket {
    pub key: String,
    pub doc_count: u64,
    pub aggs: Aggs,
}

/// `InternalFilters`.
#[derive(Clone, Debug)]
pub struct Filters {
    pub buckets: Vec<FilterBucket>,
    pub keyed: bool,
    pub keyed_bucket: bool,
}

impl Internal for Filters {
    fn type_name(&self) -> &'static str {
        "filters"
    }
    fn class_name(&self) -> &'static str {
        "InternalFilters"
    }
    fn must_reduce_single(&self) -> bool {
        true
    }
    fn reduce(list: Vec<Self>, ctx: &mut ReduceCtx<'_>) -> Result<Self> {
        let mut it = list.into_iter();
        let mut first = it.next().expect("non-empty");
        let rest: Vec<Filters> = it.collect();
        let mut out = Vec::with_capacity(first.buckets.len());
        for (i, b) in first.buckets.drain(..).enumerate() {
            let mut doc_count = b.doc_count;
            let mut lists = vec![b.aggs];
            for r in &rest {
                if let Some(o) = r.buckets.get(i) {
                    doc_count += o.doc_count;
                    lists.push(o.aggs.clone());
                }
            }
            out.push(FilterBucket { key: b.key, doc_count, aggs: Aggs::reduce_hashed(lists, ctx)? });
        }
        ctx.consume_buckets(out.len())?;
        first.buckets = out;
        Ok(first)
    }
    fn render(&self, out: &mut Json, rc: &RenderCtx) {
        let as_object = self.keyed && self.keyed_bucket;
        let render_bucket = |b: &FilterBucket| -> Json {
            let mut o = Json::obj();
            if self.keyed && !self.keyed_bucket {
                o.push("key", Json::Str(b.key.clone()));
            }
            o.push("doc_count", Json::from(b.doc_count));
            b.aggs.render_into(&mut o, rc);
            o
        };
        if as_object {
            out.push("buckets", Json::Obj(self.buckets.iter().map(|b| (b.key.clone(), render_bucket(b))).collect()));
        } else {
            out.push("buckets", Json::Arr(self.buckets.iter().map(render_bucket).collect()));
        }
    }
    fn property(&self, name: &str, path: &[String]) -> Result<Prop<'_>> {
        multi_bucket_property(self, name, path)
    }
}

impl MultiBuckets for Filters {
    fn len(&self) -> usize {
        self.buckets.len()
    }
    fn doc_count(&self, i: usize) -> u64 {
        self.buckets[i].doc_count
    }
    fn key(&self, i: usize) -> Json {
        Json::Str(self.buckets[i].key.clone())
    }
    fn key_string(&self, i: usize) -> String {
        self.buckets[i].key.clone()
    }
    fn aggs(&self, i: usize) -> &Aggs {
        &self.buckets[i].aggs
    }
    fn aggs_mut(&mut self, i: usize) -> &mut Aggs {
        &mut self.buckets[i].aggs
    }
    fn retain_buckets(&mut self, keep: &[bool]) {
        let mut i = 0;
        self.buckets.retain(|_| {
            i += 1;
            keep[i - 1]
        });
    }
    fn reorder(&mut self, order: &[usize]) {
        let mut slots: Vec<Option<FilterBucket>> = self.buckets.drain(..).map(Some).collect();
        self.buckets = order.iter().map(|&i| slots[i].take().expect("bucket")).collect();
    }
}

/// `InternalAdjacencyMatrix`.
#[derive(Clone, Debug)]
pub struct AdjacencyMatrix {
    pub buckets: Vec<FilterBucket>,
}

impl Internal for AdjacencyMatrix {
    fn type_name(&self) -> &'static str {
        "adjacency_matrix"
    }
    fn class_name(&self) -> &'static str {
        "InternalAdjacencyMatrix"
    }
    fn must_reduce_single(&self) -> bool {
        true
    }
    fn reduce(list: Vec<Self>, ctx: &mut ReduceCtx<'_>) -> Result<Self> {
        let mut order: Vec<String> = Vec::new();
        let mut groups: hashbrown::HashMap<String, Vec<FilterBucket>> = hashbrown::HashMap::new();
        for l in list {
            for b in l.buckets {
                if !groups.contains_key(&b.key) {
                    order.push(b.key.clone());
                }
                groups.entry(b.key.clone()).or_default().push(b);
            }
        }
        let mut out = Vec::new();
        for k in order {
            let g = groups.remove(&k).unwrap_or_default();
            let doc_count: u64 = g.iter().map(|b| b.doc_count).sum();
            if doc_count >= 1 {
                let aggs = Aggs::reduce_hashed(g.into_iter().map(|b| b.aggs).collect(), ctx)?;
                out.push(FilterBucket { key: k, doc_count, aggs });
            }
        }
        ctx.consume_buckets(out.len())?;
        out.sort_by(|a, b| a.key.encode_utf16().cmp(b.key.encode_utf16()));
        Ok(AdjacencyMatrix { buckets: out })
    }
    fn render(&self, out: &mut Json, rc: &RenderCtx) {
        out.push(
            "buckets",
            Json::Arr(
                self.buckets
                    .iter()
                    .map(|b| {
                        let mut o = Json::obj().with("key", b.key.as_str()).with("doc_count", Json::from(b.doc_count));
                        b.aggs.render_into(&mut o, rc);
                        o
                    })
                    .collect(),
            ),
        );
    }
    fn property(&self, name: &str, path: &[String]) -> Result<Prop<'_>> {
        multi_bucket_property(self, name, path)
    }
}

impl MultiBuckets for AdjacencyMatrix {
    fn len(&self) -> usize {
        self.buckets.len()
    }
    fn doc_count(&self, i: usize) -> u64 {
        self.buckets[i].doc_count
    }
    fn key(&self, i: usize) -> Json {
        Json::Str(self.buckets[i].key.clone())
    }
    fn key_string(&self, i: usize) -> String {
        self.buckets[i].key.clone()
    }
    fn aggs(&self, i: usize) -> &Aggs {
        &self.buckets[i].aggs
    }
    fn aggs_mut(&mut self, i: usize) -> &mut Aggs {
        &mut self.buckets[i].aggs
    }
    fn retain_buckets(&mut self, keep: &[bool]) {
        let mut i = 0;
        self.buckets.retain(|_| {
            i += 1;
            keep[i - 1]
        });
    }
    fn reorder(&mut self, order: &[usize]) {
        let mut slots: Vec<Option<FilterBucket>> = self.buckets.drain(..).map(Some).collect();
        self.buckets = order.iter().map(|&i| slots[i].take().expect("bucket")).collect();
    }
}
