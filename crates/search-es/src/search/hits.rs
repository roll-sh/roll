//! The fetch phase: one hit object per selected document (`SearchHit.toXContent` order:
//! `_shard`/`_node` with explanations, `_index`, `_id`, `_nested`, `_version`, `_seq_no`,
//! `_primary_term`, `_score`, metadata fields, `_source`, `fields`, `highlight`, `sort`,
//! `matched_queries`, `_explanation`, `inner_hits`).
//!
//! `fields` values come from `_source` through each field type's value parser and formatter
//! (`ValueFetcher`), `docvalue_fields` from doc values (`DocValueFormat`), `stored_fields` from
//! stored fields.

use crate::engine::fields::{FieldInfo, FieldTable, Kind};
use crate::error::{EsError, EsResult};
use crate::fetch::FetchSource;
use crate::json::{Map, Number, Value};
use crate::search_source::FieldAndFormat;
use search_codec::points::encode;
use search_codec::stored::OwnedStoredValue;
use search_index::{Searcher, SegmentContext};

/// Reads a doc's metadata doc values.
pub fn meta_dv(seg: &SegmentContext, local: u32, field: u32) -> i64 {
    seg.reader.numeric(field).and_then(|v| v.get(local)).unwrap_or(0)
}

/// The `_id` of a stored doc.
pub fn stored_id(stored: &[(u32, OwnedStoredValue)]) -> Option<String> {
    stored.iter().find(|(f, _)| *f == search_index::schema::ID_FIELD).and_then(|(_, v)| match v {
        OwnedStoredValue::Bytes(b) => Some(String::from_utf8_lossy(b).into_owned()),
        OwnedStoredValue::Str(s) => Some(s.clone()),
        _ => None,
    })
}

