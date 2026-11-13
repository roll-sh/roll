//! The `fields` option (`FetchFieldsPhase` / `ValueFetcher`): values of mapped fields read from
//! `_source` (multi-fields from their parent's values), parsed and formatted by the field type
//! (numbers coerced to the type, dates printed with the field's or the requested `format`,
//! keywords normalized, `ignore_above` values moved to `ignored_field_values`, `null_value`
//! substituted, geo points as GeoJSON), fields inside `nested` objects grouped per nested
//! object, and `include_unmapped` source values.

use crate::engine::fields::{FieldInfo, FieldTable, Kind};
use crate::error::EsResult;
use crate::json::{Map, Number, Value};
use crate::search_source::FieldAndFormat;
use search_index::SegmentContext;

fn source_path(f: &FieldInfo) -> &str {
    f.parent.as_deref().unwrap_or(&f.name)
}

/// Field types whose values the `fields` option can format (`format` is rejected for the rest).
fn takes_format(k: Kind) -> bool {
    matches!(k, Kind::Date | Kind::DateNanos | Kind::DateRange | Kind::GeoPoint | Kind::GeoShape | Kind::Point | Kind::Shape | Kind::DenseVector)
}

/// `format` on a field that has none: Elasticsearch fails the fetch, naming the pattern.
pub fn check_formats(table: &FieldTable, specs: &[FieldAndFormat]) -> EsResult<()> {
    for s in specs {
        if s.format.is_none() {
            continue;
        }
        let names = if s.field.contains('*') { table.matching(&s.field) } else { vec![s.field.clone()] };
        for n in names {
            // A key of a `flattened` field (`flattened.bar`) is a field of that type.
            let f = match table.get(&n) {
                Some(f) => f.clone(),
                None => match n.match_indices('.').map(|(i, _)| &n[..i]).find_map(|p| table.get(p).filter(|f| f.kind == Kind::Flattened)) {
                    Some(f) => f.clone(),
                    None => continue,
                },
            };
            if !takes_format(f.kind) {
                let pattern = if s.field.contains('*') { format!(" which matched [{}]", s.field) } else { String::new() };
                let msg = format!("Field [{n}] of type [{}] doesn't support formats.", f.ty);
                return Err(crate::error::EsError::illegal_argument(format!("error fetching [{n}]{pattern}: {msg}")).caused_by(crate::error::EsError::illegal_argument(msg)));
            }
        }
    }
    Ok(())
}

/// Metadata fields the `fields` option refuses (`Cannot fetch values for internal field`).
pub fn check_internal(specs: &[FieldAndFormat]) -> EsResult<()> {
    // OpenSearch's `FieldFetcher` skips metadata fields instead.
    if crate::opensearch::active() {
        return Ok(());
    }
    const INTERNAL: &[&str] = &["_seq_no", "_source", "_feature", "_nested_path", "_field_names"];
    for s in specs {
        if INTERNAL.contains(&s.field.as_str()) {
            let msg = format!("Cannot fetch values for internal field [{}].", s.field);
            return Err(crate::error::EsError::illegal_argument(format!("error fetching [{}]: {msg}", s.field)).caused_by(crate::error::EsError::illegal_argument(msg)));
        }
    }
    Ok(())
}

/// Metadata fields requested by name (or through an alias): `(output name, metadata field)`.
/// Patterns never match metadata fields. (Elasticsearch only.)
pub fn metadata_requests(table: &FieldTable, specs: &[FieldAndFormat]) -> Vec<(String, String)> {
    // OpenSearch's `FieldFetcher` skips metadata fields (`isMetadataField`): none answer.
    if crate::opensearch::active() {
        return Vec::new();
    }
    const META: &[&str] = &["_id", "_index", "_ignored", "_tier", "_ts_routing_hash", "_tsid", "_version", "_doc_count"];
    let mut out: Vec<(String, String)> = Vec::new();
    let mut push = |name: String, meta: String| {
        if !out.iter().any(|(n, _)| *n == name) {
            out.push((name, meta));
        }
    };
    for s in specs {
        if !s.field.contains('*') {
            if META.contains(&s.field.as_str()) {
                push(s.field.clone(), s.field.clone());
            } else if let Some(t) = table.aliases.get(&s.field).filter(|t| META.contains(&t.as_str())) {
                push(s.field.clone(), t.clone());
            }
        } else {
            for (a, t) in &table.aliases {
                if META.contains(&t.as_str()) && crate::xcontent::filter::glob_match(&s.field, a) && !a.starts_with('_') {
                    push(a.clone(), t.clone());
                }
            }
        }
    }
    out
}

