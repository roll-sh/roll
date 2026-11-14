//! Highlighting (`HighlightBuilder` + `HighlightPhase`): request options (global and per field,
//! field patterns), the highlighted query (the search query or `highlight_query`), field values
//! from `_source` (multi-fields read their parent's values) analyzed with the field's index
//! analyzer, and search-index's unified / plain / fvh highlighters.

use super::query::QueryCtx;
use crate::engine::fields::{FieldInfo, Kind};
use crate::error::{EsError, EsResult};
use crate::json::{Map, Value};
use search_index::highlight::{BoundaryScanner, Encoder, Fragmenter, HighlightOptions, HighlighterType};
use search_index::QueryRef;
use std::sync::Arc;

/// The weights [`matching_part`] builds for the leaf queries of a shard's highlighted
/// queries, shared by the hits of a page: a leaf's weight (its term statistics) doesn't depend
/// on the hit. Entries hold their query, so a query's address identifies it while cached.
#[derive(Default)]
pub struct MatchWeights(parking_lot::Mutex<std::collections::HashMap<usize, (QueryRef, LeafWeight)>>);

type LeafWeight = Arc<dyn search_index::Weight>;

impl MatchWeights {
    fn get(&self, searcher: &search_index::Searcher, q: &QueryRef) -> search_index::Result<LeafWeight> {
        let key = Arc::as_ptr(q).cast::<()>() as usize;
        if let Some((_, w)) = self.0.lock().get(&key) {
            return Ok(w.clone());
        }
        let w: LeafWeight = Arc::from(searcher.create_weight(q, search_index::ScoreMode::CompleteNoScores)?);
        self.0.lock().insert(key, (q.clone(), w.clone()));
        Ok(w)
    }
}

/// One field's highlight request.
#[derive(Clone, Debug)]
pub struct FieldSpec {
    pub pattern: String,
    pub opts: Map,
}

/// A parsed `highlight` section.
#[derive(Clone, Debug)]
pub struct Spec {
    pub global: Map,
    pub fields: Vec<FieldSpec>,
    /// Whether the unified highlighter may use weight matches for this search
    /// (`CustomUnifiedHighlighter.weightMatchesUnsupported`): not with knn, nested or join
    /// queries. Set by the caller from the search source ([`weight_matches_supported`]).
    pub weight_matches: bool,
    /// The query embeddings of `semantic_text` fields, for the `semantic` highlighter (set by
    /// the caller from the rewritten query, `xpack::inference::highlight::probes`).
    pub semantic: Vec<(String, crate::xpack::inference::highlight::Probe)>,
}

/// Whether a search's queries support Lucene's matches API, which weight matches need:
/// Elasticsearch turns them off for knn (`KnnScoreDocQuery`), nested
/// (`ESToParentBlockJoinQuery`) and join (`LateParsingQuery`) queries, also under `must_not`.
pub fn weight_matches_supported(src: &crate::search_source::SearchSource) -> bool {
    fn unsupported(v: &Value) -> bool {
        let Value::Obj(m) = v else { return false };
        let Some((k, body)) = m.iter().next() else { return false };
        match k.as_str() {
            "nested" | "has_child" | "has_parent" | "knn" | "parent_id" => true,
            "bool" => ["must", "should", "filter", "must_not"].iter().any(|c| match body.get(c) {
                Some(Value::Arr(a)) => a.iter().any(unsupported),
                Some(q) => unsupported(q),
                None => false,
            }),
            "dis_max" => matches!(body.get("queries"), Some(Value::Arr(a)) if a.iter().any(unsupported)),
            "constant_score" => body.get("filter").is_some_and(unsupported),
            "function_score" | "script_score" => body.get("query").is_some_and(unsupported),
            "boosting" => body.get("positive").is_some_and(unsupported) || body.get("negative").is_some_and(unsupported),
            _ => false,
        }
    }
    if !src.knn.is_empty() || src.retriever.is_some() {
        return false;
    }
    !src.query.as_ref().is_some_and(|q| unsupported(&q.to_value()))
}

