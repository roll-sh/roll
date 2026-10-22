//! The remaining Query DSL conversions: scoring compounds (`function_score`, `script_score`),
//! `script`, `pinned`, `distance_feature`, `rank_feature`, `terms_set`, kNN, geo queries on
//! points and shapes, spans, intervals and `more_like_this`, plus helpers other conversions
//! share (`wildcard` fields, terms lookups, `match_only_text` confirmation, script sorts).

use super::query::{QueryCtx, arc, index_err, match_none, qse, value_text};
use super::scripts::Painless;
use crate::engine::fields::{FEATURE_FIELD, FieldInfo, Kind};
use crate::error::{EsError, EsResult};
use crate::json::{Number, Value};
use crate::query::Query;
use crate::query::compound::{FunctionScore, ScoreFunctionKind};
use search_index::collector::top_field::SortKey;
use search_index::query::function_score::{CombineFunction, DecayFn, DecayOrigin, FunctionEntry, FunctionScoreMode, Modifier, MultiValueMode, NumDecode, RandomField, ScoreFunction};
use search_index::query::{ConstantScoreQuery, MultiTermQuery, TermQuery};
use search_index::QueryRef;
use std::sync::Arc;

/// Queries not handled by the core conversion.
pub fn to_query(ctx: &QueryCtx, q: &Query) -> EsResult<QueryRef> {
    match q {
        Query::Pinned(p) => {
            let organic = ctx.to_query(&p.organic)?;
            let ids: Vec<String> = match (&p.ids, &p.docs) {
                (Some(ids), _) => ids.clone(),
                (None, Some(docs)) => docs.iter().filter(|d| d.index.as_deref().is_none_or(|i| i == ctx.idx.name)).map(|d| d.id.clone()).collect(),
                _ => Vec::new(),
            };
            pinned(organic, &ids)
        }
        Query::Script(s) => {
            let p = painless(ctx, &s.script, search_painless::Context::Filter)?;
            Ok(arc(search_index::query::ScriptQuery { script: Arc::new(p) }))
        }
        Query::ScriptScore(s) => {
            let inner = ctx.to_query(&s.query)?;
            let p = painless(ctx, &s.script, search_painless::Context::Score)?;
            Ok(arc(search_index::query::ScriptScoreQuery { query: inner, script: Arc::new(p), min_score: s.min_score }))
        }
        Query::FunctionScore(f) => function_score(ctx, f),
        Query::DistanceFeature(d) => distance_feature(ctx, d),
        Query::RankFeature(r) => rank_feature(ctx, r),
        Query::TermsSet(t) => terms_set(ctx, t),
        Query::Knn(k) => knn_query(ctx, k),
        Query::GeoDistance(g) => geo_distance(ctx, g),
        Query::GeoBoundingBox(g) => geo_bbox(ctx, g),
        Query::GeoPolygon(g) => geo_polygon(ctx, g),
        Query::GeoShape(g) => super::geo::geo_shape_query(ctx, g),
        Query::Shape(s) => super::geo::shape_query(ctx, s),
        Query::SpanTerm(_)
        | Query::SpanMulti(_)
        | Query::SpanFirst(_)
        | Query::SpanNear(_)
        | Query::SpanOr(_)
        | Query::SpanNot(_)
        | Query::SpanContaining(_)
        | Query::SpanWithin(_)
        | Query::SpanFieldMasking(_) => {
            let node = span_node(ctx, q)?;
            match node {
                Some(n) => Ok(arc(search_index::query::SpanQuery::new(n).map_err(index_err)?)),
                None => Ok(match_none("unmapped span field")),
            }
        }
        Query::Intervals(i) => super::geo::intervals_query(ctx, i),
        Query::HasChild(h) => super::join::has_child(ctx, h),
        Query::HasParent(h) => super::join::has_parent(ctx, h),
        Query::ParentId(p) => super::join::parent_id(ctx, p),
        Query::Percolate(p) => super::percolate::percolate(ctx, p),
        Query::MoreLikeThis(m) => super::geo::more_like_this(ctx, m),
        other => Err(qse(ctx, format!("[{}] queries are not supported yet", other.name()))),
    }
}

/// A script of a query compiled for `context` as an engine hook.
pub fn painless(ctx: &QueryCtx, s: &crate::query::script::Script, context: search_painless::Context) -> EsResult<Painless> {
    let bound = ctx.cluster.scripts.compile(ctx.st, s, context)?;
    Ok(Painless { bound, table: ctx.table.clone(), shard: ctx.shard as i32, index: ctx.idx.name.clone() })
}

/// `PinnedQueryBuilder.doToQuery`: pinned ids score above every organic hit, in order.
fn pinned(organic: QueryRef, ids: &[String]) -> EsResult<QueryRef> {
    const MAX_ORGANIC: f32 = 3.4028235e38 / 10.0;
    let capped: QueryRef = arc(search_index::query::CappedScoreQuery::new(organic, MAX_ORGANIC).map_err(index_err)?);
    if ids.is_empty() {
        return Ok(capped);
    }
    let mut b = search_index::query::DisMaxQuery::new(vec![capped], 0.0);
    let n = ids.len() as f32;
    for (i, id) in ids.iter().enumerate() {
        let boost = MAX_ORGANIC + n - i as f32;
        b.disjuncts.push(arc(search_index::query::BoostQuery::new(
            arc(ConstantScoreQuery::new(arc(TermQuery::new(search_index::schema::ID_FIELD, id.as_bytes().to_vec())))),
            boost,
        )));
    }
    Ok(arc(b))
}