/// The fields matched by the requested patterns, in request order.
fn requested(table: &FieldTable, specs: &[FieldAndFormat]) -> Vec<(String, Option<String>, bool)> {
    let mut out: Vec<(String, Option<String>, bool)> = Vec::new();
    for s in specs {
        let names = if s.field.contains('*') {
            table.matching(&s.field)
        } else if table.get(&s.field).is_some() || table.keyed(&s.field).is_some() {
            vec![s.field.clone()]
        } else {
            Vec::new()
        };
        for n in names {
            if let Some(f) = table.get(&n) {
                // Internal subfields (`._index_prefix`, `._2gram`, `._keyed`) are not fields.
                if f.parent.is_some() && f.name.rsplit('.').next().is_some_and(|l| l.starts_with('_')) {
                    continue;
                }
                if f.kind == Kind::Join && n.contains('#') {
                    continue;
                }
            }
            if !out.iter().any(|(x, _, _)| *x == n) {
                out.push((n, s.format.clone(), s.include_unmapped.unwrap_or(false)));
            }
        }
        if s.include_unmapped == Some(true) && !s.field.contains('*') && table.get(&s.field).is_none() && !out.iter().any(|(x, _, _)| *x == s.field) {
            out.push((s.field.clone(), s.format.clone(), true));
        }
    }
    out
}

/// One source value parsed for a field (`parseSourceValue`); `Err(v)` = ignored (`ignore_above`,
/// malformed) with the value to report.
/// A float vector's component as the float it was indexed as.
fn vector_component(v: &Value) -> Value {
    match v {
        Value::Num(n) => Value::Num(crate::json::Number::Float(crate::fetch::java_float_as_double(n.as_f64() as f32))),
        other => other.clone(),
    }
}

