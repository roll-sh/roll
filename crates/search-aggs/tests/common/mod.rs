//! Test harness: builds search-index shards from a mapping and JSON documents the way
//! `search-es` will (doc values per Elasticsearch type, nested objects as child docs), a
//! [`FieldLookup`] over them, and a tiny Query DSL subset for filter aggregations.
#![allow(dead_code)]

use std::alloc::{GlobalAlloc, Layout, System};
use std::sync::Arc;

use std::collections::HashMap;
use std::sync::Mutex;

use search_aggs::shape_field::{Coords, Geometry};
use search_aggs::field::{AggField, FetchHit, FieldLookup, HitSort, NestedLookup, NestedPath, NumericType, SortMissing, SortMode, SourceType, TopHitsFetcher};
use search_aggs::format::parse_ip;
use search_aggs::range_field::{self, RangeType};
use search_aggs::{AggError, Json};
use search_codec::DocValuesType;
use search_codec::points::encode;
use search_index::query::boolean::{BooleanQuery, Occur};
use search_index::query::range::{DocValuesRangeQuery, DocValuesTermsQuery, DvRange};
use search_index::query::basic::{FieldExistsQuery, MatchAllQuery};
use search_index::schema::{FIRST_USER_FIELD, FieldSchema};
use search_index::{DocValue, IndexOp, IndexableDoc, IndexableField, QueryRef, Schema, Searcher, Shard, ShardConfig};

/// Fails any single allocation over 1 GiB (a buffer sized from untrusted counts).
struct Capped;

const MAX_ALLOC: usize = 1 << 30;

unsafe impl GlobalAlloc for Capped {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        if layout.size() > MAX_ALLOC { std::ptr::null_mut() } else { unsafe { System.alloc(layout) } }
    }
    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        if layout.size() > MAX_ALLOC { std::ptr::null_mut() } else { unsafe { System.alloc_zeroed(layout) } }
    }
    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        if new_size > MAX_ALLOC { std::ptr::null_mut() } else { unsafe { System.realloc(ptr, layout, new_size) } }
    }
    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        unsafe { System.dealloc(ptr, layout) }
    }
}

#[global_allocator]
static ALLOC: Capped = Capped;

/// One mapped field of the test index.
#[derive(Clone, Debug)]
pub struct MappedField {
    pub path: String,
    pub type_name: String,
    pub number: u32,
    pub nested: Option<String>,
    /// `time_series_dimension: true`.
    pub dimension: bool,
    /// `time_series_metric: counter`.
    pub counter: bool,
}

/// A `join` field: its relations, parent type -> child types.
#[derive(Clone, Debug)]
pub struct JoinDef {
    pub field: String,
    pub relations: Vec<(String, Vec<String>)>,
}

/// An index built from a mapping: schema, field table, nested paths.
pub struct TestIndex {
    pub fields: Vec<MappedField>,
    pub joins: Vec<JoinDef>,
    pub nested_paths: Vec<String>,
    pub nested_field: u32,
    /// A sorted-set field holding each root doc's `_id` (for the test fetch phase).
    pub id_field: u32,
    pub schema: Schema,
    /// `_index` of fetched hits.
    pub name: String,
    /// Sources by `_id` of every indexed doc.
    pub sources: Mutex<HashMap<String, Json>>,
}

fn dv_type(t: &str) -> DocValuesType {
    match t {
        "keyword" | "ip" | "join" | "join_parent" => DocValuesType::SortedSet,
        t if t.ends_with("_range") => DocValuesType::Binary,
        "histogram" | "exponential_histogram" | "geo_shape" | "shape" | "shape_centroid" => DocValuesType::Binary,
        _ => DocValuesType::SortedNumeric,
    }
}

