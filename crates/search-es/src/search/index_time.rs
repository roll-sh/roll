//! Index-time scripts: mapped fields with a `script` (`keyword`, `long`, `double`, `date`,
//! `boolean`, `ip`, `geo_point` and the other numeric types), whose values a script computes
//! from the document being indexed rather than reading them from `_source`.
//!
//! After a document is parsed, each script field's script runs over it (Elasticsearch's
//! `DocumentParserContext.executeIndexTimeScripts`): `doc[...]` reads the parsed values of the
//! document as its doc values would hold them (another script field's are computed first, on
//! demand), `params._source` is the document and `params._fields` its stored fields. The
//! emitted values are added to the document like parsed values of the field, so they are
//! indexed and kept in doc values as the mapping says. A failing script fails the document
//! (`Error executing script on field [f]`), or, with `on_script_error: continue`, records the
//! field in `_ignored`.
//!
//! At search time the fields are ordinary mapped fields; only the `fields` option runs the
//! script again (`FieldValues.valueFetcher`), giving the values in emission order
//! ([`fetch_values`]).

use super::runtime::{self, RtField, RtType, RtValue};
use crate::document::{DocField, FieldValue, ParsedDocument};
use crate::error::{EsError, EsResult};
use crate::json::Value;
use crate::mapping::{Mapping, MapperRef};
use crate::query::script::Script;
use search_painless::host::{DocValues, Host, HostError};
use std::cell::RefCell;
use std::sync::Arc;

/// The script fields of a mapping.
#[derive(Debug, Default)]
pub struct IndexScripts {
    pub fields: Vec<Arc<RtField>>,
    /// Each field's mapping type (`integer`, `float`, …: the value type of `RtField::ty` is
    /// the script context's).
    types: Vec<String>,
}

impl IndexScripts {
    fn position(&self, name: &str) -> Option<usize> {
        self.fields.iter().position(|f| f.name == name)
    }

    pub fn get(&self, name: &str) -> Option<&Arc<RtField>> {
        self.fields.iter().find(|f| f.name == name)
    }
}

/// The script context type of a mapped field type that takes a `script`.
fn script_type(ty: &str) -> Option<RtType> {
    Some(match ty {
        "keyword" => RtType::Keyword,
        "long" | "integer" | "short" | "byte" => RtType::Long,
        "double" | "float" | "half_float" => RtType::Double,
        "date" | "date_nanos" => RtType::Date,
        "boolean" => RtType::Boolean,
        "ip" => RtType::Ip,
        "geo_point" => RtType::GeoPoint,
        _ => return None,
    })
}

fn build(mapping: &Mapping) -> IndexScripts {
    let mut out = IndexScripts::default();
    for (path, m) in mapping.field_paths() {
        let MapperRef::Field(f) = m else { continue };
        let Some(v) = f.raw("script").filter(|v| !v.is_null()) else { continue };
        let Some(ty) = script_type(f.ty) else { continue };
        let mut p = crate::query::XParser::from_value(v);
        p.next_token();
        let Ok(script) = Script::parse(&mut p) else { continue };
        let format = match f.raw("format").and_then(Value::as_str) {
            Some(fmt) => crate::time::DateFormatter::new(fmt).map(Arc::new).unwrap_or_else(|_| runtime::default_format()),
            None => runtime::default_format(),
        };
        let lenient = f.raw("on_script_error").and_then(Value::as_str) == Some("continue");
        out.fields.push(Arc::new(RtField { name: path, ty, script: Some(script), composite_key: None, format, lenient }));
        out.types.push(f.ty.to_string());
    }
    out
}

/// The script fields of `mapping`, cached by the mapping's identity.
pub fn of(mapping: &Arc<Mapping>) -> Arc<IndexScripts> {
    type Cache = std::collections::HashMap<usize, (std::sync::Weak<Mapping>, Arc<IndexScripts>)>;
    static CACHE: std::sync::OnceLock<parking_lot::Mutex<Cache>> = std::sync::OnceLock::new();
    let cache = CACHE.get_or_init(Default::default);
    let key = Arc::as_ptr(mapping) as usize;
    if let Some((w, f)) = cache.lock().get(&key)
        && std::ptr::eq(w.as_ptr(), Arc::as_ptr(mapping))
        && w.strong_count() > 0
    {
        return f.clone();
    }
    let fields = Arc::new(build(mapping));
    let mut c = cache.lock();
    if c.len() >= 64 {
        c.retain(|_, (w, _)| w.strong_count() > 0);
    }
    c.insert(key, (Arc::downgrade(mapping), fields.clone()));
    fields
}

/// Whether `mapping` has script fields.
pub fn has_scripts(mapping: &Arc<Mapping>) -> bool {
    !of(mapping).fields.is_empty()
}