fn parse_value(f: &FieldInfo, v: &Value, format: Option<&str>) -> Result<Option<Value>, Value> {
    let text = |v: &Value| -> String {
        match v {
            Value::Str(s) => s.clone(),
            other => other.to_es_string(),
        }
    };
    Ok(Some(match f.kind {
        Kind::Text | Kind::MatchOnlyText | Kind::SearchAsYouType => Value::Str(text(v)),
        Kind::Keyword if f.template_id => Value::Str(crate::engine::pattern_text::template_id(&text(v))),
        Kind::Keyword | Kind::Version | Kind::Wildcard | Kind::CountedKeyword => {
            let s = text(v);
            if f.ignore_above.is_some_and(|m| crate::document::exceeds_ignore_above(&s, m)) {
                return Err(v.clone());
            }
            Value::Str(match &f.normalizer {
                Some(n) => n.normalize(&s),
                None => s,
            })
        }
        Kind::ConstantKeyword => Value::Str(f.constant_value.clone().unwrap_or_else(|| text(v))),
        // A `DocValueFetcher`: the indexed count (nothing without doc values).
        Kind::TokenCount => {
            if !f.doc_values {
                return Ok(None);
            }
            Value::from(crate::engine::convert::token_count(f, &text(v)))
        }
        Kind::Boolean => match v {
            Value::Bool(b) => Value::Bool(*b),
            Value::Str(s) if s == "true" => Value::Bool(true),
            Value::Str(s) if s == "false" || s.is_empty() => Value::Bool(false),
            other => return Err(other.clone()),
        },
        Kind::Long | Kind::Integer | Kind::Short | Kind::Byte => {
            let d = match v {
                Value::Num(Number::Int(i)) => return Ok(Some(Value::from(*i))),
                Value::Num(n) => n.as_f64(),
                Value::Str(s) => match s.trim().parse::<i64>() {
                    Ok(i) => return Ok(Some(Value::from(i))),
                    Err(_) => s.trim().parse::<f64>().map_err(|_| v.clone())?,
                },
                other => return Err(other.clone()),
            };
            if !d.is_finite() {
                return Err(v.clone());
            }
            Value::from(d.trunc() as i64)
        }
        Kind::UnsignedLong => match v {
            Value::Num(Number::Int(i)) if *i >= 0 => Value::from(*i),
            Value::Num(Number::UInt(u)) => Value::Num(Number::UInt(*u)),
            Value::Str(s) => match s.trim().parse::<u64>() {
                Ok(u) if u <= i64::MAX as u64 => Value::from(u as i64),
                Ok(u) => Value::Num(Number::UInt(u)),
                Err(_) => return Err(v.clone()),
            },
            other => return Err(other.clone()),
        },
        Kind::Double | Kind::Float | Kind::HalfFloat | Kind::ScaledFloat(_) => {
            let d = match v {
                Value::Num(n) => n.as_f64(),
                Value::Str(s) => s.trim().parse::<f64>().map_err(|_| v.clone())?,
                other => return Err(other.clone()),
            };
            match f.kind {
                Kind::Float => Value::Num(Number::Float(crate::fetch::java_float_as_double(d as f32))),
                Kind::HalfFloat => {
                    let h = search_codec::points::encode::f16_to_f32(search_codec::points::encode::f16_bits(d as f32));
                    Value::Num(Number::Float(crate::fetch::java_float_as_double(h)))
                }
                Kind::ScaledFloat(sf) => Value::Num(Number::Float(search_aggs::format::java_round(d * sf) as f64 / sf)),
                _ => Value::Num(Number::Float(d)),
            }
        }
        Kind::Date | Kind::DateNanos => {
            let fmt = f.date_format.clone();
            let Some(fmt) = fmt else { return Ok(Some(v.clone())) };
            let s = match v {
                Value::Num(Number::Float(x)) => format!("{x}"),
                other => text(other),
            };
            let inst = fmt.parse(&s, None, false).map_err(|_| v.clone())?;
            let out_fmt = match format {
                Some(p) => std::sync::Arc::new(fmt.derive(p).map_err(|_| v.clone())?),
                None => fmt,
            };
            Value::Str(out_fmt.format(inst, &crate::time::Zone::UTC))
        }
        Kind::Ip => {
            let s = text(v);
            match crate::document::values::parse_ip(&s) {
                Some(b) => Value::Str(search_index::query::range::inet_to_string(&b)),
                None => return Err(v.clone()),
            }
        }
        Kind::GeoPoint => {
            // The field's own parser: `[lon, lat]` reads numbers through Jackson's accessors
            // (numeric strings coerce), and `ignore_malformed` wraps out-of-range points.
            let parsed = match v {
                Value::Arr(a) if a.first().is_some_and(|x| matches!(x, Value::Num(_))) && a.len() >= 2 => {
                    let num = |x: &Value| match x {
                        Value::Num(n) => Some(n.as_f64()),
                        Value::Str(s) => crate::units::parse_java_double(s.trim()),
                        _ => None,
                    };
                    num(&a[0]).zip(num(&a[1])).map(|(lon, lat)| crate::query::geo::GeoPoint { lat, lon })
                }
                _ => {
                    let mut p = crate::query::XParser::from_value(v);
                    p.next_token();
                    // Indexed values passed the field's parser, which drops z values.
                    if crate::opensearch::active() { crate::query::geo::parse_point(&mut p).ok() } else { crate::query::geo::parse_field_point(&mut p, crate::query::geo::PointKind::Geo, true).ok() }
                }
            };
            let Some(mut pt) = parsed else { return Err(v.clone()) };
            let in_range = (-90.0..=90.0).contains(&pt.lat) && (-180.0..=180.0).contains(&pt.lon);
            if !in_range {
                if f.mapper.raw("ignore_malformed").and_then(Value::as_bool) != Some(true) {
                    return Err(v.clone());
                }
                (pt.lat, pt.lon) = crate::document::values::normalize_point(pt.lat, pt.lon);
            }
            if format == Some("wkt") {
                Value::Str(format!("POINT ({} {})", crate::json::write::java_double(pt.lon), crate::json::write::java_double(pt.lat)))
            } else {
                crate::obj! { "coordinates" => Value::Arr(vec![Value::Num(Number::Float(pt.lon)), Value::Num(Number::Float(pt.lat))]), "type" => "Point" }
            }
        }
        // Flattened leaves past `ignore_above` aren't indexed and aren't fetched.
        Kind::Flattened => match f.ignore_above {
            Some(max) => match drop_long_leaves(v, max) {
                Some(x) => x,
                None => return Ok(None),
            },
            None => v.clone(),
        },
        // Float vectors come back as the floats they were indexed as (`[1]` reads `[1.0]`).
        Kind::DenseVector if f.vector.is_none_or(|s| !matches!(s.element, search_codec::VectorElement::Byte | search_codec::VectorElement::Bit)) => match v {
            Value::Arr(a) => Value::Arr(a.iter().map(vector_component).collect()),
            other => vector_component(other),
        },
        Kind::Binary | Kind::DenseVector | Kind::SparseVector | Kind::RankFeatures | Kind::RankFeature | Kind::Completion => v.clone(),
        // Cartesian points read as GeoJSON (or WKT), without their z value.
        Kind::Point => {
            let parsed = match v {
                // `[x, y]` reads numbers through Jackson's accessors (numeric strings coerce).
                Value::Arr(a) if a.first().is_some_and(|x| matches!(x, Value::Num(_))) && a.len() >= 2 => {
                    let num = |x: &Value| match x {
                        Value::Num(n) => Some(n.as_f64()),
                        Value::Str(s) => crate::units::parse_java_double(s.trim()),
                        _ => None,
                    };
                    num(&a[0]).zip(num(&a[1])).map(|(lon, lat)| crate::query::geo::GeoPoint { lat, lon })
                }
                _ => {
                    let mut p = crate::query::XParser::from_value(v);
                    p.next_token();
                    crate::query::geo::parse_field_point(&mut p, crate::query::geo::PointKind::Cartesian, true).ok()
                }
            };
            let Some(pt) = parsed else { return Err(v.clone()) };
            if format == Some("wkt") {
                Value::Str(format!("POINT ({} {})", crate::json::write::java_double(pt.lon), crate::json::write::java_double(pt.lat)))
            } else {
                crate::obj! { "coordinates" => Value::Arr(vec![Value::Num(Number::Float(pt.lon)), Value::Num(Number::Float(pt.lat))]), "type" => "Point" }
            }
        }
        // Shapes that don't parse were ignored (malformed) at index time.
        Kind::GeoShape | Kind::Shape => {
            let mut p = crate::query::XParser::from_value(v);
            p.next_token();
            if crate::query::geo::parse_geometry(&mut p).is_err() {
                return Err(v.clone());
            }
            v.clone()
        }
        _ => v.clone(),
    }))
}

