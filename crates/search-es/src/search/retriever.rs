//! Retrievers (`retriever` in the search body): the `standard`, `knn` and `rescorer`
//! retrievers, rewritten into the plain search they stand for, as Elasticsearch's
//! `RetrieverBuilder.extractToSearchSourceBuilder` does once the retriever tree is rewritten.
//!
//! - `standard` gives the query (with its `filter`s and `min_score` the query becomes a `bool`
//!   with the filters and the query as `must`), `sort`, `search_after`, `terminate_after`,
//!   `min_score` and `collapse`;
//! - `knn` adds a top-level kNN search (`filter` and `_name` included);
//! - `rescorer` extracts its child (as a child of a compound retriever: no `search_after` or
//!   `terminate_after`) and appends its rescorers, after checking `window_size` against `size`.
//!
//! - `diversify` (MMR) extracts its child the same way, searching `rank_window_size` hits,
//!   then keeps the [`Diversify::mmr`] selection of them in their original order.
//!
//! - `rrf` and `linear` (compound, `CompoundRetrieverBuilder`): each child runs as its own
//!   search of `rank_window_size` hits; their rankings fuse into one ([`Fusion`]) and the
//!   request then runs with the fused documents and scores, its aggregations and total over
//!   everything the children match (see [`super::fusion`]).
//!
//! The rewritten parts are assembled as a request body and parsed by the normal search source
//! parser, so every section keeps its own validation. A request with a retriever may not set
//! the sections a retriever produces (`SearchSourceBuilder.validate`).

use crate::error::{EsError, EsResult};
use crate::json::{Map, Value};
use crate::search_source::SearchSource;

/// `RetrieverBuilder.MAX_NESTED_DEPTH`.
const MAX_NESTED_DEPTH: usize = 128;

/// Replaces `source.retriever` by the sections it stands for.
pub fn apply(source: &mut SearchSource, url_sort: bool) -> EsResult<()> {
    rank_to_retriever(source)?;
    if source.sub_searches.len() == 1 && source.query.is_none() {
        source.query = source.sub_searches.pop();
    }
    let Some(r) = source.retriever.take() else { return Ok(()) };
    // `SearchSourceBuilder.validate`: sections a retriever would set.
    let mut specified = Vec::new();
    if source.query.is_some() {
        specified.push("query");
    }
    if !source.knn.is_empty() {
        specified.push("knn");
    }
    if source.search_after.is_some() {
        specified.push("search_after");
    }
    if source.terminate_after.is_some() {
        specified.push("terminate_after");
    }
    if !source.sort.is_empty() || url_sort {
        specified.push("sort");
    }
    if source.rank.is_some() {
        specified.push("rank");
    }
    if !source.rescore.is_empty() {
        specified.push("rescore");
    }
    let mut body = Map::new();
    let mut checks = Vec::new();
    let mut diversify = None;
    let mut fusion = None;
    match entry(&r)?.0 {
        "rrf" | "linear" => fusion = Some(compound(&r, 0, &[], &mut checks)?),
        _ => extract(&r, false, 0, &[], &mut body, &mut checks, &mut diversify, &mut fusion)?,
    }
    let size = source.size.unwrap_or(10) as i64;
    let mut errors: Vec<String> = Vec::new();
    for (name, field, window) in &checks {
        // Compound retrievers only check a `size` the request sets.
        if matches!(name.as_str(), "rrf" | "linear" | "pinned") && source.size.is_none() {
            if *window < 0 {
                errors.push(format!("[{field}] parameter cannot be negative, found [{window}]"));
            }
            continue;
        }
        if size > *window {
            errors.push(format!("[{name}] requires [{field}: {window}] be greater than or equal to [size: {size}]"));
        }
        if *window < 0 {
            errors.push(format!("[{field}] parameter cannot be negative, found [{window}]"));
        }
    }
    if let Some(d) = &diversify {
        errors.extend(d.errors.iter().cloned());
    }
    if !specified.is_empty() {
        errors.push(format!("cannot specify [retriever] and [{}]", specified.join(", ")));
    }
    if !errors.is_empty() {
        return Err(EsError::action_request_validation(&errors));
    }
    let parsed = SearchSource::parse(&Value::Obj(body), None)?;
    source.query = parsed.query;
    source.sort = parsed.sort;
    source.search_after = parsed.search_after;
    source.terminate_after = parsed.terminate_after;
    if parsed.min_score.is_some() {
        source.min_score = parsed.min_score;
    }
    if parsed.collapse.is_some() {
        source.collapse = parsed.collapse;
    }
    source.knn = parsed.knn;
    source.rescore = parsed.rescore;
    source.rescore_before_collapse = !source.rescore.is_empty() && source.collapse.is_some();
    source.diversify = diversify;
    source.fusion = fusion.map(Box::new);
    Ok(())
}