impl TestIndex {
    pub fn from_mapping(mapping: &Json) -> TestIndex {
        let mut fields = Vec::new();
        let mut nested_paths = Vec::new();
        let mut next = FIRST_USER_FIELD;
        let mut joins = Vec::new();
        fn walk(
            props: &Json,
            prefix: &str,
            nested: Option<String>,
            fields: &mut Vec<MappedField>,
            nested_paths: &mut Vec<String>,
            joins: &mut Vec<JoinDef>,
            next: &mut u32,
        ) {
            for (name, def) in props.as_object().unwrap_or(&[]) {
                let path = if prefix.is_empty() { name.clone() } else { format!("{prefix}.{name}") };
                let t = def.get("type").and_then(Json::as_str).unwrap_or("object");
                if t == "nested" || t == "object" {
                    let n = if t == "nested" {
                        nested_paths.push(path.clone());
                        Some(path.clone())
                    } else {
                        nested.clone()
                    };
                    if let Some(p) = def.get("properties") {
                        walk(p, &path, n, fields, nested_paths, joins, next);
                    }
                    continue;
                }
                if t == "aggregate_metric_double" {
                    // The field itself (for lookups) and one doc-values sub-field per metric.
                    fields.push(MappedField { path: path.clone(), type_name: t.to_string(), number: *next, nested: nested.clone(), dimension: false, counter: false });
                    *next += 1;
                    for m in def.get("metrics").and_then(Json::as_array).unwrap_or(&[]) {
                        let m = m.as_str().unwrap();
                        let ty = if m == "value_count" { "aggregate_metric_count" } else { "aggregate_metric_double_part" };
                        fields.push(MappedField { path: format!("{path}.{m}"), type_name: ty.into(), number: *next, nested: nested.clone(), dimension: false, counter: false });
                        *next += 1;
                    }
                    continue;
                }
                if t == "geo_shape" || t == "shape" {
                    // The triangles and a companion field with the centroid.
                    fields.push(MappedField { path: path.clone(), type_name: t.to_string(), number: *next, nested: nested.clone(), dimension: false, counter: false });
                    fields.push(MappedField { path: format!("{path}#centroid"), type_name: "shape_centroid".into(), number: *next + 1, nested: nested.clone(), dimension: false, counter: false });
                    *next += 2;
                    continue;
                }
                if t == "join" {
                    // The join field holds the relation name; `<join>#<parent>` the parent's id.
                    let mut relations = Vec::new();
                    for (parent, children) in def.get("relations").and_then(Json::as_object).unwrap_or(&[]) {
                        let children: Vec<String> = match children {
                            Json::Arr(a) => a.iter().filter_map(Json::as_str).map(str::to_string).collect(),
                            other => vec![other.as_str().unwrap_or("").to_string()],
                        };
                        relations.push((parent.clone(), children));
                    }
                    fields.push(MappedField { path: path.clone(), type_name: "join".into(), number: *next, nested: nested.clone(), dimension: false, counter: false });
                    *next += 1;
                    for (parent, _) in &relations {
                        fields.push(MappedField { path: format!("{path}#{parent}"), type_name: "join_parent".into(), number: *next, nested: nested.clone(), dimension: false, counter: false });
                        *next += 1;
                    }
                    joins.push(JoinDef { field: path, relations });
                    continue;
                }
                let dimension = def.get("time_series_dimension").and_then(Json::as_bool).unwrap_or(false);
                let counter = def.get("time_series_metric").and_then(Json::as_str) == Some("counter");
                fields.push(MappedField { path, type_name: t.to_string(), number: *next, nested: nested.clone(), dimension, counter });
                *next += 1;
            }
        }
        walk(mapping.get("properties").unwrap_or(&Json::obj()), "", None, &mut fields, &mut nested_paths, &mut joins, &mut next);
        let nested_field = next;
        let mut schema = Schema::new();
        for f in &fields {
            schema = schema.with_field(FieldSchema::new(f.number, f.path.clone()).with_doc_values(dv_type(&f.type_name))).unwrap();
        }
        schema = schema.with_field(FieldSchema::new(nested_field, "_nested_path").with_doc_values(DocValuesType::SortedSet)).unwrap();
        let id_field = nested_field + 1;
        schema = schema.with_field(FieldSchema::new(id_field, "_test_id").with_doc_values(DocValuesType::SortedSet)).unwrap();
        TestIndex { fields, joins, nested_paths, nested_field, id_field, schema, name: "test".into(), sources: Mutex::new(HashMap::new()) }
    }

    pub fn field(&self, path: &str) -> Option<&MappedField> {
        self.fields.iter().find(|f| f.path == path)
    }

    fn values_of(&self, f: &MappedField, v: &Json, out: &mut Vec<DocValue>) {
        let items: Vec<&Json> = match v {
            Json::Arr(a) if !matches!(f.type_name.as_str(), "geo_point" | "point") || a.first().is_some_and(|x| !x.is_number()) => a.iter().collect(),
            other => vec![other],
        };
        for it in items {
            if it.is_null() {
                continue;
            }
            let dv = match f.type_name.as_str() {
                "keyword" => DocValue::SortedSet(it.as_str().map(|s| s.to_string()).unwrap_or_else(|| it.to_java_string()).into_bytes().into()),
                "join" => {
                    let name = if let Json::Obj(_) = it { it.get("name").and_then(Json::as_str).unwrap_or("") } else { it.as_str().unwrap_or("") };
                    DocValue::SortedSet(name.as_bytes().to_vec().into())
                }
                "join_parent" => continue,
                "ip" => DocValue::SortedSet(parse_ip(it.as_str().unwrap()).unwrap().to_vec().into()),
                "long" | "integer" | "short" | "byte" => DocValue::SortedNumeric(it.as_f64().unwrap() as i64),
                "double" => DocValue::SortedNumeric(encode::f64_sortable(it.as_f64().unwrap())),
                "float" => DocValue::SortedNumeric(encode::f32_sortable(it.as_f64().unwrap() as f32) as i64),
                "half_float" => DocValue::SortedNumeric(encode::f16_sortable(it.as_f64().unwrap() as f32) as i64),
                "boolean" => DocValue::SortedNumeric(if it.as_bool().unwrap_or(false) { 1 } else { 0 }),
                "date" => DocValue::SortedNumeric(it.as_f64().unwrap() as i64),
                "text" => continue,
                "geo_point" => {
                    let (lat, lon) = match it {
                        Json::Obj(_) => (it.get("lat").unwrap().as_f64().unwrap(), it.get("lon").unwrap().as_f64().unwrap()),
                        Json::Arr(a) => (a[1].as_f64().unwrap(), a[0].as_f64().unwrap()),
                        _ => panic!("geo point"),
                    };
                    DocValue::SortedNumeric(search_index::util::geo::encode_geo_dv(lat, lon))
                }
                "point" => {
                    let (x, y) = match it {
                        Json::Obj(_) => (it.get("x").unwrap().as_f64().unwrap(), it.get("y").unwrap().as_f64().unwrap()),
                        Json::Arr(a) => (a[0].as_f64().unwrap(), a[1].as_f64().unwrap()),
                        Json::Str(s) => search_aggs::metrics::cartesian::parse_point_text(s).unwrap(),
                        _ => panic!("point"),
                    };
                    DocValue::SortedNumeric(search_aggs::metrics::cartesian::encode_xy(x, y))
                }
                t => panic!("unsupported type {t}"),
            };
            out.push(dv);
        }
    }