/// `v` without string leaves longer than `max`; `None` when nothing is left.
fn drop_long_leaves(v: &Value, max: usize) -> Option<Value> {
    match v {
        Value::Obj(m) => {
            let out: Map = m.iter().filter_map(|(k, x)| drop_long_leaves(x, max).map(|y| (k.clone(), y))).collect();
            (!out.is_empty()).then_some(Value::Obj(out))
        }
        Value::Arr(a) => {
            let out: Vec<Value> = a.iter().filter_map(|x| drop_long_leaves(x, max)).collect();
            match out.len() {
                0 => None,
                1 => out.into_iter().next(),
                _ => Some(Value::Arr(out)),
            }
        }
        Value::Str(s) if crate::document::exceeds_ignore_above(s, max) => None,
        Value::Null => None,
        other => Some(other.clone()),
    }
}

fn flatten_values(v: &Value, f: &FieldInfo, out: &mut Vec<Value>) {
    match v {
        // Arrays of point coordinates and vectors are one value.
        Value::Arr(a) if matches!(f.kind, Kind::DenseVector) || (matches!(f.kind, Kind::GeoPoint | Kind::Point) && a.first().is_some_and(|x| matches!(x, Value::Num(_)))) => {
            out.push(v.clone())
        }
        Value::Arr(a) => {
            for x in a {
                flatten_values(x, f, out);
            }
        }
        other => out.push(other.clone()),
    }
}

