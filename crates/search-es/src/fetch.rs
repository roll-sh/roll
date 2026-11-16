//! What get and the search fetch phase read back from a stored document: `_source` (with
//! filtering), stored fields rendered by their field types, `_routing` and `_ignored`.

use crate::engine::fields::{FieldInfo, FieldTable, IGNORED_FIELD, Kind, ROUTING_FIELD, SOURCE_FIELD};
use crate::engine::source::SourceFilter;
use crate::error::{EsError, EsResult};
use crate::json::{Map, Number, Value};
use search_codec::stored::OwnedStoredValue;

/// `FetchSourceContext`: whether to return `_source` and how to filter it.
#[derive(Clone, Debug)]
pub struct FetchSource {
    pub fetch: bool,
    pub includes: Vec<String>,
    pub excludes: Vec<String>,
    /// `exclude_vectors`; by default vectors are left out unless `includes` are given.
    pub exclude_vectors: Option<bool>,
}

impl Default for FetchSource {
    fn default() -> Self {
        FetchSource { fetch: true, includes: Vec::new(), excludes: Vec::new(), exclude_vectors: None }
    }
}

impl FetchSource {
    pub fn filter(&self) -> Option<SourceFilter> {
        if self.includes.is_empty() && self.excludes.is_empty() { None } else { Some(SourceFilter::new(&self.includes, &self.excludes)) }
    }

    pub fn from_search(s: &crate::search_source::FetchSource) -> FetchSource {
        FetchSource { fetch: s.fetch, includes: s.includes.clone(), excludes: s.excludes.clone(), exclude_vectors: s.exclude_vectors }
    }

    /// `FetchSourceContext.fromXContent`: a boolean, a string or list of includes, or an object
    /// with `includes` / `excludes`.
    pub fn from_value(v: &Value) -> EsResult<FetchSource> {
        let list = |v: &Value| -> EsResult<Vec<String>> {
            match v {
                Value::Str(s) => Ok(vec![s.clone()]),
                Value::Arr(a) => a
                    .iter()
                    .map(|x| match x {
                        Value::Str(s) => Ok(s.clone()),
                        _ => Err(EsError::parse("Unknown key for a VALUE_NUMBER in [_source].")),
                    })
                    .collect(),
                _ => Err(EsError::parse("Unknown key for a START_OBJECT in [_source].")),
            }
        };
        Ok(match v {
            Value::Bool(b) => FetchSource { fetch: *b, ..Default::default() },
            Value::Str(_) | Value::Arr(_) => FetchSource { fetch: true, includes: list(v)?, ..Default::default() },
            Value::Obj(o) => {
                let mut fs = FetchSource::default();
                for (k, x) in o {
                    match k.as_str() {
                        "includes" | "include" => fs.includes = list(x)?,
                        "excludes" | "exclude" => fs.excludes = list(x)?,
                        // `top_hits` and `inner_hits` take it as the search's `_source` does.
                        "exclude_vectors" => {
                            fs.exclude_vectors = Some(match x {
                                Value::Bool(b) => *b,
                                Value::Str(s) if s == "true" || s == "false" => s == "true",
                                _ => return Err(EsError::parse(format!("Unknown key for a VALUE_STRING in [{k}]."))),
                            })
                        }
                        _ => return Err(EsError::parse(format!("Unknown key for a VALUE_STRING in [{k}]."))),
                    }
                }
                fs
            }
            _ => return Err(EsError::parse("Expected one of [VALUE_BOOLEAN, VALUE_STRING, START_ARRAY, START_OBJECT] but found [VALUE_NUMBER]")),
        })
    }

    /// `FetchSourceContext.parseFromRestRequest`: `_source` (a boolean or includes),
    /// `_source_includes`, `_source_excludes`. `None` when none is given.
    pub fn from_params(source: Option<&str>, includes: Option<&str>, excludes: Option<&str>) -> EsResult<Option<FetchSource>> {
        let list = |s: &str| -> Vec<String> { s.split(',').map(str::trim).filter(|x| !x.is_empty()).map(str::to_string).collect() };
        let mut fetch = None;
        let mut inc = None;
        if let Some(s) = source {
            // `Booleans.isTrue` / `isFalse`: anything else is a list of includes.
            match s {
                "true" | "" => fetch = Some(true),
                "false" => fetch = Some(false),
                _ => inc = Some(list(s)),
            }
        }
        if let Some(i) = includes {
            inc = Some(list(i));
        }
        let exc = excludes.map(list);
        if fetch.is_none() && inc.is_none() && exc.is_none() {
            return Ok(None);
        }
        check_ambiguous_source(inc.as_deref().unwrap_or_default(), exc.as_deref().unwrap_or_default())?;
        Ok(Some(FetchSource { fetch: fetch.unwrap_or(true), includes: inc.unwrap_or_default(), excludes: exc.unwrap_or_default(), exclude_vectors: None }))
    }
}