fn num_decode(f: &FieldInfo) -> NumDecode {
    match f.kind {
        Kind::Double => NumDecode::Double,
        Kind::Float => NumDecode::Float,
        Kind::HalfFloat => NumDecode::HalfFloat,
        Kind::ScaledFloat(sf) => NumDecode::Scaled(sf),
        Kind::UnsignedLong => NumDecode::UnsignedLong,
        _ => NumDecode::Long,
    }
}

/// Java `String.hashCode`.
fn java_hash(s: &str) -> i32 {
    let mut h: i32 = 0;
    for u in s.encode_utf16() {
        h = h.wrapping_mul(31).wrapping_add(u as i32);
    }
    h
}

fn function_score(ctx: &QueryCtx, f: &FunctionScore) -> EsResult<QueryRef> {
    let inner = ctx.to_query(&f.query)?;
    let mut entries = Vec::with_capacity(f.functions.len());
    for ff in &f.functions {
        let named_before = ctx.named.borrow().len();
        let filter = match &ff.filter {
            Query::MatchAll(_) => None,
            q => Some(ctx.to_filter(q)?),
        };
        let named_after = ctx.named.borrow().len();
        let function = match &ff.function.kind {
            ScoreFunctionKind::Weight => ScoreFunction::Weight,
            ScoreFunctionKind::Script(s) => ScoreFunction::Script(Arc::new(painless(ctx, s, search_painless::Context::Score)?)),
            ScoreFunctionKind::Random { seed, field } => {
                let salt = (java_hash(&ctx.idx.name) << 10) | ctx.shard as i32;
                match seed {
                    None => ScoreFunction::Random { seed: rand::random::<i32>(), salt, field: None },
                    Some(seed) => {
                        let rf = match field.as_deref() {
                            None | Some("_seq_no") => {
                                if field.is_none() {
                                    ctx.cluster_warn("As of version 7.0 Elasticsearch will require that a [field] parameter is provided when a [seed] is set");
                                }
                                Some(RandomField::Number { field: search_index::schema::SEQ_NO_FIELD, name: "_seq_no".into(), decode: NumDecode::Long })
                            }
                            Some(name) => match ctx.field(name) {
                                Some(fi) if fi.kind.is_keyword_like() => Some(RandomField::Bytes { field: fi.number, name: fi.name.clone() }),
                                Some(fi) => Some(RandomField::Number { field: fi.number, name: fi.name.clone(), decode: num_decode(&fi) }),
                                None => None,
                            },
                        };
                        ScoreFunction::Random { seed: *seed, salt, field: rf }
                    }
                }
            }
            ScoreFunctionKind::FieldValueFactor { field, factor, missing, modifier } => {
                let Some(fi) = ctx.field(field) else {
                    // An unmapped field scores `missing` (or fails for every doc).
                    return Err(EsError::new(400, "elasticsearch_exception", format!("Unable to find a field mapper for field [{field}]. No 'missing' value defined.")))
                        .or_else(|e| if missing.is_some() { Ok(match_none("")) } else { Err(e) });
                };
                let m = match *modifier {
                    "log" => Modifier::Log,
                    "log1p" => Modifier::Log1p,
                    "log2p" => Modifier::Log2p,
                    "ln" => Modifier::Ln,
                    "ln1p" => Modifier::Ln1p,
                    "ln2p" => Modifier::Ln2p,
                    "square" => Modifier::Square,
                    "sqrt" => Modifier::Sqrt,
                    "reciprocal" => Modifier::Reciprocal,
                    _ => Modifier::None,
                };
                ScoreFunction::FieldValueFactor { field: fi.number, name: fi.name.clone(), factor: *factor, modifier: m, missing: *missing, decode: num_decode(&fi) }
            }
            ScoreFunctionKind::Decay { kind, field, body, multi_value_mode } => decay(ctx, kind, field, body, multi_value_mode)?,
        };
        // A named function (`_name` beside the function) takes the place of its filter's name.
        if ff.name.is_some() && named_after > named_before {
            ctx.named.borrow_mut().truncate(named_before);
        }
        // A named function filter matches with the function's score (its weighted value), as
        // `FunctionScoreQuery` reports named functions.
        if ff.name.is_none() && named_after > named_before {
            let mut named = ctx.named.borrow_mut();
            for (_, q) in named[named_before..named_after].iter_mut() {
                let mut fq = search_index::query::FunctionScoreQuery::new(q.clone(), vec![FunctionEntry { filter: None, function: function.clone(), weight: ff.function.weight }]);
                fq.boost_mode = CombineFunction::Replace;
                *q = arc(fq);
            }
        }
        // A named function (`_name` beside the function) matches its filter with its score.
        if let Some(name) = &ff.name {
            let base = filter.clone().unwrap_or_else(|| arc(search_index::query::MatchAllQuery));
            let mut fq = search_index::query::FunctionScoreQuery::new(base, vec![FunctionEntry { filter: None, function: function.clone(), weight: ff.function.weight }]);
            fq.boost_mode = CombineFunction::Replace;
            ctx.named.borrow_mut().push((name.clone(), arc(fq)));
        }
        entries.push(FunctionEntry { filter, function, weight: ff.function.weight });
    }
    let mut q = search_index::query::FunctionScoreQuery::new(inner, entries);
    q.score_mode = match f.score_mode {
        "first" => FunctionScoreMode::First,
        "avg" => FunctionScoreMode::Avg,
        "max" => FunctionScoreMode::Max,
        "sum" => FunctionScoreMode::Sum,
        "min" => FunctionScoreMode::Min,
        _ => FunctionScoreMode::Multiply,
    };
    q.boost_mode = match f.boost_mode.unwrap_or("multiply") {
        "replace" => CombineFunction::Replace,
        "sum" => CombineFunction::Sum,
        "avg" => CombineFunction::Avg,
        "min" => CombineFunction::Min,
        "max" => CombineFunction::Max,
        _ => CombineFunction::Multiply,
    };
    q.max_boost = f.max_boost;
    q.min_score = f.min_score;
    Ok(arc(q))
}