/// `UnmappedFieldFetcher`: source leaves that no field maps, whose path matches an
/// `include_unmapped` pattern. Values keep Elasticsearch's order: inside an array, values of
/// inner arrays first (they recurse at once), then the array's own scalars.
fn collect_unmapped(table: &FieldTable, patterns: &[&str], src: &Map, prefix: &str, out: &mut Vec<(String, Vec<Value>)>) {
    for (k, v) in src {
        let path = format!("{prefix}{k}");
        if table.get(&path).is_some_and(|f| f.parent.is_none()) {
            continue;
        }
        // Nested objects report their unmapped values inside their own entries.
        if table.objects.get(&path).is_some_and(|o| o.nested) {
            continue;
        }
        match v {
            Value::Obj(m) => crate::stack::maybe_grow(|| collect_unmapped(table, patterns, m, &format!("{path}."), out)),
            Value::Arr(a) => collect_unmapped_list(table, patterns, a, &path, out),
            Value::Null => {}
            leaf => {
                if patterns.iter().any(|p| crate::xcontent::filter::glob_match(p, &path)) {
                    add_unmapped(out, &path, vec![leaf.clone()]);
                }
            }
        }
    }
}

fn collect_unmapped_list(table: &FieldTable, patterns: &[&str], a: &[Value], path: &str, out: &mut Vec<(String, Vec<Value>)>) {
    let mut list = Vec::new();
    let accept = patterns.iter().any(|p| crate::xcontent::filter::glob_match(p, path));
    for x in a {
        match x {
            Value::Obj(m) => crate::stack::maybe_grow(|| collect_unmapped(table, patterns, m, &format!("{path}."), out)),
            Value::Arr(inner) => crate::stack::maybe_grow(|| collect_unmapped_list(table, patterns, inner, path, out)),
            Value::Null => {}
            leaf if accept => list.push(leaf.clone()),
            _ => {}
        }
    }
    if !list.is_empty() {
        add_unmapped(out, path, list);
    }
}

fn add_unmapped(out: &mut Vec<(String, Vec<Value>)>, path: &str, mut vals: Vec<Value>) {
    match out.iter_mut().find(|(p, _)| p == path) {
        Some((_, v)) => v.append(&mut vals),
        None => out.push((path.to_string(), vals)),
    }
}

/// Fills `fields` (and `ignored_field_values`) for one hit.
pub fn fetch_fields(table: &FieldTable, source: Option<&Value>, specs: &[FieldAndFormat], fields: &mut Map, ignored: &mut Map, _seg: &SegmentContext, _local: u32) -> EsResult<()> {
    let Some(Value::Obj(src)) = source else { return Ok(()) };
    let unmapped_patterns: Vec<&str> = specs.iter().filter(|s| s.include_unmapped == Some(true) && s.field.contains('*')).map(|s| s.field.as_str()).collect();
    if !unmapped_patterns.is_empty() {
        let mut found = Vec::new();
        collect_unmapped(table, &unmapped_patterns, src, "", &mut found);
        for (p, v) in found {
            fields.insert(p, Value::Arr(v));
        }
    }
    for (name, format, include_unmapped) in requested(table, specs) {
        let Some(f) = table.get(&name).cloned().or_else(|| table.keyed(&name)) else {
            if include_unmapped {
                let mut raw = Vec::new();
                crate::engine::source::extract_values(src, &name, &mut raw);
                let vals: Vec<Value> = raw.into_iter().filter(|v| !v.is_null()).cloned().collect();
                if !vals.is_empty() {
                    fields.insert(name, Value::Arr(vals));
                }
            }
            continue;
        };
        if f.nested_path.is_some() {
            // Nested fields are returned under their nested object (below).
            continue;
        }
        let mut raw = Vec::new();
        let coords = matches!(f.kind, Kind::GeoPoint | Kind::Point);
        let extract = if coords { crate::engine::source::extract_values_keeping_coordinates } else { crate::engine::source::extract_values };
        extract(src, source_path(&f), &mut raw);
        for s in table.copy_sources.get(&f.name).into_iter().flatten() {
            extract(src, s, &mut raw);
        }
        let mut vals = Vec::new();
        let mut bad = Vec::new();
        let null_value = f.mapper.raw("null_value").cloned();
        for v in raw {
            let mut flat = Vec::new();
            flatten_values(v, &f, &mut flat);
            for x in flat {
                if x.is_null() {
                    if let Some(nv) = &null_value
                        && let Ok(Some(p)) = parse_value(&f, nv, format.as_deref())
                    {
                        vals.push(p);
                    }
                    continue;
                }
                match parse_value(&f, &x, format.as_deref()) {
                    Ok(Some(p)) => vals.push(p),
                    Ok(None) => {}
                    Err(b) => bad.push(b),
                }
            }
        }
        if !vals.is_empty() {
            fields.insert(name.clone(), Value::Arr(vals));
        }
        // Values past `ignore_above`, and malformed values `ignore_malformed` kept out of the
        // index, are returned as `ignored_field_values`.
        // (Scalar types only: malformed points and shapes are not reported.)
        let scalar = f.kind.is_numeric() || f.kind.is_date() || matches!(f.kind, Kind::Ip | Kind::Boolean);
        if !bad.is_empty() && (f.kind.is_keyword_like() || (scalar && f.mapper.raw("ignore_malformed").and_then(Value::as_bool) == Some(true))) {
            ignored.insert(name, Value::Arr(bad));
        }
    }
    // Nested objects: `{"path": [{"field": [..]}, ..]}` per nested object, recursively for
    // nested objects inside them.
    let wanted = requested(table, specs);
    let mut nested_paths: Vec<String> = table.nested_paths().filter(|o| o.parent_nested.is_none()).map(|o| o.path.clone()).collect();
    nested_paths.sort();
    for p in nested_paths {
        let mut objs = Vec::new();
        crate::engine::source::extract_values(src, &p, &mut objs);
        if let Some(v) = nested_values(table, &wanted, &unmapped_patterns, &p, &objs) {
            fields.insert(p, v);
        }
    }
        Ok(())
}