    /// The engine doc (with nested children) of a JSON source.
    pub fn doc(&self, id: &str, src: &Json) -> IndexableDoc {
        let mut doc = IndexableDoc::new(id.as_bytes().to_vec());
        doc.fields.push(IndexableField::DocValue { field: self.id_field, value: DocValue::SortedSet(id.as_bytes().to_vec().into()) });
        self.fill(&mut doc, src, "");
        for j in &self.joins {
            let Some(v) = src.get(&j.field) else { continue };
            let (name, parent) = match v {
                Json::Obj(_) => (v.get("name").and_then(Json::as_str).unwrap_or(""), v.get("parent").map(|p| p.as_str().map(str::to_string).unwrap_or_else(|| p.to_java_string()))),
                other => (other.as_str().unwrap_or(""), None),
            };
            for (p, children) in &j.relations {
                let field = self.field(&format!("{}#{p}", j.field)).unwrap().number;
                let value = if p == name {
                    Some(id.to_string())
                } else if children.iter().any(|c| c == name) {
                    parent.clone()
                } else {
                    None
                };
                if let Some(value) = value {
                    doc.fields.push(IndexableField::DocValue { field, value: DocValue::SortedSet(value.into_bytes().into()) });
                }
            }
        }
        self.sources.lock().unwrap().insert(id.to_string(), src.clone());
        doc
    }

    fn fill(&self, doc: &mut IndexableDoc, obj: &Json, prefix: &str) {
        for (k, v) in obj.as_object().unwrap_or(&[]) {
            let path = if prefix.is_empty() { k.clone() } else { format!("{prefix}.{k}") };
            if self.nested_paths.contains(&path) {
                let items: Vec<&Json> = match v {
                    Json::Arr(a) => a.iter().collect(),
                    o => vec![o],
                };
                for it in items {
                    let mut child = IndexableDoc::new(Vec::<u8>::new());
                    child.fields.push(IndexableField::DocValue { field: self.nested_field, value: DocValue::SortedSet(path.clone().into_bytes().into()) });
                    self.fill(&mut child, it, &path);
                    doc.nested.push(child);
                }
                continue;
            }
            if let Some(f) = self.field(&path)
                && f.type_name == "histogram"
            {
                let values: Vec<f64> = v.get("values").unwrap().as_array().unwrap().iter().map(|x| x.as_f64().unwrap()).collect();
                let counts: Vec<u64> = v.get("counts").unwrap().as_array().unwrap().iter().map(|x| x.as_i64().unwrap() as u64).collect();
                let enc = search_aggs::histogram_field::encode_histogram(&values, &counts);
                doc.fields.push(IndexableField::DocValue { field: f.number, value: DocValue::Binary(enc.into()) });
            } else if let Some(f) = self.field(&path)
                && (f.type_name == "geo_shape" || f.type_name == "shape")
            {
                let coords = if f.type_name == "geo_shape" { Coords::Geo } else { Coords::Cartesian };
                let values: Vec<&Json> = match v {
                    Json::Arr(a) => a.iter().collect(),
                    o => vec![o],
                };
                let mut triangles = Vec::new();
                let mut calc = search_aggs::shape_field::CentroidCalculator::new();
                for value in values {
                    let g = parse_shape(value);
                    triangles.extend(search_aggs::shape_field::shape_triangles(&g, coords).unwrap());
                    calc.add(&g).unwrap();
                }
                if !triangles.is_empty() {
                    let dv = search_index::query::shape::doc_value(&triangles);
                    doc.fields.push(IndexableField::DocValue { field: f.number, value: DocValue::Binary(dv) });
                    let c = search_aggs::shape_field::encode_centroid(&calc, coords).unwrap();
                    let cf = self.field(&format!("{path}#centroid")).unwrap();
                    doc.fields.push(IndexableField::DocValue { field: cf.number, value: DocValue::Binary(c.to_vec().into()) });
                }
            } else if let Some(f) = self.field(&path)
                && f.type_name == "exponential_histogram"
            {
                doc.fields.push(IndexableField::DocValue { field: f.number, value: DocValue::Binary(encode_exp_histogram(v).into()) });
            } else if let Some(f) = self.field(&path)
                && f.type_name == "aggregate_metric_double"
            {
                for (m, x) in v.as_object().unwrap() {
                    let Some(sf) = self.field(&format!("{path}.{m}")) else { continue };
                    let d = x.as_f64().unwrap();
                    let raw = if m == "value_count" { d as i64 } else { encode::f64_sortable(d) };
                    doc.fields.push(IndexableField::DocValue { field: sf.number, value: DocValue::SortedNumeric(raw) });
                }
            } else if let Some(f) = self.field(&path)
                && f.type_name.ends_with("_range")
            {
                if let Some(v) = encode_range_value(&f.type_name, v) {
                    doc.fields.push(IndexableField::DocValue { field: f.number, value: DocValue::Binary(v.into()) });
                }
            } else if let Some(f) = self.field(&path) {
                let mut dvs = Vec::new();
                self.values_of(f, v, &mut dvs);
                for dv in dvs {
                    doc.fields.push(IndexableField::DocValue { field: f.number, value: dv });
                }
            } else if matches!(v, Json::Obj(_)) {
                self.fill(doc, v, &path);
            }
        }
    }

    /// One shard holding `docs` in order (one refresh: one segment, insertion order kept);
    /// with `segments > 1` the docs are spread over that many refreshes.
    pub fn shard(&self, docs: &[(String, Json)], segments: usize) -> Shard {
        let shard = Shard::open(ShardConfig { merge_threads: 0, ..Default::default() }, self.schema.clone(), Vec::new()).unwrap();
        let per = docs.len().div_ceil(segments.max(1)).max(1);
        for (i, (id, src)) in docs.iter().enumerate() {
            shard.index(IndexOp::new(self.doc(id, src))).unwrap();
            if (i + 1) % per == 0 {
                shard.refresh().unwrap();
            }
        }
        shard.refresh().unwrap();
        shard
    }
}

