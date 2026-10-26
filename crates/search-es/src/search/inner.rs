//! `inner_hits` of `nested` queries (`InnerHitsPhase` / `NestedInnerHitSubContext`): for each top
//! hit, the nested documents of its block that match the nested query, scored (or sorted),
//! paged (`from`, `size` 3), each with its `_nested` identity (path and offset in the parent's
//! `_source` array) and its own slice of `_source`.

use super::query::InnerHitsDef;
use super::{ShardResult, Target};
use crate::engine::fields::{NESTED_PATH_FIELD, FieldTable};
use crate::error::{EsError, EsResult};
use crate::json::{Map, Number, Value};
use crate::obj;
use search_index::{ScoreMode, Searcher};

/// Whether a segment doc is a root (non-nested) doc.
fn is_root(seg: &search_index::SegmentContext, d: u32) -> bool {
    seg.reader.numeric(search_index::schema::PRIMARY_TERM_FIELD).and_then(|v| v.get(d)).is_some()
}

fn path_of(seg: &search_index::SegmentContext, d: u32, path: &str) -> bool {
    let Some(terms) = seg.reader.terms(NESTED_PATH_FIELD) else { return false };
    let Some(st) = terms.seek_exact(path.as_bytes()) else { return false };
    let mut pc = terms.postings(&st, search_codec::PostingsFlags::DOCS);
    pc.advance(d) == d
}

/// The `inner_hits` object of one hit.
pub fn render(qctx: &super::query::QueryCtx, t: &Target, r: &ShardResult, parent: u32, source: Option<&Value>, total_as_int: bool) -> EsResult<Value> {
    let mut out = Map::new();
    for def in &r.inner_hits {
        let mut h = match &def.join {
            Some(j) => super::join::inner_hits(qctx, t, r, parent, def, j)?,
            None => nested_hits(qctx, t, &r.searcher, parent, def, source, None, total_as_int)?,
        };
        if total_as_int {
            total_to_int(&mut h);
        }
        out.insert(def.name.clone(), obj! { "hits" => h });
    }
    // `SearchHit.innerHits` is a `HashMap` filled by name.
    Ok(Value::Obj(crate::json::java_hash_map(out, 16)))
}

/// `rest_total_hits_as_int`: `total` as a bare number.
fn total_to_int(h: &mut Value) {
    if let Value::Obj(m) = h
        && let Some(v) = m.get("total").and_then(|t| t.get("value")).cloned()
    {
        m.insert("total".into(), v);
    }
}