fn decay(ctx: &QueryCtx, kind: &str, field: &str, body: &Value, mode: &str) -> EsResult<ScoreFunction> {
    let func = match kind {
        "gauss" => DecayFn::Gauss,
        "exp" => DecayFn::Exp,
        _ => DecayFn::Linear,
    };
    let mode = match mode {
        "max" => MultiValueMode::Max,
        "avg" => MultiValueMode::Avg,
        "sum" => MultiValueMode::Sum,
        "median" => MultiValueMode::Median,
        _ => MultiValueMode::Min,
    };
    let Some(fi) = ctx.field(field) else {
        return Err(EsError::parse(format!("unknown field [{field}]")));
    };
    let get = |k: &str| body.get(k);
    let decay_v = get("decay").map(|v| match v {
        Value::Num(n) => n.as_f64(),
        other => value_text(other).parse().unwrap_or(0.5),
    }).unwrap_or(0.5);
    let num = |v: &Value| -> EsResult<f64> {
        match v {
            Value::Num(n) => Ok(n.as_f64()),
            other => value_text(other).parse::<f64>().map_err(|_| EsError::number_format(&value_text(other))),
        }
    };
    let (origin, scale, offset) = match fi.kind {
        Kind::Date | Kind::DateNanos => {
            let origin = match get("origin") {
                Some(o) => super::range::date_value(ctx, &fi, o, None, None, false)?,
                None => ctx.now_ms,
            };
            let tv = |k: &str| -> EsResult<f64> {
                match get(k) {
                    Some(v) => Ok(crate::units::TimeValue::parse(&value_text(v), &format!("{kind}.{k}"))?.as_millis() as f64),
                    None => Ok(0.0),
                }
            };
            let scale = match get("scale") {
                Some(_) => tv("scale")?,
                None => return Err(EsError::parse(format!("[{kind}] must be set for date fields."))),
            };
            (DecayOrigin::Number(origin as f64, NumDecode::Long), scale, tv("offset")?)
        }
        Kind::GeoPoint => {
            let o = get("origin").ok_or_else(|| EsError::parse("[origin] must be set for geo fields.".to_string()))?;
            let pts = super::sort::parse_points(o)?;
            let p = pts.first().copied().ok_or_else(|| EsError::parse("illegal origin"))?;
            let dist = |k: &str| -> EsResult<f64> {
                match get(k) {
                    Some(v) => crate::query::geo::parse_distance(&value_text(v), 1.0),
                    None => Ok(0.0),
                }
            };
            (DecayOrigin::Geo { lat: p.lat, lon: p.lon }, dist("scale")?, dist("offset")?)
        }
        k if k.is_numeric() => {
            let origin = get("origin").map(num).transpose()?.ok_or_else(|| EsError::parse("[origin] must be set for numeric fields.".to_string()))?;
            let scale = get("scale").map(num).transpose()?.ok_or_else(|| EsError::parse("[scale] must be set for numeric fields.".to_string()))?;
            (DecayOrigin::Number(origin, num_decode(&fi)), scale, get("offset").map(num).transpose()?.unwrap_or(0.0))
        }
        _ => return Err(EsError::illegal_argument(format!("field [{field}] is of type [{}], but only numeric types are supported.", fi.ty))),
    };
    Ok(ScoreFunction::Decay { field: fi.number, name: fi.name.clone(), func, origin, scale, decay: decay_v, offset, mode })
}