/// Runs the index-time scripts of `mapping` over a parsed document (`source` is its
/// `_source`), adding their values to `parsed`.
pub fn apply(scripts: &crate::scripting::ScriptService, st: &crate::meta::ClusterState, mapping: &Arc<Mapping>, source: &Value, parsed: &mut ParsedDocument) -> EsResult<()> {
    let fields = of(mapping);
    if fields.fields.is_empty() {
        return Ok(());
    }
    let state = State {
        scripts,
        st,
        mapping,
        source,
        parsed,
        fields: &fields,
        computed: RefCell::new(vec![None; fields.fields.len()]),
        running: RefCell::new(Vec::new()),
        painless_source: RefCell::new(None),
    };
    let mut ignored = Vec::new();
    for (i, f) in fields.fields.iter().enumerate() {
        match state.compute(i) {
            Ok(_) => {}
            Err(_) if f.lenient => ignored.push(f.name.clone()),
            Err(e) => return Err(EsError::document_parsing(format!("[-1:-1] Error executing script on field [{}]", f.name)).caused_by(e)),
        }
    }
    let computed = state.computed.into_inner();
    for (i, vals) in computed.into_iter().enumerate() {
        let Some(Ok(vals)) = vals else { continue };
        let f = &fields.fields[i];
        let name: Arc<str> = Arc::from(f.name.as_str());
        for v in vals {
            if let Some(value) = to_field_value(&fields.types[i], v) {
                parsed.fields.push(DocField { name: name.clone(), value });
            }
        }
    }
    if !ignored.is_empty() {
        parsed.ignored.extend(ignored);
        parsed.ignored.sort();
        parsed.ignored.dedup();
    }
    Ok(())
}

/// A computed value as the mapped field type indexes it.
fn to_field_value(ty: &str, v: RtValue) -> Option<FieldValue> {
    Some(match (ty, v) {
        (_, RtValue::Str(s)) => FieldValue::Keyword(s),
        ("float" | "half_float", RtValue::Double(d)) => FieldValue::Float(d as f32),
        (_, RtValue::Double(d)) => FieldValue::Double(d),
        (_, RtValue::Long(l)) => FieldValue::Long(l),
        ("date_nanos", RtValue::Date(ms)) => FieldValue::Date(ms.checked_mul(1_000_000)?),
        (_, RtValue::Date(ms)) => FieldValue::Date(ms),
        (_, RtValue::Bool(b)) => FieldValue::Boolean(b),
        (_, RtValue::Ip(b)) => FieldValue::Ip(b),
        (_, RtValue::Geo(lat, lon)) => FieldValue::GeoPoint(crate::query::geo::GeoPoint { lat, lon }),
    })
}

/// A parsed value as doc values of a field of type `ty` hold it.
fn to_rt_value(v: &FieldValue) -> Option<RtValue> {
    Some(match v {
        FieldValue::Keyword(s) => RtValue::Str(s.clone()),
        FieldValue::Long(l) => RtValue::Long(*l),
        FieldValue::Double(d) => RtValue::Double(*d),
        FieldValue::Float(f) => RtValue::Double(*f as f64),
        FieldValue::Boolean(b) => RtValue::Bool(*b),
        FieldValue::Date(ms) => RtValue::Date(*ms),
        FieldValue::Ip(b) => RtValue::Ip(*b),
        FieldValue::GeoPoint(p) => RtValue::Geo(p.lat, p.lon),
        _ => return None,
    })
}

/// The doc values type of a mapped field type (`None`: not readable through `doc[...]` here).
fn doc_type(ty: &str) -> Option<RtType> {
    Some(match ty {
        "keyword" | "constant_keyword" | "wildcard" => RtType::Keyword,
        "long" | "integer" | "short" | "byte" => RtType::Long,
        "double" | "float" | "half_float" => RtType::Double,
        "date" => RtType::Date,
        "boolean" => RtType::Boolean,
        "ip" => RtType::Ip,
        "geo_point" => RtType::GeoPoint,
        _ => return None,
    })
}

/// A script field's values in emission order, or why its script failed.
type Computed = Result<Vec<RtValue>, EsError>;

struct State<'a> {
    scripts: &'a crate::scripting::ScriptService,
    st: &'a crate::meta::ClusterState,
    mapping: &'a Mapping,
    source: &'a Value,
    parsed: &'a ParsedDocument,
    fields: &'a IndexScripts,
    /// Each script field's values, in emission order, once computed.
    computed: RefCell<Vec<Option<Computed>>>,
    /// The script fields being computed, outermost first.
    running: RefCell<Vec<usize>>,
    painless_source: RefCell<Option<search_painless::Value>>,
}

