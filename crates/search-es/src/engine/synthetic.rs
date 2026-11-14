//! Synthetic `_source` (`index.mapping.source.mode: synthetic`, the `logsdb` and `time_series`
//! modes, and `force_synthetic_source`): the document as Elasticsearch rebuilds it from doc
//! values, stored fields and `_ignored_source`.
//!
//! Elasticsearch decides, while parsing, which parts of a document are kept verbatim in
//! `_ignored_source` (disabled objects, `dynamic: false` and runtime fields, fields past the
//! dynamic limits, `synthetic_source_keep`, `copy_to` sources and destinations, field types
//! without native support) and rebuilds the rest from each field's columns when the document is
//! read. The rebuilt form is a pure function of the document and the mapping, so it is computed
//! once here, at index time, and stored as the `_source`; every reader (get, search, update,
//! reindex) then sees exactly what Elasticsearch would show.
//!
//! The pass mirrors `DocumentParser` in two phases:
//!
//! 1. [`Walk`] follows the document through the object mappers and records, per (nested)
//!    document scope, the column values of each leaf field (typed by the field's parser, so
//!    coercion and `null_value` apply) and the verbatim `_ignored_source` entries, keyed by the
//!    object they belong to.
//! 2. [`load`] writes each object like `ObjectMapper.SyntheticSourceFieldLoader`: children in
//!    full-path order (a `TreeMap`), an ignored entry replacing the field loader of the same
//!    name, empty objects dropped, one value as a scalar and several as an array. Leaf values
//!    come out the way their doc values store them: sorted sets are sorted and deduplicated,
//!    sorted numerics sorted, stored fields in document order; `ignore_above` and malformed
//!    values follow in document order. With `synthetic_source_keep: arrays` the types that
//!    record array offsets keep document order, duplicates and `null`s.

use super::fields::{FieldInfo, FieldTable, Kind};
use super::ranges;
use crate::document::FieldValue;
use crate::json::{Map, Number, Value};
use crate::mapping::{Dynamic, FieldMapper, Mapper, Mapping, MappingCtx, ObjectMapper};
use std::collections::{BTreeMap, HashMap, HashSet};

/// `synthetic_source_keep`: what an object or field keeps verbatim.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Keep {
    None,
    Arrays,
    All,
}

impl Keep {
    pub fn parse(s: Option<&str>) -> Keep {
        match s {
            Some("arrays") => Keep::Arrays,
            Some("all") => Keep::All,
            _ => Keep::None,
        }
    }
}

/// How an index keeps `_source`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SourceMode {
    Stored,
    Synthetic,
    Disabled,
}

/// The `_source` mode of an index: `index.mapping.source.mode`, the mapping's deprecated
/// `_source.mode`, then the index mode's default (`logsdb` and `time_series` synthesize).
pub fn source_mode(idx: &crate::meta::IndexMeta) -> SourceMode {
    let setting = idx.settings.get("index.mapping.source.mode").map(str::to_ascii_lowercase);
    let mode = setting.or_else(|| idx.mapping.source.mode.as_deref().map(str::to_ascii_lowercase)).unwrap_or_else(|| {
        crate::meta::index_mode::IndexMode::of(&idx.settings).default_source_mode().to_ascii_lowercase()
    });
    match mode.as_str() {
        // `columnar_stored` stores the columns' rendering of the document.
        "synthetic" | "columnar_stored" => SourceMode::Synthetic,
        "disabled" => SourceMode::Disabled,
        _ if idx.mapping.source.enabled == Some(false) => SourceMode::Disabled,
        _ => SourceMode::Stored,
    }
}

/// Index settings the synthesis depends on.
#[derive(Clone, Copy, Debug)]
pub struct Options {
    /// `index.mapping.synthetic_source_keep` (`arrays` by default in `logsdb`).
    pub keep: Keep,
    /// `index.use_time_series_doc_values_format` (the default of the time series, logs and
    /// columnar modes): values kept beside the columns (`ignore_above`, text fallback) live in
    /// sorted binary doc values rather than in stored fields, so they come back sorted.
    pub binary_fallback: bool,
}

pub fn options(idx: &crate::meta::IndexMeta) -> Options {
    use crate::meta::index_mode::IndexMode;
    let mode = IndexMode::of(&idx.settings);
    let keep = match idx.settings.get("index.mapping.synthetic_source_keep") {
        Some(s) => Keep::parse(Some(s)),
        // Columns keep each document's values in order (`doc_values.multi_value`).
        None if mode.is_logs() || mode.is_strict_columnar() => Keep::Arrays,
        None => Keep::None,
    };
    let binary_fallback = match idx.settings.get("index.use_time_series_doc_values_format") {
        Some(v) => v == "true",
        None => matches!(mode, IndexMode::TimeSeries | IndexMode::Logsdb | IndexMode::Columnar | IndexMode::LogsdbColumnar),
    };
    Options { keep, binary_fallback }
}

/// Leaves vector fields out of a fetched `_source`, except those `keep` matches (the request's
/// includes), and shows the kept ones as the index holds them (floats for float vectors).
/// `keep: None` keeps every vector.
pub fn vectors(table: &FieldTable, path: &str, m: &mut Map, exclude: bool) {
    let none = super::source::Globs::new(&[]);
    vectors_keeping(table, path, m, exclude.then_some(&none), true);
}

/// Returns whether a vector stayed in.
///
/// Kept vectors print as Elasticsearch's patched ones do (`source_vectors`): at the end of
/// their object, floats as floats, `sparse_vector` features sorted and as their postings keep
/// them. A vector of an `index: false` field is never taken out of Elasticsearch's source and
/// prints as sent. Normalizing twice changes nothing; `normalize: false` only keeps and drops,
/// for a source [`vectors`] already went over.
pub fn vectors_keeping(table: &FieldTable, path: &str, m: &mut Map, keep: Option<&super::source::Globs>, normalize: bool) -> bool {
    let mut drop = Vec::new();
    let mut last = Vec::new();
    let mut kept = false;
    for (k, v) in m.iter_mut() {
        let full = join(path, k);
        if let Some(info) = table.get(&full)
            && info.is_source_vector()
        {
            // Elasticsearch takes vectors out of the stored `_source` at index time, a null one
            // too, and patches back only the values the field holds: a null never shows.
            if v.is_null() || keep.is_some_and(|g| !g.matches(&full)) {
                drop.push(k.clone());
            } else if !normalize || (info.kind == Kind::DenseVector && !info.index) {
                kept = true;
            } else {
                normalize_vector(info, v);
                last.push(k.clone());
                kept = true;
            }
            continue;
        }
        match v {
            Value::Obj(inner) => kept |= crate::stack::maybe_grow(|| vectors_keeping(table, &full, inner, keep, normalize)),
            Value::Arr(a) => {
                for e in a {
                    if let Value::Obj(inner) = e {
                        kept |= crate::stack::maybe_grow(|| vectors_keeping(table, &full, inner, keep, normalize));
                    }
                }
            }
            _ => {}
        }
    }
    for k in drop {
        m.shift_remove(&k);
    }
    for k in last {
        if let Some(v) = m.shift_remove(&k) {
            m.insert(k, v);
        }
    }
    kept
}

