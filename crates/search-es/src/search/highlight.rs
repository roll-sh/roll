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
            if !OPTIONS.contains(&k.as_str()) {
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
        Ok(Spec { global, fields })
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
/// doc) is given (`HighlightUtils.loadFieldValues`), from `source` otherwise.
#[allow(clippy::too_many_arguments)]
pub fn highlight_hit(qctx: &QueryCtx, spec: &Spec, query: &QueryRef, source: Option<&Value>, doc_id: &str, searcher: &search_index::Searcher, doc: Option<u32>) -> EsResult<Map> {
    let mut out = Map::new();
    let empty = Map::new();
    let stored = match doc {
        Some(d) => Some(searcher.stored(d).map_err(crate::engine::convert::index_error)?),
        None => None,
    };
    let src = match source {
        Some(Value::Obj(src)) => src,
        _ if stored.is_some() => &empty,
        _ => return Ok(out),
    };
    for fs in &spec.fields {
        let names: Vec<String> = if fs.pattern.contains('*') { qctx.table.matching(&fs.pattern) } else { vec![fs.pattern.clone()] };
        for name in names {
            let Some(f) = qctx.field(&name) else { continue };
            if !highlightable(&f) || (fs.pattern.contains('*') && f.name.rsplit('.').next().is_some_and(|l| l.starts_with('_'))) {
                continue;
            }
            let mut o = options(&f, &fs.opts, &spec.global, qctx.idx)?;
            // The unified highlighter names the Lucene doc in its errors, the others the `_id`.
            o.doc_id = match (o.kind, doc.and_then(|d| searcher.segment_of(d))) {
                (search_index::highlight::HighlighterType::Unified, Some((_, local))) => local.to_string(),
                _ => doc_id.to_string(),
            };
            if let Some(Value::Arr(mf)) = get(&fs.opts, &spec.global, "matched_fields") {
                o.matched_fields = mf.iter().filter_map(|v| v.as_str()).filter_map(|n| qctx.field(n)).map(|x| x.number).collect();
            }
            let q: QueryRef = match get(&fs.opts, &spec.global, "highlight_query") {
                Some(hq) => {
                    let (parsed, _) = crate::query::parse_query(hq, None, &[])?;
                    qctx.to_query(&parsed)?
                }
                None => query.clone(),
            };
            let values: Vec<String> = match &stored {
                Some(st) if f.store => st
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
                    raw.into_iter().filter(|v| !v.is_null()).map(super::query::value_text).collect()
                }
            };
            // Values past `ignore_above` were never indexed: nothing to highlight in them.
            let values: Vec<String> = match f.ignore_above {
                Some(n) => values.into_iter().filter(|v| v.chars().count() <= n).collect(),
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