/// The inner hits of `def` for root hit `parent` (global doc), or, with `scope`, for the
/// nested document `scope` (segment-local) of an enclosing definition's hit: the candidates
/// are the nested documents of the root's block (or the scope's descendants, which the block
/// stores right before it).
#[allow(clippy::too_many_arguments)]
fn nested_hits(
    qctx: &super::query::QueryCtx,
    t: &Target,
    searcher: &Searcher,
    parent: u32,
    def: &InnerHitsDef,
    source: Option<&Value>,
    scope: Option<u32>,
    total_as_int: bool,
) -> EsResult<Value> {
    let table: &FieldTable = qctx.table;
    let ie = crate::engine::convert::index_error;
    let Some(path) = &def.nested_path else { return Ok(obj! { "total" => obj! { "value" => 0, "relation" => "eq" }, "max_score" => Value::Null, "hits" => Value::Arr(Vec::new()) }) };
    let (seg, local) = searcher.segment_of(parent).ok_or_else(|| EsError::illegal_state("hit out of range"))?;
    let (start, end) = match scope {
        None => {
            let mut start = local;
            while start > 0 && !is_root(seg, start - 1) {
                start -= 1;
            }
            (start, local)
        }
        Some(p) => {
            let deeper = doc_path(table, seg, p).map(|pp| format!("{pp}.")).unwrap_or_default();
            let mut start = p;
            while start > 0 && doc_path(table, seg, start - 1).is_some_and(|x| x.starts_with(&deeper)) {
                start -= 1;
            }
            (start, p)
        }
    };
    let w = searcher.create_weight(&def.query, ScoreMode::Complete).map_err(ie)?;
    // A block's nested documents live and die with their root (a hit, so live): every match
    // in the block counts.
    let mut matches: Vec<(u32, f32)> = Vec::new();
    if let Some(mut s) = w.scorer(seg).map_err(ie)? {
        let mut d = s.advance(start);
        while d < end {
            matches.push((d, s.score()));
            d = s.next();
        }
    }
    let total = matches.len();
    let spec = &def.spec;
    let sorted = spec.sort.is_some();
    let from = spec.from.unwrap_or(0).max(0) as usize;
    let size = spec.size.unwrap_or(3).max(0) as usize;
    // No hits asked for: only the total is counted (`max_score` NaN). A sorted search tracks
    // the best score only with `track_scores` (`MaxScoreCollector`).
    let max_score = if (sorted && !spec.track_scores) || size == 0 { None } else { matches.iter().map(|m| m.1).reduce(f32::max) };
    let mut sort_values: Vec<Vec<Value>> = Vec::new();
    if let Some(sort) = &spec.sort {
        let global: Vec<(u32, f32)> = matches.iter().map(|&(d, s)| (seg.base + d, s)).collect();
        let top = sorted_top(qctx, searcher, sort, Some(path), &global, from.saturating_add(size))?;
        matches = top.iter().map(|(d, s, _)| (d - seg.base, *s)).collect();
        sort_values = top.into_iter().map(|(_, _, v)| v).collect();
    } else {
        matches.sort_by(|a, b| b.1.total_cmp(&a.1).then(a.0.cmp(&b.0)));
    }
    let parent_id = {
        let stored = searcher.stored(parent).map_err(ie)?;
        super::hits::stored_id(&stored).unwrap_or_default()
    };
    let fs = match &spec.source {
        Some(v) => crate::fetch::FetchSource::from_value(v)?,
        None => crate::fetch::FetchSource::default(),
    };
    let highlight = match &spec.highlight {
        Some(Value::Obj(hl)) => Some(super::highlight::Spec::parse(hl)?),
        _ => None,
    };
    let docvalue_specs = spec.docvalue_fields.as_ref().map(field_specs).unwrap_or_default();
    let mut hits = Vec::new();
    for (i, (d, score)) in matches.iter().enumerate().skip(from).take(size) {
        // The `_nested` identity: every enclosing nested level with its offset in its parent.
        let identity = nested_identity(table, seg, *d);
        let mut h = Map::new();
        h.insert("_index".into(), Value::Str(t.idx.name.clone()));
        h.insert("_id".into(), Value::Str(parent_id.clone()));
        h.insert("_nested".into(), identity.as_ref().map(NestedIdentity::to_value).unwrap_or(Value::Null));
        // Nested documents share their root's version and sequence number.
        if spec.version {
            h.insert("_version".into(), Value::from(super::hits::meta_dv(seg, local, search_index::schema::VERSION_FIELD)));
        }
        if spec.seq_no_primary_term {
            h.insert("_seq_no".into(), Value::from(super::hits::meta_dv(seg, local, search_index::schema::SEQ_NO_FIELD)));
            h.insert("_primary_term".into(), Value::from(super::hits::meta_dv(seg, local, search_index::schema::PRIMARY_TERM_FIELD)));
        }
        // Sorted inner hits print no score, `track_scores` or not: `TopFieldCollector` leaves
        // `ScoreDoc.score` NaN and only the main query phase populates it.
        h.insert("_score".into(), if sorted { Value::Null } else { Value::Num(Number::Float(crate::fetch::java_float_as_double(*score))) });
        let obj = match (&identity, source) {
            (Some(id), Some(src)) => id.source_of(src),
            _ => None,
        };
        if fs.fetch
            && let Some(o) = &obj
        {
            // Source filters name full paths (`nest.k`): the nested object is filtered in place
            // in its document, then taken out again.
            let full = identity.as_ref().and_then(|id| id.levels.last()).map(|l| l.0.clone());
            let mut v = match (fs.filter(), o, full) {
                (Some(f), Value::Obj(om), Some(full)) => {
                    let mut wrapped = Value::Obj(om.clone());
                    for part in full.rsplit('.') {
                        let mut m = Map::new();
                        m.insert(part.to_string(), wrapped);
                        wrapped = Value::Obj(m);
                    }
                    let Value::Obj(wm) = wrapped else { unreachable!("built as an object") };
                    let mut cur = Value::Obj(f.apply(&wm));
                    for part in full.split('.') {
                        cur = match cur {
                            Value::Obj(mut m) => m.shift_remove(part).unwrap_or_else(|| Value::Obj(Map::new())),
                            _ => Value::Obj(Map::new()),
                        };
                    }
                    cur
                }
                (Some(f), Value::Obj(om), None) => Value::Obj(f.apply(om)),
                (_, v, _) => v.clone(),
            };
            // OpenSearch extracts the nested object from the `HashMap`s the source parses into.
            if t.idx.dialect == crate::cluster::Dialect::OpenSearch {
                crate::opensearch::hash_maps(&mut v);
            }
            h.insert("_source".into(), v);
        }
        if let Some(fv) = &spec.fields {
            let specs = field_specs(fv);
            if let Some(o) = &obj {
                let mut wrapped = Map::new();
                wrapped.insert(path.clone(), o.clone());
                let mut fields = Map::new();
                let mut ignored = Map::new();
                super::fields::fetch_fields(table, Some(&Value::Obj(wrapped)), &specs, &mut fields, &mut ignored, seg, *d)?;
                if !fields.is_empty() {
                    h.insert("fields".into(), Value::Obj(fields));
                }
            }
        }
        if !docvalue_specs.is_empty() {
            let (meta, own): (Vec<_>, Vec<_>) = docvalue_specs.iter().cloned().partition(|f| matches!(f.field.as_str(), "_seq_no" | "_version" | "_primary_term"));
            let mut fields = match h.shift_remove("fields") {
                Some(Value::Obj(m)) => m,
                _ => Map::new(),
            };
            // No cluster state here: `_id` fielddata follows the dialect's default.
            let id_fd = crate::opensearch::active();
            super::hits::docvalue_fields(table, seg, *d, &own, id_fd, &mut fields)?;
            super::hits::docvalue_fields(table, seg, local, &meta, id_fd, &mut fields)?;
            if !fields.is_empty() {
                let at = h.get_index_of("_source").map(|i| i + 1).unwrap_or(h.len());
                h.shift_insert(at, "fields".into(), Value::Obj(fields));
            }
        }
        if let Some(hl) = &highlight {
            // The nested object, at its path, is the document the highlighter reads.
            let wrapped = obj.as_ref().map(|o| {
                let mut v = o.clone();
                for part in path.rsplit('.') {
                    let mut m = Map::new();
                    m.insert(part.to_string(), v);
                    v = Value::Obj(m);
                }
                v
            });
            let out = super::highlight::highlight_hit(qctx, hl, &def.query, wrapped.as_ref(), &parent_id, searcher, Some(seg.base + *d))?;
            if !out.is_empty() {
                h.insert("highlight".into(), Value::Obj(out));
            }
        }
        if let Some(vals) = sort_values.get_mut(i) {
            h.insert("sort".into(), Value::Arr(std::mem::take(vals)));
        }
        if !def.children.is_empty() {
            let mut inner = Map::new();
            for c in &def.children {
                let mut ch = nested_hits(qctx, t, searcher, parent, c, source, Some(*d), total_as_int)?;
                if total_as_int {
                    total_to_int(&mut ch);
                }
                inner.insert(c.name.clone(), obj! { "hits" => ch });
            }
            h.insert("inner_hits".into(), Value::Obj(crate::json::java_hash_map(inner, 16)));
        }
        hits.push(Value::Obj(h));
    }
    Ok(obj! {
        "total" => obj! { "value" => total as i64, "relation" => "eq" },
        "max_score" => match max_score { Some(s) => Value::Num(Number::Float(crate::fetch::java_float_as_double(s))), None => Value::Null },
        "hits" => Value::Arr(hits),
    })
}