const OPTIONS: &[&str] = &[
    "pre_tags", "post_tags", "tags_schema", "fragment_size", "number_of_fragments", "encoder", "require_field_match", "boundary_max_scan",
    "boundary_chars", "boundary_scanner", "boundary_scanner_locale", "type", "fragmenter", "no_match_size", "force_source", "highlight_filter",
    "phrase_limit", "max_analyzed_offset", "order", "highlight_query", "matched_fields", "options", "fragment_offset", "fields",
];

impl Spec {
    pub fn parse(m: &Map) -> EsResult<Spec> {
        let mut global = Map::new();
        let mut fields = Vec::new();
        for (k, v) in m {
            // OpenSearch names the offset limit `max_analyzer_offset` (its own option, with
            // its own rules) and doesn't know Elasticsearch's `max_analyzed_offset`.
            if crate::opensearch::active() && (k == "max_analyzer_offset" || k == "max_analyzed_offset") {
                if k == "max_analyzed_offset" {
                    return Err(EsError::parse("[highlight] unknown field [max_analyzed_offset] did you mean [max_analyzer_offset]?"));
                }
            } else if !OPTIONS.contains(&k.as_str()) {
                return Err(EsError::parse(format!("[highlight] unknown field [{k}]")));
            }
            if k == "fields" {
                match v {
                    Value::Obj(fm) => {
                        for (name, o) in fm {
                            fields.push(FieldSpec { pattern: name.clone(), opts: o.as_object().cloned().unwrap_or_default() });
                        }
                    }
                    Value::Arr(a) => {
                        for item in a {
                            let Value::Obj(one) = item else { return Err(EsError::parse("[highlight] fields array elements must be objects")) };
                            if one.len() != 1 {
                                return Err(EsError::parse("[highlight] failed to parse field [fields]").caused_by(EsError::parse("[fields] can be a single object with any number of fields or an array where each entry is an object with a single field")));
                            }
                            let (name, o) = one.iter().next().unwrap();
                            fields.push(FieldSpec { pattern: name.clone(), opts: o.as_object().cloned().unwrap_or_default() });
                        }
                    }
                    _ => return Err(EsError::parse("[highlight] fields must be an object")),
                }
            } else {
                global.insert(k.clone(), v.clone());
            }
        }
        Ok(Spec { global, fields, weight_matches: true, semantic: Vec::new() })
    }
}

fn get<'a>(field: &'a Map, global: &'a Map, k: &str) -> Option<&'a Value> {
    field.get(k).or_else(|| global.get(k))
}

fn strings(v: &Value) -> Vec<String> {
    match v {
        Value::Arr(a) => a.iter().map(|x| x.to_es_string()).collect(),
        other => vec![other.to_es_string()],
    }
}

fn usize_of(v: Option<&Value>, d: usize) -> usize {
    v.and_then(|x| x.as_i64().or_else(|| x.as_str().and_then(|s| s.parse().ok()))).map(|n| n.max(0) as usize).unwrap_or(d)
}

