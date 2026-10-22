//! Parent/child (`join` field) queries and their inner hits.
//!
//! - `has_child` (`HasChildQueryBuilder`): the child type's docs matching the inner query give
//!   their parent ids (`<join>#<parent>`); parents of that type with a collected id match,
//!   scored by the `score_mode` aggregate of their children's scores, within
//!   `min_children..=max_children`.
//! - `has_parent`: the other way round, children of the parent type's matching docs; scored
//!   by the parent's score with `score: true`, else constant.
//! - `parent_id`: the children of one parent id (`<join>#<parent> = id` plus the type filter).
//!
//! Both joins run as `search_index::query::JoinQuery` (global ordinals, Lucene's
//! `JoinUtil`). Inner hits (`ParentChildInnerHitContextBuilder`) are, per top hit, the
//! children of the hit (`has_child`) or its parent (`has_parent`) that match the inner query,
//! rendered like top-level hits.

use super::query::{InnerHitsDef, QueryCtx, arc, match_none, qse};
use super::{ShardResult, Target};
use crate::error::{EsError, EsResult};
use crate::join::Joiner;
use crate::json::{Map, Number, Value};
use crate::obj;
use crate::query::joining::{HasChild, HasParent, InnerHits, JoinScoreMode, ParentId};
use search_index::query::{BooleanQuery, ConstantScoreQuery, JoinQuery, Occur, TermQuery};
use search_index::{QueryRef, ScoreMode, Searcher};

/// The inner hits side of a join query.
#[derive(Clone, Debug)]
pub struct JoinInner {
    /// The relation the inner hits are of (the child type, or the parent type).
    pub type_name: String,
    /// `has_child`: the hit's children; `has_parent`: the hit's parent.
    pub children: bool,
}

fn expensive_check(ctx: &QueryCtx) -> EsResult<()> {
    if !ctx.allow_expensive {
        return Err(EsError::new(400, "exception", "[joining] queries cannot be executed when 'search.allow_expensive_queries' is set to false."));
    }
    Ok(())
}

/// The field number of a relation field (`join`, or `join#<parent>`).
fn field_number(ctx: &QueryCtx, joiner: &Joiner, sub: Option<&str>) -> Option<u32> {
    let f = ctx.table.get(&joiner.field)?;
    match sub {
        None => Some(f.number),
        Some(p) => f.subfields.iter().find(|(name, _)| name == p).map(|(_, n)| *n),
    }
}

fn type_filter(join: u32, ty: &str) -> QueryRef {
    arc(TermQuery::new(join, ty.as_bytes().to_vec()))
}

fn engine_mode(m: JoinScoreMode) -> search_index::query::JoinScoreMode {
    match m {
        JoinScoreMode::None => search_index::query::JoinScoreMode::None,
        JoinScoreMode::Avg => search_index::query::JoinScoreMode::Avg,
        JoinScoreMode::Max => search_index::query::JoinScoreMode::Max,
        JoinScoreMode::Min => search_index::query::JoinScoreMode::Min,
        JoinScoreMode::Total => search_index::query::JoinScoreMode::Total,
    }
}

fn register_inner(ctx: &QueryCtx, ih: Option<&InnerHits>, default_name: &str, query: QueryRef, join: JoinInner) {
    if let Some(ih) = ih {
        let name = ih.name.clone().unwrap_or_else(|| default_name.to_string());
        ctx.inner_hits.borrow_mut().push(InnerHitsDef { name, spec: ih.clone(), nested_path: None, query, join: Some(join) });
    }
}

/// Boolean rewrites identify clauses by their Lucene string, and a constant-scoring join's
/// (`GlobalOrdinalsQuery{joinField=…}`) leaves out its from query, so two different joins on
/// one field would merge into one. From the second such join of a request on, the join gets
/// an unreachable children limit: it scores and matches the same, but its string (with
/// `fromQuery`) tells it apart.
fn distinct(ctx: &QueryCtx, mut q: JoinQuery) -> JoinQuery {
    if q.score_mode == search_index::query::JoinScoreMode::None && q.min <= 1 && q.max == i32::MAX as u32 {
        let n = ctx.constant_joins.get();
        ctx.constant_joins.set(n + 1);
        if n > 0 {
            q.max = i32::MAX as u32 - 1;
        }
    }
    q
}