fn normalize_vector(info: &FieldInfo, v: &mut Value) {
    if info.kind == Kind::SparseVector {
        // Rebuilt from the feature postings: one object (an array of objects merges), features
        // in term order, each value as its term frequency keeps it (`FeatureField`).
        let mut features: Vec<(String, Value)> = Vec::new();
        let mut take = |m: &Map| {
            for (k, x) in m {
                let x = match x {
                    Value::Num(n) => float_value(f32::from_bits(((n.as_f64() as f32).to_bits() >> 15) << 15)),
                    other => other.clone(),
                };
                features.push((k.clone(), x));
            }
        };
        match &*v {
            Value::Obj(m) => take(m),
            Value::Arr(items) => items.iter().for_each(|it| if let Value::Obj(m) = it { take(m) }),
            _ => return,
        }
        features.sort_by(|a, b| a.0.cmp(&b.0));
        *v = Value::Obj(features.into_iter().collect());
        return;
    }
    // A base64 (or hex) encoded vector reads back as the values it encodes.
    if let (Value::Str(s), Some(spec)) = (&*v, info.vector) {
        match crate::search::knn::decode_query_vector(s, spec.element, spec.dims as usize) {
            Ok(crate::query::specialized::VectorData::Floats(f)) => *v = Value::Arr(f.into_iter().map(float_value).collect()),
            Ok(crate::query::specialized::VectorData::Bytes(b)) => *v = Value::Arr(b.into_iter().map(|x| Value::from(x as i64)).collect()),
            _ => {}
        }
        return;
    }
    if !info.vector.is_none_or(|s| s.element == search_codec::VectorElement::Float32) {
        return;
    }
    match v {
        Value::Num(n) => *v = float_value(n.as_f64() as f32),
        Value::Arr(a) => a.iter_mut().for_each(|x| normalize_vector(info, x)),
        _ => {}
    }
}

/// The synthetic `_source` of `doc` under `mapping`.
pub fn synthesize(mapping: &Mapping, ctx: &MappingCtx, table: &FieldTable, opts: Options, doc: &Map) -> Map {
    let keep = opts.keep;
    if !mapping.root.is_enabled() {
        // The whole document is one `_ignored_source` value.
        return doc.iter().filter(|(k, _)| !crate::mapping::METADATA_FIELDS.contains(&k.as_str())).map(|(k, v)| (k.clone(), v.clone())).collect();
    }
    let mut copy_dests = HashSet::new();
    collect_copy_dests(&mapping.root, &mut copy_dests);
    let w = Walk { mapping, ctx, table, keep, copy_dests, binary_fallback: opts.binary_fallback };
    let mut scope = Scope::default();
    let root_dynamic = mapping.root.dynamic.unwrap_or(Dynamic::True);
    w.object_fields(&mapping.root, "", doc, &mut scope, root_dynamic, Flags::default());
    let mut out = load(&w, &mapping.root, "", &scope).unwrap_or_default();
    // `_doc_count` has its own loader, writing the value among the root's fields in name order.
    if let Some(c) = doc.get("_doc_count").and_then(Value::as_i64).filter(|c| *c > 0) {
        let at = out.keys().position(|k| k.as_str() > "_doc_count").unwrap_or(out.len());
        out.shift_insert(at, "_doc_count".to_string(), Value::from(c));
    }
    out
}

fn collect_copy_dests(o: &ObjectMapper, out: &mut HashSet<String>) {
    for c in o.children.values() {
        match c {
            Mapper::Field(f) => {
                for t in &f.copy_to {
                    out.insert(t.clone());
                }
            }
            Mapper::Object(child) => collect_copy_dests(child, out),
            Mapper::Alias(_) => {}
        }
    }
}

fn join(path: &str, name: &str) -> String {
    if path.is_empty() { name.to_string() } else { format!("{path}.{name}") }
}

/// One column value of a leaf field and its doc-values order.
#[derive(Clone, Debug)]
struct Dv {
    key: Key,
    v: Value,
}

/// Doc-values sort order (only compared within one field).
#[derive(Clone, Debug, PartialEq, PartialOrd)]
enum Key {
    Int(i128),
    Bytes(Vec<u8>),
    /// A `null` slot of an offsets-tracked array.
    Null,
}

/// Values of one leaf field in one document scope.
#[derive(Default, Debug)]
struct Leaf {
    /// Column values in document order.
    dv: Vec<Dv>,
    /// Values kept beside the column (`ignore_above`, malformed, text fallback), in document
    /// order, each with its binary doc-values sort key.
    extra: Vec<(Value, Vec<u8>)>,
    /// The kept values come back sorted (binary doc values) rather than in document order
    /// (stored fields).
    extra_sorted: bool,
    /// Recorded with array offsets: document order, duplicates and nulls.
    offsets: bool,
    /// The document gave an array (offsets keep single-element and empty arrays).
    array: bool,
}

/// An `_ignored_source` entry.
#[derive(Debug)]
struct Ignored {
    name: String,
    parent: String,
    value: Value,
}

/// The values of one Lucene document (the root or a nested object).
#[derive(Default, Debug)]
struct Scope {
    leaves: HashMap<String, Leaf>,
    ignored: Vec<Ignored>,
    nested: Vec<(String, Scope)>,
}

#[derive(Clone, Copy, Default)]
struct Flags {
    /// Inside an array of (non-nested) objects (`DocumentParserContext.Scope.ARRAY`).
    in_array: bool,
    /// Inside a nested document (no array offsets there).
    in_nested: bool,
}