/// Doc values of one field of one doc, rendered with the field type's `DocValueFormat`.
pub fn doc_values(seg: &SegmentContext, local: u32, f: &FieldInfo, format: Option<&str>) -> EsResult<Vec<Value>> {
    let r = &seg.reader;
    let mut out = Vec::new();
    let n = f.number;
    match f.kind {
        // Text with `doc_values: true` keeps whole values, as keywords do.
        Kind::Text | Kind::MatchOnlyText if f.doc_values => {
            if let Some(dv) = r.sorted_set(n) {
                let mut buf = Vec::new();
                for o in dv.ords(local) {
                    buf.clear();
                    dv.lookup_ord(o, &mut buf);
                    out.push(Value::Str(String::from_utf8_lossy(&buf).into_owned()));
                }
            }
        }
        Kind::Keyword | Kind::Version | Kind::Wildcard | Kind::Flattened | Kind::CountedKeyword | Kind::ConstantKeyword | Kind::Join => {
            if f.kind == Kind::ConstantKeyword {
                if let Some(c) = &f.constant_value {
                    out.push(Value::Str(c.clone()));
                }
                return Ok(out);
            }
            if let Some(key) = &f.flattened_key {
                // A keyed sub-field's values are the `key\0value` doc values of its key.
                if let Some(dv) = r.sorted_set(n) {
                    let mut buf = Vec::new();
                    for o in dv.ords(local) {
                        buf.clear();
                        dv.lookup_ord(o, &mut buf);
                        if buf.len() > key.len() && buf.starts_with(key.as_bytes()) && buf[key.len()] == 0 {
                            out.push(Value::Str(String::from_utf8_lossy(&buf[key.len() + 1..]).into_owned()));
                        }
                    }
                }
            } else if let Some(dv) = r.sorted_set(n) {
                let mut buf = Vec::new();
                // `counted_keyword`: each value as often as the doc holds it.
                let counts = f.subfield("count").and_then(|c| r.binary(c)).and_then(|c| c.get(local));
                let mut counts = counts.map(search_aggs::values::VIntIter::array);
                for o in dv.ords(local) {
                    buf.clear();
                    dv.lookup_ord(o, &mut buf);
                    let v = Value::Str(if f.kind == Kind::Version { super::version_field::decode(&buf) } else { String::from_utf8_lossy(&buf).into_owned() });
                    let times = counts.as_mut().and_then(Iterator::next).unwrap_or(1).max(1);
                    for _ in 1..times {
                        out.push(v.clone());
                    }
                    out.push(v);
                }
            } else if let Some(dv) = r.sorted(n)
                && let Some(o) = dv.ord(local)
            {
                // Single-valued keyword-like fields (a join field's parent ids).
                let mut buf = Vec::new();
                dv.lookup_ord(o, &mut buf);
                out.push(Value::Str(String::from_utf8_lossy(&buf).into_owned()));
            }
        }
        Kind::Ip => {
            if let Some(dv) = r.sorted_set(n) {
                let mut buf = Vec::new();
                for o in dv.ords(local) {
                    buf.clear();
                    dv.lookup_ord(o, &mut buf);
                    out.push(Value::Str(search_index::query::range::inet_to_string(&buf)));
                }
            }
        }
        Kind::Text | Kind::MatchOnlyText => {
            if let Some(fd) = f.subfield("fielddata")
                && let Some(dv) = r.sorted_set(fd)
            {
                let mut buf = Vec::new();
                for o in dv.ords(local) {
                    buf.clear();
                    dv.lookup_ord(o, &mut buf);
                    out.push(Value::Str(String::from_utf8_lossy(&buf).into_owned()));
                }
            }
        }
        Kind::GeoPoint => {
            // `DocValueFormat.RAW` of a `GeoPoint`: `"lat, lon"`.
            if format.is_some() {
                return Err(EsError::illegal_argument(format!("Field [{}] of type [geo_point] does not support custom formats", f.name)));
            }
            if let Some(dv) = r.sorted_numeric(n) {
                for v in dv.values(local) {
                    let lat = encode::decode_latitude((v >> 32) as i32);
                    let lon = encode::decode_longitude(v as i32);
                    out.push(Value::Str(format!("{}, {}", crate::json::write::java_double(lat), crate::json::write::java_double(lon))));
                }
            }
        }
        k if k.is_numeric() || k.is_date() || k == Kind::Boolean => {
            if let Some(dv) = r.sorted_numeric(n) {
                for v in dv.values(local) {
                    out.push(format_numeric(f, v, format)?);
                }
            }
        }
        Kind::DenseVector => {
            if let Some(v) = dense_vector_value(seg, local, f, format)? {
                out.push(v);
            }
        }
        Kind::Binary => {
            if let Some(dv) = r.binary(n)
                && let Some(b) = dv.get(local)
            {
                use base64::Engine as _;
                for v in crate::engine::convert::decode_binary_dv(b) {
                    out.push(Value::Str(base64::engine::general_purpose::STANDARD.encode(v)));
                }
            }
        }
        _ => {}
    }
    Ok(out)
}