pub(super) fn field_specs(v: &Value) -> Vec<crate::search_source::FieldAndFormat> {
    let one = |x: &Value| -> Option<crate::search_source::FieldAndFormat> {
        match x {
            Value::Str(s) => Some(crate::search_source::FieldAndFormat { field: s.clone(), format: None, include_unmapped: None }),
            Value::Obj(m) => Some(crate::search_source::FieldAndFormat {
                field: m.get("field").and_then(Value::as_str).unwrap_or_default().to_string(),
                format: m.get("format").and_then(Value::as_str).map(str::to_string),
                include_unmapped: m.get("include_unmapped").and_then(Value::as_bool),
            }),
            _ => None,
        }
    };
    match v {
        Value::Arr(a) => a.iter().filter_map(one).collect(),
        other => one(other).into_iter().collect(),
    }
}

/// Validates an inner hits `sort` while its query is built (`InnerHitBuilder.setupInnerHitsContext`
/// builds the sort in the query phase, so a bad one fails the shard there).
pub(super) fn check_sort(qctx: &super::query::QueryCtx, sort: Option<&Value>, nested_path: Option<&str>) -> EsResult<()> {
    if let Some(sort) = sort {
        build_sort(qctx, sort, nested_path)?;
    }
    Ok(())
}

/// An inner hits sort, built in the definition's nested scope (`nested_path`).
fn build_sort(qctx: &super::query::QueryCtx, sort: &Value, nested_path: Option<&str>) -> EsResult<super::sort::BuiltSort> {
    let sorts = crate::search_source::parse_sorts_value(sort)?;
    let prev = qctx.nested.replace(nested_path.map(str::to_string));
    let built = super::sort::build(qctx, &sorts);
    qctx.nested.replace(prev);
    built
}