fn distance_feature(ctx: &QueryCtx, d: &crate::query::specialized::DistanceFeature) -> EsResult<QueryRef> {
    let Some(f) = ctx.field(&d.field) else { return Ok(match_none(&format!("unmapped field [{}]", d.field))) };
    let boost = 1.0;
    match f.kind {
        Kind::Date | Kind::DateNanos => {
            // `DateFieldType.distanceFeatureQuery` parses the origin rounding up.
            let origin = super::range::date_value(ctx, &f, &d.origin, None, None, true)?;
            let pivot = crate::units::TimeValue::parse(&d.pivot, "pivot")?;
            let pivot = if f.kind == Kind::DateNanos { (pivot.nanos()).clamp(1, i64::MAX as i128) as i64 } else { pivot.as_millis() };
            if !f.index && f.doc_values {
                if pivot <= 0 {
                    return Err(EsError::illegal_argument(format!("pivotDistance must be > 0, got {pivot}")));
                }
                return Ok(super::dv_distance::DocValuesDistanceFeatureQuery::query(f.number, search_index::query::distance_feature::Origin::Long { origin, pivot }));
            }
            search_index::query::DistanceFeatureQuery::long(f.number, boost, origin, pivot).map_err(index_err)
        }
        Kind::GeoPoint => {
            let p = super::sort::parse_points(&d.origin)?.first().copied().ok_or_else(|| EsError::parse("illegal origin"))?;
            let pivot = crate::query::geo::parse_distance(&d.pivot, 1.0)?;
            if !f.index && f.doc_values {
                return Ok(super::dv_distance::DocValuesDistanceFeatureQuery::query(f.number, search_index::query::distance_feature::Origin::Geo { lat: p.lat, lon: p.lon, pivot }));
            }
            search_index::query::DistanceFeatureQuery::geo(f.number, boost, p.lat, p.lon, pivot).map_err(index_err)
        }
        _ => Err(EsError::illegal_argument(format!(
            "Illegal data type of [{}]! [distance_feature] query can only be run on a date, date_nanos or geo_point field type!",
            f.ty
        ))),
    }
}

fn rank_feature(ctx: &QueryCtx, r: &crate::query::specialized::RankFeature) -> EsResult<QueryRef> {
    use crate::query::specialized::RankFeatureFunction as F;
    let (field, feature, positive): (u32, String, bool) = match ctx.field(&r.field) {
        Some(f) if f.kind == Kind::RankFeature => (FEATURE_FIELD, f.name.clone(), f.mapper.raw("positive_score_impact").and_then(Value::as_bool).unwrap_or(true)),
        Some(f) => return Err(EsError::illegal_argument(format!("[rank_feature] query only works on [rank_feature] fields and features of [rank_features] fields, not [{}]", f.ty))),
        None => {
            // A feature of a `rank_features` field: `features.name`.
            let Some((parent, name)) = r.field.rsplit_once('.') else { return Ok(match_none("unmapped")) };
            match ctx.field(parent) {
                Some(p) if matches!(p.kind, Kind::RankFeatures | Kind::SparseVector) => (p.number, name.to_string(), p.mapper.raw("positive_score_impact").and_then(Value::as_bool).unwrap_or(true)),
                _ => return Ok(match_none("unmapped")),
            }
        }
    };
    let _ = positive;
    let q = match &r.function {
        F::Linear => search_index::query::FeatureQuery::linear(field, feature.into_bytes(), 1.0),
        F::Log { scaling_factor } => search_index::query::FeatureQuery::log(field, feature.into_bytes(), 1.0, *scaling_factor),
        F::Saturation { pivot } => search_index::query::FeatureQuery::saturation(field, feature.into_bytes(), 1.0, *pivot),
        F::Sigmoid { pivot, exponent } => search_index::query::FeatureQuery::sigmoid(field, feature.into_bytes(), 1.0, *pivot, *exponent),
    };
    q.map_err(index_err)
}

#[derive(Debug)]
struct MinMatchPainless(Painless);

impl std::fmt::Debug for Painless {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.bound.describe)
    }
}

struct MinLeaf(super::scripts::Leaf);

impl search_index::query::covering::LeafMinMatch for MinLeaf {
    fn value(&mut self, doc: u32) -> Option<i64> {
        self.0.run_f64(doc, 0.0).ok().map(|v| v as i64)
    }
}

impl search_index::query::covering::MinMatchScript for MinMatchPainless {
    fn leaf(&self, seg: &search_index::SegmentContext) -> search_index::Result<Box<dyn search_index::query::covering::LeafMinMatch>> {
        let s = self.0.bound.painless().ok_or_else(|| search_index::Error::illegal("not painless"))?;
        let host = super::scripts::DocHost::new(self.0.table.clone(), seg.clone(), self.0.shard, self.0.index.clone());
        let mut leaf = super::scripts::Leaf::new(s, &self.0.bound.params, host);
        let _ = &mut leaf;
        Ok(Box::new(MinLeaf(leaf)))
    }
    fn describe(&self) -> String {
        self.0.bound.describe.clone()
    }
}

fn terms_set(ctx: &QueryCtx, t: &crate::query::term_level::TermsSet) -> EsResult<QueryRef> {
    let Some(f) = ctx.field(&t.field) else { return Ok(match_none("unmapped")) };
    let mut queries = Vec::with_capacity(t.values.len());
    for v in &t.values {
        queries.push(ctx.term_query(&f, v, false)?);
    }
    let min = if let Some(mf) = &t.minimum_should_match_field {
        let Some(m) = ctx.field(mf) else { return Ok(match_none("unmapped minimum_should_match_field")) };
        search_index::query::MinMatch::Field { field: m.number, name: m.name.clone() }
    } else if let Some(s) = &t.minimum_should_match_script {
        let mut s2 = s.clone();
        s2.params.insert("num_terms".into(), Value::from(t.values.len() as i64));
        let p = painless(ctx, &s2, search_painless::Context::TermsSet)?;
        search_index::query::MinMatch::Script(Arc::new(MinMatchPainless(p)))
    } else if let Some(m) = &t.minimum_should_match {
        search_index::query::MinMatch::Constant(super::query::calculate_msm(t.values.len() as i32, m)? as i64)
    } else {
        return Err(EsError::illegal_argument("No minimum should match has been specified"));
    };
    Ok(arc(search_index::query::CoveringQuery { queries, min }))
}