/// `HasChildQueryBuilder.doToQuery`.
pub fn has_child(ctx: &QueryCtx, q: &HasChild) -> EsResult<QueryRef> {
    expensive_check(ctx)?;
    let Some(joiner) = Joiner::of(&ctx.idx.mapping) else {
        if q.ignore_unmapped {
            return Ok(match_none("[has_child] no join field"));
        }
        return Err(qse(ctx, "[has_child] no join field has been configured"));
    };
    let Some(parent) = joiner.parent_of(&q.ty).map(str::to_string) else {
        if q.ignore_unmapped {
            return Ok(match_none("[has_child] unknown child type"));
        }
        return Err(qse(ctx, format!("[has_child] join field [{}] doesn't hold [{}] as a child", joiner.field, q.ty)));
    };
    let (Some(join), Some(jf)) = (field_number(ctx, &joiner, None), field_number(ctx, &joiner, Some(&parent))) else {
        if q.ignore_unmapped {
            return Ok(match_none("[has_child] unmapped parent join field"));
        }
        return Err(qse(ctx, format!("[has_child] no parent join field [{}#{parent}] configured", joiner.field)));
    };
    let inner = ctx.to_query(&q.query)?;
    let from = arc(BooleanQuery::new().add(Occur::Must, inner.clone()).add(Occur::Filter, type_filter(join, &q.ty)));
    register_inner(ctx, q.inner_hits.as_ref(), &q.ty, inner, JoinInner { type_name: q.ty.clone(), children: true });
    let mut jq = JoinQuery::new(jf, from, type_filter(join, &parent), engine_mode(q.score_mode));
    jq.min = q.min_children.max(0) as u32;
    jq.max = q.max_children.max(0) as u32;
    Ok(arc(distinct(ctx, jq)))
}

/// `HasParentQueryBuilder.doToQuery`.
pub fn has_parent(ctx: &QueryCtx, q: &HasParent) -> EsResult<QueryRef> {
    expensive_check(ctx)?;
    let Some(joiner) = Joiner::of(&ctx.idx.mapping) else {
        if q.ignore_unmapped {
            return Ok(match_none("[has_parent] no join field"));
        }
        return Err(qse(ctx, "[has_parent] no join field has been configured"));
    };
    if !joiner.is_parent(&q.parent_type) {
        if q.ignore_unmapped {
            return Ok(match_none("[has_parent] unknown parent type"));
        }
        return Err(qse(ctx, format!("[has_parent] join field [{}] doesn't hold [{}] as a parent", joiner.field, q.parent_type)));
    }
    let (Some(join), Some(jf)) = (field_number(ctx, &joiner, None), field_number(ctx, &joiner, Some(&q.parent_type))) else {
        return Ok(match_none("[has_parent] unmapped join field"));
    };
    let inner = ctx.to_query(&q.query)?;
    let from = arc(BooleanQuery::new().add(Occur::Must, inner.clone()).add(Occur::Filter, type_filter(join, &q.parent_type)));
    let mut children = BooleanQuery::new();
    for c in joiner.children(&q.parent_type) {
        children = children.add(Occur::Should, type_filter(join, c));
    }
    let to: QueryRef = arc(ConstantScoreQuery::new(arc(children)));
    register_inner(ctx, q.inner_hits.as_ref(), &q.parent_type, inner, JoinInner { type_name: q.parent_type.clone(), children: false });
    let mode = if q.score { search_index::query::JoinScoreMode::Max } else { search_index::query::JoinScoreMode::None };
    Ok(arc(distinct(ctx, JoinQuery::new(jf, from, to, mode))))
}