struct Walk<'a> {
    mapping: &'a Mapping,
    ctx: &'a MappingCtx,
    table: &'a FieldTable,
    keep: Keep,
    copy_dests: HashSet<String>,
    binary_fallback: bool,
}

// The walk threads the document position (path, scope, flags) through every step.
#[allow(clippy::too_many_arguments)]
impl<'a> Walk<'a> {
    fn object_fields(&self, o: &'a ObjectMapper, path: &str, m: &Map, scope: &mut Scope, dynamic: Dynamic, fl: Flags) {
        for (k, v) in m {
            if path.is_empty() && crate::mapping::METADATA_FIELDS.contains(&k.as_str()) {
                continue;
            }
            crate::stack::maybe_grow(|| self.field(o, path, k, v, scope, dynamic, fl));
        }
    }

    fn keep_of(&self, own: &Option<String>) -> Keep {
        match own {
            Some(s) => Keep::parse(Some(s)),
            None => self.keep,
        }
    }

    /// Whether `o` holds objects (strict columnar mappings keep dotted names at the root).
    fn subobjects(&self, o: &ObjectMapper) -> bool {
        self.ctx.columnar.is_none() && o.subobjects_enabled()
    }

    fn raw(&self, scope: &mut Scope, name: String, parent: &str, v: &Value) {
        scope.ignored.push(Ignored { name, parent: parent.to_string(), value: v.clone() });
    }

    /// One key of an object (`DocumentParser.innerParseObject`).
    fn field(&self, o: &'a ObjectMapper, path: &str, name: &str, v: &Value, scope: &mut Scope, dynamic: Dynamic, fl: Flags) {
        let full = join(path, name);
        match o.children.get(name) {
            Some(Mapper::Field(f)) => self.field_value(path, f, &full, v, scope, fl),
            Some(Mapper::Object(c)) => self.object_value(path, c, &full, v, scope, c.dynamic.unwrap_or(dynamic), fl),
            Some(Mapper::Alias(_)) => {}
            None => {
                // `DotExpandingXContentParser`: `a.b` is `a: {b}` where objects can hold objects.
                if self.subobjects(o)
                    && let Some((head, rest)) = name.split_once('.')
                    && !head.is_empty()
                    && !rest.is_empty()
                {
                    let mut inner = Map::new();
                    inner.insert(rest.to_string(), v.clone());
                    return self.field(o, path, head, &Value::Obj(inner), scope, dynamic, fl);
                }
                if !self.subobjects(o)
                    && let Value::Obj(inner) = v
                {
                    for (k, x) in inner {
                        self.field(o, path, &format!("{name}.{k}"), x, scope, dynamic, fl);
                    }
                    return;
                }
                if self.mapping.runtime.contains_key(&full) {
                    // Runtime fields keep their values verbatim, one entry per value.
                    return self.runtime_values(path, &full, v, scope);
                }
                self.unmapped(path, full, v, scope, dynamic);
            }
        }
    }

    fn runtime_values(&self, path: &str, full: &str, v: &Value, scope: &mut Scope) {
        match v {
            Value::Null => {}
            Value::Arr(a) => a.iter().for_each(|x| self.runtime_values(path, full, x, scope)),
            other => self.raw(scope, full.to_string(), path, other),
        }
    }

    /// A key no mapper took (after the document's dynamic update was merged).
    fn unmapped(&self, path: &str, full: String, v: &Value, scope: &mut Scope, dynamic: Dynamic) {
        match dynamic {
            Dynamic::Runtime => self.runtime_values(path, &full, v, scope),
            Dynamic::True => {
                // A dynamic field that didn't make it into the mapping was dropped by the field
                // limits and is kept verbatim; null and empty arrays create no field at all.
                if has_content(v) {
                    self.raw(scope, full, path, v);
                }
            }
            // Strict columnar indices keep no values outside their columns.
            Dynamic::False if self.ctx.columnar.is_some() => {}
            _ => {
                if !v.is_null() {
                    self.raw(scope, full, path, v);
                }
            }
        }
    }

    fn object_value(&self, path: &str, c: &'a ObjectMapper, full: &str, v: &Value, scope: &mut Scope, dynamic: Dynamic, fl: Flags) {
        let keep = self.keep_of(&c.source_keep);
        match v {
            Value::Null => {}
            Value::Obj(m) => {
                if !c.is_enabled() || keep == Keep::All || (keep == Keep::Arrays && fl.in_array) {
                    return self.raw(scope, full.to_string(), path, v);
                }
                if c.is_nested() {
                    let mut child = Scope::default();
                    self.object_fields(c, full, m, &mut child, dynamic, Flags { in_nested: true, ..fl });
                    scope.nested.push((full.to_string(), child));
                } else {
                    self.object_fields(c, full, m, scope, dynamic, fl);
                }
            }
            Value::Arr(a) => {
                // `parseArrayElements` with an object mapper.
                if keep == Keep::All || (keep == Keep::Arrays && !c.is_nested()) || !c.is_enabled() {
                    return self.raw(scope, full.to_string(), path, v);
                }
                let fl = Flags { in_array: fl.in_array || !c.is_nested(), ..fl };
                for e in a {
                    self.object_value(path, c, full, e, scope, dynamic, fl);
                }
            }
            _ => {}
        }
    }

    /// Whether a field's synthetic source is the verbatim fallback (`SyntheticSourceMode.FALLBACK`).
    fn fallback(&self, f: &FieldMapper, info: Option<&FieldInfo>) -> bool {
        if !f.copy_to.is_empty() {
            return true;
        }
        let Some(info) = info else { return true };
        match info.kind {
            Kind::Keyword => {
                let normalized = f.raw("normalizer").is_some_and(|v| !v.is_null());
                if normalized && !self.skip_store_original(f) {
                    return true;
                }
                !(info.store || info.doc_values)
            }
            Kind::Text | Kind::MatchOnlyText => false,
            Kind::Long
            | Kind::Integer
            | Kind::Short
            | Kind::Byte
            | Kind::Double
            | Kind::Float
            | Kind::HalfFloat
            | Kind::ScaledFloat(_)
            | Kind::UnsignedLong
            | Kind::Boolean
            | Kind::Date
            | Kind::DateNanos
            | Kind::Ip => !(info.doc_values || info.store),
            Kind::GeoPoint | Kind::Binary | Kind::Wildcard | Kind::Version | Kind::Flattened => !info.doc_values,
            Kind::IntegerRange | Kind::LongRange | Kind::FloatRange | Kind::DoubleRange | Kind::DateRange | Kind::IpRange => !info.doc_values,
            Kind::ConstantKeyword | Kind::DenseVector | Kind::Histogram => false,
            _ => true,
        }
    }