/// A `dense_vector` doc value (`DenseVectorFieldMapper` docvalue fetcher): the vector as an
/// array (floats, or ints for byte and bit vectors), or with `format: binary` its base64
/// big-endian bytes (`bfloat16` as two bytes per dimension).
fn dense_vector_value(seg: &SegmentContext, local: u32, f: &FieldInfo, format: Option<&str>) -> EsResult<Option<Value>> {
    use base64::Engine as _;
    let r = &seg.reader;
    let raw: Option<Vec<u8>> = if f.index {
        r.vectors(f.number).and_then(|v| v.ord(local).map(|o| v.vector(o).to_vec()))
    } else {
        r.binary(f.number).and_then(|b| b.get(local).map(<[u8]>::to_vec))
    };
    let Some(raw) = raw else { return Ok(None) };
    let element = f.mapper.raw("element_type").and_then(Value::as_str).map(str::to_string);
    let element = element.unwrap_or_else(|| match f.vector.map(|s| s.element) {
        Some(search_codec::VectorElement::Byte) => "byte".into(),
        Some(search_codec::VectorElement::Bit) => "bit".into(),
        _ => "float".into(),
    });
    let floats = || raw.as_chunks::<4>().0.iter().map(|c| f32::from_le_bytes(*c)).collect::<Vec<f32>>();
    let bf16 = |x: f32| -> u16 {
        let b = x.to_bits();
        ((b + 0x7fff + ((b >> 16) & 1)) >> 16) as u16
    };
    match format {
        None | Some("array") => Ok(Some(Value::Arr(match element.as_str() {
            "byte" | "bit" => raw.iter().map(|b| Value::from(*b as i8 as i64)).collect(),
            "bfloat16" => floats().into_iter().map(|x| crate::query::xparser::float_value(f32::from_bits((bf16(x) as u32) << 16))).collect(),
            _ => floats().into_iter().map(crate::query::xparser::float_value).collect(),
        }))),
        Some("binary") => {
            let bytes: Vec<u8> = match element.as_str() {
                "byte" | "bit" => raw,
                "bfloat16" => floats().into_iter().flat_map(|x| bf16(x).to_be_bytes()).collect(),
                _ => floats().into_iter().flat_map(f32::to_be_bytes).collect(),
            };
            Ok(Some(Value::Str(base64::engine::general_purpose::STANDARD.encode(bytes))))
        }
        Some(other) => Err(EsError::illegal_argument(format!("Unknown format [{other}] for field [{}], supported formats are [array, binary]", f.name))),
    }
}

/// A document's `dense_vector` value as floats (byte and bit vectors by their byte values).
pub fn vector_of(table: &FieldTable, searcher: &Searcher, doc: u32, field: &str) -> Option<Vec<f32>> {
    let f = table.get(field)?;
    if f.kind != Kind::DenseVector {
        return None;
    }
    let (seg, local) = searcher.segment_of(doc)?;
    match dense_vector_value(seg, local, f, None).ok().flatten()? {
        Value::Arr(a) => Some(a.iter().map(|v| v.as_f64().unwrap_or(0.0) as f32).collect()),
        _ => None,
    }
}

/// A numeric doc value as `DocValueFormat` prints it (with a `format` pattern when given).
pub fn format_numeric(f: &FieldInfo, v: i64, format: Option<&str>) -> EsResult<Value> {
    Ok(match f.kind {
        Kind::Date | Kind::DateNanos => {
            let fmt = match format {
                Some(p) => std::sync::Arc::new(match &f.date_format {
                    Some(d) => d.derive(p),
                    None => crate::time::DateFormatter::new(p),
                }
                .map_err(EsError::illegal_argument)?),
                None => f.date_format.clone().ok_or_else(|| EsError::illegal_state("date field without format"))?,
            };
            Value::Str(fmt.format(crate::fetch::instant_of(f.kind == Kind::DateNanos, v), &crate::time::Zone::UTC))
        }
        Kind::Boolean => Value::Bool(v != 0),
        Kind::Double => num_formatted(encode::sortable_f64(v), format),
        // Doc values print as the doubles the field data hands out: a float widened exactly
        // (`-34.869998931884766`, not `-34.87`), a scaled value times the inverse factor
        // (`ScaledFloatLeafFieldData`: `1037 * 0.01` is `10.370000000000001`).
        Kind::Float => num_formatted(encode::sortable_f32(v as i32) as f64, format),
        Kind::HalfFloat => num_formatted(encode::sortable_f16(v as i16) as f64, format),
        Kind::ScaledFloat(sf) => num_formatted(scaled_double(v, sf), format),
        Kind::UnsignedLong => {
            let u = (v as u64) ^ (1 << 63);
            if u <= i64::MAX as u64 { Value::from(u as i64) } else { Value::Num(Number::UInt(u)) }
        }
        _ => match format {
            Some(p) => Value::Str(decimal_format(v as f64, p)),
            None => Value::from(v),
        },
    })
}