/// A range field's doc value as `RangeFieldMapper` builds it: each range's bounds parsed
/// (numbers; ip strings or CIDRs), exclusive bounds stepped inward, missing bounds the type's
/// extremes, duplicates (same bounds and inclusiveness) dropped, then `BinaryRangeUtil`'s
/// encoding.
/// A GeoJSON or WKT shape value.
fn parse_shape(v: &Json) -> Geometry {
    if let Some(s) = v.as_str() {
        return search_aggs::shape_field::parse_wkt(s).unwrap();
    }
    let pt = |p: &Json| (p.as_array().unwrap()[0].as_f64().unwrap(), p.as_array().unwrap()[1].as_f64().unwrap());
    let line = |l: &Json| l.as_array().unwrap().iter().map(pt).collect::<Vec<_>>();
    let poly = |p: &Json| {
        let rings: Vec<Vec<(f64, f64)>> = p.as_array().unwrap().iter().map(line).collect();
        (rings[0].clone(), rings[1..].to_vec())
    };
    let c = v.get("coordinates");
    match v.get("type").and_then(Json::as_str).unwrap().to_lowercase().as_str() {
        "point" => {
            let (x, y) = pt(c.unwrap());
            Geometry::Point(x, y)
        }
        "multipoint" => Geometry::MultiPoint(line(c.unwrap())),
        "linestring" => Geometry::Line(line(c.unwrap())),
        "multilinestring" => Geometry::MultiLine(c.unwrap().as_array().unwrap().iter().map(line).collect()),
        "polygon" => {
            let (shell, holes) = poly(c.unwrap());
            Geometry::Polygon(shell, holes)
        }
        "multipolygon" => Geometry::MultiPolygon(c.unwrap().as_array().unwrap().iter().map(poly).collect()),
        "envelope" => {
            let a = line(c.unwrap());
            Geometry::Rectangle { min_x: a[0].0, max_x: a[1].0, min_y: a[1].1, max_y: a[0].1 }
        }
        "geometrycollection" => Geometry::Collection(v.get("geometries").unwrap().as_array().unwrap().iter().map(parse_shape).collect()),
        t => panic!("shape type {t}"),
    }
}

/// An `exponential_histogram` source value (`scale`, `sum`, `min`, `max`, `zero`, `positive`,
/// `negative`) as `search-es` indexes it.
fn encode_exp_histogram(v: &Json) -> Vec<u8> {
    let buckets = |side: &str| -> Vec<(i64, u64)> {
        let Some(o) = v.get(side) else { return Vec::new() };
        let idx = o.get("indices").and_then(Json::as_array).map(|a| a.to_vec()).unwrap_or_default();
        let cnt = o.get("counts").and_then(Json::as_array).map(|a| a.to_vec()).unwrap_or_default();
        idx.iter().zip(&cnt).map(|(i, c)| (i.as_i64().unwrap(), c.as_i64().unwrap() as u64)).collect()
    };
    let zero = v.get("zero");
    search_aggs::exp_histogram::encode(&search_aggs::exp_histogram::ExpHistogramValue {
        scale: v.get("scale").and_then(Json::as_i64).unwrap() as i32,
        zero_threshold: zero.and_then(|z| z.get("threshold")).and_then(Json::as_f64).unwrap_or(0.0),
        zero_count: zero.and_then(|z| z.get("count")).and_then(Json::as_i64).unwrap_or(0) as u64,
        negative: buckets("negative"),
        positive: buckets("positive"),
        sum: v.get("sum").and_then(Json::as_f64),
        min: v.get("min").and_then(Json::as_f64),
        max: v.get("max").and_then(Json::as_f64),
    })
}

fn encode_range_value(type_name: &str, v: &Json) -> Option<Vec<u8>> {
    let items: Vec<&Json> = match v {
        Json::Arr(a) => a.iter().filter(|x| !x.is_null()).collect(),
        Json::Null => return None,
        o => vec![o],
    };
    if items.is_empty() {
        return None;
    }
    // (from, to, include_from, include_to) as f64 bits / longs, for dedup.
    let mut seen: Vec<(u64, u64, bool, bool)> = Vec::new();
    let mut longs = Vec::new();
    let mut doubles = Vec::new();
    let mut floats = Vec::new();
    let mut ips = Vec::new();
    for it in items {
        if type_name == "ip_range" {
            let (from, to) = match it {
                Json::Str(cidr) => {
                    let (addr, bits) = cidr.split_once('/').unwrap();
                    let a = parse_ip(addr).unwrap();
                    let bits: u32 = bits.parse().unwrap();
                    let bits = if addr.contains(':') { bits } else { bits + 96 };
                    let mut lo = a;
                    let mut hi = a;
                    for i in bits..128 {
                        lo[(i / 8) as usize] &= !(0x80 >> (i % 8));
                        hi[(i / 8) as usize] |= 0x80 >> (i % 8);
                    }
                    (lo, hi)
                }
                o => {
                    let from = o.get("gte").map(|x| parse_ip(x.as_str().unwrap()).unwrap()).unwrap_or([0; 16]);
                    let to = o.get("lte").map(|x| parse_ip(x.as_str().unwrap()).unwrap()).unwrap_or([0xff; 16]);
                    (from, to)
                }
            };
            ips.push((from, to));
            continue;
        }
        let mut from = None;
        let mut to = None;
        let (mut inc_from, mut inc_to) = (true, true);
        for (k, x) in it.as_object().unwrap() {
            let d = x.as_f64().unwrap();
            match k.as_str() {
                "gte" => from = Some(d),
                "gt" => {
                    from = Some(d);
                    inc_from = false;
                }
                "lte" => to = Some(d),
                "lt" => {
                    to = Some(d);
                    inc_to = false;
                }
                _ => {}
            }
        }
        let key = match type_name {
            "long_range" | "date_range" | "integer_range" => {
                let (min, max) = if type_name == "integer_range" { (i32::MIN as i64, i32::MAX as i64) } else { (i64::MIN, i64::MAX) };
                let f = from.map_or(min, |d| d as i64 + i64::from(!inc_from));
                let t = to.map_or(max, |d| d as i64 - i64::from(!inc_to));
                longs.push((f, t));
                (f as u64, t as u64, inc_from, inc_to)
            }
            "double_range" => {
                let f = from.map_or(f64::NEG_INFINITY, |d| if inc_from { d } else { d.next_up() });
                let t = to.map_or(f64::INFINITY, |d| if inc_to { d } else { d.next_down() });
                doubles.push((f, t));
                (f.to_bits(), t.to_bits(), inc_from, inc_to)
            }
            "float_range" => {
                let f = from.map_or(f32::NEG_INFINITY, |d| if inc_from { d as f32 } else { (d as f32).next_up() });
                let t = to.map_or(f32::INFINITY, |d| if inc_to { d as f32 } else { (d as f32).next_down() });
                floats.push((f, t));
                (f.to_bits() as u64, t.to_bits() as u64, inc_from, inc_to)
            }
            t => panic!("range type {t}"),
        };
        if seen.contains(&key) {
            longs.truncate(longs.len().min(seen.len()));
            doubles.truncate(doubles.len().min(seen.len()));
            floats.truncate(floats.len().min(seen.len()));
        } else {
            seen.push(key);
        }
    }
    Some(match type_name {
        "ip_range" => range_field::encode_ip_ranges(&ips),
        "double_range" => range_field::encode_double_ranges(&doubles),
        "float_range" => range_field::encode_float_ranges(&floats),
        _ => range_field::encode_long_ranges(&longs),
    })
}