/// OpenSearch's `FetchSourceContext` refuses a path both included and excluded (the first
/// such include is named); Elasticsearch lets the exclude win.
pub fn check_ambiguous_source(includes: &[String], excludes: &[String]) -> EsResult<()> {
    if !crate::opensearch::active() {
        return Ok(());
    }
    match includes.iter().find(|i| excludes.contains(i)) {
        Some(i) => Err(crate::error::EsError::illegal_argument(format!("The same entry [{i}] cannot be both included and excluded in _source."))),
        None => Ok(()),
    }
}

/// The stored `_source` bytes of a document.
pub fn source_bytes(stored: &[(u32, OwnedStoredValue)]) -> Option<&[u8]> {
    stored.iter().find(|(f, _)| *f == SOURCE_FIELD).and_then(|(_, v)| match v {
        OwnedStoredValue::Bytes(b) => Some(b.as_slice()),
        _ => None,
    })
}

/// Whether hits and gets print `_routing`: not when the index keeps it as doc values
/// (`_routing: {doc_values: true}`, the default of strict columnar modes).
pub fn routing_shown(idx: &crate::meta::IndexMeta) -> bool {
    !idx.mapping.routing_doc_values.unwrap_or_else(|| crate::meta::index_mode::IndexMode::of(&idx.settings).is_strict_columnar())
}

pub fn routing(stored: &[(u32, OwnedStoredValue)]) -> Option<String> {
    stored.iter().find(|(f, _)| *f == ROUTING_FIELD).and_then(|(_, v)| match v {
        OwnedStoredValue::Str(s) => Some(s.clone()),
        _ => None,
    })
}

pub fn ignored(stored: &[(u32, OwnedStoredValue)]) -> Vec<String> {
    stored
        .iter()
        .filter(|(f, _)| *f == IGNORED_FIELD)
        .filter_map(|(_, v)| match v {
            OwnedStoredValue::Str(s) => Some(s.clone()),
            _ => None,
        })
        .collect()
}

/// Parses stored `_source` bytes.
pub fn parse_source(bytes: &[u8]) -> EsResult<Value> {
    Ok(crate::json::parse(bytes)?.unwrap_or(Value::Null))
}

/// What the index changes in a fetched `_source` before it is filtered: synthesized on
/// request (`force_synthetic_source`), vectors left out (`index.mapping.exclude_source_vectors`).
#[derive(Clone, Copy)]
pub struct SourceView<'a> {
    pub idx: &'a crate::meta::IndexMeta,
    pub table: &'a FieldTable,
    pub force_synthetic: bool,
    /// The search fetch phase, whose forced synthetic `_source` keeps every vector
    /// (`get` leaves them out of it as from any other source), and whose hits print their
    /// vectors before the view applies (`search::hits`: the view only keeps or drops them).
    pub search: bool,
}

impl SourceView<'_> {
    /// The `_source` as fetched under `fs` (filtering included).
    pub fn apply(&self, v: Value, fs: &FetchSource) -> Value {
        self.apply_parsed(v, fs, false)
    }

    /// [`apply`](Self::apply), where `parsed` says another fetch sub-phase already read the
    /// source as a map (`fields`): Elasticsearch then filters the map (`Source.filter`), where
    /// `*` spans dots, instead of the bytes.
    pub fn apply_parsed(&self, v: Value, fs: &FetchSource, parsed: bool) -> Value {
        use crate::engine::synthetic;
        let mut v = v;
        // `semantic_text` embeddings (`_inference_fields`) are vectors too: shown only with
        // `exclude_vectors: false` or when the includes name them.
        if let Value::Obj(m) = &mut v
            && fs.exclude_vectors != Some(false)
            && !fs.includes.iter().any(|i| i.starts_with("_inference_fields"))
        {
            m.retain(|k, _| !crate::xpack::inference::semantic::is_inference_key(k));
        }
        // A forced synthetic `_source` of a stored-source index is built whole from the
        // document. A search shows its vectors; `get` leaves them out as from a synthetic
        // source (all of them unless the request filters it).
        let forced = self.force_synthetic && synthetic::source_mode(self.idx) == synthetic::SourceMode::Stored;
        if forced && let Value::Obj(m) = &v {
            let ctx = self.idx.mapping_ctx();
            v = Value::Obj(synthetic::synthesize(&self.idx.mapping, &ctx, self.table, synthetic::options(self.idx), m));
        }
        if self.table.has_vectors
            && !(forced && self.search)
            && self.idx.settings.get("index.mapping.exclude_source_vectors") != Some("false")
            && let Value::Obj(m) = &mut v
        {
            // Vectors the includes name are shown unless `exclude_vectors` is false (all shown).
            // Synthetic sources filtered by the request show them all as well (the synthetic
            // loader only leaves vectors out when it builds the whole document).
            let synthetic = self.force_synthetic || synthetic::source_mode(self.idx) == synthetic::SourceMode::Synthetic;
            let filtered = !fs.includes.is_empty() || !fs.excludes.is_empty();
            let kept = if fs.exclude_vectors == Some(false) || (synthetic && filtered && fs.exclude_vectors.is_none()) {
                synthetic::vectors_keeping(self.table, "", m, None, !self.search)
            } else {
                synthetic::vectors_keeping(self.table, "", m, Some(&crate::engine::source::Globs::new(&fs.includes)), !self.search)
            };
            // Patching vectors back in turns the source into a map, which Elasticsearch
            // filters with map semantics (`*` spans dots) rather than over the bytes.
            if kept && filtered {
                return Value::Obj(crate::engine::source::SourceFilter::over_map(&fs.includes, &fs.excludes).apply(m));
            }
        }
        match (fs.filter(), v) {
            (Some(_), Value::Obj(m)) if parsed => Value::Obj(crate::engine::source::SourceFilter::over_map(&fs.includes, &fs.excludes).apply(&m)),
            (Some(f), Value::Obj(m)) => Value::Obj(f.apply(&m)),
            (_, v) => v,
        }
    }
}