/// The `knn` query (`KnnVectorQueryBuilder`).
fn knn_query(ctx: &QueryCtx, k: &crate::query::specialized::Knn) -> EsResult<QueryRef> {
    let Some(f) = ctx.field(&k.field) else {
        if ctx.idx.settings.get("index.query.parse.allow_unmapped_fields") == Some("false") {
            return Err(qse(ctx, format!("No field mapping can be found for the field with name [{}]", k.field)));
        }
        return Ok(match_none(&format!("unmapped field [{}]", k.field)));
    };
    let spec = f.vector.ok_or_else(|| qse(ctx, "[knn] queries are only supported on [dense_vector] fields".to_string()))?;
    if spec.dims == 0 {
        // No document set the dimensions yet: nothing to match.
        return Ok(match_none("dense_vector field without dims"));
    }
    if !f.index {
        return Err(EsError::illegal_argument(format!("to perform knn search on field [{}], its mapping must have [index] set to [true]", f.name)));
    }
    let vector = super::knn::query_vector(&f, &spec, k.query_vector.as_ref())?;
    let num = k.num_candidates.map(|n| n as usize);
    let kk = k.k.map(|v| v as usize).unwrap_or_else(|| num.unwrap_or(10).clamp(1, 10));
    let num_candidates = num.unwrap_or_else(|| (kk as f64 * 1.5).ceil() as usize).max(kk).min(10_000);
    let exact = ctx.exact_knn.get();
    let (kk, num_candidates) = if exact { (10_000, 10_000) } else { (kk, num_candidates) };
    let mut q = search_index::query::KnnQuery::new(f.number, vector, kk, num_candidates);
    q.exact = exact;
    let parent_of_nested = |np: &str| -> QueryRef {
        match &ctx.table.objects.get(np).and_then(|o| o.parent_nested.clone()) {
            None => search_index::query::nested::root_filter(),
            Some(p) => arc(TermQuery::new(crate::engine::fields::NESTED_PATH_FIELD, p.as_bytes().to_vec())),
        }
    };
    // The alias filter is one more pre-filter (`KnnVectorQueryBuilder` gets it from the
    // `SearchExecutionContext`).
    let mut filters: Vec<&Query> = k.filter.iter().collect();
    if let Some(af) = &ctx.alias_filter {
        filters.push(af);
    }
    if !filters.is_empty() {
        let mut b = search_index::query::BooleanQuery::new();
        for x in filters {
            let mut fq = ctx.to_filter(x)?;
            // Nested vectors: a filter that may match parents is a filter over parents, joined
            // to their children (`KnnVectorQueryBuilder.doToQuery`).
            if let Some(np) = &f.nested_path
                && might_match_non_nested(ctx, x, np)
            {
                let parent = parent_of_nested(np);
                let parents = arc(search_index::query::BooleanQuery::new().add(search_index::query::Occur::Must, fq).add(search_index::query::Occur::Filter, parent.clone()));
                fq = arc(search_index::query::ToChildQuery { parent: parents, parent_filter: parent });
            }
            b = b.add(search_index::query::Occur::Filter, fq);
        }
        q.filter = Some(arc(b));
    }
    if let Some(sim) = k.similarity {
        q.min_score = Some(super::knn::similarity_to_score(&spec, sim));
    }
    q.oversample = k.rescore_oversample;
    let nested_parent = f.nested_path.as_ref().map(|np| parent_of_nested(np));
    if let Some(parent) = nested_parent.as_ref().filter(|_| !exact) {
        // Diversified by parent (`DiversifyingChildrenFloatKnnVectorQuery`) in or out of a
        // `nested` query; out of one, the result is joined to the parents.
        q.parent_filter = Some(parent.clone());
    }
    let kq: QueryRef = if f.vector_layouts.is_empty() {
        arc(q)
    } else {
        // Vectors written under earlier index options live in their own engine fields.
        let mut all = vec![q.clone()];
        for n in &f.vector_layouts {
            let mut o = q.clone();
            o.field = *n;
            all.push(o);
        }
        arc(LayoutsKnn { queries: all, k: kk })
    };
    if let Some(parent) = nested_parent
        && ctx.nested.borrow().is_none()
    {
        return Ok(arc(search_index::query::ToParentQuery { child: kq, parent_filter: parent, score_mode: search_index::query::JoinScoreMode::Max }));
    }
    Ok(kq)
}

/// A kNN search over every layout of a vector field (index options changed by mapping
/// updates): one search per layout's engine field, the best `k` of all of them (what a
/// single Lucene field searched across segments of different formats finds).
#[derive(Debug)]
struct LayoutsKnn {
    queries: Vec<search_index::query::KnnQuery>,
    k: usize,
}