/// The first `n` of `matches` (global docs in doc order, with their scores) under an inner hits
/// `sort`, as Lucene's `TopFieldCollector` orders them (the request sort's `missing`, `mode`,
/// `numeric_type`, `_score`, `_doc`; ties by doc), each with its printed sort values.
pub(super) fn sorted_top(
    qctx: &super::query::QueryCtx,
    searcher: &Searcher,
    sort: &Value,
    nested_path: Option<&str>,
    matches: &[(u32, f32)],
    n: usize,
) -> EsResult<Vec<(u32, f32, Vec<Value>)>> {
    use search_index::Collector;
    let ie = crate::engine::convert::index_error;
    let built = build_sort(qctx, sort, nested_path)?;
    let n = n.min(matches.len());
    if n == 0 {
        return Ok(Vec::new());
    }
    let mut c = search_index::collector::top_field::TopFieldCollector::new(built.keys.clone(), n, u64::MAX, true).with_searcher(searcher);
    let mut i = 0;
    while i < matches.len() {
        let Some((seg, _)) = searcher.segment_of(matches[i].0) else { break };
        let end = seg.base + seg.reader.max_doc();
        let mut leaf = c.leaf(seg).map_err(ie)?;
        while i < matches.len() && matches[i].0 < end {
            let (d, score) = matches[i];
            leaf.collect(d - seg.base, &mut search_index::search::ScoreValue(score)).map_err(ie)?;
            i += 1;
        }
    }
    let top = c.top_docs();
    Ok(top
        .hits
        .into_iter()
        .map(|h| {
            let score = matches.binary_search_by_key(&h.doc, |m| m.0).map(|p| matches[p].1).unwrap_or(f32::NAN);
            let vals = h.values.iter().zip(&built.formats).map(|(v, f)| super::sort::render_value(v, f, h.doc, score)).collect();
            (h.doc, score, vals)
        })
        .collect())
}