/// The deprecated `rank` section (`RRFRankBuilder.toRetriever`): the `query` (or each of
/// `sub_searches`) becomes a `standard` retriever and each `knn` search a `knn` retriever,
/// fused by an `rrf` retriever with the rank's window and constant.
fn rank_to_retriever(source: &mut SearchSource) -> EsResult<()> {
    let Some(rank) = source.rank.take() else { return Ok(()) };
    if source.retriever.is_some() {
        return Err(EsError::illegal_argument("Cannot specify both [rank] and [retriever]."));
    }
    let mut entries = rank.iter();
    let Some((name, body)) = entries.next() else {
        return Err(EsError::parse("expected a rank name, but found token [START_OBJECT] for [rank]"));
    };
    if name != "rrf" {
        return Err(EsError::new(400, "named_object_not_found_exception", format!("[1:1] unknown field [{name}]")));
    }
    if entries.next().is_some() {
        return Err(EsError::parse("expected token '}', but found token [START_OBJECT] for [rank]"));
    }
    let Value::Obj(body) = body else { return Err(EsError::x_content_parse(1, 1, "[rrf] expected object")) };
    // `declareInt`: numbers coerce (truncate), strings parse.
    let int = |k: &str| -> EsResult<Option<i64>> {
        match body.get(k) {
            None | Some(Value::Null) => Ok(None),
            Some(v) => match v.as_f64().or_else(|| v.as_str().and_then(|s| s.parse::<f64>().ok())) {
                Some(f) => Ok(Some(f.trunc() as i64)),
                None => Err(EsError::x_content_parse(1, 1, format!("[rrf] failed to parse field [{k}]"))),
            },
        }
    };
    for k in body.keys() {
        if !matches!(k.as_str(), "rank_window_size" | "rank_constant") {
            return Err(EsError::x_content_parse(1, 1, format!("[rrf] unknown field [{k}]")));
        }
    }
    let window = int("rank_window_size")?.unwrap_or(10);
    let constant = int("rank_constant")?.unwrap_or(60);
    if constant < 1 {
        return Err(EsError::illegal_argument("[rank_constant] must be greater or equal to [1] for [rrf]"));
    }
    let mut queries = std::mem::take(&mut source.sub_searches);
    queries.extend(source.query.take());
    if queries.len() + source.knn.len() < 2 {
        return Err(EsError::illegal_argument("[rrf] requires at least 2 sub-queries to be defined"));
    }
    let mut retrievers = Vec::new();
    for q in &queries {
        let mut standard = Map::new();
        standard.insert("query".into(), q.to_value());
        if let Some(n) = q.query_name() {
            standard.insert("_name".into(), Value::Str(n.to_string()));
        }
        retrievers.push(crate::obj! { "standard" => Value::Obj(standard) });
    }
    // `KnnSearchBuilder.Builder.build(size)`: `k` defaults to the request's size and
    // `num_candidates` to `1.5 k` (at most 10,000). The knn search's filters don't carry over.
    let size = source.size.unwrap_or(10).max(0) as i64;
    for knn in std::mem::take(&mut source.knn) {
        let mut k = Map::new();
        for key in ["field", "query_vector", "query_vector_builder", "k", "num_candidates", "similarity", "rescore_vector", "visit_percentage", "_name"] {
            if let Some(v) = knn.get(key) {
                k.insert(key.into(), v.clone());
            }
        }
        let kk = k.get("k").and_then(Value::as_i64).unwrap_or(size);
        k.entry("k".to_string()).or_insert(Value::from(kk));
        k.entry("num_candidates".to_string()).or_insert(Value::from((1.5 * kk as f64).min(10_000.0).round() as i64));
        retrievers.push(crate::obj! { "knn" => Value::Obj(k) });
    }
    let mut r = Map::new();
    r.insert("rrf".into(), crate::obj! { "retrievers" => Value::Arr(retrievers), "rank_window_size" => Value::from(window), "rank_constant" => Value::from(constant) });
    source.retriever = Some(r);
    Ok(())
}

/// How a compound retriever fuses its children's rankings.
#[derive(Clone, Debug, PartialEq)]
pub enum FusionKind {
    /// `rrf`: `Σ weight · 1 / (rank_constant + rank)`.
    Rrf { rank_constant: i64, weights: Vec<f32> },
    /// `linear`: `Σ weight · normalize(score)`.
    Linear { weights: Vec<f32>, normalizers: Vec<Normalizer> },
    /// `pinned` over a compound retriever: the pins (index, id) above its one child's
    /// ranking; a pin naming another index still takes its pin position.
    Pinned { pins: Vec<(Option<String>, String)>, organic: Option<Value> },
}

/// A `linear` child's score normalizer (`ScoreNormalizer`).
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Normalizer {
    None,
    MinMax,
    L2Norm,
}

impl Normalizer {
    fn parse(s: &str) -> EsResult<Normalizer> {
        Ok(match s {
            "none" => Normalizer::None,
            "minmax" => Normalizer::MinMax,
            "l2_norm" => Normalizer::L2Norm,
            other => return Err(EsError::illegal_argument(format!("Unknown normalizer [{other}]"))),
        })
    }

    /// `normalizeScores` over one child's ranked scores.
    /// `ScoreNormalizer.getName`.
    pub fn name(self) -> &'static str {
        match self {
            Normalizer::None => "none",
            Normalizer::MinMax => "minmax",
            Normalizer::L2Norm => "l2_norm",
        }
    }

    pub fn apply(self, scores: &mut [f32]) {
        const EPSILON: f32 = 1e-6;
        match self {
            Normalizer::None => {}
            Normalizer::MinMax => {
                let (mut min, mut max) = (f32::MAX, f32::from_bits(1));
                for s in scores.iter() {
                    min = min.min(*s);
                    max = max.max(*s);
                }
                if scores.is_empty() {
                    return;
                }
                let flat = (min - max).abs() < EPSILON;
                for s in scores.iter_mut() {
                    *s = if flat { 1.0 } else { (*s - min) / (max - min) };
                }
            }
            Normalizer::L2Norm => {
                let norm = scores.iter().map(|s| (*s as f64) * (*s as f64)).sum::<f64>().sqrt();
                if norm < EPSILON as f64 {
                    return;
                }
                for s in scores.iter_mut() {
                    *s = (*s as f64 / norm) as f32;
                }
            }
        }
    }
}