impl LayoutsKnn {
    fn top(&self, s: &search_index::Searcher) -> search_index::Result<Vec<(u32, f32)>> {
        let mut all = Vec::new();
        for q in &self.queries {
            all.extend(q.search(s)?);
        }
        all.sort_by(|a, b| b.1.total_cmp(&a.1).then(a.0.cmp(&b.0)));
        all.dedup_by_key(|x| x.0);
        all.truncate(self.k);
        Ok(all)
    }
}

impl search_index::Query for LayoutsKnn {
    fn weight(&self, s: &search_index::Searcher, mode: search_index::ScoreMode, boost: f32) -> search_index::Result<Box<dyn search_index::Weight>> {
        search_index::query::ScoreDocQuery::lucene(self.top(s)?).weight(s, mode, boost)
    }

    fn rewrite(&self, s: &search_index::Searcher) -> search_index::Result<Option<QueryRef>> {
        let r = self.top(s)?;
        if r.is_empty() {
            return Ok(Some(arc(search_index::query::MatchNoneQuery::new(""))));
        }
        Ok(Some(arc(search_index::query::ScoreDocQuery::lucene(r))))
    }

    fn lucene_string(&self, schema: &search_index::Schema) -> String {
        self.queries[0].lucene_string(schema)
    }

    fn as_any(&self) -> &dyn std::any::Any {
        self
    }
}

/// `NestedHelper.mightMatchNonNestedDocs` on the query a filter converts to: whether it may
/// match documents other than the nested documents of `nested_path`. Only term-like, range and
/// boolean shapes are looked into; anything else may.
pub fn might_match_non_nested(ctx: &QueryCtx, q: &Query, nested_path: &str) -> bool {
    let field = |name: &str| -> bool {
        if name.starts_with('_') {
            return true;
        }
        let Some(f) = ctx.field(name) else { return false };
        match &f.nested_path {
            Some(p) if p.starts_with(nested_path) => {
                p != nested_path || ctx.table.objects.get(p.as_str()).is_some_and(|o| o.include_in_parent || o.include_in_root)
            }
            _ => true,
        }
    };
    match q {
        Query::MatchAll(_) => true,
        Query::MatchNone(_) => false,
        Query::Term(t) => field(&t.field),
        Query::Terms(t) => t.values.as_ref().is_none_or(|v| !v.is_empty()) && field(&t.field),
        Query::Match(m) => field(&m.field),
        Query::Range(r) => match ctx.field(&r.field) {
            Some(f) if f.kind.is_numeric() || f.kind.is_date() || f.kind == Kind::Ip => field(&r.field),
            Some(_) => true,
            None => false,
        },
        Query::ConstantScore(c) => might_match_non_nested(ctx, &c.filter, nested_path),
        Query::Bool(b) => {
            if !b.must.is_empty() || !b.filter.is_empty() {
                b.must.iter().chain(&b.filter).all(|c| might_match_non_nested(ctx, c, nested_path))
            } else if b.should.is_empty() {
                // Only `must_not`: the converted query requires match-all.
                true
            } else {
                b.should.iter().any(|c| might_match_non_nested(ctx, c, nested_path))
            }
        }
        _ => true,
    }
}

fn geo_point_field(ctx: &QueryCtx, field: &str, ignore_unmapped: bool, query: &str) -> EsResult<Option<Arc<FieldInfo>>> {
    match ctx.field(field) {
        Some(f) if f.kind == Kind::GeoPoint || f.kind == Kind::GeoShape => Ok(Some(f)),
        Some(f) => Err(qse(ctx, format!("Field [{}] is of unsupported type [{}] for [{query}] query", f.name, f.ty))),
        None if ignore_unmapped => Ok(None),
        None => Err(qse(ctx, format!("failed to find geo field [{field}]"))),
    }
}

fn geo_distance(ctx: &QueryCtx, g: &crate::query::geo::GeoDistance) -> EsResult<QueryRef> {
    let Some(f) = geo_point_field(ctx, &g.field, g.ignore_unmapped, "geo_distance")? else { return Ok(match_none("unmapped")) };
    if f.kind == Kind::GeoShape {
        let geom = vec![search_index::query::shape::QueryGeometry::Circle { lat: g.center.lat, lon: g.center.lon, radius: g.distance }];
        return search_index::query::shape::geo_shape_query(f.number, search_index::query::ShapeRelation::Intersects, geom, f.index, f.doc_values).map_err(index_err);
    }
    let shape = search_index::query::GeoShape::Circle { lat: g.center.lat, lon: g.center.lon, radius: g.distance };
    super::geo::point_query(&f, search_index::query::ShapeRelation::Intersects, vec![shape], true)
}