/// Options for one field.
fn options(f: &FieldInfo, field: &Map, global: &Map, index: &crate::meta::IndexMeta) -> EsResult<HighlightOptions> {
    let mut o = HighlightOptions::default();
    let tv_offsets = f.mapper.raw("term_vector").and_then(Value::as_str).is_some_and(|t| t.contains("offsets"));
    o.kind = match get(field, global, "type").and_then(Value::as_str) {
        Some("plain") => HighlighterType::Plain,
        Some("fvh") => HighlighterType::Fvh,
        Some("unified") | None => HighlighterType::Unified,
        Some(other) => return Err(EsError::illegal_argument(format!("unknown highlighter type [{other}] for the field [{}]", f.name))),
    };
    if o.kind == HighlighterType::Fvh && !tv_offsets {
        return Err(EsError::illegal_argument(format!(
            "the field [{}] should be indexed with term vector with position offsets to be used with fast vector highlighter",
            f.name
        )));
    }
    o.term_vectors_with_offsets = tv_offsets;
    o.offsets_indexed = tv_offsets || f.index_options == search_codec::IndexOptions::DocsAndFreqsAndPositionsAndOffsets;
    if get(field, global, "tags_schema").and_then(Value::as_str) == Some("styled") {
        let (pre, post) = HighlightOptions::styled_tags();
        o.pre_tags = pre;
        o.post_tags = post;
    }
    if let Some(v) = get(field, global, "pre_tags") {
        o.pre_tags = strings(v);
    }
    if let Some(v) = get(field, global, "post_tags") {
        o.post_tags = strings(v);
    }
    o.fragment_size = usize_of(get(field, global, "fragment_size"), 100);
    o.number_of_fragments = usize_of(get(field, global, "number_of_fragments"), 5);
    o.no_match_size = usize_of(get(field, global, "no_match_size"), 0);
    o.encoder = match get(field, global, "encoder").and_then(Value::as_str) {
        Some("html") => Encoder::Html,
        _ => Encoder::Default,
    };
    o.order_by_score = get(field, global, "order").and_then(Value::as_str) == Some("score");
    o.boundary_scanner = match get(field, global, "boundary_scanner").and_then(Value::as_str) {
        Some("chars") => Some(BoundaryScanner::Chars),
        Some("word") => Some(BoundaryScanner::Word),
        Some("sentence") => Some(BoundaryScanner::Sentence),
        Some(other) => return Err(EsError::illegal_argument(format!("Invalid boundary scanner type [{other}]"))),
        None => None,
    };
    if let Some(Value::Str(c)) = get(field, global, "boundary_chars") {
        o.boundary_chars = c.clone();
    }
    o.boundary_max_scan = usize_of(get(field, global, "boundary_max_scan"), 20);
    o.fragmenter = match get(field, global, "fragmenter").and_then(Value::as_str) {
        Some("simple") => Fragmenter::Simple,
        Some("span") | None => Fragmenter::Span,
        Some(other) => return Err(EsError::illegal_argument(format!("unknown fragmenter option [{other}] for the field [{}]", f.name))),
    };
    if let Some(v) = get(field, global, "fragment_offset") {
        o.fragment_offset = Some(usize_of(Some(v), 0));
    }
    o.phrase_limit = usize_of(get(field, global, "phrase_limit"), 256);
    o.require_field_match = get(field, global, "require_field_match").and_then(Value::as_bool).unwrap_or(true);
    o.index_max_analyzed_offset = index.settings.get("index.highlight.max_analyzed_offset").and_then(|v| v.parse().ok()).unwrap_or(1_000_000);
    // `-1` stands for the index limit, applied by truncating instead of failing.
    o.max_analyzed_offset = get(field, global, "max_analyzed_offset")
        .and_then(|v| match v {
            Value::Str(s) => crate::units::parse_java_double(s.trim()).map(|d| d as i64),
            other => other.as_f64().map(|d| d as i64),
        })
        .and_then(|v| match v {
            -1 => Some(o.index_max_analyzed_offset),
            v if v > 0 => Some(v as usize),
            _ => None,
        });
    if crate::opensearch::active() {
        o.opensearch_offsets = true;
        o.max_analyzed_offset = match get(field, global, "max_analyzer_offset") {
            None => None,
            Some(v) => {
                let n = match v {
                    Value::Str(s) => crate::units::parse_java_int(s.trim()).map(i64::from),
                    other => other.as_f64().map(|d| d as i64),
                };
                match n {
                    Some(n) if n > 0 => Some(n as usize),
                    Some(n) => return Err(EsError::illegal_argument(format!("the value [{n}] of max_analyzer_offset is invalid"))),
                    None => return Err(EsError::number_format(&v.to_es_string())),
                }
            }
        };
    }
    o.tokenized = f.kind.is_text();
    // The unified highlighter reads offsets from postings or term vectors when the field has
    // them: nothing is re-analyzed, so the index limit doesn't apply.
    let offsets = f.index_options == search_codec::IndexOptions::DocsAndFreqsAndPositionsAndOffsets
        || f.mapper.raw("term_vector").and_then(Value::as_str).is_some_and(|t| t.contains("offsets"));
    if offsets && o.kind == search_index::highlight::HighlighterType::Unified {
        o.index_max_analyzed_offset = usize::MAX;
    }
    o.field_name = f.name.clone();
    o.index_name = index.name.clone();
    Ok(o)
}