/// A scaled float's doc value as a double (`ScaledFloatLeafFieldData.getDoubleValues`: the
/// long times `1 / scaling_factor`, which rounds differently from a division).
pub fn scaled_double(v: i64, scaling_factor: f64) -> f64 {
    v as f64 * (1.0 / scaling_factor)
}

fn num_formatted(d: f64, format: Option<&str>) -> Value {
    match format {
        Some(p) => Value::Str(decimal_format(d, p)),
        None => Value::Num(Number::Float(d)),
    }
}

/// `java.text.DecimalFormat` patterns (through search-aggs' formatter).
pub fn decimal_format(d: f64, pattern: &str) -> String {
    match search_aggs::format::builtin_decimal_format(pattern) {
        Ok(f) => f.format_double(d),
        Err(_) => d.to_string(),
    }
}

/// `docvalue_fields`: values of the requested fields (patterns allowed).
pub fn docvalue_fields(table: &FieldTable, seg: &SegmentContext, local: u32, specs: &[FieldAndFormat], id_fielddata: bool, out: &mut Map) -> EsResult<()> {
    let mut deferred: Vec<FieldAndFormat> = Vec::new();
    for spec in specs {
        if spec.field.contains('*') {
            // Patterns reach metadata fields too (`getMatchingFieldNames`): the first one in the
            // matched set's (hash) order without fielddata fails the request; the others answer
            // after every spec.
            let meta = metadata_matches(table, &spec.field, id_fielddata);
            if let Some((name, _)) = meta.iter().find(|(_, ok)| !ok) {
                return Err(if name == "_id" { super::id_fielddata::disabled() } else { no_fielddata(name) });
            }
            deferred.extend(meta.into_iter().map(|(n, _)| FieldAndFormat { field: n, ..spec.clone() }));
        } else if metadata_field(&spec.field, id_fielddata) == Some(false) {
            return Err(if spec.field == "_id" { super::id_fielddata::disabled() } else { no_fielddata(&spec.field) });
        }
        // Time series metadata: `_ts_routing_hash` as stored, `_tsid` base64url-encoded.
        if spec.field == "_ts_routing_hash" || spec.field == "_tsid" {
            let n = if spec.field == "_tsid" { crate::engine::fields::TSID_FIELD } else { crate::engine::fields::TS_ROUTING_HASH_FIELD };
            if let Some(dv) = seg.reader.sorted(n)
                && let Some(o) = dv.ord(local)
            {
                let mut buf = Vec::new();
                dv.lookup_ord(o, &mut buf);
                let v = if spec.field == "_tsid" { crate::engine::tsdb::tsid_display(&buf) } else { String::from_utf8_lossy(&buf).into_owned() };
                out.insert(spec.field.clone(), Value::Arr(vec![Value::Str(v)]));
            }
            continue;
        }
        match spec.field.as_str() {
            "_seq_no" | "_version" => {
                let n = if spec.field == "_seq_no" { search_index::schema::SEQ_NO_FIELD } else { search_index::schema::VERSION_FIELD };
                if let Some(v) = seg.reader.numeric(n).and_then(|dv| dv.get(local)) {
                    out.insert(spec.field.clone(), Value::Arr(vec![Value::from(v)]));
                }
                continue;
            }
            "_id" => {
                if !id_fielddata {
                    return Err(super::id_fielddata::disabled());
                }
                let mut buf = Vec::new();
                if super::id_fielddata::value(seg, local, &mut buf) {
                    out.insert(spec.field.clone(), Value::Arr(vec![Value::Str(String::from_utf8_lossy(&buf).into_owned())]));
                }
                continue;
            }
            "_routing" | "_source" | "_doc_count" => {
                return Err(EsError::illegal_argument(format!("Fielddata is not supported on field [{0}] of type [{0}]", spec.field)));
            }
            _ => {}
        }
        if spec.field == "_ignored" {
            if let Some(dv) = seg.reader.sorted_set(crate::engine::fields::IGNORED_FIELD) {
                let mut vals = Vec::new();
                let mut buf = Vec::new();
                for o in dv.ords(local) {
                    buf.clear();
                    dv.lookup_ord(o, &mut buf);
                    vals.push(Value::Str(String::from_utf8_lossy(&buf).into_owned()));
                }
                if !vals.is_empty() {
                    out.insert(spec.field.clone(), Value::Arr(vals));
                }
            }
            continue;
        }
        let names = if spec.field.contains('*') { table.matching(&spec.field) } else { vec![spec.field.clone()] };
        for name in names {
            let keyed;
            let f = match table.get(&name) {
                Some(f) => f,
                None => match table.keyed(&name) {
                    Some(k) => {
                        keyed = k;
                        &keyed
                    }
                    None => continue,
                },
            };
            // `FlatObjectFieldType.docValueFormat`: only a dotted sub-field has values.
            if f.ty == "flat_object" && f.flattened_key.is_none() {
                return Err(EsError::illegal_argument(format!("Field [{name}] of type [flat_object] does not support doc_value in root field")));
            }
            if f.kind.is_text() && f.subfield("fielddata").is_none() && !f.doc_values {
                if spec.field.contains('*') {
                    continue;
                }
                return Err(EsError::illegal_argument(format!(
                    "Text fields are not optimised for operations that require per-document field data like aggregations and sorting, so these operations are disabled by default. Please use a keyword field instead. Alternatively, set fielddata=true on [{name}] in order to load field data by uninverting the inverted index. Note that this can use significant memory."
                )));
            }
            if !f.doc_values && !f.kind.is_text() && !matches!(f.kind, Kind::ConstantKeyword | Kind::DenseVector) {
                if spec.field.contains('*') {
                    continue;
                }
                return Err(EsError::illegal_argument(format!("Can't load fielddata on [{name}] because fielddata is unsupported on fields of type [{}]. Use doc values instead.", f.ty)));
            }
            let vals = doc_values(seg, local, f, spec.format.as_deref())?;
            if !vals.is_empty() {
                // OpenSearch's `FetchDocValuesPhase` appends each entry's values to the field's
                // list (one field asked for in several formats lists them all); Elasticsearch
                // keeps the last.
                match out.get_mut(&name) {
                    Some(Value::Arr(prev)) if crate::opensearch::active() => prev.extend(vals),
                    _ => {
                        out.insert(name, Value::Arr(vals));
                    }
                }
            }
        }
    }
    if !deferred.is_empty() {
        docvalue_fields(table, seg, local, &deferred, id_fielddata, out)?;
    }
    Ok(())
}

