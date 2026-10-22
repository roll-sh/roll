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
        _ => extract(&r, false, 0, &[], &mut body, &mut checks, &mut diversify)?,
    }
    let size = source.size.unwrap_or(10) as i64;
    let mut errors: Vec<String> = Vec::new();
    for (name, field, window) in &checks {
        // Compound retrievers only check a `size` the request sets.
        if matches!(name.as_str(), "rrf" | "linear") && source.size.is_none() {
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

/// How a compound retriever fuses its children's rankings.
#[derive(Clone, Debug, PartialEq)]
pub enum FusionKind {
    /// `rrf`: `Σ 1 / (rank_constant + rank)`.
    Rrf { rank_constant: i64 },
    /// `linear`: `Σ weight · normalize(score)`.
    Linear { weights: Vec<f32>, normalizers: Vec<Normalizer> },
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
}

impl Fusion {
    /// Combines the children's ranked hits (`combineInnerRetrieverResults`): keys with their
    /// fused scores, best first (ties keep the earlier key order), at most `window`.
    pub fn combine<K: Clone + Eq + std::hash::Hash + Ord>(&self, ranked: &[Vec<(K, f32)>]) -> Vec<(K, f32)> {
        let mut fused: std::collections::HashMap<K, f32> = std::collections::HashMap::new();
        for (i, list) in ranked.iter().enumerate() {
            match &self.kind {
                FusionKind::Rrf { rank_constant } => {
                    for (rank, (k, _)) in list.iter().enumerate() {
                        *fused.entry(k.clone()).or_insert(0.0) += 1.0 / (*rank_constant as f32 + (rank + 1) as f32);
                    }
                }
                FusionKind::Linear { weights, normalizers } => {
                    let mut scores: Vec<f32> = list.iter().map(|(_, s)| *s).collect();
                    normalizers.get(i).copied().unwrap_or(Normalizer::None).apply(&mut scores);
                    let w = weights.get(i).copied().unwrap_or(1.0);
                    for ((k, _), s) in list.iter().zip(scores) {
                        let s = if s.is_nan() { 0.0 } else { s };
                        *fused.entry(k.clone()).or_insert(0.0) += w * s;
                    }
                }
            }
        }
        let mut out: Vec<(K, f32)> = fused.into_iter().collect();
        out.sort_by(|a, b| b.1.total_cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
        out.truncate(self.window.max(0) as usize);
        if let Some(m) = self.min_score {
            out.retain(|(_, s)| *s >= m);
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
    extract(r, true, depth, inherited, &mut body, checks, &mut diversify)?;
    let mut s = SearchSource::parse(&Value::Obj(body), None)?;
    s.diversify = diversify;
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
    for c in list {
        let Value::Obj(cm) = c else { return Err(EsError::parse(format!("[{name}] retrievers must be objects"))) };
        // `{"retriever": {...}, "weight": .., "normalizer": ..}` or a bare retriever.
        let (inner, weight, normalizer) = match cm.get("retriever") {
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
        FusionKind::Rrf { rank_constant: int("rank_constant")?.unwrap_or(60) }
    };
    let min_score = body.get("min_score").and_then(Value::as_f64).map(|m| m as f32);
    Ok(Fusion { kind, window, min_score, children })
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
fn extract(
    r: &Map,
    compound: bool,
    depth: usize,
    inherited: &[Value],
    out: &mut Map,
    checks: &mut Vec<(String, &'static str, i64)>,
    diversify: &mut Option<Diversify>,
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
            crate::stack::maybe_grow(|| extract(inner, true, depth + 1, &fs, out, checks, diversify))?;
            match out.get_mut("rescore") {
                Some(Value::Arr(a)) => a.extend(rescorers),
                _ => {
                    out.insert("rescore".into(), Value::Arr(rescorers));
                }
            }
            Ok(())
        }
        "diversify" => {
            let d = parse_diversify(body)?;
            let Some(Value::Obj(inner)) = body.get("retriever") else { return Err(EsError::illegal_argument("Required [retriever]")) };
            let mut fs = filters(body.get("filter"));
            fs.extend(inherited.iter().cloned());
            crate::stack::maybe_grow(|| extract(inner, true, depth + 1, &fs, out, checks, diversify))?;
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