    fn skip_store_original(&self, f: &FieldMapper) -> bool {
        skip_store_original(f, self.ctx)
    }

    /// Whether the field records array offsets (`FieldArrayContext.getOffsetsFieldName`).
    fn offsets(&self, f: &FieldMapper, info: Option<&FieldInfo>, fl: Flags) -> bool {
        let Some(info) = info else { return false };
        let supported = matches!(
            info.kind,
            Kind::Keyword
                | Kind::Ip
                | Kind::Boolean
                | Kind::Long
                | Kind::Integer
                | Kind::Short
                | Kind::Byte
                | Kind::Double
                | Kind::Float
                | Kind::HalfFloat
                | Kind::ScaledFloat(_)
                | Kind::UnsignedLong
        );
        supported
            && self.keep_of(&f.source_keep) == Keep::Arrays
            && info.doc_values
            && !info.store
            && !fl.in_nested
            // Inside an array of objects every value is kept verbatim instead, so values of
            // all the objects merge in document order.
            && !fl.in_array
            && f.copy_to.is_empty()
            && f.fields.is_empty()
    }

    fn field_value(&self, path: &str, f: &'a FieldMapper, full: &str, v: &Value, scope: &mut Scope, fl: Flags) {
        let info = self.table.get(full).map(|a| &**a);
        let takes_array = matches!(f.ty, "dense_vector" | "rank_vectors")
            || (matches!(f.ty, "geo_point" | "point") && matches!(v, Value::Arr(a) if a.first().is_some_and(|x| matches!(x, Value::Num(_)))));
        // Point mappers parse arrays themselves (`parsesArrayValue`): never kept verbatim.
        if let Value::Arr(a) = v
            && !takes_array
            && matches!(f.ty, "geo_point" | "point")
            && !self.fallback(f, info)
            && !self.copy_dests.contains(full)
        {
            for e in a {
                self.single(path, f, info, full, e, scope, fl, true, true);
            }
            if let Some(l) = scope.leaves.get_mut(full) {
                l.offsets = false;
            }
            return;
        }
        if let Value::Arr(a) = v
            && !takes_array
        {
            let offsets = self.offsets(f, info, fl);
            if offsets {
                scope.leaves.entry(full.to_string()).or_default().offsets = true;
            }
            if !offsets && (self.fallback(f, info) || self.keep_of(&f.source_keep) != Keep::None || self.copy_dests.contains(full)) {
                return self.raw(scope, full.to_string(), path, v);
            }
            if offsets {
                scope.leaves.entry(full.to_string()).or_default().array = true;
            }
            for e in a {
                self.element(path, f, info, full, e, scope, fl, offsets);
            }
            return;
        }
        self.single(path, f, info, full, v, scope, fl, false, takes_array);
    }

    fn element(&self, path: &str, f: &'a FieldMapper, info: Option<&FieldInfo>, full: &str, e: &Value, scope: &mut Scope, fl: Flags, offsets: bool) {
        match e {
            Value::Arr(inner) if !matches!(f.ty, "geo_point" | "point") || !inner.first().is_some_and(|x| matches!(x, Value::Num(_))) => {
                for x in inner {
                    self.element(path, f, info, full, x, scope, fl, offsets);
                }
            }
            _ => self.single(path, f, info, full, e, scope, fl, offsets, false),
        }
    }

    /// One value of a field (`parseObjectOrField`).
    fn single(&self, path: &str, f: &'a FieldMapper, info: Option<&FieldInfo>, full: &str, v: &Value, scope: &mut Scope, fl: Flags, offsets: bool, takes_array: bool) {
        let keep = self.keep_of(&f.source_keep);
        if !offsets
            && (self.fallback(f, info) || keep == Keep::All || (keep == Keep::Arrays && fl.in_array && !takes_array) || self.copy_dests.contains(full))
        {
            return self.raw(scope, full.to_string(), path, v);
        }
        let Some(info) = info else { return };
        let leaf = scope.leaves.entry(full.to_string()).or_default();
        leaf.offsets |= offsets;
        let mut ignored = std::collections::BTreeSet::new();
        let parsed = crate::document::values::parse_standalone(f, full, v, self.ctx, &mut ignored);
        match parsed {
            Some(vals) if !vals.is_empty() => {
                for fv in vals {
                    if let Some(dv) = render(info, f, &fv, self.ctx) {
                        leaf.dv.push(dv);
                    }
                }
            }
            Some(_) if ignored.contains(full) && !v.is_null() => {
                // ignore_above (as text) or ignore_malformed (as sent).
                match info.kind {
                    Kind::Keyword | Kind::Wildcard | Kind::Flattened => {
                        let kept = match v {
                            Value::Str(_) => v.clone(),
                            other => Value::Str(other.to_es_string()),
                        };
                        let key = kept.as_str().unwrap_or_default().as_bytes().to_vec();
                        leaf.extra.push((kept, key));
                        leaf.extra_sorted |= self.binary_fallback;
                    }
                    // The histogram mappers keep a copy of the value as XContent
                    // (`IgnoreMalformedStoredValues.storedField(name, XContentBuilder)`): its
                    // JSON bytes order it.
                    Kind::Histogram => {
                        leaf.extra.push((v.clone(), v.to_json().into_bytes()));
                        leaf.extra_sorted = true;
                    }
                    _ => {
                        // `IgnoreMalformedStoredValues`: sorted binary doc values.
                        leaf.extra.push((v.clone(), xcontent_key(v)));
                        leaf.extra_sorted = true;
                    }
                }
            }
            Some(_) => {
                if offsets && v.is_null() {
                    leaf.dv.push(Dv { key: Key::Null, v: Value::Null });
                }
            }
            None => {
                leaf.extra.push((v.clone(), xcontent_key(v)));
                leaf.extra_sorted = true;
            }
        }
        if matches!(info.kind, Kind::Text | Kind::MatchOnlyText) && !info.store && !info.doc_values {
            // Text keeps what its keyword delegate can't: the value moves to the fallback layer.
            if let Value::Str(s) = v {
                if !delegate_takes(self.table, self.ctx, f, full, s) {
                    if let Some(last) = leaf.dv.pop() {
                        leaf.extra.push((last.v, s.as_bytes().to_vec()));
                        leaf.extra_sorted |= self.binary_fallback;
                    }
                } else if let Some(norm) = delegate(self.table, self.ctx, f, full).and_then(|d| d.normalizer.as_ref())
                    && let Some(last) = leaf.dv.last_mut()
                {
                    // The delegate's column holds the normalized value.
                    let n = norm.normalize(s);
                    last.key = Key::Bytes(n.as_bytes().to_vec());
                    last.v = Value::Str(n);
                }
            }
        }
    }
}