fn no_fielddata(field: &str) -> EsError {
    EsError::illegal_argument(format!("Fielddata is not supported on field [{field}] of type [{field}]"))
}

/// The dialect's metadata fields (`MapperService` metadata mappers) and whether each loads
/// fielddata (`Some(true)`), refuses it (`Some(false)`), or isn't a field (`None`).
fn metadata_field(name: &str, id_fielddata: bool) -> Option<bool> {
    let os = crate::opensearch::active();
    Some(match name {
        "_id" => id_fielddata,
        "_index" | "_seq_no" => true,
        "_routing" | "_source" | "_doc_count" | "_field_names" | "_data_stream_timestamp" | "_nested_path" | "_feature" => false,
        // Elasticsearch's `_ignored` has doc values; OpenSearch's only an index.
        "_ignored" => !os,
        "_version" => !os,
        "_ignored_source" | "_tier" if !os => false,
        "_index_mode" if !os => true,
        _ => return None,
    })
}

const METADATA_FIELDS: &[&str] = &[
    "_data_stream_timestamp",
    "_doc_count",
    "_feature",
    "_field_names",
    "_id",
    "_ignored",
    "_ignored_source",
    "_index",
    "_index_mode",
    "_nested_path",
    "_routing",
    "_seq_no",
    "_source",
    "_tier",
    "_version",
];