/// The [`FieldLookup`] of a [`TestIndex`].
pub struct Lookup<'a> {
    pub index: &'a TestIndex,
    pub now: i64,
    /// A script engine (tests that exercise script hooks bring their own).
    pub scripts: Option<Arc<dyn search_aggs::ScriptHost>>,
}

impl Lookup<'_> {
    fn agg_field(&self, f: &MappedField) -> AggField {
        let source = match f.type_name.as_str() {
            "keyword" | "join" | "join_parent" => SourceType::Keyword,
            "ip" => SourceType::Ip,
            "long" => SourceType::Numeric(NumericType::Long),
            "integer" => SourceType::Numeric(NumericType::Integer),
            "short" => SourceType::Numeric(NumericType::Short),
            "byte" => SourceType::Numeric(NumericType::Byte),
            "double" => SourceType::Numeric(NumericType::Double),
            "float" => SourceType::Numeric(NumericType::Float),
            "half_float" => SourceType::Numeric(NumericType::HalfFloat),
            "boolean" => SourceType::Boolean,
            "date" => SourceType::Date { nanos: false },
            "geo_point" => SourceType::GeoPoint,
            "point" => SourceType::CartesianPoint,
            "histogram" => SourceType::Histogram,
            "exponential_histogram" => SourceType::ExponentialHistogram,
            "geo_shape" => SourceType::GeoShape { centroid: self.index.field(&format!("{}#centroid", f.path)).map(|f| f.number) },
            "shape" => SourceType::CartesianShape { centroid: self.index.field(&format!("{}#centroid", f.path)).map(|f| f.number) },
            "aggregate_metric_double" => {
                let sub = |m: &str| self.index.field(&format!("{}.{m}", f.path)).map(|f| f.number);
                SourceType::AggregateMetric(search_aggs::histogram_field::AggregateMetricFields {
                    min: sub("min"),
                    max: sub("max"),
                    sum: sub("sum"),
                    value_count: sub("value_count"),
                })
            }
            "integer_range" => SourceType::Range(RangeType::Integer),
            "long_range" => SourceType::Range(RangeType::Long),
            "float_range" => SourceType::Range(RangeType::Float),
            "double_range" => SourceType::Range(RangeType::Double),
            "date_range" => SourceType::Range(RangeType::Date),
            "ip_range" => SourceType::Range(RangeType::Ip),
            _ => SourceType::None,
        };
        let mut a = AggField::new(f.path.clone(), f.type_name.clone(), source, f.number);
        a.nested_path = f.nested.clone();
        // Counters are counters only in a time series index.
        a.counter = f.counter && self.index.fields.iter().any(|f| f.dimension);
        a
    }

    fn numeric_bound(&self, f: &MappedField, v: &Json) -> i64 {
        let d = match v {
            Json::Str(s) => s.parse::<f64>().unwrap(),
            Json::Bool(b) => f64::from(u8::from(*b)),
            o => o.as_f64().unwrap(),
        };
        match f.type_name.as_str() {
            "double" => encode::f64_sortable(d),
            "float" => encode::f32_sortable(d as f32) as i64,
            "boolean" => i64::from(v.as_bool().unwrap_or(d != 0.0)),
            _ => d as i64,
        }
    }

    /// A Query DSL subset: match_all, exists, term, range, bool.
    pub fn build_query(&self, q: &Json) -> Result<QueryRef, AggError> {
        let (kind, body) = q.as_object().and_then(|o| o.first()).ok_or_else(|| AggError::illegal_argument("bad query"))?;
        Ok(match kind.as_str() {
            "match_all" => Arc::new(MatchAllQuery),
            "exists" => {
                let field = body.get("field").and_then(|f| f.as_str()).unwrap_or_default();
                match self.index.field(field) {
                    Some(f) => Arc::new(FieldExistsQuery { field: f.number }),
                    None => Arc::new(search_index::query::basic::MatchNoneQuery { reason: String::new() }),
                }
            }
            "term" => {
                let (field, v) = body.as_object().unwrap().first().unwrap();
                let v = v.get("value").unwrap_or(v);
                let f = self.index.field(field).ok_or_else(|| AggError::illegal_argument("unmapped"))?;
                match f.type_name.as_str() {
                    "keyword" => Arc::new(DocValuesTermsQuery::new(f.number, [v.as_str().unwrap().as_bytes().to_vec()])),
                    "ip" => Arc::new(DocValuesTermsQuery::new(f.number, [parse_ip(v.as_str().unwrap()).unwrap().to_vec()])),
                    _ => {
                        let x = self.numeric_bound(f, v);
                        Arc::new(DocValuesRangeQuery { field: f.number, range: DvRange::Long { lower: x, upper: x } })
                    }
                }
            }
            "range" => {
                let (field, r) = body.as_object().unwrap().first().unwrap();
                let f = self.index.field(field).ok_or_else(|| AggError::illegal_argument("unmapped"))?;
                let mut lower = i64::MIN;
                let mut upper = i64::MAX;
                for (op, v) in r.as_object().unwrap() {
                    let x = self.numeric_bound(f, v);
                    match op.as_str() {
                        "gte" => lower = x,
                        "gt" => lower = x + 1,
                        "lte" => upper = x,
                        "lt" => upper = x - 1,
                        _ => {}
                    }
                }
                Arc::new(DocValuesRangeQuery { field: f.number, range: DvRange::Long { lower, upper } })
            }
            "bool" => {
                let mut b = BooleanQuery::new();
                for (occ, list) in body.as_object().unwrap() {
                    let occur = match occ.as_str() {
                        "must" => Occur::Must,
                        "filter" => Occur::Filter,
                        "should" => Occur::Should,
                        _ => Occur::MustNot,
                    };
                    let items: Vec<&Json> = match list {
                        Json::Arr(a) => a.iter().collect(),
                        o => vec![o],
                    };
                    for it in items {
                        b = b.add(occur, self.build_query(it)?);
                    }
                }
                Arc::new(b)
            }
            other => return Err(AggError::illegal_argument(format!("unsupported test query {other}"))),
        })
    }
}