/// `ParentIdQueryBuilder.doToQuery`.
pub fn parent_id(ctx: &QueryCtx, q: &ParentId) -> EsResult<QueryRef> {
    expensive_check(ctx)?;
    let Some(joiner) = Joiner::of(&ctx.idx.mapping) else {
        if q.ignore_unmapped {
            return Ok(match_none("[parent_id] no join field"));
        }
        return Err(qse(ctx, format!("[parent_id] no join field found for index [{}]", ctx.idx.name)));
    };
    let Some(parent) = joiner.parent_of(&q.ty).map(str::to_string) else {
        if q.ignore_unmapped {
            return Ok(match_none("[parent_id] unknown child type"));
        }
        return Err(qse(ctx, format!("[parent_id] no relation found for child [{}]", q.ty)));
    };
    let (Some(join), Some(jf)) = (field_number(ctx, &joiner, None), field_number(ctx, &joiner, Some(&parent))) else {
        return Ok(match_none("[parent_id] unmapped join field"));
    };
    Ok(arc(BooleanQuery::new()
        .add(Occur::Must, arc(TermQuery::new(jf, q.id.as_bytes().to_vec())))
        .add(Occur::Filter, type_filter(join, &q.ty))))
}

/// The sorted doc value of `field` for a segment doc.
fn sorted_value(seg: &search_index::SegmentContext, field: u32, local: u32) -> Option<String> {
    let dv = match seg.reader.doc_values(field)? {
        search_codec::DocValuesReader::Sorted(v) => v,
        _ => return None,
    };
    let ord = dv.ord(local)?;
    let mut out = Vec::new();
    dv.lookup_ord(ord, &mut out);
    Some(String::from_utf8_lossy(&out).into_owned())
}