fn geo_bbox(ctx: &QueryCtx, g: &crate::query::geo::GeoBoundingBox) -> EsResult<QueryRef> {
    let Some(f) = geo_point_field(ctx, &g.field, g.ignore_unmapped, "geo_bounding_box")? else { return Ok(match_none("unmapped")) };
    if f.kind == Kind::GeoShape {
        let geom = vec![search_index::query::shape::QueryGeometry::Rectangle { min_lat: g.bottom, max_lat: g.top, min_lon: g.left, max_lon: g.right }];
        return search_index::query::shape::geo_shape_query(f.number, search_index::query::ShapeRelation::Intersects, geom, f.index, f.doc_values).map_err(index_err);
    }
    let q = match (f.index, f.doc_values) {
        (true, true) => arc(search_index::query::IndexOrDocValuesQuery {
            index: search_index::query::geo::bounding_box(f.number, g.bottom, g.top, g.left, g.right).map_err(index_err)?,
            dv: search_index::query::GeoPointDvQuery::bounding_box(f.number, g.bottom, g.top, g.left, g.right).map_err(index_err)?,
        }),
        (true, false) => search_index::query::geo::bounding_box(f.number, g.bottom, g.top, g.left, g.right).map_err(index_err)?,
        (false, true) => search_index::query::GeoPointDvQuery::bounding_box(f.number, g.bottom, g.top, g.left, g.right).map_err(index_err)?,
        _ => return Err(super::query::not_searchable(&f)),
    };
    Ok(arc(ConstantScoreQuery::new(q)))
}

fn geo_polygon(ctx: &QueryCtx, g: &crate::query::geo::GeoPolygon) -> EsResult<QueryRef> {
    let Some(f) = geo_point_field(ctx, &g.field, g.ignore_unmapped, "geo_polygon")? else { return Ok(match_none("unmapped")) };
    let outer: Vec<(f64, f64)> = g.points.iter().map(|p| (p.lat, p.lon)).collect();
    let poly = search_index::query::geo::Polygon { outer, holes: Vec::new() };
    super::geo::point_query(&f, search_index::query::ShapeRelation::Intersects, vec![search_index::query::GeoShape::Polygon(poly)], true)
}

/// A span query node (`SpanQueryBuilder.toQuery`); `None` for unmapped fields.
fn span_node(ctx: &QueryCtx, q: &Query) -> EsResult<Option<search_index::query::SpanNode>> {
    use search_index::query::SpanNode;
    Ok(Some(match q {
        Query::SpanTerm(t) => {
            let Some(f) = ctx.field(&t.field) else { return Ok(None) };
            let term = if f.kind.is_text() { value_text(&t.value) } else { super::query::normalize(&f, &value_text(&t.value)) };
            SpanNode::Term { field: f.number, term: term.into_bytes() }
        }
        Query::SpanNear(n) => {
            let mut clauses = Vec::new();
            for c in &n.clauses {
                if let Query::SpanGap(g) = c {
                    let Some(f) = ctx.field(&g.field) else { return Ok(None) };
                    clauses.push(SpanNode::Gap { field: f.number, width: g.width.max(0) as u32 });
                    continue;
                }
                match span_node(ctx, c)? {
                    Some(x) => clauses.push(x),
                    None => return Ok(None),
                }
            }
            SpanNode::Near { clauses, slop: n.slop.max(0) as u32, in_order: n.in_order }
        }
        Query::SpanOr(o) => {
            let mut clauses = Vec::new();
            for c in &o.clauses {
                if let Some(x) = span_node(ctx, c)? {
                    clauses.push(x);
                }
            }
            if clauses.is_empty() {
                return Ok(None);
            }
            SpanNode::Or(clauses)
        }
        Query::SpanFirst(f) => {
            let Some(inner) = span_node(ctx, &f.query)? else { return Ok(None) };
            SpanNode::First { inner: Box::new(inner), end: f.end.max(0) as u32 }
        }
        Query::SpanNot(n) => {
            let (Some(i), Some(e)) = (span_node(ctx, &n.include)?, span_node(ctx, &n.exclude)?) else { return Ok(None) };
            SpanNode::Not { include: Box::new(i), exclude: Box::new(e), pre: n.pre.max(0) as u32, post: n.post.max(0) as u32 }
        }
        Query::SpanContaining(c) => {
            let (Some(b), Some(l)) = (span_node(ctx, &c.big)?, span_node(ctx, &c.little)?) else { return Ok(None) };
            SpanNode::Containing { big: Box::new(b), little: Box::new(l) }
        }
        Query::SpanWithin(c) => {
            let (Some(b), Some(l)) = (span_node(ctx, &c.big)?, span_node(ctx, &c.little)?) else { return Ok(None) };
            SpanNode::Within { big: Box::new(b), little: Box::new(l) }
        }
        Query::SpanFieldMasking(m) => {
            let Some(inner) = span_node(ctx, &m.query)? else { return Ok(None) };
            let Some(f) = ctx.field(&m.field) else { return Ok(None) };
            SpanNode::Mask { inner: Box::new(inner), field: f.number }
        }
        Query::SpanMulti(m) => {
            let mut inner = ctx.to_query(&m.query)?;
            // Wrappers of the converted query (constant scoring, boosts) don't matter to spans.
            loop {
                let next = if let Some(c) = inner.as_any().downcast_ref::<ConstantScoreQuery>() {
                    c.query.clone()
                } else if let Some(b) = inner.as_any().downcast_ref::<search_index::query::BoostQuery>() {
                    b.query.clone()
                } else {
                    break;
                };
                inner = next;
            }
            // A prefix within a text field's `index_prefixes` is a term of the prefix field,
            // reported as the text field (`TextFieldType.spanPrefixQuery`).
            if let (Some(t), Query::Prefix(p)) = (inner.as_any().downcast_ref::<TermQuery>(), &m.query)
                && let Some(f) = ctx.field(&p.field)
                && f.prefix_field.is_some_and(|(pf, _, _)| pf == t.field)
            {
                return Ok(Some(SpanNode::Mask { inner: Box::new(SpanNode::Term { field: t.field, term: t.term.clone() }), field: f.number }));
            }
            let mt = inner
                .as_any()
                .downcast_ref::<MultiTermQuery>()
                .cloned()
                .ok_or_else(|| EsError::illegal_argument("[span_multi] can only wrap multi-term queries"))?;
            SpanNode::Multi(Box::new(mt))
        }
        other => return Err(EsError::illegal_argument(format!("[{}] is not a span query", other.name()))),
    }))
}