/// `XContentDataHelper.encodeToken`: a type byte, then the value (numbers little-endian), the
/// order malformed values come back in.
fn xcontent_key(v: &Value) -> Vec<u8> {
    let mut out = Vec::new();
    match v {
        Value::Str(s) => {
            out.push(b'S');
            out.extend_from_slice(s.as_bytes());
        }
        Value::Num(Number::Int(i)) if i32::try_from(*i).is_ok() => {
            out.push(b'I');
            out.extend_from_slice(&(*i as i32).to_le_bytes());
        }
        Value::Num(Number::Int(i)) => {
            out.push(b'L');
            out.extend_from_slice(&i.to_le_bytes());
        }
        Value::Num(Number::UInt(_)) | Value::Num(Number::Big(_)) => {
            out.push(b'i');
            out.extend_from_slice(v.to_es_string().as_bytes());
        }
        Value::Num(Number::Float(d)) => {
            out.push(b'D');
            out.extend_from_slice(&d.to_le_bytes());
        }
        Value::Bool(b) => out.push(if *b { b't' } else { b'f' }),
        Value::Null => out.push(b'n'),
        other => {
            out.push(b'c');
            out.extend_from_slice(other.to_json().as_bytes());
        }
    }
    out
}

/// Whether a value has anything a dynamic mapping would be created for.
fn has_content(v: &Value) -> bool {
    match v {
        Value::Null => false,
        Value::Arr(a) => a.iter().any(has_content),
        _ => true,
    }
}

/// `normalizer_skip_store_original_value`: defaults to true for the built-in `lowercase`.
fn skip_store_original(f: &FieldMapper, ctx: &MappingCtx) -> bool {
    match f.raw("normalizer_skip_store_original_value").and_then(Value::as_bool) {
        Some(b) => b,
        None => f.raw("normalizer").and_then(Value::as_str) == Some("lowercase") && !ctx.normalizers.contains("lowercase"),
    }
}

/// Whether the keyword multi-field a text field rebuilds its source from takes `s`.
fn delegate_takes(table: &FieldTable, ctx: &MappingCtx, f: &FieldMapper, full: &str, s: &str) -> bool {
    delegate(table, ctx, f, full).is_some_and(|info| !info.ignore_above.is_some_and(|m| crate::document::exceeds_ignore_above(s, m)))
}

/// The keyword multi-field text loads from (`TextFieldMapper.SyntheticSourceHelper`): the
/// first keyword with doc values or `store` whose normalizer keeps no original value.
fn delegate<'t>(table: &'t FieldTable, ctx: &MappingCtx, f: &FieldMapper, full: &str) -> Option<&'t FieldInfo> {
    for (name, sub) in &f.fields {
        if sub.ty != "keyword" {
            continue;
        }
        let Some(info) = table.get(&format!("{full}.{name}")) else { continue };
        let normalized = sub.raw("normalizer").is_some_and(|v| !v.is_null());
        if (normalized && !skip_store_original(sub, ctx)) || !(info.doc_values || info.store) {
            continue;
        }
        return Some(info);
    }
    None
}

fn sortable_f64(d: f64) -> i128 {
    let bits = d.to_bits() as i64;
    (bits ^ ((bits >> 63) & 0x7fff_ffff_ffff_ffff)) as i128
}

fn sortable_f32(f: f32) -> i128 {
    let bits = f.to_bits() as i32;
    (bits ^ ((bits >> 31) & 0x7fff_ffff)) as i128
}

/// A float as Elasticsearch prints it (`Float.toString` digits).
fn float_value(f: f32) -> Value {
    if !f.is_finite() {
        return Value::Num(Number::Float(f as f64));
    }
    let s = crate::json::write::java_float(f);
    Value::Num(Number::Float(s.parse::<f64>().unwrap_or(f as f64)))
}

fn date_value(info: &FieldInfo, f: &FieldMapper, ctx: &MappingCtx, v: i64) -> Value {
    let nanos = info.kind == Kind::DateNanos;
    let fmt = match &info.date_format {
        Some(d) => d.clone(),
        None => match f.date_formatter(ctx) {
            Ok(d) => d,
            Err(_) => return Value::from(v),
        },
    };
    let instant = if nanos {
        crate::time::Instant { secs: v.div_euclid(1_000_000_000), nanos: v.rem_euclid(1_000_000_000) as u32 }
    } else {
        crate::time::Instant::from_millis(v)
    };
    Value::Str(fmt.format(instant, &crate::time::Zone::UTC))
}