impl FieldLookup for Lookup<'_> {
    fn field(&self, name: &str) -> search_aggs::Result<Option<AggField>> {
        Ok(self.index.field(name).map(|f| self.agg_field(f)))
    }

    fn index_name(&self) -> &str {
        &self.index.name
    }

    fn scripts(&self) -> Option<Arc<dyn search_aggs::ScriptHost>> {
        self.scripts.clone()
    }

    /// A mapping with dimension fields stands for a `time_series`-mode index (docs are indexed
    /// in Elasticsearch's `_tsid` order, so no `_tsid` field is needed).
    fn time_series(&self) -> Option<search_aggs::TimeSeriesInfo> {
        let dimensions: Vec<AggField> = self.index.fields.iter().filter(|f| f.dimension).map(|f| self.agg_field(f)).collect();
        let timestamp_field = self.index.field("@timestamp").map(|f| f.number);
        // A `_tsid` keyword field stands for the stored `_tsid` (docs then needn't be in its order).
        let tsid_field = self.index.field("_tsid").map(|f| f.number);
        (!dimensions.is_empty()).then_some(search_aggs::TimeSeriesInfo { dimensions, tsid_field, timestamp_field })
    }

    fn join(&self, child_type: Option<&str>, _parent_type: Option<&str>) -> Option<search_aggs::field::JoinRelation> {
        let child = child_type?;
        for j in &self.index.joins {
            for (parent, children) in &j.relations {
                if children.iter().any(|c| c == child) {
                    let join = self.index.field(&j.field)?.number;
                    let by_name = |n: &str| -> QueryRef { Arc::new(DocValuesTermsQuery::new(join, [n.as_bytes().to_vec()])) };
                    return Some(search_aggs::field::JoinRelation {
                        join_field: self.index.field(&format!("{}#{parent}", j.field))?.number,
                        parent_filter: by_name(parent),
                        child_filter: by_name(child),
                    });
                }
            }
        }
        None
    }

    fn query(&self, query: &Json) -> search_aggs::Result<QueryRef> {
        self.build_query(query)
    }

    fn nested(&self, path: &str) -> NestedLookup {
        if !self.index.nested_paths.iter().any(|p| p == path) {
            return NestedLookup::NotFound;
        }
        let child: QueryRef = Arc::new(DocValuesTermsQuery::new(self.index.nested_field, [path.as_bytes().to_vec()]));
        let parent = match path.rfind('.') {
            Some(i) if self.index.nested_paths.iter().any(|p| p == &path[..i]) => {
                Arc::new(DocValuesTermsQuery::new(self.index.nested_field, [path.as_bytes()[..i].to_vec()])) as QueryRef
            }
            _ => search_index::query::nested::root_filter(),
        };
        NestedLookup::Nested(NestedPath { child_filter: child, parent_filter: parent })
    }

    fn now_millis(&self) -> i64 {
        self.now
    }

    fn fetcher(&self) -> Option<Arc<dyn TopHitsFetcher>> {
        Some(Arc::new(Fetcher {
            index: self.index.name.clone(),
            id_field: self.index.id_field,
            sources: self.index.sources.lock().unwrap().clone(),
        }))
    }

    fn text_terms(&self) -> Option<Arc<dyn search_aggs::field::TextTerms>> {
        Some(Arc::new(TextAnalysis {
            id_field: self.index.id_field,
            sources: self.index.sources.lock().unwrap().clone(),
            types: self.index.fields.iter().map(|f| (f.path.clone(), f.type_name.clone())).collect(),
        }))
    }

    fn hit_sort(&self, sort: &Json) -> search_aggs::Result<Vec<HitSort>> {
        let items: Vec<&Json> = match sort {
            Json::Arr(a) => a.iter().collect(),
            o => vec![o],
        };
        let mut out = Vec::new();
        for it in items {
            let (name, opts) = match it {
                Json::Str(s) => (s.clone(), Json::obj()),
                Json::Obj(o) => {
                    let (k, v) = o.first().unwrap();
                    let opts = match v {
                        Json::Str(order) => Json::obj().with("order", Json::Str(order.clone())),
                        o => o.clone(),
                    };
                    (k.clone(), opts)
                }
                _ => return Err(AggError::illegal_argument("bad sort")),
            };
            let order = opts.get("order").and_then(Json::as_str);
            match name.as_str() {
                "_score" => out.push(HitSort::Score { reverse: order == Some("asc") }),
                "_doc" => out.push(HitSort::Doc { reverse: order == Some("desc") }),
                f => {
                    let field = self.field(f)?.ok_or_else(|| AggError::new(400, "query_shard_exception", format!("No mapping found for [{f}] in order to sort on")))?;
                    let missing = match opts.get("missing") {
                        None => SortMissing::Last,
                        Some(Json::Str(s)) if s == "_last" => SortMissing::Last,
                        Some(Json::Str(s)) if s == "_first" => SortMissing::First,
                        Some(v) => SortMissing::Value(v.clone()),
                    };
                    let mode = opts.get("mode").and_then(Json::as_str).map(|m| match m {
                        "min" => SortMode::Min,
                        "max" => SortMode::Max,
                        "sum" => SortMode::Sum,
                        "avg" => SortMode::Avg,
                        _ => SortMode::Median,
                    });
                    out.push(HitSort::Field { field, reverse: order == Some("desc"), missing, mode });
                }
            }
        }
        Ok(out)
    }
}