fn highlightable(f: &FieldInfo) -> bool {
    f.kind.is_text() || matches!(f.kind, Kind::Keyword | Kind::ConstantKeyword | Kind::Wildcard | Kind::Version)
}

/// Highlights one hit.
///
/// Values come from the field's stored values when it is `store: true` and `doc` (the global
/// doc) is given (`HighlightUtils.loadFieldValues`), from `source` otherwise. `stored` is the
/// doc's stored fields when the caller has read them already; otherwise they are read only if
/// a highlighted field is stored. `weights` keeps the leaf weights of weight matches across
/// the hits of one searcher.
#[allow(clippy::too_many_arguments)]
pub fn highlight_hit(
    qctx: &QueryCtx,
    spec: &Spec,
    query: &QueryRef,
    source: Option<&Value>,
    doc_id: &str,
    searcher: &search_index::Searcher,
    doc: Option<u32>,
    stored: Option<&[(u32, search_codec::stored::OwnedStoredValue)]>,
    weights: Option<&MatchWeights>,
) -> EsResult<Map> {
    let mut out = Map::new();
    let empty = Map::new();
    let mut loaded = None;
    let src = match source {
        Some(Value::Obj(src)) => src,
        _ if doc.is_some() => &empty,
        _ => return Ok(out),
    };
    for fs in &spec.fields {
        let names: Vec<String> = if fs.pattern.contains('*') { qctx.table.matching(&fs.pattern) } else { vec![fs.pattern.clone()] };
        for name in names {
            // `semantic_text`: the `semantic` highlighter (its default).
            if let Some(crate::mapping::MapperRef::Field(sf)) = qctx.idx.mapping.lookup(&name)
                && sf.ty == "semantic_text"
            {
                let ty = get(&fs.opts, &spec.global, "type").and_then(Value::as_str);
                if ty.is_none_or(|t| t == "semantic")
                    && let Some(settings) = crate::xpack::inference::semantic::model_settings(sf)
                {
                    let probe = spec.semantic.iter().find(|(f, _)| *f == name).map(|(_, p)| p);
                    let fragments = usize_of(get(&fs.opts, &spec.global, "number_of_fragments"), 5);
                    let by_score = get(&fs.opts, &spec.global, "order").and_then(Value::as_str) == Some("score");
                    let similarity = settings.similarity.as_deref().unwrap_or("dot_product");
                    if let Some(snippets) = crate::xpack::inference::highlight::highlight(src, &name, probe, similarity, fragments, by_score, &|f| {
                        qctx.table.source_filter.as_ref().is_none_or(|fls| fls.searchable(f))
                    }) {
                        out.insert(name.clone(), Value::Arr(snippets.into_iter().map(Value::from).collect()));
                    }
                }
                continue;
            }
            let Some(f) = qctx.field(&name) else { continue };
            if !highlightable(&f) || (fs.pattern.contains('*') && f.name.rsplit('.').next().is_some_and(|l| l.starts_with('_'))) {
                continue;
            }
            // The `semantic` highlighter leaves other fields alone.
            if get(&fs.opts, &spec.global, "type").and_then(Value::as_str) == Some("semantic") {
                continue;
            }
            let mut o = options(&f, &fs.opts, &spec.global, qctx.idx)?;
            o.weight_matches = spec.weight_matches && o.require_field_match && qctx.idx.settings.get("index.highlight.weight_matches_mode.enabled") != Some("false");
            // The unified highlighter names the Lucene doc in its errors, the others the `_id`.
            o.doc_id = match (o.kind, doc.and_then(|d| searcher.segment_of(d))) {
                (search_index::highlight::HighlighterType::Unified, Some((_, local))) => local.to_string(),
                _ => doc_id.to_string(),
            };
            if let Some(Value::Arr(mf)) = get(&fs.opts, &spec.global, "matched_fields") {
                let mfs: Vec<_> = mf.iter().filter_map(|v| v.as_str()).filter_map(|n| qctx.field(n)).collect();
                o.matched_fields = mfs.iter().map(|x| x.number).collect();
                o.matched_analyzers = mfs.iter().map(|x| x.analyzer.clone()).collect();
            }
            let q: QueryRef = match get(&fs.opts, &spec.global, "highlight_query") {
                Some(hq) => {
                    let (parsed, _) = crate::query::parse_query(hq, None, &[])?;
                    qctx.to_query(&parsed)?
                }
                None => query.clone(),
            };
            let q = match doc.and_then(|d| searcher.segment_of(d)) {
                Some((seg, local)) if o.weight_matches && o.kind == search_index::highlight::HighlighterType::Unified => {
                    // A `highlight_query` that misses the hit highlights nothing in it.
                    let local_weights;
                    let weights = match weights {
                        Some(w) => w,
                        None => {
                            local_weights = MatchWeights::default();
                            &local_weights
                        }
                    };
                    matching_part(searcher, weights, &q, seg, local)
                        .map_err(crate::engine::convert::index_error)?
                        .unwrap_or_else(|| Arc::new(search_index::query::MatchNoneQuery::default()))
                }
                _ => q,
            };
            if let Some(d) = doc
                && f.store
                && stored.is_none()
                && loaded.is_none()
            {
                loaded = Some(searcher.stored(d).map_err(crate::engine::convert::index_error)?);
            }
            // An index-time script field's values come from its script (`FieldValues.valueFetcher`).
            let scripted = index_time_values(qctx, &f.name, searcher, doc)?;
            let values: Vec<String> = match stored.or(loaded.as_deref()) {
                _ if scripted.is_some() => scripted.unwrap_or_default(),
                Some(st) if f.store && doc.is_some() => st
                    .iter()
                    .filter(|(n, _)| *n == f.number)
                    .filter_map(|(_, v)| match v {
                        search_codec::stored::OwnedStoredValue::Str(s) => Some(s.clone()),
                        _ => None,
                    })
                    .collect(),
                _ => {
                    let path = f.parent.as_deref().unwrap_or(&f.name);
                    let mut raw = Vec::new();
                    crate::engine::source::extract_values(src, path, &mut raw);
                    let values: Vec<String> = raw.into_iter().filter(|v| !v.is_null()).map(super::query::value_text).collect();
                    // `SourceSimpleFragmentsBuilder`: term vector offsets point into the values
                    // in indexing order, which synthetic `_source` doesn't keep.
                    if o.kind == search_index::highlight::HighlighterType::Fvh
                        && values.len() > 1
                        && crate::engine::synthetic::source_mode(qctx.idx) == crate::engine::synthetic::SourceMode::Synthetic
                    {
                        return Err(EsError::illegal_argument(format!(
                            "The fast vector highlighter doesn't support loading multi-valued fields from _source in index [{}] because _source can reorder field values",
                            qctx.idx.name
                        )));
                    }
                    values
                }
            };
            // Values past `ignore_above` were never indexed: nothing to highlight in them.
            let values: Vec<String> = match f.ignore_above {
                Some(n) => values.into_iter().filter(|v| !crate::document::exceeds_ignore_above(v, n)).collect(),
                None => values,
            };
            if values.is_empty() {
                continue;
            }
            let refs: Vec<&str> = values.iter().map(String::as_str).collect();
            let analyzer = f.analyzer.clone().or_else(|| qctx.table.analysis.analyzer("keyword"));
            let frags = search_index::highlight::highlight_in(searcher, &q, &refs, analyzer.as_ref(), f.number, &o).map_err(crate::engine::convert::index_error)?;
            if !frags.is_empty() {
                out.insert(name.clone(), Value::Arr(frags.into_iter().map(Value::Str).collect()));
            }
        }
    }
    let _ = Arc::<()>::new(());
    Ok(out)
}