/// A field value as its synthetic source shows it, with its doc-values order.
fn render(info: &FieldInfo, f: &FieldMapper, fv: &FieldValue, ctx: &MappingCtx) -> Option<Dv> {
    Some(match fv {
        FieldValue::Keyword(s) => {
            let s = match (&info.normalizer, info.kind) {
                (Some(n), Kind::Keyword) => n.normalize(s),
                _ => s.clone(),
            };
            let key = match info.kind {
                // Doc values order: the encoded version (`VersionEncoder`).
                Kind::Version => Key::Bytes(search_aggs::version_string::encode(&s).unwrap_or_else(|_| s.as_bytes().to_vec())),
                _ => Key::Bytes(s.as_bytes().to_vec()),
            };
            Dv { key, v: Value::Str(s) }
        }
        FieldValue::Text(s) => Dv { key: Key::Bytes(s.as_bytes().to_vec()), v: Value::Str(s.clone()) },
        FieldValue::Long(i) => match info.kind {
            Kind::ScaledFloat(_) => {
                let factor = f.effective("scaling_factor", ctx).as_f64().unwrap_or(1.0);
                Dv { key: Key::Int(*i as i128), v: Value::Num(Number::Float(*i as f64 / factor)) }
            }
            Kind::Date | Kind::DateNanos => Dv { key: Key::Int(*i as i128), v: date_value(info, f, ctx, *i) },
            _ => Dv { key: Key::Int(*i as i128), v: Value::from(*i) },
        },
        FieldValue::UnsignedLong(u) => {
            let v = if *u <= i64::MAX as u64 { Value::Num(Number::Int(*u as i64)) } else { Value::Num(Number::UInt(*u)) };
            Dv { key: Key::Int(*u as i128), v }
        }
        FieldValue::Double(d) => Dv { key: Key::Int(sortable_f64(*d)), v: Value::Num(Number::Float(*d)) },
        FieldValue::Float(x) => Dv { key: Key::Int(sortable_f32(*x)), v: float_value(*x) },
        FieldValue::Boolean(b) => Dv { key: Key::Int(*b as i128), v: Value::Bool(*b) },
        FieldValue::Date(ms) => Dv { key: Key::Int(*ms as i128), v: date_value(info, f, ctx, *ms) },
        FieldValue::Ip(b) => Dv { key: Key::Bytes(b.to_vec()), v: Value::Str(search_index::query::range::inet_to_string(b)) },
        FieldValue::Binary(b) => {
            use base64::Engine;
            Dv { key: Key::Bytes(b.clone()), v: Value::Str(base64::engine::general_purpose::STANDARD.encode(b)) }
        }
        FieldValue::GeoPoint(p) => {
            use search_codec::points::encode;
            let (la, lo) = (encode::encode_latitude(p.lat), encode::encode_longitude(p.lon));
            let mut m = Map::new();
            m.insert("lat".into(), Value::Num(Number::Float(encode::decode_latitude(la))));
            m.insert("lon".into(), Value::Num(Number::Float(encode::decode_longitude(lo))));
            Dv { key: Key::Int((((la as i64) << 32) | (lo as u32 as i64)) as i128), v: Value::Obj(m) }
        }
        FieldValue::Range { from, to, include_from, include_to } => {
            let (lo, hi) = ranges::bounds(info, info.kind, from, to, *include_from, *include_to).ok()?;
            let mut m = Map::new();
            m.insert("gte".into(), range_bound(info, f, ctx, &lo.value, true));
            m.insert("lte".into(), range_bound(info, f, ctx, &hi.value, false));
            // `BinaryRangeUtil` keeps a sorted set: IP ranges by their address strings.
            let key = match (&lo.value, &hi.value) {
                (ranges::BoundValue::Ip(a), ranges::BoundValue::Ip(b)) => {
                    let mut k = search_index::query::range::inet_to_string(a).into_bytes();
                    k.push(0);
                    k.extend_from_slice(search_index::query::range::inet_to_string(b).as_bytes());
                    k
                }
                _ => {
                    let mut k = lo.packed.clone();
                    k.extend_from_slice(&hi.packed);
                    k
                }
            };
            Dv { key: Key::Bytes(key), v: Value::Obj(m) }
        }
        FieldValue::DenseVector(v) => Dv { key: Key::Int(0), v: Value::Arr(v.iter().map(|x| float_value(*x)).collect()) },
        FieldValue::ByteVector(v) => Dv { key: Key::Int(0), v: Value::Arr(v.iter().map(|x| Value::from(*x as i64)).collect()) },
        FieldValue::Flattened(pairs) => Dv { key: Key::Int(0), v: Value::Arr(pairs.iter().map(|(k, v)| Value::Arr(vec![Value::Str(k.clone()), Value::Str(v.clone())])).collect()) },
        // One value per document, rendered from its doc value.
        FieldValue::Histogram(b) => Dv { key: Key::Int(0), v: histogram_json(f.ty, b)? },
        _ => return None,
    })
}

/// `ExponentialHistogramXContent.serialize`.
pub(crate) fn exponential_json(h: &search_aggs::exp_histogram::ExpHistogram) -> Value {
    let mut m = Map::new();
    m.insert("scale".into(), Value::from(h.scale as i64));
    if h.sum != 0.0 || h.value_count() > 0 {
        m.insert("sum".into(), Value::from(h.sum));
    }
    if !h.min.is_nan() {
        m.insert("min".into(), Value::from(h.min));
    }
    if !h.max.is_nan() {
        m.insert("max".into(), Value::from(h.max));
    }
    let (threshold, count) = (h.zero.zero_threshold(), h.zero.count);
    if count != 0 || threshold != 0.0 {
        let mut z = Map::new();
        if count != 0 {
            z.insert("count".into(), Value::from(count as i64));
        }
        if threshold != 0.0 {
            z.insert("threshold".into(), Value::from(threshold));
        }
        m.insert("zero".into(), Value::Obj(z));
    }
    for (k, buckets) in [("positive", &h.positive), ("negative", &h.negative)] {
        if !buckets.is_empty() {
            let indices: Vec<Value> = buckets.iter().map(|b| Value::from(b.0)).collect();
            let counts: Vec<Value> = buckets.iter().map(|b| Value::from(b.1 as i64)).collect();
            m.insert(k.into(), crate::obj! { "indices" => Value::Arr(indices), "counts" => Value::Arr(counts) });
        }
    }
    Value::Obj(m)
}

/// A pre-aggregated histogram's synthetic source: `histogram` as `{values, counts}`
/// (`HistogramFieldMapper`), `tdigest` as `{min, max, sum, centroids, counts}`
/// (`TDigestFieldMapper`; no summary for an empty digest), both without zero counts, and
/// `exponential_histogram` as [`exponential_json`].
fn histogram_json(ty: &str, b: &[u8]) -> Option<Value> {
    let centroids = |m: &mut Map, values_key: &str, bytes: &[u8]| -> Option<()> {
        let mut c = Vec::new();
        search_aggs::histogram_field::decode_histogram(bytes, &mut c).ok()?;
        m.insert(values_key.into(), Value::Arr(c.iter().map(|x| Value::from(x.0)).collect()));
        m.insert("counts".into(), Value::Arr(c.iter().map(|x| Value::from(x.1 as i64)).collect()));
        Some(())
    };
    let mut m = Map::new();
    match ty {
        "exponential_histogram" => {
            let mut h = search_aggs::exp_histogram::ExpHistogram::empty();
            search_aggs::exp_histogram::decode_into(b, &mut h).ok()?;
            return Some(exponential_json(&h));
        }
        "tdigest" => {
            let (summary, bytes) = crate::document::values::tdigest_parts(b)?;
            if let Some([min, max, sum]) = summary {
                m.insert("min".into(), Value::from(min));
                m.insert("max".into(), Value::from(max));
                m.insert("sum".into(), Value::from(sum));
            }
            centroids(&mut m, "centroids", bytes)?;
        }
        _ => centroids(&mut m, "values", b)?,
    }
    Some(Value::Obj(m))
}