/// A stand-in for the standard analyzer (lowercased runs of letters and digits), over the
/// sources of the test index.
struct TextAnalysis {
    id_field: u32,
    sources: HashMap<String, Json>,
    types: HashMap<String, String>,
}

fn standard_tokens(text: &str) -> Vec<Vec<u8>> {
    text.split(|c: char| !c.is_alphanumeric()).filter(|w| !w.is_empty()).map(|w| w.to_lowercase().into_bytes()).collect()
}

fn doc_id(seg: &search_index::SegmentContext, id_field: u32, doc: u32) -> Option<String> {
    let ords = search_aggs::values::DvOrds::open(seg, id_field);
    let mut ord = None;
    ords.for_each(doc, |o| ord = Some(o));
    let mut id = Vec::new();
    ords.lookup(ord?, &mut id);
    String::from_utf8(id).ok()
}

impl search_aggs::field::TextTerms for TextAnalysis {
    fn analyzed(&self, field: &str) -> bool {
        matches!(self.types.get(field).map(String::as_str), Some("text" | "keyword"))
    }

    fn tokens(&self, seg: &search_index::SegmentContext, doc: u32, _field: &str, source_fields: &[String]) -> search_aggs::Result<Vec<Vec<Vec<u8>>>> {
        let Some(id) = doc_id(seg, self.id_field, doc) else { return Ok(Vec::new()) };
        let src = &self.sources[&id];
        let mut out = Vec::new();
        for f in source_fields {
            match src.get(f) {
                Some(Json::Str(s)) => out.push(standard_tokens(s)),
                Some(Json::Arr(a)) => out.extend(a.iter().filter_map(|v| v.as_str()).map(standard_tokens)),
                _ => {}
            }
        }
        Ok(out)
    }

    fn doc_freq(&self, searcher: &Searcher, field: &str, term: &[u8]) -> search_aggs::Result<u64> {
        // The shard's documents (root docs carry the id field).
        let mut n = 0;
        for seg in searcher.segments() {
            for doc in 0..seg.max_doc() {
                let Some(id) = doc_id(seg, self.id_field, doc) else { continue };
                let has = self.sources[&id].get(field).and_then(Json::as_str).is_some_and(|t| standard_tokens(t).iter().any(|x| x.as_slice() == term));
                n += u64::from(has);
            }
        }
        Ok(n)
    }

    fn term_query(&self, _field: &str, _term: &[u8]) -> search_aggs::Result<QueryRef> {
        Err(AggError::illegal_argument("no text term queries in tests"))
    }
}

/// The test fetch phase: `_index`, `_id`, `_score`, `_source` (filtered by `_source`) and `sort`.
struct Fetcher {
    index: String,
    id_field: u32,
    sources: HashMap<String, Json>,
}

fn filter_source(src: &Json, filter: Option<&Json>) -> Option<Json> {
    let includes: Vec<String> = match filter {
        None | Some(Json::Bool(true)) => return Some(src.clone()),
        Some(Json::Bool(false)) => return None,
        Some(Json::Str(s)) => vec![s.clone()],
        Some(Json::Arr(a)) => a.iter().filter_map(|x| x.as_str().map(String::from)).collect(),
        Some(o) => o.get("includes").or_else(|| o.get("include")).and_then(Json::as_array).map(|a| a.iter().filter_map(|x| x.as_str().map(String::from)).collect()).unwrap_or_default(),
    };
    let mut out = Json::obj();
    for (k, v) in src.as_object().unwrap_or(&[]) {
        if includes.iter().any(|i| i == k) {
            out.push(k.clone(), v.clone());
        }
    }
    Some(out)
}

impl TopHitsFetcher for Fetcher {
    fn fetch(&self, searcher: &Searcher, hits: &[FetchHit], spec: &Json) -> search_aggs::Result<Vec<Json>> {
        let mut out = Vec::new();
        for h in hits {
            let (seg, local) = searcher.segment_of(h.doc).unwrap();
            let ords = search_aggs::values::DvOrds::open(seg, self.id_field);
            let mut id = Vec::new();
            let mut ord = None;
            ords.for_each(local, |o| ord = Some(o));
            ords.lookup(ord.expect("root doc"), &mut id);
            let id = String::from_utf8(id).unwrap();
            let mut o = Json::obj();
            o.push("_index", Json::Str(self.index.clone()));
            o.push("_id", Json::Str(id.clone()));
            o.push("_score", if h.score.is_nan() { Json::Null } else { Json::Float32(h.score) });
            if let Some(src) = filter_source(&self.sources[&id], spec.get("_source")) {
                o.push("_source", src);
            }
            if !h.sort.is_empty() {
                o.push("sort", Json::Arr(h.sort.clone()));
            }
            out.push(o);
        }
        Ok(out)
    }
}

