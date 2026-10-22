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
            None => nested_hits(qctx, t, &r.searcher, parent, def, source)?,
        };
        if total_as_int
            && let Value::Obj(m) = &mut h
            && let Some(v) = m.get("total").and_then(|t| t.get("value")).cloned()
        {
            m.insert("total".into(), v);
        }
        out.insert(def.name.clone(), obj! { "hits" => h });
    }
    // `SearchHit.innerHits` is a `HashMap` filled by name.
    Ok(Value::Obj(crate::json::java_hash_map(out, 16)))
}

fn nested_hits(qctx: &super::query::QueryCtx, t: &Target, searcher: &Searcher, parent: u32, def: &InnerHitsDef, source: Option<&Value>) -> EsResult<Value> {
    let table: &FieldTable = qctx.table;
    let ie = crate::engine::convert::index_error;
    let Some(path) = &def.nested_path else { return Ok(obj! { "total" => obj! { "value" => 0, "relation" => "eq" }, "max_score" => Value::Null, "hits" => Value::Arr(Vec::new()) }) };
    let (seg, local) = searcher.segment_of(parent).ok_or_else(|| EsError::illegal_state("hit out of range"))?;
    let mut start = local;
    while start > 0 && !is_root(seg, start - 1) {
        start -= 1;
    }
    let w = searcher.create_weight(&def.query, ScoreMode::Complete).map_err(ie)?;
    let mut matches: Vec<(u32, f32)> = Vec::new();
    if let Some(mut s) = w.scorer(seg).map_err(ie)? {
        let mut d = s.advance(start);
        while d < local {
            if seg.is_live(d) || true {
                matches.push((d, s.score()));
            }
            d = s.next();
        }
    }
    let total = matches.len();
    let spec = &def.spec;
    let sorted = spec.sort.is_some();
    if let Some(sort) = &spec.sort {
        let keys = sort_keys(sort);
        matches.sort_by(|a, b| {
            for (field, desc) in &keys {
                let va = first_value(table, seg, a.0, field);
                let vb = first_value(table, seg, b.0, field);
                let o = va.partial_cmp(&vb).unwrap_or(std::cmp::Ordering::Equal);
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
    let max_score = if sorted { None } else { matches.iter().map(|m| m.1).reduce(f32::max) };
    let parent_id = {
        let stored = searcher.stored(parent).map_err(ie)?;
        super::hits::stored_id(&stored).unwrap_or_default()
    };
    let objects: Vec<Value> = match source {
        Some(Value::Obj(m)) => {
            let mut v = Vec::new();
            crate::engine::source::extract_values(m, path, &mut v);
            v.into_iter().cloned().collect()
        }
        _ => Vec::new(),
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
    for (d, score) in matches.iter().skip(from).take(size) {
        let offset = (start..*d).filter(|x| path_of(seg, *x, path)).count();
        let mut h = Map::new();
        h.insert("_index".into(), Value::Str(t.idx.name.clone()));
        h.insert("_id".into(), Value::Str(parent_id.clone()));
        h.insert("_nested".into(), obj! { "field" => path.as_str(), "offset" => offset as i64 });
        // Nested documents share their root's version and sequence number.
        if spec.version {
            h.insert("_version".into(), Value::from(super::hits::meta_dv(seg, local, search_index::schema::VERSION_FIELD)));
        }
        if spec.seq_no_primary_term {
            h.insert("_seq_no".into(), Value::from(super::hits::meta_dv(seg, local, search_index::schema::SEQ_NO_FIELD)));
            h.insert("_primary_term".into(), Value::from(super::hits::meta_dv(seg, local, search_index::schema::PRIMARY_TERM_FIELD)));
        }
        h.insert("_score".into(), if sorted && !spec.track_scores { Value::Null } else { Value::Num(Number::Float(crate::fetch::java_float_as_double(*score))) });
        let obj = objects.get(offset).cloned();
        if fs.fetch
            && let Some(o) = &obj
        {
            let v = match (fs.filter(), o) {
                (Some(f), Value::Obj(om)) => Value::Obj(f.apply(om)),
                (_, v) => v.clone(),
            };
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
            super::hits::docvalue_fields(table, seg, *d, &own, &mut fields)?;
            super::hits::docvalue_fields(table, seg, local, &meta, &mut fields)?;
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
        if let Some(sort) = &spec.sort {
            let vals: Vec<Value> = sort_keys(sort).iter().map(|(f, _)| first_json(table, seg, *d, f)).collect();
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

pub(super) fn sort_keys(v: &Value) -> Vec<(String, bool)> {
    let one = |x: &Value| -> Option<(String, bool)> {
        match x {
            Value::Str(s) => Some((s.clone(), s == "_score")),
            Value::Obj(m) => m.iter().next().map(|(k, o)| {
                let desc = match o {
                    Value::Str(s) => s == "desc",
                    Value::Obj(om) => om.get("order").and_then(Value::as_str) == Some("desc"),
                    _ => false,
                };
                (k.clone(), desc)
            }),
            _ => None,
        }
    };
    match v {
        Value::Arr(a) => a.iter().filter_map(one).collect(),
        other => one(other).into_iter().collect(),
    }
}

pub(super) fn first_value(table: &FieldTable, seg: &search_index::SegmentContext, d: u32, field: &str) -> Option<f64> {
    let f = table.get(field)?;
    let r = &seg.reader;
    if let Some(dv) = r.sorted_numeric(f.number) {
        return dv.values(d).next().map(|v| match f.kind {
            crate::engine::fields::Kind::Double => search_codec::points::encode::sortable_f64(v),
            crate::engine::fields::Kind::Float => search_codec::points::encode::sortable_f32(v as i32) as f64,
            _ => v as f64,
        });
    }
    if let Some(dv) = r.sorted_set(f.number) {
        return dv.ords(d).next().map(|o| o as f64);
    }
    None
}

pub(super) fn first_json(table: &FieldTable, seg: &search_index::SegmentContext, d: u32, field: &str) -> Value {
    let Some(f) = table.get(field) else { return Value::Null };
    match super::hits::doc_values(seg, d, f, None) {
        Ok(v) => match f.kind {
            k if k.is_date() => {
                // Sort values of dates are epoch numbers.
                seg.reader.sorted_numeric(f.number).and_then(|dv| dv.values(d).next()).map(Value::from).unwrap_or(Value::Null)
            }
            _ => v.into_iter().next().unwrap_or(Value::Null),
        },
        Err(_) => Value::Null,
    }
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