fn range_bound(info: &FieldInfo, f: &FieldMapper, ctx: &MappingCtx, b: &ranges::BoundValue, low: bool) -> Value {
    use ranges::BoundValue as B;
    match (info.kind, b) {
        (Kind::IntegerRange, B::Long(v)) if (low && *v == i32::MIN as i64) || (!low && *v == i32::MAX as i64) => Value::Null,
        (Kind::LongRange | Kind::DateRange, B::Long(v)) if (low && *v == i64::MIN) || (!low && *v == i64::MAX) => Value::Null,
        (Kind::DateRange, B::Long(v)) => date_value(info, f, ctx, *v),
        (_, B::Long(v)) => Value::from(*v),
        (_, B::Float(x)) if x.is_infinite() => Value::Null,
        (_, B::Float(x)) => float_value(*x),
        (_, B::Double(d)) if d.is_infinite() => Value::Null,
        (_, B::Double(d)) => Value::Num(Number::Float(*d)),
        (_, B::Ip(ip)) if (low && *ip == [0u8; 16]) || (!low && *ip == [0xffu8; 16]) => Value::Null,
        (_, B::Ip(ip)) => Value::Str(search_index::query::range::inet_to_string(ip)),
    }
}

/// Writes one object (`ObjectMapper.SyntheticSourceFieldLoader.write`).
fn load(w: &Walk<'_>, o: &ObjectMapper, path: &str, scope: &Scope) -> Option<Map> {
    enum Entry<'s> {
        Raw(String, Vec<&'s Value>),
        Value(String, Value),
    }
    let mut entries: BTreeMap<String, Entry<'_>> = BTreeMap::new();
    for ig in scope.ignored.iter().filter(|i| i.parent == path) {
        let leaf = if path.is_empty() { ig.name.clone() } else { ig.name[path.len() + 1..].to_string() };
        match entries.entry(ig.name.clone()).or_insert_with(|| Entry::Raw(leaf, Vec::new())) {
            Entry::Raw(_, parts) => parts.push(&ig.value),
            Entry::Value(..) => {}
        }
    }
    for (name, child) in &o.children {
        let full = join(path, name);
        if entries.contains_key(&full) {
            continue;
        }
        let v = match child {
            Mapper::Field(f) => {
                let Some(info) = w.table.get(&full) else { continue };
                write_leaf(w, info, f, &full, scope)
            }
            Mapper::Object(c) if c.is_nested() => {
                if w.keep_of(&c.source_keep) == Keep::All {
                    continue;
                }
                let docs: Vec<Value> =
                    scope.nested.iter().filter(|(p, _)| *p == full).map(|(_, s)| Value::Obj(load(w, c, &full, s).unwrap_or_default())).collect();
                match docs.len() {
                    0 => None,
                    1 => docs.into_iter().next(),
                    _ => Some(Value::Arr(docs)),
                }
            }
            Mapper::Object(c) => load(w, c, &full, scope).map(Value::Obj),
            Mapper::Alias(_) => None,
        };
        if let Some(v) = v {
            entries.insert(full, Entry::Value(name.clone(), v));
        }
    }
    if entries.is_empty() {
        return None;
    }
    let mut out = Map::new();
    for (_, e) in entries {
        match e {
            Entry::Value(k, v) => {
                out.insert(k, v);
            }
            Entry::Raw(k, parts) => {
                out.insert(k, merged(&parts));
            }
        }
    }
    Some(out)
}

/// `XContentDataHelper.writeMerged`: one part as is, several as one array (arrays flattened).
fn merged(parts: &[&Value]) -> Value {
    if parts.len() == 1 {
        return parts[0].clone();
    }
    let mut out = Vec::new();
    for p in parts {
        match p {
            Value::Arr(a) => out.extend(a.iter().cloned()),
            other => out.push((*other).clone()),
        }
    }
    Value::Arr(out)
}

fn write_leaf(w: &Walk<'_>, info: &FieldInfo, f: &FieldMapper, full: &str, scope: &Scope) -> Option<Value> {
    if info.kind == Kind::ConstantKeyword {
        // The value comes from the mapping, for documents that had it.
        let leaf = scope.leaves.get(full)?;
        if leaf.dv.is_empty() {
            return None;
        }
        return info.constant_value.clone().map(Value::Str).or_else(|| leaf.dv.first().map(|d| d.v.clone()));
    }
    let leaf = scope.leaves.get(full)?;
    if info.kind == Kind::Flattened {
        return flattened(w, f, full, leaf);
    }
    let mut dv: Vec<Value>;
    if leaf.offsets {
        dv = leaf.dv.iter().map(|d| d.v.clone()).collect();
    } else {
        let mut sorted: Vec<&Dv> = leaf.dv.iter().collect();
        // Text read back from doc values comes sorted (and unique), as the doc values hold it.
        let in_order = info.store || info.kind == Kind::DenseVector || (matches!(info.kind, Kind::Text | Kind::MatchOnlyText) && !info.doc_values);
        let unique = matches!(info.kind, Kind::Keyword | Kind::Ip | Kind::Wildcard | Kind::Version | Kind::Binary | Kind::Text | Kind::MatchOnlyText)
            || info.kind.is_range();
        // `match_only_text` loads from a keyword multi-field as `text` does.
        let text_delegate = matches!(info.kind, Kind::Text | Kind::MatchOnlyText) && !info.store && delegate(w.table, w.ctx, f, full).is_some();
        if !in_order || text_delegate {
            sorted.sort_by(|a, b| a.key.partial_cmp(&b.key).unwrap_or(std::cmp::Ordering::Equal));
            if unique || text_delegate {
                sorted.dedup_by(|a, b| a.key == b.key);
            }
        }
        dv = sorted.into_iter().map(|d| d.v.clone()).collect();
    }
    let mut extra: Vec<&(Value, Vec<u8>)> = leaf.extra.iter().collect();
    if leaf.extra_sorted {
        extra.sort_by(|a, b| a.1.cmp(&b.1));
    }
    let mut all: Vec<Value> = Vec::with_capacity(dv.len() + extra.len());
    if matches!(info.kind, Kind::Text | Kind::MatchOnlyText) {
        // The fallback layer comes first.
        all.extend(extra.iter().map(|e| e.0.clone()));
        all.append(&mut dv);
    } else {
        all.append(&mut dv);
        all.extend(extra.iter().map(|e| e.0.clone()));
    }
    if leaf.offsets && leaf.array {
        return Some(Value::Arr(all));
    }
    match all.len() {
        0 => None,
        1 => all.pop(),
        _ => Some(Value::Arr(all)),
    }
}