/// The top-level query of a request body (`match_all` when absent), restricted to root docs.
pub fn top_query(lookup: &Lookup<'_>, body: &Json) -> QueryRef {
    let root = search_index::query::nested::root_filter();
    match body.get("query") {
        // Without nested mappings every doc is a root doc (Elasticsearch then runs a plain
        // `MatchAllDocsQuery`, which lets top-level aggregations read segment statistics).
        None if lookup.index.nested_paths.is_empty() => Arc::new(MatchAllQuery),
        None => root,
        Some(q) => Arc::new(BooleanQuery::new().add(Occur::Must, lookup.build_query(q).unwrap()).add(Occur::Filter, root)),
    }
}

/// Runs a request body's `aggs` over shards and reduces them.
pub fn run(index: &TestIndex, searchers: &[Searcher], body_text: &str, typed_keys: bool) -> Result<Json, AggError> {
    run_with(index, searchers, body_text, typed_keys, None, &search_aggs::ReduceOptions::default())
}

/// [`run`] with a script engine and reduce options (`scripts` is used on both sides).
#[allow(dead_code)]
pub fn run_with(
    index: &TestIndex,
    searchers: &[Searcher],
    body_text: &str,
    typed_keys: bool,
    scripts: Option<Arc<dyn search_aggs::ScriptHost>>,
    reduce_opts: &search_aggs::ReduceOptions,
) -> Result<Json, AggError> {
    let body = Json::parse(body_text).unwrap();
    let aggs = body.get("aggs").cloned().unwrap_or(Json::obj());
    let opts = search_aggs::ParseOptions { src: Some(body_text.as_bytes()), base: vec![search_aggs::xparser::Seg::Key("aggs".into())], ..Default::default() };
    let req = search_aggs::parse(&aggs, &opts)?;
    let errs = req.validate();
    if !errs.is_empty() {
        let mut msg = "Validation Failed: ".to_string();
        for (i, e) in errs.iter().enumerate() {
            msg.push_str(&format!("{}: {e};", i + 1));
        }
        return Err(AggError::new(400, "action_request_validation_exception", msg));
    }
    let lookup = Lookup { index, now: 1_700_000_000_000, scripts: scripts.clone() };
    let q = top_query(&lookup, &body);
    let mut parts = Vec::new();
    for s in searchers {
        parts.push(req.run(&lookup, s, &q, &search_aggs::ShardOptions::default())?);
    }
    let opts = search_aggs::ReduceOptions { scripts: scripts.or_else(|| reduce_opts.scripts.clone()), ..reduce_opts.clone() };
    let reduced = search_aggs::reduce(parts, &req, &opts)?;
    Ok(reduced.to_json(typed_keys))
}

/// Structural JSON equality with exact numbers; returns the first difference.
pub fn diff(a: &Json, b: &Json, path: &str) -> Option<String> {
    diff_rel(a, b, path, 0.0)
}

/// [`diff`] that accepts doubles within a relative error of `rel` (multi-shard responses:
/// Elasticsearch's reduce order depends on when its partial reductions run, which moves the last
/// bits of floating-point sums and everything derived from them).
pub fn diff_rel(a: &Json, b: &Json, path: &str, rel: f64) -> Option<String> {
    let close = |x: f64, y: f64| {
        x.to_bits() == y.to_bits() || (x.is_nan() && y.is_nan()) || (x.is_finite() && y.is_finite() && (x - y).abs() <= rel * x.abs().max(y.abs()))
    };
    let diff = |a: &Json, b: &Json, path: &str| diff_rel(a, b, path, rel);
    match (a, b) {
        (Json::Obj(x), Json::Obj(y)) => {
            let kx: Vec<&String> = x.iter().map(|e| &e.0).collect();
            let ky: Vec<&String> = y.iter().map(|e| &e.0).collect();
            if kx != ky {
                return Some(format!("{path}: keys {kx:?} != {ky:?}"));
            }
            for ((k, v1), (_, v2)) in x.iter().zip(y.iter()) {
                if let Some(d) = diff(v1, v2, &format!("{path}.{k}")) {
                    return Some(d);
                }
            }
            None
        }
        (Json::Arr(x), Json::Arr(y)) => {
            if x.len() != y.len() {
                return Some(format!("{path}: len {} != {}\n got: {}\nwant: {}", x.len(), y.len(), a, b));
            }
            for (i, (v1, v2)) in x.iter().zip(y.iter()).enumerate() {
                if let Some(d) = diff(v1, v2, &format!("{path}[{i}]")) {
                    return Some(d);
                }
            }
            None
        }
        (Json::Int(x), Json::Int(y)) if x == y => None,
        (Json::Float(x), Json::Float(y)) if close(*x, *y) => None,
        // `matrix_stats` skewness divides by `Math.pow(var, 1.5)`: HotSpot's intrinsic and the
        // platform `pow` disagree in the last bit for ~0.1% of inputs.
        (Json::Float(x), Json::Float(y)) if path.ends_with(".skewness") && (x.to_bits() as i64 - y.to_bits() as i64).abs() <= 2 => None,
        (Json::Float32(x), Json::Float(y)) if search_aggs::json::java_float(*x) == search_aggs::json::java_double(*y) => None,
        // Non-finite doubles are written as strings ("Infinity", "NaN").
        (Json::Float(x), Json::Str(y)) if !x.is_finite() && search_aggs::json::java_double(*x) == *y => None,
        (Json::Str(x), Json::Str(y)) if x == y => None,
        (Json::Bool(x), Json::Bool(y)) if x == y => None,
        (Json::Null, Json::Null) => None,
        _ => Some(format!("{path}: {a} != {b}")),
    }
}