/// The metadata fields a pattern matches, in the order a Java `HashSet` of every matched name
/// (mapped ones included) iterates them, with [`metadata_field`]'s verdict. Names sharing a
/// bucket keep the order of the (larger) map they were collected from.
fn metadata_matches(table: &FieldTable, pattern: &str, id_fielddata: bool) -> Vec<(String, bool)> {
    let glob = |n: &str| crate::xcontent::filter::glob_match(pattern, n);
    let meta: Vec<(&str, bool)> = METADATA_FIELDS.iter().filter(|n| glob(n)).filter_map(|n| metadata_field(n, id_fielddata).map(|ok| (*n, ok))).collect();
    if meta.is_empty() {
        return Vec::new();
    }
    let size = meta.len() + table.matching(pattern).len();
    let mut cap = 16usize;
    while size > cap / 4 * 3 {
        cap *= 2;
    }
    let spread = |n: &str| {
        let h = n.encode_utf16().fold(0u32, |h, c| h.wrapping_mul(31).wrapping_add(u32::from(c)));
        h ^ (h >> 16)
    };
    let mut ordered = meta;
    ordered.sort_by_key(|(n, _)| {
        let h = spread(n) as usize;
        (h & (cap - 1), h & (cap * 2 - 1), *n)
    });
    ordered.into_iter().map(|(n, ok)| (n.to_string(), ok)).collect()
}

/// The options of the fetch phase.
#[derive(Clone, Debug, Default)]
pub struct FetchOpts {
    pub source: Option<FetchSource>,
    pub stored_fields: Option<Option<Vec<String>>>,
    pub docvalue_fields: Vec<FieldAndFormat>,
    pub fields: Vec<FieldAndFormat>,
    pub version: bool,
    pub seq_no_primary_term: bool,
    /// `force_synthetic_source`.
    pub force_synthetic: bool,
    /// `_id` fielddata may load ([`super::id_fielddata::enabled`]): `docvalue_fields: [_id]`.
    pub id_fielddata: bool,
    /// Script fields leave `_source` out unless `_source` or `stored_fields: [_source]` asks
    /// for it (`SearchService.parseSource`: script fields without a `FetchSourceContext`
    /// fetch no source; Elasticsearch and OpenSearch alike).
    pub script_fields_only: bool,
    /// A stored `_source` may go into the hit as its bytes (`json::raw`) when nothing reads or
    /// changes it: the REST `_search` and scroll pages.
    pub raw_source: bool,
    /// `_id` shows even under `stored_fields: _none_`: Elasticsearch's script fields and
    /// highlight phases load it (OpenSearch's don't).
    pub keep_id: bool,
}

/// Whether `_source` is returned under these options.
pub fn wants_source(o: &FetchOpts) -> FetchSource {
    match (&o.source, &o.stored_fields) {
        (Some(fs), _) => fs.clone(),
        (None, Some(Some(sf))) => FetchSource { fetch: sf.iter().any(|f| f == "_source"), ..Default::default() },
        (None, Some(None)) => FetchSource { fetch: false, ..Default::default() },
        (None, None) if o.script_fields_only => FetchSource { fetch: false, ..Default::default() },
        (None, None) => FetchSource::default(),
    }
}

/// Renders one hit's document part (from `_version` to `fields`) into `m`.
#[allow(clippy::too_many_arguments)]
pub fn render_doc(table: &FieldTable, idx: Option<&crate::meta::IndexMeta>, searcher: &Searcher, doc: u32, o: &FetchOpts, m: &mut Map, score: Option<f32>, ctx_now: i64) -> EsResult<Option<Map>> {
    if searcher.segment_of(doc).is_none() {
        return Ok(None);
    }
    let stored = searcher.stored(doc).map_err(crate::engine::convert::index_error)?;
    render_doc_stored(table, idx, searcher, doc, &stored, o, m, score, ctx_now)
}