/// The values of index-time script field `name` for global doc `doc` (`None`: not one).
fn index_time_values(qctx: &QueryCtx, name: &str, searcher: &search_index::Searcher, doc: Option<u32>) -> EsResult<Option<Vec<String>>> {
    let scripts = super::index_time::of(&qctx.idx.mapping);
    let (Some(f), Some((seg, local))) = (scripts.get(name), doc.and_then(|d| searcher.segment_of(d))) else { return Ok(None) };
    let ev = super::runtime::Evaluator::new(&qctx.cluster.scripts, qctx.st, qctx.table, qctx.shard as i32, &qctx.idx.name, f.clone())?.counted();
    let vals = super::index_time::fetch_values(&ev, seg, local)?;
    Ok(Some(vals.into_iter().filter_map(|v| if let super::runtime::RtValue::Str(s) = v { Some(s) } else { None }).collect()))
}

/// The part of `q` that matches the doc, as Lucene's `Weight.matches` sees it in the unified
/// highlighter's weight matches mode: a `bool` whose required clause misses the doc (or with
/// too few matching `should`s) contributes nothing, nor do `should` / `dis_max` clauses that
/// miss it, nor `must_not` clauses; the rest keep their structure. `None` when nothing of `q`
/// matches. Only terms of the remaining clauses are highlighted.
fn matching_part(searcher: &search_index::Searcher, weights: &MatchWeights, q: &QueryRef, seg: &search_index::SegmentContext, local: u32) -> search_index::Result<Option<QueryRef>> {
    use search_index::query::{BooleanQuery, BoostQuery, ConstantScoreQuery, DisMaxQuery, FunctionScoreQuery, Occur};
    let any = q.as_any();
    if let Some(b) = any.downcast_ref::<BooleanQuery>() {
        let mut kept = BooleanQuery::new();
        kept.minimum_should_match = b.minimum_should_match;
        let (mut shoulds, mut matched_shoulds, mut required) = (0usize, 0usize, false);
        for c in &b.clauses {
            match c.occur {
                Occur::MustNot => {}
                Occur::Must | Occur::Filter => {
                    required = true;
                    match matching_part(searcher, weights, &c.query, seg, local)? {
                        Some(p) => kept = kept.add(c.occur, p),
                        None => return Ok(None),
                    }
                }
                Occur::Should => {
                    shoulds += 1;
                    if let Some(p) = matching_part(searcher, weights, &c.query, seg, local)? {
                        matched_shoulds += 1;
                        kept = kept.add(Occur::Should, p);
                    }
                }
            }
        }
        if matched_shoulds < b.minimum_should_match || (!required && shoulds > 0 && matched_shoulds == 0) {
            return Ok(None);
        }
        return Ok(Some(Arc::new(kept)));
    }
    if let Some(d) = any.downcast_ref::<DisMaxQuery>() {
        let mut kept = Vec::new();
        for q in &d.disjuncts {
            if let Some(p) = matching_part(searcher, weights, q, seg, local)? {
                kept.push(p);
            }
        }
        return Ok((!kept.is_empty()).then(|| Arc::new(DisMaxQuery::new(kept, d.tie_breaker)) as QueryRef));
    }
    if let Some(c) = any.downcast_ref::<ConstantScoreQuery>() {
        return Ok(matching_part(searcher, weights, &c.query, seg, local)?.map(|p| Arc::new(ConstantScoreQuery::new(p)) as QueryRef));
    }
    if let Some(b) = any.downcast_ref::<BoostQuery>() {
        return Ok(matching_part(searcher, weights, &b.query, seg, local)?.map(|p| Arc::new(BoostQuery { query: p, boost: b.boost }) as QueryRef));
    }
    if let Some(f) = any.downcast_ref::<FunctionScoreQuery>() {
        // Its functions don't take part in matches; `min_score` filtering is the search's.
        return matching_part(searcher, weights, &f.query, seg, local);
    }
    let w = weights.get(searcher, q)?;
    if !search_index::query::basic::matches_doc(&*w, seg, local)? {
        return Ok(None);
    }
    // A phrase prefix matches as the phrase of its expansions (one span per match).
    if any.is::<search_index::query::MultiPhrasePrefixQuery>() {
        return Ok(Some(searcher.rewrite(q)?));
    }
    Ok(Some(q.clone()))
}