/// A compound retriever: its children (each a search of its own) and how their rankings fuse.
#[derive(Clone, Debug, PartialEq)]
pub struct Fusion {
    pub kind: FusionKind,
    pub window: i64,
    pub min_score: Option<f32>,
    pub children: Vec<SearchSource>,
    /// Per child: its retriever's `_name`.
    pub names: Vec<Option<String>>,
    /// Per child: a `standard` retriever's `topDocsQuery` (its query, with its filters as
    /// filter clauses), which the final phase matches the totals and aggregations with.
    pub tails: Vec<Option<Value>>,
}

/// A fused document: its key, fused score, and per child its position there (0-based) and
/// normalized score (`linear`), if the child ranked it.
#[derive(Clone, Debug, PartialEq)]
pub struct FusedDoc<K> {
    pub key: K,
    pub score: f32,
    pub per_child: Vec<Option<(u32, f32)>>,
}

impl Fusion {
    /// Combines the children's ranked hits (`combineInnerRetrieverResults`): keys with their
    /// fused scores, best first (ties keep the earlier key order), at most `window`.
    pub fn combine<K: Clone + Eq + std::hash::Hash + Ord>(&self, ranked: &[Vec<(K, f32)>]) -> Vec<(K, f32)> {
        self.combine_detailed(ranked).into_iter().map(|d| (d.key, d.score)).collect()
    }

    /// [`combine`](Self::combine), keeping what explanations report per child
    /// (`RRFRankDoc.positions`, `LinearRankDoc.normalizedScores`).
    pub fn combine_detailed<K: Clone + Eq + std::hash::Hash + Ord>(&self, ranked: &[Vec<(K, f32)>]) -> Vec<FusedDoc<K>> {
        let n = ranked.len();
        let mut fused: std::collections::HashMap<K, FusedDoc<K>> = std::collections::HashMap::new();
        fn entry<'a, K: Clone + Eq + std::hash::Hash>(fused: &'a mut std::collections::HashMap<K, FusedDoc<K>>, k: &K, n: usize) -> &'a mut FusedDoc<K> {
            fused.entry(k.clone()).or_insert_with(|| FusedDoc { key: k.clone(), score: 0.0, per_child: vec![None; n] })
        }
        for (i, list) in ranked.iter().enumerate() {
            match &self.kind {
                FusionKind::Rrf { rank_constant, weights } => {
                    let w = weights.get(i).copied().unwrap_or(1.0);
                    for (rank, (k, s)) in list.iter().enumerate() {
                        let d = entry(&mut fused, k, n);
                        d.score += w * (1.0 / (*rank_constant as f32 + (rank + 1) as f32));
                        d.per_child[i] = Some((rank as u32, *s));
                    }
                }
                // Pins are resolved to documents by the caller (`fusion::rank`), which hands
                // them in as a ranking of their own ahead of the child's.
                FusionKind::Pinned { .. } => {
                    for (rank, (k, s)) in list.iter().enumerate() {
                        let d = entry(&mut fused, k, n);
                        d.per_child[i] = Some((rank as u32, *s));
                        d.score = d.score.max(*s);
                    }
                }
                FusionKind::Linear { weights, normalizers } => {
                    let mut scores: Vec<f32> = list.iter().map(|(_, s)| *s).collect();
                    normalizers.get(i).copied().unwrap_or(Normalizer::None).apply(&mut scores);
                    let w = weights.get(i).copied().unwrap_or(1.0);
                    for (rank, ((k, _), s)) in list.iter().zip(scores).enumerate() {
                        let d = entry(&mut fused, k, n);
                        d.per_child[i] = Some((rank as u32, s));
                        let s = if s.is_nan() { 0.0 } else { s };
                        d.score += w * s;
                    }
                }
            }
        }
        let mut out: Vec<FusedDoc<K>> = fused.into_values().collect();
        out.sort_by(|a, b| b.score.total_cmp(&a.score).then_with(|| a.key.cmp(&b.key)));
        out.truncate(self.window.max(0) as usize);
        if let Some(m) = self.min_score {
            out.retain(|d| d.score >= m);
        }
        out
    }
}