impl State<'_> {
    fn compute(&self, i: usize) -> EsResult<Vec<RtValue>> {
        if let Some(r) = &self.computed.borrow()[i] {
            return r.clone();
        }
        if self.running.borrow().contains(&i) {
            let mut chain: Vec<&str> = self.running.borrow().iter().map(|&j| self.fields.fields[j].name.as_str()).collect();
            chain.push(&self.fields.fields[i].name);
            return Err(EsError::illegal_argument(format!("Cyclic dependency detected while resolving runtime fields: {}", chain.join(" -> "))));
        }
        self.running.borrow_mut().push(i);
        let r = self.run(i);
        self.running.borrow_mut().pop();
        self.computed.borrow_mut()[i] = Some(r.clone());
        r
    }

    fn run(&self, i: usize) -> EsResult<Vec<RtValue>> {
        let f = &self.fields.fields[i];
        let Some(script) = &f.script else { return Ok(Vec::new()) };
        let bound = self.scripts.compile(self.st, script, f.ty.context())?;
        let Some(p) = bound.painless() else { return Ok(Vec::new()) };
        let mut inst = p.instance();
        inst.set_params(&bound.params);
        inst.set_field_name(&f.name);
        let mut host = DocumentHost { state: self };
        inst.execute(&mut host).map_err(crate::scripting::script_error)?;
        let mut out = Vec::new();
        for v in inst.emitted() {
            runtime::from_painless(f.ty, v, &mut out);
        }
        if f.ty == RtType::Boolean {
            out.sort_by(|a, b| a.partial_cmp_same(b));
        }
        Ok(out)
    }

    /// The values of `field` as doc values hold them (`None`: not a field `doc[...]` reads).
    fn doc_values(&self, field: &str) -> Result<Option<(RtType, Vec<RtValue>)>, HostError> {
        if let Some(i) = self.fields.position(field) {
            let ty = self.fields.fields[i].ty;
            let mut vals = self.compute(i).map_err(|e| HostError::illegal_argument(runtime::innermost_reason(&e)))?;
            if !matches!(ty, RtType::Keyword | RtType::GeoPoint) {
                vals.sort_by(|a, b| a.partial_cmp_same(b));
            }
            return Ok(Some((ty, vals)));
        }
        let Some(MapperRef::Field(fm)) = self.mapping.lookup(field) else { return Ok(None) };
        let Some(ty) = doc_type(fm.ty) else { return Ok(None) };
        let mut vals: Vec<RtValue> = self.parsed.fields.iter().filter(|f| &*f.name == field).filter_map(|f| to_rt_value(&f.value)).collect();
        match ty {
            RtType::GeoPoint => {}
            // Sorted set doc values: each term once.
            RtType::Keyword => {
                runtime::sort_as_doc_values(&mut vals);
                vals.dedup();
            }
            _ => vals.sort_by(|a, b| a.partial_cmp_same(b)),
        }
        Ok(Some((ty, vals)))
    }
}

/// The host of an index-time script: the document being indexed.
struct DocumentHost<'s, 'a> {
    state: &'s State<'a>,
}

impl Host for DocumentHost<'_, '_> {
    fn doc_values(&mut self, field: &str, out: &mut DocValues) -> Result<bool, HostError> {
        match self.state.doc_values(field)? {
            Some((ty, vals)) => {
                runtime::fill_doc_values(ty, &vals, out);
                Ok(true)
            }
            None => Ok(false),
        }
    }

    fn source(&mut self) -> Option<search_painless::Value> {
        let mut cached = self.state.painless_source.borrow_mut();
        if cached.is_none() {
            *cached = search_painless::xcontent::parse_source(&self.state.source.to_json()).ok();
        }
        cached.clone()
    }

    fn stored_field(&mut self, field: &str) -> Result<Option<Vec<search_painless::Value>>, HostError> {
        let Some(MapperRef::Field(fm)) = self.state.mapping.lookup(field) else { return Ok(None) };
        if fm.raw("store").and_then(Value::as_bool) != Some(true) {
            return Ok(Some(Vec::new()));
        }
        Ok(Some(self.state.parsed.fields.iter().filter(|f| &*f.name == field).filter_map(|f| stored_value(&f.value)).collect()))
    }

    fn now_millis(&mut self) -> i64 {
        crate::time::now_millis()
    }
}

/// A stored value as `params._fields` shows it.
fn stored_value(v: &FieldValue) -> Option<search_painless::Value> {
    use search_painless::Value as P;
    Some(match v {
        FieldValue::Keyword(s) | FieldValue::Text(s) => P::str(s),
        FieldValue::Long(l) => P::Long(*l),
        FieldValue::Double(d) => P::Double(*d),
        FieldValue::Float(f) => P::Float(*f),
        FieldValue::Boolean(b) => P::Bool(*b),
        FieldValue::Date(ms) => search_painless::value::zoned_date_time_millis(*ms),
        FieldValue::Ip(b) => P::str(&search_index::query::range::inet_to_string(b)),
        _ => return None,
    })
}

/// The values of index-time script field `f` for a hit, in emission order, as the `fields`
/// option fetches them (the script runs again on the indexed document).
pub fn fetch_values(ev: &runtime::Evaluator, seg: &search_index::SegmentContext, doc: u32) -> EsResult<Vec<RtValue>> {
    ev.leaf(seg)?.emitted_values(doc)
}