/// Where a nested document sits: its root document and, from the outermost nested level in,
/// each level's full path and offset (the `_nested` identity `SearchHit.NestedIdentity`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NestedIdentity {
    /// The root document (segment-local).
    pub root: u32,
    pub levels: Vec<(String, usize)>,
}

/// The nested path of a segment doc (`None` for roots).
fn doc_path(table: &FieldTable, seg: &search_index::SegmentContext, d: u32) -> Option<String> {
    if is_root(seg, d) {
        return None;
    }
    table.nested_paths().find(|o| path_of(seg, d, &o.path)).map(|o| o.path.clone())
}

/// The identity of segment doc `local` when it is a nested document (`None` for roots).
/// Blocks hold children before their parent, so the parent is the first later doc of the
/// parent level, and the offset counts earlier siblings back to the block's start.
pub fn nested_identity(table: &FieldTable, seg: &search_index::SegmentContext, local: u32) -> Option<NestedIdentity> {
    let path = doc_path(table, seg, local)?;
    let parent_path = table.objects.get(&path).and_then(|o| o.parent_nested.clone());
    let max = seg.reader.max_doc();
    let mut p = local + 1;
    while p < max {
        let hit = match &parent_path {
            None => is_root(seg, p),
            Some(pp) => path_of(seg, p, pp),
        };
        if hit {
            break;
        }
        p += 1;
    }
    if p >= max {
        return None;
    }
    let deeper = format!("{path}.");
    let mut offset = 0usize;
    let mut x = local;
    while x > 0 {
        x -= 1;
        match doc_path(table, seg, x) {
            Some(xp) if xp == path => offset += 1,
            Some(xp) if xp.starts_with(&deeper) => {}
            _ => break,
        }
    }
    let (root, mut levels) = match parent_path {
        None => (p, Vec::new()),
        Some(_) => {
            let up = nested_identity(table, seg, p)?;
            (up.root, up.levels)
        }
    };
    levels.push((path, offset));
    Some(NestedIdentity { root, levels })
}

impl NestedIdentity {
    /// `_nested` as hits print it: the outer level's full path, inner levels relative.
    pub fn to_value(&self) -> Value {
        let mut out: Option<Value> = None;
        for (i, (path, offset)) in self.levels.iter().enumerate().rev() {
            let field = match i {
                0 => path.clone(),
                _ => path.strip_prefix(&format!("{}.", self.levels[i - 1].0)).unwrap_or(path).to_string(),
            };
            let mut m = Map::new();
            m.insert("field".into(), Value::Str(field));
            m.insert("offset".into(), Value::from(*offset as i64));
            if let Some(inner) = out.take() {
                m.insert("_nested".into(), inner);
            }
            out = Some(Value::Obj(m));
        }
        out.unwrap_or(Value::Null)
    }

    /// The nested object inside the root's `_source`.
    pub fn source_of(&self, root: &Value) -> Option<Value> {
        let mut cur = root.clone();
        let mut prefix = String::new();
        for (path, offset) in &self.levels {
            let rel = if prefix.is_empty() { path.as_str() } else { path.strip_prefix(&prefix).unwrap_or(path) };
            let Value::Obj(m) = &cur else { return None };
            let mut vals = Vec::new();
            crate::engine::source::extract_values(m, rel, &mut vals);
            let next = vals.get(*offset).map(|v| (*v).clone())?;
            cur = next;
            prefix = format!("{path}.");
        }
        Some(cur)
    }
}