/// [`render_doc`] with the hit's stored fields already loaded (`searcher.stored(doc)`): the
/// fetch phase reads them once per hit, for `_id` and the rest alike.
#[allow(clippy::too_many_arguments)]
pub fn render_doc_stored(
    table: &FieldTable,
    idx: Option<&crate::meta::IndexMeta>,
    searcher: &Searcher,
    doc: u32,
    stored: &[(u32, search_codec::stored::OwnedStoredValue)],
    o: &FetchOpts,
    m: &mut Map,
    score: Option<f32>,
    ctx_now: i64,
) -> EsResult<Option<Map>> {
    let _ = ctx_now;
    let Some((seg, local)) = searcher.segment_of(doc) else { return Ok(None) };
    if o.version {
        m.insert("_version".into(), Value::from(meta_dv(seg, local, search_index::schema::VERSION_FIELD)));
    }
    if o.seq_no_primary_term {
        if idx.is_some_and(crate::write::seq_no_disabled) {
            m.insert("_seq_no".into(), Value::from(-2i64));
            m.insert("_primary_term".into(), Value::from(0i64));
        } else {
            m.insert("_seq_no".into(), Value::from(meta_dv(seg, local, search_index::schema::SEQ_NO_FIELD)));
            m.insert("_primary_term".into(), Value::from(meta_dv(seg, local, search_index::schema::PRIMARY_TERM_FIELD)));
        }
    }
    match score {
        Some(s) if !s.is_nan() => m.insert("_score".into(), Value::Num(Number::Float(crate::fetch::java_float_as_double(s)))),
        _ => m.insert("_score".into(), Value::Null),
    };
    if o.stored_fields != Some(None) {
        if let Some(r) = crate::fetch::routing(stored) {
            m.insert("_routing".into(), Value::Str(r));
        }
        let ig = crate::fetch::ignored(stored);
        if !ig.is_empty() {
            m.insert("_ignored".into(), Value::Arr(ig.into_iter().map(Value::Str).collect()));
        }
    }
    let fs = wants_source(o);
    // The stored bytes as they are, when nothing reads or changes them (`SourceView` would
    // return them unchanged): no filtering, `fields`, vectors or forced synthetic source.
    let raw_source = if o.raw_source
        && fs.fetch
        && o.fields.is_empty()
        && fs.filter().is_none()
        && !table.has_vectors
        && let Some(idx) = idx
        && !(o.force_synthetic && crate::engine::synthetic::source_mode(idx) == crate::engine::synthetic::SourceMode::Stored)
        && let Some(bytes) = crate::fetch::source_bytes(stored)
        && let Some(end) = crate::json::raw::single_object_end(bytes)
        && let Ok(text) = std::str::from_utf8(&bytes[..end])
    {
        Some(crate::json::raw::raw(text))
    } else {
        None
    };
    let mut source = if raw_source.is_none() && (fs.fetch || !o.fields.is_empty()) { crate::fetch::source_bytes(stored).map(crate::fetch::parse_source).transpose()? } else { None };
    // Vectors kept out of `_source` read back as the index holds them (floats).
    if let (Some(idx), Some(Value::Obj(sm))) = (idx, source.as_mut())
        && table.has_vectors
        && idx.settings.get("index.mapping.exclude_source_vectors") != Some("false")
    {
        crate::engine::synthetic::vectors(table, "", sm, false);
    }
    if let Some(raw) = raw_source {
        m.insert("_source".into(), raw);
    }
    if fs.fetch && source.is_some() {
        // Only `fields` reads the source again; otherwise it moves into the hit uncopied.
        let owned = |source: &mut Option<Value>| if o.fields.is_empty() { source.take().unwrap_or_default() } else { source.clone().unwrap_or_default() };
        let v = match (idx, fs.filter(), &source) {
            (Some(idx), _, _) => {
                // `fields` naming a mapped field reads the source as a map first.
                let parsed = (!fs.includes.is_empty() || !fs.excludes.is_empty()) && o.fields.iter().any(|f| !f.field.starts_with('_') && !table.matching(&f.field).is_empty());
                crate::fetch::SourceView { idx, table, force_synthetic: o.force_synthetic, search: true }.apply_parsed(owned(&mut source), &fs, parsed)
            }
            (None, Some(f), Some(Value::Obj(sm))) => Value::Obj(f.apply(sm)),
            (None, _, _) => owned(&mut source),
        };
        m.insert("_source".into(), v);
    }
    let mut fields = Map::new();
    if let Some(Some(sf)) = &o.stored_fields {
        for (k, v) in crate::fetch::stored_fields(table, stored, sf) {
            fields.insert(k, v);
        }
    }
    docvalue_fields(table, seg, local, &o.docvalue_fields, o.id_fielddata, &mut fields)?;
    // `_index` (and Elasticsearch's `_index_mode`) answer the index's constant.
    for spec in &o.docvalue_fields {
        let hit = |n: &str| if spec.field.contains('*') { crate::xcontent::filter::glob_match(&spec.field, n) } else { spec.field == n };
        if hit("_index")
            && let Some(v) = m.get("_index")
        {
            fields.insert("_index".into(), Value::Arr(vec![v.clone()]));
        }
        if hit("_index_mode") && !crate::opensearch::active() {
            let mode = idx.and_then(|i| i.settings.get("index.mode")).unwrap_or("standard");
            fields.insert("_index_mode".into(), Value::Arr(vec![Value::Str(mode.to_string())]));
        }
    }
    if !o.fields.is_empty() {
        let mut ignored = Map::new();
        let mut fetched = Map::new();
        for (name, meta) in super::fields::metadata_requests(table, &o.fields) {
            let v: Option<Value> = match meta.as_str() {
                "_id" | "_index" => m.get(meta.as_str()).cloned(),
                "_version" => Some(Value::from(meta_dv(seg, local, search_index::schema::VERSION_FIELD))),
                "_ignored" => Some(Value::Arr(crate::fetch::ignored(stored).into_iter().map(Value::Str).collect())),
                // Only documents that set `_doc_count` have one.
                "_doc_count" => seg.reader.numeric(crate::engine::fields::DOC_COUNT_FIELD).and_then(|dv| dv.get(local)).map(|c| Value::Arr(vec![Value::from(c)])),
                "_tier" => None,
                _ => {
                    let mut tmp = Map::new();
                    docvalue_fields(table, seg, local, &[FieldAndFormat { field: meta.clone(), format: None, include_unmapped: None }], o.id_fielddata, &mut tmp)?;
                    tmp.shift_remove(meta.as_str())
                }
            };
            match v {
                Some(Value::Arr(a)) if !a.is_empty() => {
                    fetched.insert(name, Value::Arr(a));
                }
                Some(Value::Arr(_)) | None => {}
                Some(other) => {
                    fetched.insert(name, Value::Arr(vec![other]));
                }
            }
        }
        super::fields::fetch_fields(table, source.as_ref(), &o.fields, &mut fetched, &mut ignored, seg, local)?;
        // The fetched fields come in field-name order into the hit's `fields` hash map.
        fetched.sort_keys();
        fields.extend(fetched);
        crate::ingest::doc::java_hash_order(&mut fields, false);
        if !fields.is_empty() {
            m.insert("fields".into(), Value::Obj(std::mem::take(&mut fields)));
        }
        if !ignored.is_empty() {
            m.insert("ignored_field_values".into(), Value::Obj(crate::json::java_hash_map(ignored, 16)));
        }
        return Ok(None);
    }
    if !fields.is_empty() {
        crate::ingest::doc::java_hash_order(&mut fields, false);
        m.insert("fields".into(), Value::Obj(fields));
    }
    Ok(None)
}