/// Queries on `wildcard` fields: the pattern against every value (doc values).
pub fn wildcard_field_query(_ctx: &QueryCtx, f: &FieldInfo, pattern: &str, case_insensitive: bool) -> EsResult<QueryRef> {
    Ok(arc(ConstantScoreQuery::new(arc(MultiTermQuery::wildcard(f.number, pattern, case_insensitive).map_err(index_err)?))))
}

pub fn wildcard_regexp(_ctx: &QueryCtx, f: &FieldInfo, r: &crate::query::term_level::Regexp) -> EsResult<QueryRef> {
    Ok(arc(ConstantScoreQuery::new(arc(
        MultiTermQuery::regexp(f.number, &r.value, crate::query::term_level::regexp_flags::to_codec(r.flags), r.case_insensitive, r.max_determinized_states as u32).map_err(index_err)?,
    ))))
}

/// `terms` with a lookup: the values at `path` of another document.
pub fn terms_lookup(ctx: &QueryCtx, l: &crate::query::term_level::TermsLookup) -> EsResult<Vec<Value>> {
    let st = ctx.st;
    let names = crate::meta::resolve::resolve(st, std::slice::from_ref(&l.index), &crate::meta::resolve::IndicesOptions::STRICT_EXPAND_OPEN, ctx.now_ms)?;
    let Some(idx) = names.first().and_then(|n| st.metadata.indices.get(n)).cloned() else {
        return Err(EsError::index_not_found(&l.index));
    };
    let engine = ctx.cluster.engine.index(ctx.cluster, st, &idx)?;
    let shard = crate::meta::routing::shard_for(&idx, &l.id, l.routing.as_deref());
    let doc = engine.shard(shard).shard.get(l.id.as_bytes(), true).map_err(index_err)?;
    let Some(doc) = doc else { return Ok(Vec::new()) };
    let Some(src) = crate::fetch::source_bytes(&doc.fields) else { return Ok(Vec::new()) };
    let v = crate::fetch::parse_source(src)?;
    let Value::Obj(m) = v else { return Ok(Vec::new()) };
    let mut out = Vec::new();
    crate::engine::source::extract_values(&m, &l.path, &mut out);
    Ok(out.into_iter().filter(|v| !v.is_null()).cloned().collect())
}

/// `match_only_text` phrase queries confirmed against `_source`.
pub fn source_confirmed(ctx: &QueryCtx, f: &FieldInfo, q: QueryRef) -> QueryRef {
    super::geo::source_confirmed(ctx, f, q)
}

struct SortLeaf(super::scripts::Leaf);

impl search_index::collector::top_field::LeafSortScript for SortLeaf {
    fn number(&mut self, doc: u32) -> search_index::Result<f64> {
        self.0.run_f64(doc, 0.0).map_err(|e| search_index::Error::es("script_exception", crate::scripting::script_error(e).reason().to_string(), 400))
    }
}

struct SortPainless(Painless);

impl search_index::collector::top_field::SortScript for SortPainless {
    fn leaf(&self, seg: &search_index::SegmentContext) -> search_index::Result<Box<dyn search_index::collector::top_field::LeafSortScript>> {
        let s = self.0.bound.painless().ok_or_else(|| search_index::Error::illegal("not painless"))?;
        let host = super::scripts::DocHost::new(self.0.table.clone(), seg.clone(), self.0.shard, self.0.index.clone());
        Ok(Box::new(SortLeaf(super::scripts::Leaf::new(s, &self.0.bound.params, host))))
    }
}

/// `_script` sorts (numbers).
pub fn script_sort(ctx: &QueryCtx, script: &crate::query::script::Script, ty: &str, reverse: bool, mode: Option<&str>, nested: Option<&crate::search_source::NestedSort>) -> EsResult<SortKey> {
    let _ = nested;
    if ty != "number" {
        return Err(qse(ctx, "[_script] sorts of type [string] are not supported yet"));
    }
    let p = painless(ctx, script, search_painless::Context::NumberSort)?;
    let mode = match mode {
        Some("max") => search_index::collector::top_field::SortMode::Max,
        Some("sum") => search_index::collector::top_field::SortMode::Sum,
        Some("avg") => search_index::collector::top_field::SortMode::Avg,
        Some("median") => search_index::collector::top_field::SortMode::Median,
        _ => search_index::collector::top_field::SortMode::Min,
    };
    Ok(SortKey::Script(search_index::collector::top_field::ScriptSort { script: Arc::new(SortPainless(p)), reverse, mode, nested: None }))
}

#[allow(dead_code)]
fn n(v: f64) -> Value {
    Value::Num(Number::Float(v))
}