/// `FlattenedFieldSyntheticWriterHelper`: keys sorted, values per key sorted and deduplicated,
/// dotted keys as objects. Keys mapped under the field's `properties` come from their own
/// mappers (typed values, the dotted key kept as one name), as `FlattenedDocValuesSyntheticFieldLoader`
/// appends its sub-field loaders.
fn flattened(w: &Walk<'_>, f: &FieldMapper, full: &str, leaf: &Leaf) -> Option<Value> {
    let mut by_key: BTreeMap<String, Vec<String>> = BTreeMap::new();
    for d in &leaf.dv {
        if let Value::Arr(pairs) = &d.v {
            for p in pairs {
                if let Value::Arr(kv) = p
                    && let (Some(Value::Str(k)), Some(Value::Str(v))) = (kv.first(), kv.get(1))
                {
                    by_key.entry(k.clone()).or_default().push(v.clone());
                }
            }
        }
    }
    if by_key.is_empty() {
        return None;
    }
    let mut root = Map::new();
    let mut mapped: Vec<(String, Value)> = Vec::new();
    for (k, mut vals) in by_key {
        if let Some(sub) = f.props.get(&k) {
            let sub_full = format!("{full}.{k}");
            if let Some(info) = w.table.get(&sub_full) {
                let mut dvs: Vec<Dv> = Vec::new();
                let mut ignored = std::collections::BTreeSet::new();
                for v in &vals {
                    for fv in crate::document::values::parse_standalone(sub, &sub_full, &Value::Str(v.clone()), w.ctx, &mut ignored).unwrap_or_default() {
                        dvs.extend(render(info, sub, &fv, w.ctx));
                    }
                }
                dvs.sort_by(|a, b| a.key.partial_cmp(&b.key).unwrap_or(std::cmp::Ordering::Equal));
                if matches!(info.kind, Kind::Keyword | Kind::Ip | Kind::Wildcard | Kind::Version) {
                    dvs.dedup_by(|a, b| a.key == b.key);
                }
                let mut out: Vec<Value> = dvs.into_iter().map(|d| d.v).collect();
                match out.len() {
                    0 => {}
                    1 => mapped.push((k, out.pop().unwrap_or(Value::Null))),
                    _ => mapped.push((k, Value::Arr(out))),
                }
                continue;
            }
        }
        vals.sort();
        vals.dedup();
        let v = if vals.len() == 1 { Value::Str(vals.pop().unwrap_or_default()) } else { Value::Arr(vals.into_iter().map(Value::Str).collect()) };
        insert_path(&mut root, &k, v);
    }
    for (k, v) in mapped {
        root.insert(k, v);
    }
    (!root.is_empty()).then_some(Value::Obj(root))
}

fn insert_path(m: &mut Map, key: &str, v: Value) {
    match key.split_once('.') {
        Some((head, rest)) if !head.is_empty() && !rest.is_empty() => {
            let child = m.entry(head.to_string()).or_insert_with(|| Value::Obj(Map::new()));
            if let Value::Obj(cm) = child {
                insert_path(cm, rest, v);
            }
        }
        _ => {
            m.insert(key.to_string(), v);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn numeric_keys_sort_like_doc_values() {
        let mut d = [1e20, -0.0, 0.1, -5.0, 5.0];
        d.sort_by_key(|x| sortable_f64(*x));
        assert_eq!(d, [-5.0, -0.0, 0.1, 5.0, 1e20]);
        let mut f = [3.4e38f32, -1.0, 1e-7];
        f.sort_by_key(|x| sortable_f32(*x));
        assert_eq!(f, [-1.0, 1e-7, 3.4e38]);
    }

    #[test]
    fn malformed_values_sort_by_their_encoding() {
        // `XContentDataHelper`: integers ('I') before strings ('S').
        let mut v = [Value::Str("hot garbage".into()), Value::from(7)];
        v.sort_by_key(xcontent_key);
        assert_eq!(v[0], Value::from(7));
        assert_eq!(xcontent_key(&Value::Bool(true)), b"t".to_vec());
        assert_eq!(xcontent_key(&Value::from(1i64 << 40))[0], b'L');
    }

    #[test]
    fn merged_parts_flatten_arrays() {
        let one = Value::from(1);
        let arr = Value::Arr(vec![Value::from(2), Value::from(3)]);
        assert_eq!(merged(&[&one]), one);
        assert_eq!(merged(&[&one, &arr]), Value::Arr(vec![Value::from(1), Value::from(2), Value::from(3)]));
    }

    #[test]
    fn flattened_keys_expand_to_objects() {
        let mut m = Map::new();
        insert_path(&mut m, "b.c", Value::from("1"));
        insert_path(&mut m, "b.d", Value::from("2"));
        insert_path(&mut m, "a", Value::from("x"));
        assert_eq!(Value::Obj(m).to_json(), r#"{"b":{"c":"1","d":"2"},"a":"x"}"#);
    }

    #[test]
    fn version_keys_order_numerically() {
        let key = |s: &str| search_aggs::version_string::encode(s).unwrap();
        assert!(key("1.2.0") < key("1.10.0"));
        assert!(key("1.0.0-alpha") < key("1.0.0"));
    }

    #[test]
    fn keep_modes_parse() {
        assert_eq!(Keep::parse(Some("arrays")), Keep::Arrays);
        assert_eq!(Keep::parse(Some("all")), Keep::All);
        assert_eq!(Keep::parse(None), Keep::None);
    }
}