/// The inner hits of a join query for top hit `doc` (`JoinFieldInnerHitSubContext.topDocs`).
pub fn inner_hits(qctx: &QueryCtx, t: &Target, r: &ShardResult, doc: u32, def: &InnerHitsDef, join: &JoinInner) -> EsResult<Value> {
    let ie = crate::engine::convert::index_error;
    let empty = || obj! { "total" => obj! { "value" => 0, "relation" => "eq" }, "max_score" => Value::Null, "hits" => Value::Arr(Vec::new()) };
    let searcher: &Searcher = &r.searcher;
    let Some(joiner) = Joiner::of(&t.idx.mapping) else { return Ok(empty()) };
    let Some(join_field) = field_number(qctx, &joiner, None) else { return Ok(empty()) };
    let Some((seg, local)) = searcher.segment_of(doc) else { return Ok(empty()) };
    // Hits outside the relation (no join name) have none.
    if seg.reader.sorted_set(join_field).is_none_or(|dv| dv.ords(local).next().is_none()) {
        return Ok(empty());
    }
    let filter: QueryRef = if join.children {
        let Some(pf) = joiner.parent_of(&join.type_name).and_then(|p| field_number(qctx, &joiner, Some(p))) else {
            return Ok(empty());
        };
        let stored = searcher.stored(doc).map_err(ie)?;
        let id = super::hits::stored_id(&stored).unwrap_or_default();
        arc(BooleanQuery::new()
            .add(Occur::Filter, arc(TermQuery::new(pf, id.into_bytes())))
            .add(Occur::Filter, type_filter(join_field, &join.type_name)))
    } else {
        let Some(cf) = field_number(qctx, &joiner, Some(&join.type_name)) else { return Ok(empty()) };
        let Some(parent_id) = sorted_value(seg, cf, local) else { return Ok(empty()) };
        super::query::ids_query(&[parent_id])
    };
    let q: QueryRef = arc(BooleanQuery::new().add(Occur::Must, def.query.clone()).add(Occur::Filter, filter));
    let spec = &def.spec;
    let sorted = spec.sort.is_some();
    let mut matches: Vec<(u32, f32)> = Vec::new();
    {
        let w = searcher.create_weight(&q, ScoreMode::Complete).map_err(ie)?;
        for seg in searcher.segments() {
            if let Some(mut s) = w.scorer(seg).map_err(ie)? {
                let mut d = s.next();
                while d != search_codec::NO_MORE_DOCS {
                    if seg.is_live(d) {
                        matches.push((seg.base + d, search_index::Scorable::score(&mut *s)));
                    }
                    d = s.next();
                }
            }
        }
    }
    let total = matches.len();
    if let Some(sort) = &spec.sort {
        let keys = super::inner::sort_keys(sort);
        let key = |d: u32, field: &str| -> Option<f64> {
            let (seg, local) = searcher.segment_of(d)?;
            super::inner::first_value(qctx.table, seg, local, field)
        };
        matches.sort_by(|a, b| {
            for (field, desc) in &keys {
                let o = if field == "_score" {
                    a.1.total_cmp(&b.1)
                } else {
                    key(a.0, field).partial_cmp(&key(b.0, field)).unwrap_or(std::cmp::Ordering::Equal)
                };
                let o = if *desc { o.reverse() } else { o };
                if o != std::cmp::Ordering::Equal {
                    return o;
                }
            }
            a.0.cmp(&b.0)
        });
    } else {
        matches.sort_by(|a, b| b.1.total_cmp(&a.1).then(a.0.cmp(&b.0)));
    }
    let from = spec.from.unwrap_or(0).max(0) as usize;
    let size = spec.size.unwrap_or(3).max(0) as usize;
    // No hits asked for: only the total is counted (`max_score` NaN).
    let max_score = if (sorted && !spec.track_scores) || size == 0 { None } else { matches.iter().map(|m| m.1).reduce(f32::max) };
    let opts = super::hits::FetchOpts {
        source: spec.source.as_ref().map(crate::fetch::FetchSource::from_value).transpose()?,
        stored_fields: spec.stored_fields.as_ref().map(|v| match v {
            Value::Str(s) if s == "_none_" => None,
            Value::Str(s) => Some(vec![s.clone()]),
            Value::Arr(a) => Some(a.iter().map(Value::to_es_string).collect()),
            _ => Some(Vec::new()),
        }),
        docvalue_fields: spec.docvalue_fields.as_ref().map(super::inner::field_specs).unwrap_or_default(),
        fields: spec.fields.as_ref().map(super::inner::field_specs).unwrap_or_default(),
        version: spec.version,
        seq_no_primary_term: spec.seq_no_primary_term,
        force_synthetic: false,
    };
    let highlight = match &spec.highlight {
        Some(Value::Obj(hl)) => Some(super::highlight::Spec::parse(hl)?),
        _ => None,
    };
    let mut hits = Vec::new();
    for (d, score) in matches.iter().skip(from).take(size) {
        let stored = searcher.stored(*d).map_err(ie)?;
        let id = super::hits::stored_id(&stored).unwrap_or_default();
        let mut h = Map::new();
        if spec.explain {
            h.insert("_shard".into(), Value::Str(format!("[{}][{}]", t.idx.name, t.shard.id)));
            h.insert("_node".into(), Value::Str(qctx.cluster.node.id.clone()));
        }
        h.insert("_index".into(), Value::Str(t.idx.name.clone()));
        h.insert("_id".into(), Value::Str(id.clone()));
        let shown = if sorted && !spec.track_scores { None } else { Some(*score) };
        super::hits::render_doc(qctx.table, Some(&t.idx), searcher, *d, &opts, &mut h, shown, qctx.now_ms)?;
        if let Some(hl) = &highlight {
            let source = crate::fetch::source_bytes(&stored).map(crate::fetch::parse_source).transpose()?;
            let out = super::highlight::highlight_hit(qctx, hl, &def.query, source.as_ref(), &id, searcher, Some(*d))?;
            if !out.is_empty() {
                h.insert("highlight".into(), Value::Obj(out));
            }
        }
        if spec.explain {
            // The inner query's explanation (the relation filter doesn't score).
            let e = searcher.explain(&def.query, *d).map_err(ie)?;
            h.insert("_explanation".into(), super::explanation_json(&e));
        }
        if let Some(sort) = &spec.sort {
            let vals: Vec<Value> = super::inner::sort_keys(sort)
                .iter()
                .map(|(f, _)| {
                    if f == "_score" {
                        return Value::Num(Number::Float(crate::fetch::java_float_as_double(*score)));
                    }
                    match searcher.segment_of(*d) {
                        Some((seg, local)) => super::inner::first_json(qctx.table, seg, local, f),
                        None => Value::Null,
                    }
                })
                .collect();
            h.insert("sort".into(), Value::Arr(vals));
        }
        hits.push(Value::Obj(h));
    }
    Ok(obj! {
        "total" => obj! { "value" => total as i64, "relation" => "eq" },
        "max_score" => match max_score { Some(s) => Value::Num(Number::Float(crate::fetch::java_float_as_double(s))), None => Value::Null },
        "hits" => Value::Arr(hits),
    })
}