/// The `fields` of the nested objects `objs` at nested path `path`: one object per nested
/// object, keyed relative to it, with deeper nested objects grouped the same way.
fn nested_values(table: &FieldTable, wanted: &[(String, Option<String>, bool)], unmapped: &[&str], path: &str, objs: &[&Value]) -> Option<Value> {
    let prefix = format!("{path}.");
    let inner: Vec<&(String, Option<String>, bool)> = wanted.iter().filter(|(n, _, _)| n.starts_with(&prefix)).collect();
    if inner.is_empty() && unmapped.is_empty() {
        return None;
    }
    let mut children: Vec<String> = table.nested_paths().filter(|o| o.parent_nested.as_deref() == Some(path)).map(|o| o.path.clone()).collect();
    children.sort();
    let mut out = Vec::new();
    for o in objs {
        let Value::Obj(om) = o else { continue };
        let mut m = Map::new();
        for (n, fmt, _) in &inner {
            // Fields of deeper nested objects belong to those.
            if children.iter().any(|c| n.starts_with(&format!("{c}."))) {
                continue;
            }
            let Some(f) = table.get(n) else { continue };
            let Some(rel) = source_path(f).strip_prefix(&prefix) else { continue };
            let mut raw = Vec::new();
            crate::engine::source::extract_values(om, rel, &mut raw);
            let mut vals = Vec::new();
            for v in raw {
                let mut flat = Vec::new();
                flatten_values(v, f, &mut flat);
                for x in flat {
                    if let Ok(Some(pv)) = parse_value(f, &x, fmt.as_deref()) {
                        vals.push(pv);
                    }
                }
            }
            if !vals.is_empty() {
                m.insert(n[prefix.len()..].to_string(), Value::Arr(vals));
            }
        }
        for c in &children {
            let rel = &c[prefix.len()..];
            let mut sub = Vec::new();
            crate::engine::source::extract_values(om, rel, &mut sub);
            if let Some(v) = nested_values(table, wanted, unmapped, c, &sub) {
                m.insert(rel.to_string(), v);
            }
        }
        // `include_unmapped` source values of this nested object, keyed relative to it.
        if !unmapped.is_empty() {
            let mut found = Vec::new();
            collect_unmapped(table, unmapped, om, &prefix, &mut found);
            for (p, v) in found {
                m.insert(p[prefix.len()..].to_string(), Value::Arr(v));
            }
        }
        if !m.is_empty() {
            out.push(Value::Obj(crate::json::java_hash_map(m, 16)));
        }
    }
    (!out.is_empty()).then_some(Value::Arr(out))
}