/// A child retriever as the search it runs (compound children fuse their own children).
fn child_source(r: &Map, depth: usize, inherited: &[Value], checks: &mut Vec<(String, &'static str, i64)>) -> EsResult<SearchSource> {
    if matches!(entry(r)?.0, "rrf" | "linear") {
        let f = compound(r, depth, inherited, checks)?;
        return Ok(SearchSource { fusion: Some(Box::new(f)), ..Default::default() });
    }
    let mut body = Map::new();
    let mut diversify = None;
    let mut fusion = None;
    extract(r, true, depth, inherited, &mut body, checks, &mut diversify, &mut fusion)?;
    let mut s = SearchSource::parse(&Value::Obj(body), None)?;
    s.diversify = diversify;
    s.fusion = fusion.map(Box::new);
    Ok(s)
}

/// `rrf` (`RRFRetrieverBuilder`) and `linear` (`LinearRetrieverBuilder`).
fn compound(r: &Map, depth: usize, inherited: &[Value], checks: &mut Vec<(String, &'static str, i64)>) -> EsResult<Fusion> {
    if depth >= MAX_NESTED_DEPTH {
        return Err(EsError::illegal_argument(format!(
            "The nested depth of the [retriever] exceeds the maximum nested depth of [{MAX_NESTED_DEPTH}] for retrievers"
        )));
    }
    let (name, body) = entry(r)?;
    let linear = name == "linear";
    for k in body.keys() {
        let ok = matches!(k.as_str(), "retrievers" | "rank_window_size" | "filter" | "min_score" | "_name")
            || (!linear && k == "rank_constant")
            || (linear && k == "normalizer");
        if !ok {
            return Err(unknown(name, k));
        }
    }
    let int = |k: &str| -> EsResult<Option<i64>> {
        match body.get(k) {
            None | Some(Value::Null) => Ok(None),
            Some(v) => v.as_i64().map(Some).ok_or_else(|| EsError::x_content_parse(1, 1, format!("[{name}] failed to parse field [{k}]"))),
        }
    };
    let window = int("rank_window_size")?.unwrap_or(10);
    let mut fs = filters(body.get("filter"));
    fs.extend(inherited.iter().cloned());
    let Some(Value::Arr(list)) = body.get("retrievers") else {
        return Err(EsError::illegal_argument(format!("[{name}] requires [retrievers]")));
    };
    let top_normalizer = match body.get("normalizer") {
        Some(v) => Some(Normalizer::parse(&v.to_es_string())?),
        None => None,
    };
    let mut children = Vec::new();
    let (mut weights, mut normalizers) = (Vec::new(), Vec::new());
    let (mut names, mut tails) = (Vec::new(), Vec::new());
    for c in list {
        let Value::Obj(cm) = c else { return Err(EsError::parse(format!("[{name}] retrievers must be objects"))) };
        // `{"retriever": {...}, "weight": .., "normalizer": ..}` or a bare retriever.
        let (inner, weight, normalizer) = match cm.get("retriever") {
            Some(Value::Obj(inner)) if !linear => {
                // `RRFRetrieverComponent.fromXContent`.
                let wrap = |e: EsError| EsError::x_content_parse(1, 1, format!("[{name}] failed to parse field [retrievers]")).caused_by(e);
                for k in cm.keys() {
                    if !matches!(k.as_str(), "retriever" | "weight") {
                        return Err(wrap(EsError::parse(format!("unknown field [{k}], expected [retriever] or [weight]"))));
                    }
                }
                let w = match cm.get("weight") {
                    None | Some(Value::Null) => 1.0,
                    Some(v) => v.as_f64().map(|w| w as f32).ok_or_else(|| wrap(EsError::x_content_parse(1, 1, "[weight] must be a number")))?,
                };
                if w < 0.0 {
                    return Err(wrap(EsError::illegal_argument(format!("[weight] must be non-negative, found [{}]", search_index::util::java::float_to_string(w)))));
                }
                (inner, w, None)
            }
            Some(Value::Obj(inner)) => {
                for k in cm.keys() {
                    if !matches!(k.as_str(), "retriever" | "weight" | "normalizer") {
                        return Err(unknown(name, k));
                    }
                }
                // `LinearRetrieverComponent` parse failures, wrapped as the parser wraps them.
                let component = |e: EsError, field: Option<&str>| {
                    let inner = match field {
                        Some(f) => EsError::x_content_parse(1, 1, format!("[retriever-component] failed to parse field [{f}]")).caused_by(e),
                        None => EsError::x_content_parse_bare("Failed to build [retriever-component] after last required field arrived").caused_by(e),
                    };
                    EsError::x_content_parse(1, 1, format!("[{name}] failed to parse field [retrievers]")).caused_by(inner)
                };
                let w = cm.get("weight").and_then(Value::as_f64).map(|w| w as f32).unwrap_or(1.0);
                if w < 0.0 {
                    return Err(component(EsError::illegal_argument("[weight] must be non-negative"), None));
                }
                let n = match cm.get("normalizer") {
                    Some(v) => Some(Normalizer::parse(&v.to_es_string()).map_err(|e| component(e, Some("normalizer")))?),
                    None => None,
                };
                (inner, w, n)
            }
            _ => (cm, 1.0, None),
        };
        children.push(crate::stack::maybe_grow(|| child_source(inner, depth + 1, &fs, checks))?);
        let (kind, ibody) = entry(inner)?;
        names.push(ibody.get("_name").map(Value::to_es_string));
        tails.push((kind == "standard").then(|| {
            let query = ibody.get("query").cloned().unwrap_or_else(|| crate::obj! { "match_all" => crate::obj! {} });
            let mut fs2 = filters(ibody.get("filter"));
            fs2.extend(fs.iter().cloned());
            if fs2.is_empty() {
                query
            } else {
                fs2.insert(0, query);
                crate::obj! { "bool" => crate::obj! { "filter" => Value::Arr(fs2) } }
            }
        }));
        weights.push(weight);
        normalizers.push(normalizer.or(top_normalizer).unwrap_or(Normalizer::None));
    }
    if children.is_empty() {
        return Err(EsError::action_request_validation(&[format!("[{name}] must provide [retrievers] or [query]")]));
    }
    checks.push((name.to_string(), "rank_window_size", window));
    let kind = if linear {
        FusionKind::Linear { weights, normalizers }
    } else {
        FusionKind::Rrf { rank_constant: int("rank_constant")?.unwrap_or(60), weights }
    };
    let min_score = body.get("min_score").and_then(Value::as_f64).map(|m| m as f32);
    Ok(Fusion { kind, window, min_score, children, names, tails })
}

/// A `diversify` retriever (`DiversifyRetrieverBuilder`, type `mmr`).
#[derive(Clone, Debug, PartialEq)]
pub struct Diversify {
    pub field: String,
    pub lambda: f32,
    pub size: i64,
    pub rank_window_size: i64,
    pub query_vector: Option<Vec<f32>>,
    /// Validation errors, reported with the request's other validation errors.
    errors: Vec<String>,
}

/// Lucene's `MAXIMUM_INNER_PRODUCT` similarity.
fn max_inner_product(a: &[f32], b: &[f32]) -> f32 {
    let dp: f32 = a.iter().zip(b).map(|(x, y)| x * y).sum();
    if dp < 0.0 { 1.0 / (1.0 - dp) } else { dp + 1.0 }
}

impl Diversify {
    /// `MMRResultDiversification.diversify`: the indices (into `vectors`, in rank order) of
    /// the selected documents, in rank order. Documents without a vector are never selected.
    pub fn mmr(&self, vectors: &[Option<Vec<f32>>]) -> Vec<usize> {
        let n = vectors.len();
        if n == 0 {
            return Vec::new();
        }
        let query: Vec<f32> = (0..n)
            .map(|i| match (&self.query_vector, &vectors[i]) {
                (Some(q), Some(v)) => max_inner_product(v, q),
                _ => 0.0,
            })
            .collect();
        // The first best match to the query (ties keep the earlier rank).
        let Some(best) = (0..n).filter(|&i| vectors[i].is_some()).reduce(|a, b| if query[a] >= query[b] { a } else { b }) else {
            return Vec::new();
        };
        let size = self.size.max(0) as usize;
        let mut selected = vec![best];
        let mut prev = best;
        let mut max_sim = vec![f32::NEG_INFINITY; n];
        let mut x = 0;
        while x < size && selected.len() < size && selected.len() < n {
            x += 1;
            let mut best_i = None;
            let mut best_score = f32::NEG_INFINITY;
            for i in 0..n {
                if selected.contains(&i) {
                    continue;
                }
                let Some(v) = vectors[i].as_ref().filter(|v| !v.is_empty()) else { continue };
                let Some(p) = vectors[prev].as_ref() else { continue };
                max_sim[i] = max_sim[i].max(max_inner_product(v, p));
                let mmr = self.lambda * query[i] - (1.0 - self.lambda) * max_sim[i];
                if mmr > best_score {
                    best_score = mmr;
                    best_i = Some(i);
                }
            }
            if let Some(i) = best_i {
                selected.push(i);
                prev = i;
            }
        }
        selected.sort_unstable();
        selected
    }
}

/// Parses a `diversify` retriever's own parameters (`DiversifyRetrieverBuilder.PARSER`).
fn parse_diversify(body: &Map) -> EsResult<Diversify> {
    for k in body.keys() {
        if !matches!(k.as_str(), "retriever" | "type" | "field" | "rank_window_size" | "query_vector" | "query_vector_builder" | "lambda" | "size" | "filter" | "min_score" | "_name") {
            return Err(unknown("diversify", k));
        }
    }
    for required in ["retriever", "type", "field"] {
        if !body.contains_key(required) {
            return Err(EsError::illegal_argument(format!("Required [{required}]")));
        }
    }
    let ty = body.get("type").map(Value::to_es_string).unwrap_or_default();
    if ty != "mmr" {
        return Err(EsError::illegal_argument(format!("unknown result diversification type [{ty}]")));
    }
    let int = |k: &str| -> EsResult<Option<i64>> {
        match body.get(k) {
            None | Some(Value::Null) => Ok(None),
            Some(v) => v.as_i64().map(Some).ok_or_else(|| EsError::illegal_argument(format!("[diversify] failed to parse field [{k}]"))),
        }
    };
    let rank_window_size = int("rank_window_size")?.unwrap_or(10);
    let size = int("size")?.unwrap_or_else(|| rank_window_size.min(10));
    let lambda = body.get("lambda").and_then(Value::as_f64).map(|l| l as f32);
    let mut errors = Vec::new();
    if body.contains_key("query_vector") && body.contains_key("query_vector_builder") {
        errors.push("[diversify] MMR result diversification can have one of [query_vector] or [query_vector_builder], but not both".into());
    }
    if size <= 0 {
        errors.push(format!("[diversify] MMR result diversification [size] of {size} must be greater than zero"));
    }
    if size > rank_window_size {
        errors.push(format!("[diversify] MMR result diversification [size] of {size} cannot be greater than the [rank_window_size] of {rank_window_size}"));
    }
    match lambda {
        Some(l) if (0.0..=1.0).contains(&l) => {}
        Some(l) => errors.push(format!(
            "[diversify] MMR result diversification must have a [lambda] between 0.0 and 1.0. The value provided was {}",
            crate::json::write::java_float(l)
        )),
        None => errors.push("[diversify] MMR result diversification must have a [lambda] between 0.0 and 1.0. The value provided was null".into()),
    }
    let query_vector = match body.get("query_vector") {
        Some(Value::Str(_)) => {
            errors.push("[diversify] retriever cannot have a [query_vector] that is string encoded".into());
            None
        }
        Some(Value::Arr(a)) => Some(a.iter().map(|v| v.as_f64().unwrap_or(0.0) as f32).collect()),
        _ => None,
    };
    Ok(Diversify {
        field: body.get("field").map(Value::to_es_string).unwrap_or_default(),
        lambda: lambda.unwrap_or(0.0),
        size,
        rank_window_size,
        query_vector,
        errors,
    })
}

/// The single `{name: body}` entry of a retriever object.
fn entry(r: &Map) -> EsResult<(&str, &Map)> {
    let mut it = r.iter();
    let (Some((name, body)), None) = (it.next(), it.next()) else {
        return Err(EsError::parse("retriever must contain exactly one retriever"));
    };
    match body {
        Value::Obj(m) => Ok((name.as_str(), m)),
        _ => Err(EsError::parse(format!("[{name}] retriever malformed, expected an object"))),
    }
}

fn unknown(retriever: &str, key: &str) -> EsError {
    EsError::x_content_parse(1, 1, format!("[{retriever}] unknown field [{key}]"))
}

/// Filters as a list (`filter` takes a query or a list of queries).
fn filters(v: Option<&Value>) -> Vec<Value> {
    match v {
        None | Some(Value::Null) => Vec::new(),
        Some(Value::Arr(a)) => a.clone(),
        Some(other) => vec![other.clone()],
    }
}

/// Adds retriever `r` to the request body `out`; compound retrievers record their window-size
/// checks (`name`, field, window).
/// `inherited`: pre-filters of enclosing compound retrievers, which apply to their children.
/// A retriever that runs as a compound one (`pinned`, a `rescorer` over `rrf`) sets `fusion`.
#[allow(clippy::too_many_arguments)]
fn extract(
    r: &Map,
    compound: bool,
    depth: usize,
    inherited: &[Value],
    out: &mut Map,
    checks: &mut Vec<(String, &'static str, i64)>,
    diversify: &mut Option<Diversify>,
    fusion: &mut Option<Fusion>,
) -> EsResult<()> {
    if depth >= MAX_NESTED_DEPTH {
        return Err(EsError::illegal_argument(format!(
            "The nested depth of the [retriever] exceeds the maximum nested depth of [{MAX_NESTED_DEPTH}] for retrievers"
        )));
    }
    let (name, body) = entry(r)?;
    match name {
        "standard" => {
            for k in body.keys() {
                if !matches!(k.as_str(), "query" | "filter" | "search_after" | "terminate_after" | "sort" | "collapse" | "min_score" | "_name") {
                    return Err(unknown(name, k));
                }
            }
            let mut fs = filters(body.get("filter"));
            fs.extend(inherited.iter().cloned());
            let query = body.get("query").cloned();
            if !fs.is_empty() || body.contains_key("min_score") {
                let mut b = Map::new();
                if let Some(q) = query {
                    b.insert("must".into(), Value::Arr(vec![q]));
                }
                if !fs.is_empty() {
                    b.insert("filter".into(), Value::Arr(fs));
                }
                out.insert("query".into(), crate::obj! { "bool" => Value::Obj(b) });
            } else if let Some(q) = query {
                out.insert("query".into(), q);
            }
            for key in ["search_after", "terminate_after"] {
                if let Some(v) = body.get(key) {
                    if compound {
                        return Err(EsError::illegal_argument(format!("[{key}] cannot be used in children of compound retrievers")));
                    }
                    out.insert(key.into(), v.clone());
                }
            }
            for key in ["sort", "min_score", "collapse"] {
                if let Some(v) = body.get(key) {
                    out.insert(key.into(), v.clone());
                }
            }
            Ok(())
        }
        "knn" => {
            let mut k = Map::new();
            for (key, v) in body {
                match key.as_str() {
                    "field" | "query_vector" | "query_vector_builder" | "k" | "num_candidates" | "similarity" | "rescore_vector" | "visit_percentage" => {
                        k.insert(key.clone(), v.clone());
                    }
                    "filter" => {
                        let mut fs = filters(Some(v));
                        fs.extend(inherited.iter().cloned());
                        k.insert("filter".into(), Value::Arr(fs));
                    }
                    "_name" => {
                        k.insert("_name".into(), v.clone());
                    }
                    "min_score" => {}
                    other => return Err(unknown(name, other)),
                }
            }
            if !inherited.is_empty() && !k.contains_key("filter") {
                k.insert("filter".into(), Value::Arr(inherited.to_vec()));
            }
            for required in ["field", "k"] {
                if !k.contains_key(required) {
                    return Err(EsError::illegal_argument(format!("Required [{required}]")));
                }
            }
            match out.get_mut("knn") {
                Some(Value::Arr(a)) => a.push(Value::Obj(k)),
                _ => {
                    out.insert("knn".into(), Value::Arr(vec![Value::Obj(k)]));
                }
            }
            Ok(())
        }
        "rescorer" => {
            let mut inner = None;
            let mut rescore = None;
            let mut fs = filters(body.get("filter"));
            fs.extend(inherited.iter().cloned());
            for (key, v) in body {
                match key.as_str() {
                    "retriever" => inner = Some(v),
                    "rescore" => rescore = Some(v),
                    "filter" | "min_score" | "_name" => {}
                    other => return Err(EsError::parse(format!("[rescorer] unknown field [{other}]"))),
                }
            }
            let Some(Value::Obj(inner)) = inner else { return Err(EsError::parse("Required [retriever]")) };
            let rescorers: Vec<Value> = match rescore {
                Some(Value::Arr(a)) => a.clone(),
                Some(v @ Value::Obj(_)) => vec![v.clone()],
                Some(_) => {
                    return Err(EsError::parse("Unknown format for [rescorer.rescore], expects an object or an array of objects"));
                }
                None => return Err(EsError::parse("Required [rescore]")),
            };
            if rescorers.is_empty() {
                return Err(EsError::parse("Missing rescore definition"));
            }
            let window = rescorers.iter().map(|r| r.get("window_size").and_then(Value::as_i64).unwrap_or(10)).min().unwrap_or(10);
            checks.push(("rescorer".into(), "window_size", window));
            // A compound retriever under the rescorer: its fused documents are rescored.
            if let (sub @ ("rrf" | "linear"), ib) = entry(inner)? {
                let sub_window = ib.get("rank_window_size").and_then(Value::as_i64).unwrap_or(10);
                for r in &rescorers {
                    let w = r.get("window_size").and_then(Value::as_i64).unwrap_or(10);
                    if w > sub_window {
                        return Err(EsError::illegal_argument(format!(
                            "[rescorer] requires [window_size: {w}] to be smaller than or equal to its sub retriever's {sub} [rank_window_size: {sub_window}]"
                        )));
                    }
                }
                *fusion = Some(crate::stack::maybe_grow(|| self::compound(inner, depth + 1, &fs, checks))?);
            } else {
                crate::stack::maybe_grow(|| extract(inner, true, depth + 1, &fs, out, checks, diversify, fusion))?;
            }
            match out.get_mut("rescore") {
                Some(Value::Arr(a)) => a.extend(rescorers),
                _ => {
                    out.insert("rescore".into(), Value::Arr(rescorers));
                }
            }
            Ok(())
        }
        "pinned" => {
            // `PinnedRetrieverBuilder`: the child's query becomes the organic query of a
            // `pinned` query (a compound child: its ranking under the pins).
            let wrap = |e: EsError| EsError::parse(e.reason().to_string()).caused_by(e);
            for k in body.keys() {
                if !matches!(k.as_str(), "ids" | "docs" | "retriever" | "rank_window_size" | "filter" | "min_score" | "_name") {
                    return Err(unknown(name, k));
                }
            }
            let (ids, docs) = (body.get("ids"), body.get("docs"));
            if ids.is_some() && docs.is_some() {
                return Err(wrap(EsError::illegal_argument("Both 'ids' and 'docs' cannot be specified at the same time")));
            }
            let non_empty = |v: Option<&Value>| matches!(v, Some(Value::Arr(a)) if !a.is_empty());
            if !non_empty(ids) && !non_empty(docs) {
                return Err(wrap(EsError::illegal_argument("Either 'ids' or 'docs' must be provided and non-empty for pinned retriever")));
            }
            let Some(Value::Obj(inner)) = body.get("retriever") else { return Err(EsError::parse("Required [retriever]")) };
            let window = body.get("rank_window_size").and_then(Value::as_i64).unwrap_or(10);
            checks.push((name.to_string(), "rank_window_size", window));
            let mut fs = filters(body.get("filter"));
            fs.extend(inherited.iter().cloned());
            if matches!(entry(inner)?.0, "rrf" | "linear") {
                let child = crate::stack::maybe_grow(|| self::compound(inner, depth + 1, &fs, checks))?;
                let pins: Vec<(Option<String>, String)> = match (ids, docs) {
                    (Some(Value::Arr(a)), _) => a.iter().map(|v| (None, v.to_es_string())).collect(),
                    (_, Some(Value::Arr(a))) => a.iter().map(|d| (d.get("_index").map(Value::to_es_string), d.get("_id").map(Value::to_es_string).unwrap_or_default())).collect(),
                    _ => Vec::new(),
                };
                let src = SearchSource { fusion: Some(Box::new(child)), ..Default::default() };
                *fusion = Some(Fusion { kind: FusionKind::Pinned { pins, organic: None }, window, min_score: None, children: vec![src], names: vec![None], tails: vec![None] });
                return Ok(());
            }
            let inner_name = entry(inner)?.1.get("_name").map(Value::to_es_string);
            let mut sub = Map::new();
            crate::stack::maybe_grow(|| extract(inner, true, depth + 1, &fs, &mut sub, checks, diversify, fusion))?;
            if let Some(sort) = sub.get("sort") {
                let first = match sort {
                    Value::Arr(a) => a.first().cloned(),
                    other => Some(other.clone()),
                };
                let by_score = match &first {
                    None => true,
                    Some(Value::Str(s)) => s == "_score",
                    Some(Value::Obj(m)) => m.contains_key("_score"),
                    _ => false,
                };
                if !by_score {
                    return Err(wrap(EsError::illegal_argument(format!(
                        "[pinned] retriever only supports sorting by score, invalid sort criterion: {}",
                        first.map(|v| v.to_json()).unwrap_or_default()
                    ))));
                }
            }
            let Some(organic) = sub.shift_remove("query") else {
                return Err(wrap(EsError::illegal_argument("pinned retriever requires retriever with associated query")));
            };
            let mut p = Map::new();
            p.insert("organic".into(), organic);
            if let Some(v) = ids {
                p.insert("ids".into(), v.clone());
            }
            if let Some(v) = docs {
                p.insert("docs".into(), v.clone());
            }
            let query = crate::obj! { "pinned" => Value::Obj(p) };
            sub.insert("query".into(), query.clone());
            let mut src = SearchSource::parse(&Value::Obj(sub), None)?;
            src.diversify = diversify.take();
            let organic = match &query {
                Value::Obj(m) => m.get("pinned").and_then(|p| p.get("organic")).cloned(),
                _ => None,
            };
            *fusion = Some(Fusion { kind: FusionKind::Pinned { pins: Vec::new(), organic }, window, min_score: None, children: vec![src], names: vec![inner_name], tails: vec![Some(query)] });
            let _ = out;
            Ok(())
        }
        "diversify" => {
            let d = parse_diversify(body)?;
            let Some(Value::Obj(inner)) = body.get("retriever") else { return Err(EsError::illegal_argument("Required [retriever]")) };
            let mut fs = filters(body.get("filter"));
            fs.extend(inherited.iter().cloned());
            crate::stack::maybe_grow(|| extract(inner, true, depth + 1, &fs, out, checks, diversify, fusion))?;
            // The child searches the window: a kNN child's `k` is capped by it.
            if let Some(Value::Arr(knn)) = out.get_mut("knn") {
                for k in knn.iter_mut() {
                    if let Value::Obj(km) = k
                        && let Some(kv) = km.get("k").and_then(Value::as_i64)
                        && kv > d.rank_window_size
                    {
                        km.insert("k".into(), Value::from(d.rank_window_size));
                    }
                }
            }
            *diversify = Some(d);
            Ok(())
        }
        other => Err(EsError::new(400, "named_object_not_found_exception", format!("[1:14] unknown field [{other}]"))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::json::parse;

    fn src(body: &str) -> EsResult<SearchSource> {
        let v = parse(body.as_bytes()).unwrap().unwrap();
        let mut s = SearchSource::parse(&v, None)?;
        apply(&mut s, false)?;
        Ok(s)
    }

    #[test]
    fn standard_retriever_becomes_the_search() {
        let s = src(r#"{"retriever":{"standard":{"query":{"match_all":{}},"filter":{"term":{"a":"b"}},"sort":["x"],"terminate_after":5}}}"#).unwrap();
        assert!(s.query.is_some());
        assert_eq!(s.sort.len(), 1);
        assert_eq!(s.terminate_after, Some(5));
        assert!(s.retriever.is_none());
    }

    #[test]
    fn retriever_conflicts_with_search_sections() {
        let e = src(r#"{"retriever":{"standard":{}},"query":{"match_all":{}},"sort":["x"]}"#).unwrap_err();
        assert!(e.reason().contains("cannot specify [retriever] and [query, sort]"), "{}", e.reason());
    }

    #[test]
    fn rescorer_window_and_compound_children() {
        let e = src(r#"{"size":10,"retriever":{"rescorer":{"rescore":{"window_size":5,"query":{"rescore_query":{"match_all":{}}}},"retriever":{"standard":{}}}}}"#).unwrap_err();
        assert!(e.reason().contains("[rescorer] requires [window_size: 5] be greater than or equal to [size: 10]"));
        let e = src(r#"{"retriever":{"rescorer":{"rescore":{"query":{"rescore_query":{"match_all":{}}}},"retriever":{"standard":{"search_after":[1]}}}}}"#).unwrap_err();
        assert_eq!(e.reason(), "[search_after] cannot be used in children of compound retrievers");
        let s = src(r#"{"size":2,"retriever":{"rescorer":{"filter":{"term":{"a":1}},"rescore":{"query":{"rescore_query":{"match_all":{}}}},"retriever":{"knn":{"field":"v","query_vector":[1],"k":2}}}}}"#).unwrap();
        assert_eq!(s.rescore.len(), 1);
        assert_eq!(s.knn.len(), 1);
        assert!(s.knn[0].get("filter").is_some());
    }

    #[test]
    fn diversify_validation_and_window() {
        let e = src(r#"{"retriever":{"diversify":{"type":"mmr","field":"v","lambda":1.5,"size":-3,"retriever":{"standard":{}}}}}"#).unwrap_err();
        assert!(e.reason().contains("[size] of -3 must be greater than zero"));
        assert!(e.reason().contains("The value provided was 1.5"));
        let e = src(r#"{"retriever":{"diversify":{"field":"v","lambda":0.5,"retriever":{"standard":{}}}}}"#).unwrap_err();
        assert_eq!(e.reason(), "Required [type]");
        let s = src(r#"{"retriever":{"diversify":{"type":"mmr","field":"v","lambda":0.5,"rank_window_size":3,"size":2,"retriever":{"knn":{"field":"v","query_vector":[1],"k":10}}}}}"#).unwrap();
        assert_eq!(s.knn[0].get("k").and_then(Value::as_i64), Some(3));
        assert_eq!(s.diversify.as_ref().map(|d| d.size), Some(2));
    }

    #[test]
    fn mmr_prefers_diverse_vectors() {
        let d = Diversify { field: "v".into(), lambda: 0.3, size: 2, rank_window_size: 10, query_vector: Some(vec![1.0, 0.0]), errors: Vec::new() };
        let vectors = vec![Some(vec![1.0, 0.0]), Some(vec![0.99, 0.01]), Some(vec![0.0, 1.0]), None];
        // The best query match first, then the vector least like it.
        assert_eq!(d.mmr(&vectors), vec![0, 2]);
        assert!(d.mmr(&[None, None]).is_empty());
        assert_eq!(max_inner_product(&[1.0, 2.0], &[3.0, 4.0]), 12.0);
        assert_eq!(max_inner_product(&[-1.0], &[1.0]), 0.5);
    }
}