/// [`render_source`] through an index's [`SourceView`].
pub fn render_source_view(stored: &[(u32, OwnedStoredValue)], fs: &FetchSource, view: &SourceView) -> EsResult<Option<Value>> {
    if !fs.fetch {
        return Ok(None);
    }
    let Some(b) = source_bytes(stored) else { return Ok(None) };
    Ok(Some(view.apply(parse_source(b)?, fs)))
}

/// The `_source` to return, filtered; `None` when there is none or it isn't wanted.
pub fn render_source(stored: &[(u32, OwnedStoredValue)], fs: &FetchSource) -> EsResult<Option<Value>> {
    if !fs.fetch {
        return Ok(None);
    }
    let Some(b) = source_bytes(stored) else { return Ok(None) };
    let v = parse_source(b)?;
    Ok(Some(match (fs.filter(), v) {
        (Some(f), Value::Obj(m)) => Value::Obj(f.apply(&m)),
        (_, v) => v,
    }))
}

/// A stored value of a field as the response prints it (`MappedFieldType.valueForDisplay`).
pub fn stored_value(f: &FieldInfo, v: &OwnedStoredValue) -> Value {
    match (f.kind, v) {
        (Kind::Boolean, OwnedStoredValue::Str(s)) => Value::Bool(s == "T"),
        (Kind::Date | Kind::DateNanos, OwnedStoredValue::Long(ms)) => match &f.date_format {
            Some(fmt) => {
                Value::Str(fmt.format(instant_of(f.kind == Kind::DateNanos, *ms), &crate::time::Zone::UTC))
            }
            None => Value::from(*ms),
        },
        (Kind::Ip, OwnedStoredValue::Bytes(b)) if b.len() == 16 => Value::Str(search_index::query::range::inet_to_string(b)),
        // Elasticsearch stores the unsigned value's text (`Long.toUnsignedString`).
        (Kind::UnsignedLong, OwnedStoredValue::Long(l)) => Value::Str(((*l as u64) ^ (1 << 63)).to_string()),
        (Kind::Binary, OwnedStoredValue::Bytes(b)) => {
            use base64::Engine as _;
            Value::Str(base64::engine::general_purpose::STANDARD.encode(b))
        }
        (_, OwnedStoredValue::Str(s)) => Value::Str(s.clone()),
        (_, OwnedStoredValue::Bytes(b)) => {
            use base64::Engine as _;
            Value::Str(base64::engine::general_purpose::STANDARD.encode(b))
        }
        (_, OwnedStoredValue::Int(i)) => Value::from(*i as i64),
        (_, OwnedStoredValue::Long(l)) => Value::from(*l),
        (_, OwnedStoredValue::Float(x)) => Value::Num(Number::Float(java_float_as_double(*x))),
        (_, OwnedStoredValue::Double(x)) => Value::Num(Number::Float(*x)),
    }
}

/// An epoch value of a `date` (millis) or `date_nanos` (nanos) field.
pub fn instant_of(nanos: bool, v: i64) -> crate::time::Instant {
    if nanos {
        crate::time::Instant { secs: v.div_euclid(1_000_000_000), nanos: v.rem_euclid(1_000_000_000) as u32 }
    } else {
        crate::time::Instant::from_millis(v)
    }
}

/// A Java `float` printed by Jackson as a float (shortest digits of the float, not the double).
pub fn java_float_as_double(x: f32) -> f64 {
    crate::json::write::java_float_value(x)
}

/// `stored_fields`: values of the requested stored fields (patterns allowed), by field name.
pub fn stored_fields(table: &FieldTable, stored: &[(u32, OwnedStoredValue)], names: &[String]) -> Map {
    let mut out = Map::new();
    for pattern in names {
        if pattern == "_source" || pattern == "_none_" {
            continue;
        }
        for name in table.matching(pattern) {
            let Some(f) = table.get(&name) else { continue };
            if !f.store {
                continue;
            }
            let vals: Vec<Value> = stored.iter().filter(|(n, _)| *n == f.number).map(|(_, v)| stored_value(f, v)).collect();
            if !vals.is_empty() && !out.contains_key(&name) {
                out.insert(name, Value::Arr(vals));
            }
        }
    }
    out
}

/// An `illegal_argument_exception` for stored-field requests on non-stored metadata.
pub fn not_stored(name: &str) -> EsError {
    EsError::illegal_argument(format!("field [{name}] isn't a leaf field"))
}
