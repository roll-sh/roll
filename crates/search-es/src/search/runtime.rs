//! Runtime fields (`RuntimeField`): the mapping's `runtime` section and a request's
//! `runtime_mappings` (which override mapping definitions of the same name).
//!
//! A runtime field's values are computed per document: by its script, compiled in the type's
//! context (`keyword_field`, `long_field`, …; the sub-fields of a `composite` field share their
//! parent's `composite_field` script and read their key of its emitted map), or, without a
//! script, read from `_source` at the field's path and parsed as the type parses values. They
//! serve:
//!
//! - the `fields` option ([`Evaluator::values`] → [`render`], dates in the field's `format`);
//! - queries ([`RuntimeQuery`], a `FilterScript`: `term`/`terms`/`match` as value sets, `range`,
//!   `prefix` and `exists` as predicates over the computed values — Elasticsearch's
//!   `AbstractScriptFieldQuery`s, which are as expensive as they sound: every candidate
//!   document runs the script);
//! - aggregations, rewritten into value scripts whose host runs the runtime script
//!   ([`agg_script`]).

use super::scripts::{DocHost, Leaf};
use crate::engine::fields::FieldTable;
use crate::error::{EsError, EsResult};
use crate::json::{Map, Number, Value};
use crate::mapping::{Mapping, RuntimeField};
use crate::query::script::Script;
use search_index::SegmentContext;
use std::collections::BTreeMap;
use std::sync::Arc;

/// The value type of a runtime field.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RtType {
    Keyword,
    Long,
    Double,
    Date,
    Boolean,
    Ip,
    GeoPoint,
}

impl RtType {
    fn of(ty: &str) -> Option<RtType> {
        Some(match ty {
            "keyword" => RtType::Keyword,
            "long" => RtType::Long,
            "double" => RtType::Double,
            "date" => RtType::Date,
            "boolean" => RtType::Boolean,
            "ip" => RtType::Ip,
            "geo_point" => RtType::GeoPoint,
            _ => return None,
        })
    }

    pub(crate) fn context(self) -> search_painless::Context {
        use search_painless::Context as C;
        match self {
            RtType::Keyword => C::KeywordField,
            RtType::Long => C::LongField,
            RtType::Double => C::DoubleField,
            RtType::Date => C::DateField,
            RtType::Boolean => C::BooleanField,
            RtType::Ip => C::IpField,
            RtType::GeoPoint => C::GeoPointField,
        }
    }

    /// The mapping type name.
    pub fn name(self) -> &'static str {
        match self {
            RtType::Keyword => "keyword",
            RtType::Long => "long",
            RtType::Double => "double",
            RtType::Date => "date",
            RtType::Boolean => "boolean",
            RtType::Ip => "ip",
            RtType::GeoPoint => "geo_point",
        }
    }
}

/// One runtime field.
#[derive(Debug)]
pub struct RtField {
    pub name: String,
    pub ty: RtType,
    /// The script (a composite sub-field's is its parent's).
    pub script: Option<Script>,
    /// A `composite` sub-field: the key of its parent's emitted map.
    pub composite_key: Option<String>,
    pub format: Arc<crate::time::DateFormatter>,
    /// `on_script_error: continue`: a failing script gives the document no values.
    pub lenient: bool,
}

/// The runtime fields a search sees.
#[derive(Clone, Debug, Default)]
pub struct RuntimeFields {
    fields: BTreeMap<String, Arc<RtField>>,
}

fn script_of(name: &str, rf: &RuntimeField) -> EsResult<Option<Script>> {
    match rf.params.get("script") {
        None | Some(Value::Null) => Ok(None),
        Some(v) => {
            let mut p = crate::query::XParser::from_value(v);
            p.next_token();
            Script::parse(&mut p).map(Some).map_err(|e| EsError::mapper_parsing(format!("Failed to parse script of runtime field [{name}]: {}", e.reason())))
        }
    }
}

fn lenient(rf: &RuntimeField) -> bool {
    rf.params.get("on_script_error").and_then(Value::as_str) == Some("continue")
}

impl RuntimeFields {
    /// The mapping's runtime fields, overridden by the request's `runtime_mappings`.
    pub fn of(mapping: &Mapping, request: Option<&Map>) -> EsResult<RuntimeFields> {
        let mut out = RuntimeFields::default();
        for (name, rf) in &mapping.runtime {
            out.add(name, rf)?;
        }
        if let Some(req) = request {
            for (name, v) in req {
                let rf = RuntimeField::parse_def(name, v)?;
                out.add(name, &rf)?;
            }
        }
        Ok(out)
    }

    fn add(&mut self, name: &str, rf: &RuntimeField) -> EsResult<()> {
        let format = |rf: &RuntimeField| -> EsResult<Arc<crate::time::DateFormatter>> {
            match rf.params.get("format").and_then(Value::as_str) {
                None => Ok(default_format()),
                Some(p) => crate::time::DateFormatter::new(p).map(Arc::new).map_err(EsError::illegal_argument),
            }
        };
        if rf.ty == "composite" {
            let script = script_of(name, rf)?;
            if let Some(Value::Obj(subs)) = rf.params.get("fields") {
                for (sub, def) in subs {
                    let ty = def.get("type").and_then(Value::as_str).and_then(RtType::of).unwrap_or(RtType::Keyword);
                    let sub_rf = RuntimeField::parse_def(sub, def).unwrap_or(RuntimeField { ty: ty.name().into(), params: Map::new() });
                    let full = format!("{name}.{sub}");
                    self.fields.insert(full.clone(), Arc::new(RtField { name: full, ty, script: script.clone(), composite_key: Some(sub.clone()), format: format(&sub_rf)?, lenient: lenient(rf) }));
                }
            }
            return Ok(());
        }
        // `lookup` fields fetch from another index; they are not evaluated here.
        let Some(ty) = RtType::of(&rf.ty) else { return Ok(()) };
        self.fields.insert(name.to_string(), Arc::new(RtField { name: name.to_string(), ty, script: script_of(name, rf)?, composite_key: None, format: format(rf)?, lenient: lenient(rf) }));
        Ok(())
    }

    pub fn is_empty(&self) -> bool {
        self.fields.is_empty()
    }

    /// The runtime fields of `mapping` (parsed once per mapping version: an index's every
    /// search would parse its runtime section otherwise), overridden by the request's.
    pub fn of_index(mapping: &Arc<Mapping>, request: Option<&Map>) -> EsResult<RuntimeFields> {
        let mut out = mapping_fields(mapping)?.fields.clone();
        if let Some(req) = request {
            for (name, v) in req {
                let rf = RuntimeField::parse_def(name, v)?;
                out.add(name, &rf)?;
            }
        }
        Ok(out)
    }

    pub fn get(&self, name: &str) -> Option<&Arc<RtField>> {
        self.fields.get(name)
    }

    /// Names matching a `fields` pattern, sorted.
    pub fn matching(&self, pattern: &str) -> Vec<String> {
        self.fields.keys().filter(|k| crate::xcontent::filter::glob_match(pattern, k)).cloned().collect()
    }
}

/// `RuntimeField` parsing compiles each script of a mapping's runtime section (the script
/// compiler is part of the mapping parser): one that doesn't compile fails the mapping as
/// `Failed to parse mapping: compile error`, caused by the `script_exception`.
pub fn check_mapping_scripts(scripts: &crate::scripting::ScriptService, st: &crate::meta::ClusterState, mapping: &Mapping) -> EsResult<()> {
    if mapping.runtime.is_empty() {
        return Ok(());
    }
    let fields = RuntimeFields::of(mapping, None)?;
    let mut done: Vec<&Script> = Vec::new();
    for f in fields.fields.values() {
        let Some(s) = &f.script else { continue };
        // A composite's sub-fields share their parent's script.
        if done.iter().any(|d| std::ptr::eq(*d, s) || **d == *s) {
            continue;
        }
        let ctx = if f.composite_key.is_some() { search_painless::Context::CompositeField } else { f.ty.context() };
        scripts.compile(st, s, ctx).map_err(crate::mapping::wrap_parse_error)?;
        done.push(s);
    }
    Ok(())
}

/// `strict_date_optional_time||epoch_millis`, shared by the runtime fields without a format.
pub(crate) fn default_format() -> Arc<crate::time::DateFormatter> {
    static DEFAULT: std::sync::OnceLock<Arc<crate::time::DateFormatter>> = std::sync::OnceLock::new();
    DEFAULT.get_or_init(|| Arc::new(crate::time::DateFormatter::new(crate::time::DateFormatter::DEFAULT).expect("the default date format parses"))).clone()
}

/// The parsed runtime section of a mapping, cached by the mapping's identity (a mapping
/// change publishes a new `Arc`; entries of dropped mappings are purged as others arrive).
fn mapping_fields(mapping: &Arc<Mapping>) -> EsResult<Arc<ScopeFields>> {
    type Cache = std::collections::HashMap<usize, (std::sync::Weak<Mapping>, Arc<ScopeFields>)>;
    static CACHE: std::sync::OnceLock<parking_lot::Mutex<Cache>> = std::sync::OnceLock::new();
    let cache = CACHE.get_or_init(Default::default);
    let key = Arc::as_ptr(mapping) as usize;
    if let Some((w, f)) = cache.lock().get(&key)
        && std::ptr::eq(w.as_ptr(), Arc::as_ptr(mapping))
        && w.strong_count() > 0
    {
        return Ok(f.clone());
    }
    let fields = Arc::new(ScopeFields::new(RuntimeFields::of(mapping, None)?, mapping, None));
    let mut c = cache.lock();
    if c.len() >= 64 {
        c.retain(|_, (w, _)| w.strong_count() > 0);
    }
    c.insert(key, (Arc::downgrade(mapping), fields.clone()));
    Ok(fields)
}

/// Runtime fields over a time series index (`TimeSeriesRoutingHashFieldMapper` /
/// `MappedFieldType.validateMatchedRoutingPath`): a request's may not match the routing path,
/// and none may shadow a dimension or metric.
pub fn check_time_series(idx: &crate::meta::IndexMeta, names: &[&str], from_request: bool) -> EsResult<()> {
    if !crate::engine::tsdb::is_time_series(idx) {
        return Ok(());
    }
    let routing = idx.settings.get_list("index.routing_path").unwrap_or_default();
    for name in names {
        if from_request && routing.iter().any(|p| crate::xcontent::filter::glob_match(p, name)) {
            return Err(EsError::illegal_argument(format!("runtime fields may not match [routing_path] but [{name}] matched")));
        }
        if let Some(crate::mapping::MapperRef::Field(f)) = idx.mapping.lookup(name) {
            if f.raw("time_series_dimension").and_then(Value::as_bool) == Some(true) {
                return Err(EsError::mapper_parsing(format!("Field [{name}] attempted to shadow a time_series_dimension")));
            }
            if f.raw("time_series_metric").is_some_and(|m| !m.is_null()) {
                return Err(EsError::mapper_parsing(format!("Field [{name}] attempted to shadow a time_series_metric")));
            }
        }
    }
    Ok(())
}

/// A computed value.
#[derive(Clone, Debug, PartialEq)]
pub enum RtValue {
    Str(String),
    Long(i64),
    Double(f64),
    Bool(bool),
    /// Epoch millis.
    Date(i64),
    Ip([u8; 16]),
    /// `(lat, lon)`.
    Geo(f64, f64),
}

impl RtValue {
    /// Orders values of one type (`None` across types).
    fn cmp(&self, other: &RtValue) -> Option<std::cmp::Ordering> {
        use RtValue as V;
        match (self, other) {
            (V::Str(a), V::Str(b)) => Some(a.cmp(b)),
            (V::Long(a), V::Long(b)) | (V::Date(a), V::Date(b)) => Some(a.cmp(b)),
            (V::Double(a), V::Double(b)) => a.partial_cmp(b),
            (V::Long(a), V::Double(b)) => (*a as f64).partial_cmp(b),
            (V::Double(a), V::Long(b)) => a.partial_cmp(&(*b as f64)),
            (V::Bool(a), V::Bool(b)) => Some(a.cmp(b)),
            (V::Ip(a), V::Ip(b)) => Some(a.cmp(b)),
            _ => None,
        }
    }

    /// [`RtValue::cmp`], values of different types as equal (for sorting one field's values).
    pub(crate) fn partial_cmp_same(&self, other: &RtValue) -> std::cmp::Ordering {
        self.cmp(other).unwrap_or(std::cmp::Ordering::Equal)
    }
}

/// A painless value as a runtime field value of `ty`.
pub(crate) fn from_painless(ty: RtType, v: &search_painless::Value, out: &mut Vec<RtValue>) {
    let j = v.to_json();
    from_json_value(ty, &crate::scripting::from_serde(&j), out);
}

/// A (script or `_source`) value parsed as the field type parses it; unparsable values are
/// skipped (`on_script_error` aside, that is what runtime fields reading `_source` do).
fn from_json_value(ty: RtType, v: &Value, out: &mut Vec<RtValue>) {
    // A geo point may be a `[lon, lat]` array (`GeoPointFieldScript`'s source parsing).
    let point_array = ty == RtType::GeoPoint && matches!(v, Value::Arr(a) if a.first().is_some_and(|x| matches!(x, Value::Num(_))));
    if let Value::Arr(a) = v
        && !point_array
    {
        for x in a {
            from_json_value(ty, x, out);
        }
        return;
    }
    let text = || match v {
        Value::Str(s) => s.clone(),
        other => other.to_es_string(),
    };
    let parsed = match (ty, v) {
        (_, Value::Null) => None,
        (RtType::GeoPoint, _) => {
            let mut p = crate::query::XParser::from_value(v);
            p.next_token();
            crate::query::geo::parse_point(&mut p).ok().map(|pt| RtValue::Geo(pt.lat, pt.lon))
        }
        (_, Value::Obj(_)) => None,
        (RtType::Keyword, _) => Some(RtValue::Str(text())),
        (RtType::Long, Value::Num(Number::Int(i))) => Some(RtValue::Long(*i)),
        (RtType::Long, Value::Num(n)) => Some(RtValue::Long(n.as_f64() as i64)),
        (RtType::Long, _) => text().trim().parse::<i64>().ok().map(RtValue::Long),
        (RtType::Double, Value::Num(n)) => Some(RtValue::Double(n.as_f64())),
        (RtType::Double, _) => text().trim().parse::<f64>().ok().map(RtValue::Double),
        (RtType::Date, Value::Num(n)) => Some(RtValue::Date(n.as_f64() as i64)),
        (RtType::Date, _) => crate::time::DateFormatter::new(crate::time::DateFormatter::DEFAULT).ok().and_then(|f| f.parse_millis(&text()).ok()).map(RtValue::Date),
        (RtType::Boolean, Value::Bool(b)) => Some(RtValue::Bool(*b)),
        (RtType::Boolean, _) => match text().as_str() {
            "true" => Some(RtValue::Bool(true)),
            "false" | "" => Some(RtValue::Bool(false)),
            _ => None,
        },
        (RtType::Ip, _) => crate::document::values::parse_ip(&text()).map(RtValue::Ip),
    };
    out.extend(parsed);
}

/// A runtime field bound to an index (script compiled once).
pub struct Evaluator {
    pub field: Arc<RtField>,
    bound: Option<crate::scripting::Bound>,
    table: Arc<FieldTable>,
    shard: i32,
    index: String,
    /// `super::id_fielddata::scripts`: whether `doc['_id']` reads.
    id_fielddata: bool,
    /// Whether the script runs as a search runs it (no loop statement limit, polling the
    /// search's deadline); off for the `_execute` API.
    pub cancellable: bool,
}

impl std::fmt::Debug for Evaluator {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Evaluator({})", self.field.name)
    }
}

impl Evaluator {
    pub fn new(scripts: &crate::scripting::ScriptService, st: &crate::meta::ClusterState, table: &Arc<FieldTable>, shard: i32, index: &str, field: Arc<RtField>) -> EsResult<Evaluator> {
        let bound = match &field.script {
            Some(s) => {
                let ctx = if field.composite_key.is_some() { search_painless::Context::CompositeField } else { field.ty.context() };
                Some(scripts.compile(st, s, ctx)?)
            }
            None => None,
        };
        Ok(Evaluator { field, bound, table: table.clone(), shard, index: index.to_string(), id_fielddata: super::id_fielddata::scripts(st), cancellable: true })
    }

    /// The evaluator of field data (fetch, aggregations, sorts, `doc[...]` reads): no call
    /// site hands its script a cancellation check, so its loops count statements as
    /// Elasticsearch's do; only runtime field queries (`AbstractScriptFieldQuery`) poll.
    pub fn counted(mut self) -> Evaluator {
        self.cancellable = false;
        self
    }

    /// An evaluator for one segment.
    pub fn leaf(&self, seg: &SegmentContext) -> EsResult<EvalLeaf> {
        self.leaf_chained(seg, vec![self.field.name.as_str().into()])
    }

    /// An evaluator for one segment whose script resolves `chain` (the runtime fields being
    /// resolved, this one last): `doc[...]` reads of other runtime fields extend it.
    pub fn leaf_chained(&self, seg: &SegmentContext, chain: Vec<Box<str>>) -> EsResult<EvalLeaf> {
        let leaf = match self.bound.as_ref().and_then(|b| b.painless().map(|s| (s, b))) {
            Some((s, b)) => {
                let mut host = DocHost::new(self.table.clone(), seg.clone(), self.shard, self.index.clone()).with_id_fielddata(self.id_fielddata);
                host.chain = chain;
                let host = if self.cancellable { host } else { host.without_cancellation() };
                let mut l = Leaf::new(s, &b.params, host);
                let parent = match &self.field.composite_key {
                    Some(k) => self.field.name.strip_suffix(&format!(".{k}")).unwrap_or(&self.field.name).to_string(),
                    None => self.field.name.clone(),
                };
                l.set_field_name(&parent);
                Some(l)
            }
            None => None,
        };
        Ok(EvalLeaf { field: self.field.clone(), leaf, seg: seg.clone() })
    }
}

/// `SearchLookup.MAX_FIELD_CHAIN_DEPTH`: runtime fields one value may need to resolve.
pub const MAX_FIELD_CHAIN_DEPTH: usize = 5;

/// The runtime fields one search sees on one index: scripts read them through `doc[...]`
/// ([`DocHost`]), queries, sorts and the fetch phase resolve them here. A search hands the
/// scope to the scripts it runs through the search-index ambient control's extension slot,
/// which follows the search onto the threads of a concurrent search.
///
/// The request's fields are compiled when the scope is built, so a bad definition fails the
/// shard whether or not the search uses it (`SearchExecutionContext` parses
/// `runtime_mappings` up front); the mapping's (validated when they were mapped) compile on
/// first use, so an index with hundreds of runtime fields costs a search nothing for the
/// ones it doesn't read.
pub struct RuntimeScope {
    /// Shared by every shard's scope of a mapping version when the request adds no fields.
    shared: Arc<ScopeFields>,
    /// One slot per field of `shared` ([`ScopeFields::slots`]).
    evaluators: Box<[std::sync::OnceLock<Option<Arc<Evaluator>>>]>,
    scripts: Arc<crate::scripting::ScriptService>,
    st: Arc<crate::meta::ClusterState>,
    table: Arc<FieldTable>,
    shard: i32,
    index: String,
}

impl std::fmt::Debug for RuntimeScope {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "RuntimeScope({:?})", self.shared.fields.fields.keys().collect::<Vec<_>>())
    }
}

/// The shard-independent part of a [`RuntimeScope`]: built once per mapping version (and per
/// search when the request defines runtime fields), so a search over many shards copies no
/// field names per shard.
#[derive(Debug)]
struct ScopeFields {
    fields: RuntimeFields,
    /// Each field's evaluator slot.
    slots: std::collections::HashMap<String, usize>,
    /// Runtime field names (mapping and request): they shadow mapped fields of the same name.
    shadowing: std::collections::HashSet<String>,
}

impl ScopeFields {
    fn new(fields: RuntimeFields, mapping: &Mapping, request: Option<&Map>) -> ScopeFields {
        let slots = fields.fields.keys().enumerate().map(|(i, k)| (k.clone(), i)).collect();
        let mut shadowing: std::collections::HashSet<String> = mapping.runtime.keys().cloned().collect();
        shadowing.extend(request.into_iter().flat_map(|r| r.keys().cloned()));
        ScopeFields { fields, slots, shadowing }
    }
}

impl RuntimeScope {
    /// The scope of a search on `idx` (`None` without runtime fields); fails on an invalid
    /// request runtime field.
    pub fn build(scripts: &Arc<crate::scripting::ScriptService>, st: Arc<crate::meta::ClusterState>, idx: &crate::meta::IndexMeta, table: &Arc<FieldTable>, shard: i32, request: Option<&Map>) -> EsResult<Option<Arc<RuntimeScope>>> {
        if idx.mapping.runtime.is_empty() && request.is_none_or(|r| r.is_empty()) {
            return Ok(None);
        }
        let request = request.filter(|r| !r.is_empty());
        let shared = match request {
            None => mapping_fields(&idx.mapping)?,
            Some(r) => Arc::new(ScopeFields::new(RuntimeFields::of_index(&idx.mapping, Some(r))?, &idx.mapping, Some(r))),
        };
        if shared.fields.is_empty() {
            return Ok(None);
        }
        let evaluators = (0..shared.fields.fields.len()).map(|_| std::sync::OnceLock::new()).collect();
        let scope = RuntimeScope { shared, evaluators, scripts: scripts.clone(), st, table: table.clone(), shard, index: idx.name.clone() };
        if let Some(r) = request {
            for (i, (name, f)) in scope.shared.fields.fields.iter().enumerate() {
                let parent = f.composite_key.as_ref().and_then(|k| name.strip_suffix(k.as_str())).and_then(|p| p.strip_suffix('.'));
                if r.contains_key(name) || parent.is_some_and(|p| r.contains_key(p)) {
                    let ev = Evaluator::new(&scope.scripts, &scope.st, &scope.table, shard, &scope.index, f.clone())?.counted();
                    let _ = scope.evaluators[i].set(Some(Arc::new(ev)));
                }
            }
        }
        Ok(Some(Arc::new(scope)))
    }

    /// The scope carried by a search control.
    pub fn of_control(ctl: Option<&search_index::search::SearchControl>) -> Option<Arc<RuntimeScope>> {
        ctl?.ext.clone()?.downcast::<RuntimeScope>().ok()
    }

    /// The scope of the search running on this thread.
    pub fn ambient() -> Option<Arc<RuntimeScope>> {
        RuntimeScope::of_control(search_index::search::ambient_control().as_ref())
    }

    /// Whether the scope was built for this index and shard.
    pub fn is_for(&self, index: &str, shard: i32) -> bool {
        self.index == index && self.shard == shard
    }

    /// The index the scope was built for.
    pub fn index_name(&self) -> &str {
        &self.index
    }

    /// The field `name`, compiled (`None`: no runtime field of the search, or one whose
    /// script doesn't compile: reading it fails as reading an unmapped field does, and the
    /// consumers that compile it report the error).
    pub fn get(&self, name: &str) -> Option<Arc<Evaluator>> {
        let cell = &self.evaluators[*self.shared.slots.get(name)?];
        cell.get_or_init(|| {
            let f = self.shared.fields.get(name)?.clone();
            Evaluator::new(&self.scripts, &self.st, &self.table, self.shard, &self.index, f).ok().map(|e| Arc::new(e.counted()))
        })
        .clone()
    }

    /// The definition of runtime field `name`.
    pub fn field(&self, name: &str) -> Option<&Arc<RtField>> {
        self.shared.fields.get(name)
    }

    /// The runtime fields of the search.
    pub fn fields(&self) -> &RuntimeFields {
        &self.shared.fields
    }

    /// Whether `name` is a runtime field (or a composite sub-field of one): it shadows a
    /// mapped field of that name.
    pub fn shadows(&self, name: &str) -> bool {
        self.shared.shadowing.contains(name) || name.rsplit_once('.').is_some_and(|(p, _)| self.shared.shadowing.contains(p))
    }
}

/// The Java frames of a field chain failure (`SearchLookup.forkAndTrackFieldReferences` at
/// `line`, reached while building the runtime field's field data), innermost first.
pub fn with_fork_frames(mut e: search_painless::HostError, ty: RtType, line: u32) -> search_painless::HostError {
    let (class, a, b) = match ty {
        RtType::Keyword => ("KeywordScriptFieldType", 164, 50),
        RtType::Long => ("LongScriptFieldType", 133, 38),
        RtType::Double => ("DoubleScriptFieldType", 136, 38),
        RtType::Date => ("DateScriptFieldType", 246, 51),
        RtType::Boolean => ("BooleanScriptFieldType", 171, 40),
        RtType::Ip => ("IpScriptFieldType", 112, 46),
        RtType::GeoPoint => ("GeoPointScriptFieldType", 124, 51),
    };
    let frames = [
        format!("search.lookup.SearchLookup.forkAndTrackFieldReferences(SearchLookup.java:{line})"),
        "search.lookup.SearchLookup.lambda$getForField$1(SearchLookup.java:160)".to_string(),
        format!("index.mapper.{class}.fielddataBuilder({class}.java:{a})"),
        format!("index.mapper.{class}.fielddataBuilder({class}.java:{b})"),
        "index.fielddata.IndexFieldDataService.getForField(IndexFieldDataService.java:92)".to_string(),
        "index.IndexService.loadFielddata(IndexService.java:1451)".to_string(),
        "index.query.SearchExecutionContext.lambda$setLookupProviders$5(SearchExecutionContext.java:570)".to_string(),
    ];
    e.frames = frames.iter().map(|f| format!("{}org.elasticsearch.{f}", search_painless::es::SERVER)).collect();
    e
}

/// The innermost reason of an error chain (a nested runtime field's failure as the reading
/// script reports it).
pub fn innermost_reason(e: &EsError) -> String {
    let mut cur = e;
    while let Some(c) = cur.caused_by.as_deref() {
        cur = c;
    }
    cur.reason().to_string()
}

/// Runtime field values as script doc values (`*ScriptDocValues`: sorted, keywords by their
/// UTF-8 bytes, geo points by their encoded value).
pub fn fill_doc_values(ty: RtType, vals: &[RtValue], out: &mut search_painless::DocValues) {
    use search_painless::DocKind;
    match ty {
        RtType::Keyword => {
            out.clear(DocKind::String);
            let mut strs: Vec<&str> = vals.iter().filter_map(|v| if let RtValue::Str(s) = v { Some(s.as_str()) } else { None }).collect();
            strs.sort_unstable();
            for s in strs {
                out.push_str(s);
            }
        }
        RtType::Ip => {
            out.clear(DocKind::Ip);
            for v in vals {
                if let RtValue::Ip(b) = v {
                    out.push_str(&search_index::query::range::inet_to_string(b));
                }
            }
        }
        RtType::Long | RtType::Date | RtType::Boolean => {
            out.clear(match ty {
                RtType::Date => DocKind::Date,
                RtType::Boolean => DocKind::Boolean,
                _ => DocKind::Long,
            });
            for v in vals {
                match v {
                    RtValue::Long(l) | RtValue::Date(l) => out.longs.push(*l),
                    RtValue::Bool(b) => out.longs.push(*b as i64),
                    _ => {}
                }
            }
        }
        RtType::Double => {
            out.clear(DocKind::Double);
            out.doubles.extend(vals.iter().filter_map(|v| if let RtValue::Double(d) = v { Some(*d) } else { None }));
        }
        RtType::GeoPoint => {
            out.clear(DocKind::GeoPoint);
            let mut pts: Vec<(i64, [f64; 2])> = vals
                .iter()
                .filter_map(|v| if let RtValue::Geo(lat, lon) = v { Some(quantized(*lat, *lon)) } else { None })
                .collect();
            pts.sort_unstable_by_key(|p| p.0);
            out.geo.extend(pts.into_iter().map(|p| p.1));
        }
    }
}

/// A point as `LatLonDocValuesField` stores it: its sort key and decoded `[lat, lon]`.
fn quantized(lat: f64, lon: f64) -> (i64, [f64; 2]) {
    use search_codec::points::encode as g;
    let (la, lo) = (g::encode_latitude(lat), g::encode_longitude(lon));
    (((la as i64) << 32) | (lo as u32 as i64), [g::decode_latitude(la), g::decode_longitude(lo)])
}

/// [`Evaluator`] on one segment.
pub struct EvalLeaf {
    field: Arc<RtField>,
    leaf: Option<Leaf>,
    seg: SegmentContext,
}

impl EvalLeaf {
    /// The values of segment doc `doc`, as doc values hold them (see below).
    pub fn values(&mut self, doc: u32) -> EsResult<Vec<RtValue>> {
        self.collect(doc, true)
    }

    /// The values in emission order, as the `_execute` API returns them (`*FieldScript`'s
    /// value array; booleans are counts either way).
    pub fn emitted_values(&mut self, doc: u32) -> EsResult<Vec<RtValue>> {
        self.collect(doc, false)
    }

    fn collect(&mut self, doc: u32, sorted: bool) -> EsResult<Vec<RtValue>> {
        let f = &self.field;
        let mut out = Vec::new();
        match &mut self.leaf {
            Some(l) => {
                if let Err(e) = l.run_emit(doc) {
                    if f.lenient {
                        return Ok(out);
                    }
                    return Err(crate::scripting::script_error(e));
                }
                match &f.composite_key {
                    // `CompositeFieldScript` keys its emitted values by the full sub-field
                    // name (`<parent>.<key>`).
                    Some(_) => {
                        if let Some((_, vals)) = l.emitted_fields().iter().find(|(n, _)| *n == f.name) {
                            for v in vals {
                                from_painless(f.ty, v, &mut out);
                            }
                        }
                    }
                    None => {
                        for v in l.emitted() {
                            from_painless(f.ty, v, &mut out);
                        }
                    }
                }
            }
            None => {
                self.seg.ensure_stored().map_err(crate::engine::convert::index_error)?;
                let stored = self.seg.reader.stored().document(doc).map_err(|e| EsError::internal(e.to_string()))?;
                if let Some(b) = crate::fetch::source_bytes(&stored) {
                    let src = crate::fetch::parse_source(b)?;
                    if let Value::Obj(m) = &src {
                        let mut raw = Vec::new();
                        if f.ty == RtType::GeoPoint {
                            crate::engine::source::extract_values_keeping_coordinates(m, &f.name, &mut raw);
                        } else {
                            crate::engine::source::extract_values(m, &f.name, &mut raw);
                        }
                        for v in raw {
                            from_json_value(f.ty, v, &mut out);
                        }
                    }
                }
            }
        }
        // Numeric, date and ip scripts collect their values sorted (`AbstractLongFieldScript`
        // and friends); booleans as counts of falses then trues; keywords in emission order.
        match f.ty {
            RtType::Keyword | RtType::GeoPoint => {}
            RtType::Boolean => out.sort_by(|a, b| a.cmp(b).unwrap_or(std::cmp::Ordering::Equal)),
            _ if sorted => out.sort_by(|a, b| a.cmp(b).unwrap_or(std::cmp::Ordering::Equal)),
            _ => {}
        }
        Ok(out)
    }
}

/// Values in doc-values order (keywords by their UTF-8 bytes; the rest are sorted already).
pub fn sort_as_doc_values(vals: &mut [RtValue]) {
    vals.sort_by(|a, b| match (a, b) {
        (RtValue::Str(x), RtValue::Str(y)) => x.as_bytes().cmp(y.as_bytes()),
        _ => std::cmp::Ordering::Equal,
    });
}

/// Values as the `fields` option prints them.
pub fn render(f: &RtField, values: &[RtValue], format: Option<&str>) -> EsResult<Vec<Value>> {
    let fmt = match format {
        Some(p) if f.ty == RtType::Date => Arc::new(crate::time::DateFormatter::new(p).map_err(EsError::illegal_argument)?),
        _ => f.format.clone(),
    };
    Ok(values
        .iter()
        .map(|v| match v {
            RtValue::Str(s) => Value::Str(s.clone()),
            RtValue::Long(l) => Value::from(*l),
            RtValue::Double(d) => Value::Num(Number::Float(*d)),
            RtValue::Bool(b) => Value::Bool(*b),
            RtValue::Date(ms) => Value::Str(fmt.format_millis(*ms)),
            RtValue::Ip(b) => Value::Str(search_index::query::range::inet_to_string(b)),
            RtValue::Geo(lat, lon) => crate::obj! { "coordinates" => Value::Arr(vec![Value::Num(Number::Float(*lon)), Value::Num(Number::Float(*lat))]), "type" => "Point" },
        })
        .collect())
}

/// What a runtime field query matches.
#[derive(Debug)]
pub enum Predicate {
    Exists,
    /// Any value equal to one of these.
    Terms(Vec<RtValue>),
    Range { lo: Option<RtValue>, hi: Option<RtValue>, include_lower: bool, include_upper: bool },
    /// Keyword values starting with this.
    Prefix(String),
    /// Keyword values equal to one of these, ignoring case (lowercased).
    TermsIgnoreCase(Vec<String>),
    /// Keyword values starting with this, ignoring case (lowercased).
    PrefixIgnoreCase(String),
    /// Keyword values an automaton accepts (`wildcard`, `regexp`, `fuzzy`), and the query's
    /// description.
    Automaton(Arc<search_index::query::MultiTermQuery>, String),
    /// Ip values in one of these inclusive ranges (a term is a one-address range, a CIDR its
    /// block).
    Ips(Vec<([u8; 16], [u8; 16])>),
    /// Boolean values: which of `true` / `false` match (`BooleanScriptFieldTermQuery`).
    Bool(bool),
    /// Geo points related to geometries (`GeoPointScriptFieldGeoShapeQuery`).
    Geo(super::runtime_geo::GeoPredicate),
}

impl Predicate {
    /// The query's `toString` (`StringScriptFieldTermQuery` and friends): `field:value` for a
    /// term, `field:[lo TO hi]` for a range, …
    fn describe(&self, f: &RtField) -> String {
        let one = |v: &RtValue| match v {
            RtValue::Str(s) => s.clone(),
            RtValue::Long(l) => l.to_string(),
            RtValue::Double(d) => crate::json::write::java_double(*d),
            RtValue::Bool(b) => b.to_string(),
            RtValue::Date(ms) => ms.to_string(),
            RtValue::Ip(b) => search_index::query::range::inet_to_string(b),
            RtValue::Geo(lat, lon) => format!("{lat},{lon}"),
        };
        match self {
            Predicate::Exists => format!("{}:*", f.name),
            Predicate::TermsIgnoreCase(ts) => format!("{}:{}", f.name, ts.join(", ")),
            Predicate::PrefixIgnoreCase(p) => format!("{}:{p}*", f.name),
            Predicate::Automaton(_, d) => format!("{}:{d}", f.name),
            Predicate::Bool(b) => format!("{}:{b}", f.name),
            Predicate::Geo(g) => g.describe(&f.name),
            Predicate::Ips(rs) => {
                let ip = search_index::query::range::inet_to_string;
                let one = |(lo, hi): &([u8; 16], [u8; 16])| if lo == hi { ip(lo) } else { format!("[{} TO {}]", ip(lo), ip(hi)) };
                format!("{}:{}", f.name, rs.iter().map(one).collect::<Vec<_>>().join(", "))
            }
            Predicate::Terms(ts) if ts.len() == 1 => format!("{}:{}", f.name, one(&ts[0])),
            Predicate::Terms(ts) => format!("{}:{}", f.name, ts.iter().map(one).collect::<Vec<_>>().join(", ")),
            Predicate::Prefix(p) => format!("{}:{p}*", f.name),
            // Numbers and dates normalize to inclusive bounds (`LongScriptFieldRangeQuery`,
            // `DoubleScriptFieldRangeQuery`).
            Predicate::Range { lo, hi, include_lower, include_upper } if matches!(f.ty, RtType::Long | RtType::Date) => {
                let num = |v: &Option<RtValue>| match v {
                    Some(RtValue::Long(l) | RtValue::Date(l)) => Some(*l),
                    Some(RtValue::Double(d)) => Some(*d as i64),
                    _ => None,
                };
                let lo = num(lo).map_or(i64::MIN, |l| if *include_lower { l } else { l.saturating_add(1) });
                let hi = num(hi).map_or(i64::MAX, |h| if *include_upper { h } else { h.saturating_sub(1) });
                format!("{}:[{lo} TO {hi}]", f.name)
            }
            Predicate::Range { lo, hi, include_lower, include_upper } if f.ty == RtType::Double => {
                let num = |v: &Option<RtValue>| match v {
                    Some(RtValue::Long(l)) => Some(*l as f64),
                    Some(RtValue::Double(d)) => Some(*d),
                    _ => None,
                };
                let lo = num(lo).map_or(f64::NEG_INFINITY, |l| if *include_lower { l } else { l.next_up() });
                let hi = num(hi).map_or(f64::INFINITY, |h| if *include_upper { h } else { h.next_down() });
                let d = crate::json::write::java_double;
                format!("{}:[{} TO {}]", f.name, d(lo), d(hi))
            }
            Predicate::Range { lo, hi, include_lower, include_upper } => format!(
                "{}:{}{} TO {}{}",
                f.name,
                if *include_lower { '[' } else { '{' },
                lo.as_ref().map(one).unwrap_or_else(|| "*".into()),
                hi.as_ref().map(one).unwrap_or_else(|| "*".into()),
                if *include_upper { ']' } else { '}' }
            ),
        }
    }

    pub(crate) fn matches(&self, values: &[RtValue]) -> bool {
        use std::cmp::Ordering;
        match self {
            Predicate::Exists => !values.is_empty(),
            Predicate::Terms(ts) => values.iter().any(|v| ts.iter().any(|t| v.cmp(t) == Some(Ordering::Equal))),
            Predicate::Prefix(p) => values.iter().any(|v| matches!(v, RtValue::Str(s) if s.starts_with(p.as_str()))),
            Predicate::TermsIgnoreCase(ts) => values.iter().any(|v| matches!(v, RtValue::Str(s) if { let l = s.to_lowercase(); ts.contains(&l) })),
            Predicate::PrefixIgnoreCase(p) => values.iter().any(|v| matches!(v, RtValue::Str(s) if s.to_lowercase().starts_with(p.as_str()))),
            Predicate::Automaton(q, _) => values.iter().any(|v| matches!(v, RtValue::Str(s) if q.matches_term(s.as_bytes()))),
            Predicate::Ips(rs) => values.iter().any(|v| matches!(v, RtValue::Ip(b) if rs.iter().any(|(lo, hi)| b >= lo && b <= hi))),
            Predicate::Bool(want) => values.iter().any(|v| matches!(v, RtValue::Bool(b) if b == want)),
            Predicate::Geo(g) => g.matches(values),
            Predicate::Range { lo, hi, include_lower, include_upper } => values.iter().any(|v| {
                let lo_ok = lo.as_ref().is_none_or(|l| match v.cmp(l) {
                    Some(Ordering::Greater) => true,
                    Some(Ordering::Equal) => *include_lower,
                    _ => false,
                });
                let hi_ok = hi.as_ref().is_none_or(|h| match v.cmp(h) {
                    Some(Ordering::Less) => true,
                    Some(Ordering::Equal) => *include_upper,
                    _ => false,
                });
                lo_ok && hi_ok
            }),
        }
    }
}

/// A query value as a value of the field's type (`RuntimeField` query parsing).
pub fn query_value(f: &RtField, v: &Value) -> EsResult<RtValue> {
    let mut out = Vec::new();
    from_json_value(f.ty, v, &mut out);
    out.pop().ok_or_else(|| match f.ty {
        RtType::Long | RtType::Double => EsError::number_format(&v.to_es_string()),
        RtType::Ip => EsError::illegal_argument(format!("'{}' is not an IP string literal.", v.as_str().unwrap_or_default())),
        _ => EsError::illegal_argument(format!("failed to parse value [{}] for runtime field [{}]", v.to_es_string(), f.name)),
    })
}

/// A query over a runtime field (`AbstractScriptFieldQuery`).
pub struct RuntimeQuery {
    pub ev: Evaluator,
    pub pred: Arc<Predicate>,
}

struct RuntimeQueryLeaf {
    leaf: EvalLeaf,
    pred: Arc<Predicate>,
}

impl search_index::query::LeafFilterScript for RuntimeQueryLeaf {
    fn matches(&mut self, doc: u32) -> search_index::Result<bool> {
        let vals = self.leaf.values(doc).map_err(super::scripts::park_error)?;
        Ok(self.pred.matches(&vals))
    }
}

impl search_index::query::FilterScript for RuntimeQuery {
    fn leaf(&self, seg: &SegmentContext) -> search_index::Result<Box<dyn search_index::query::LeafFilterScript>> {
        let leaf = self.ev.leaf(seg).map_err(super::scripts::park_error)?;
        Ok(Box::new(RuntimeQueryLeaf { leaf, pred: self.pred.clone() }))
    }
    fn describe(&self) -> String {
        self.pred.describe(&self.ev.field)
    }
    fn runtime_field(&self) -> bool {
        true
    }
}

/// A runtime field's sort `missing`: `_last`, `_first`, or a value (bytes for keywords and ips,
/// a number otherwise).
#[derive(Clone, Debug, PartialEq)]
pub enum SortMissing {
    Last,
    First,
    Bytes(Vec<u8>),
    Number(f64),
}

/// A field sort on a runtime field: keywords and ips through shard-global ordinals of their
/// values (as `string` script sorts; ips by their 16-byte encoding), the other types as
/// numbers. Each doc sorts by its values under `mode` (`MultiValueMode`: min ascending and max
/// descending by default); a doc without values by `missing`.
pub fn sort_key(ev: Arc<Evaluator>, searcher: search_index::Searcher, reverse: bool, mode: search_index::collector::top_field::SortMode, missing: SortMissing) -> (search_index::collector::top_field::SortKey, super::sort::ValueFormat) {
    use search_index::collector::top_field::{ScriptSort, SortKey};
    if matches!(ev.field.ty, RtType::Keyword | RtType::Ip) {
        let terms = Arc::new(super::script_terms_sort::ScriptTerms::runtime(ev, searcher, reverse, mode, missing));
        let sc = Arc::new(super::script_terms_sort::TermsSortScript(terms.clone()));
        return (SortKey::Script(ScriptSort { script: sc, reverse, mode, nested: None }), super::sort::ValueFormat::ScriptTerms(terms));
    }
    let format = match ev.field.ty {
        RtType::Double => super::sort::ValueFormat::Double,
        _ => super::sort::ValueFormat::Long,
    };
    let integral = ev.field.ty != RtType::Double;
    let missing = match missing {
        SortMissing::Number(d) => d,
        SortMissing::First if reverse => f64::INFINITY,
        SortMissing::First => f64::NEG_INFINITY,
        _ if reverse => f64::NEG_INFINITY,
        _ => f64::INFINITY,
    };
    (SortKey::Script(ScriptSort { script: Arc::new(NumberSort { ev, mode, missing, integral }), reverse, mode, nested: None }), format)
}

struct NumberSort {
    ev: Arc<Evaluator>,
    mode: search_index::collector::top_field::SortMode,
    /// What a doc without values sorts as.
    missing: f64,
    /// Long and date values: averages and medians round as `MultiValueMode` does for longs.
    integral: bool,
}

struct NumberSortLeaf {
    leaf: EvalLeaf,
    mode: search_index::collector::top_field::SortMode,
    missing: f64,
    integral: bool,
    nums: Vec<f64>,
}

impl search_index::collector::top_field::SortScript for NumberSort {
    fn leaf(&self, seg: &SegmentContext) -> search_index::Result<Box<dyn search_index::collector::top_field::LeafSortScript>> {
        Ok(Box::new(NumberSortLeaf { leaf: self.ev.leaf(seg).map_err(super::scripts::park_error)?, mode: self.mode, missing: self.missing, integral: self.integral, nums: Vec::new() }))
    }
}

impl search_index::collector::top_field::LeafSortScript for NumberSortLeaf {
    fn number(&mut self, doc: u32) -> search_index::Result<f64> {
        let vals = self.leaf.values(doc).map_err(super::scripts::park_error)?;
        self.nums.clear();
        self.nums.extend(vals.iter().filter_map(|v| match v {
            RtValue::Long(l) | RtValue::Date(l) => Some(*l as f64),
            RtValue::Double(d) => Some(*d),
            RtValue::Bool(b) => Some(*b as i64 as f64),
            _ => None,
        }));
        Ok(select_number(&mut self.nums, self.mode, self.integral).unwrap_or(self.missing))
    }
}

/// `MultiValueMode.select` over a doc's numbers (`None` without any). Long values average and
/// take medians rounded half up (`Math.round`).
fn select_number(nums: &mut [f64], mode: search_index::collector::top_field::SortMode, integral: bool) -> Option<f64> {
    use search_index::collector::top_field::SortMode;
    if nums.is_empty() {
        return None;
    }
    let round = |x: f64| if integral { (x + 0.5).floor() } else { x };
    Some(match mode {
        SortMode::Min => nums.iter().copied().fold(f64::INFINITY, f64::min),
        SortMode::Max => nums.iter().copied().fold(f64::NEG_INFINITY, f64::max),
        SortMode::Sum => nums.iter().sum(),
        SortMode::Avg => round(nums.iter().sum::<f64>() / nums.len() as f64),
        SortMode::Median => {
            nums.sort_unstable_by(f64::total_cmp);
            let n = nums.len();
            if n % 2 == 1 { nums[n / 2] } else { round((nums[n / 2 - 1] + nums[n / 2]) / 2.0) }
        }
    })
}

/// The runtime field a marked aggregation script stands for, rebuilt from its parameters
/// (`def`: the field's definition, a composite's for its sub-fields).
pub fn from_agg_params(params: &Value) -> Option<Arc<RtField>> {
    let name = params.get(AGG_MARKER)?.as_str()?;
    let mut def = params.get("def")?.clone();
    if let Value::Obj(m) = &mut def {
        m.shift_remove(REQUEST_MARKER);
    }
    let def = &def;
    let (root, _) = name.rsplit_once('.').unwrap_or((name, ""));
    let mut defs = Map::new();
    let key = if def.get("type").and_then(Value::as_str) == Some("composite") { root } else { name };
    defs.insert(key.to_string(), def.clone());
    let rt = RuntimeFields::of(&Mapping::empty(), Some(&defs)).ok()?;
    rt.get(name).cloned()
}

/// A runtime field's definition (mapping or request) by name; a composite sub-field's is its
/// parent's.
pub fn definition(mapping: &Mapping, request: Option<&Map>, name: &str) -> Option<Value> {
    let lookup = |n: &str| -> Option<Value> {
        request.and_then(|r| r.get(n).cloned()).or_else(|| mapping.runtime.get(n).map(|rf| {
            let mut m = Map::new();
            m.insert("type".into(), Value::from(rf.ty.clone()));
            for (k, v) in &rf.params {
                m.insert(k.clone(), v.clone());
            }
            Value::Obj(m)
        }))
    };
    lookup(name).or_else(|| name.rsplit_once('.').and_then(|(root, _)| lookup(root)))
}

/// Script values of a runtime field for an aggregation.
pub fn to_script_value(v: &RtValue) -> search_aggs::script::ScriptValue {
    use search_aggs::script::ScriptValue as S;
    match v {
        RtValue::Str(s) => S::Str(s.clone()),
        RtValue::Long(l) => S::Long(*l),
        RtValue::Double(d) => S::Double(*d),
        RtValue::Bool(b) => S::Bool(*b),
        RtValue::Date(ms) => S::Date(*ms),
        RtValue::Ip(b) => S::Str(search_index::query::range::inet_to_string(b)),
        RtValue::Geo(lat, lon) => S::GeoPoint(*lat, *lon),
    }
}

/// Marks a runtime field definition of an aggregation script that comes from the request's
/// `runtime_mappings` (it shadows concrete fields of the same name on every index).
pub const REQUEST_MARKER: &str = "__roll_request";

/// Whether an aggregation script standing for runtime field `name` reads the concrete field of
/// that name on an index instead: the index maps it, and neither the index's runtime section
/// nor the request defines it as a runtime field.
pub fn agg_reads_concrete(params: &Value, mapping: &Mapping, table: &FieldTable) -> Option<String> {
    let name = params.get(AGG_MARKER)?.as_str()?;
    if params.get("def").and_then(|d| d.get(REQUEST_MARKER)).is_some() {
        return None;
    }
    let root = name.rsplit_once('.').map_or(name, |(p, _)| p);
    if mapping.runtime.contains_key(name) || mapping.runtime.contains_key(root) || table.get(name).is_none() {
        return None;
    }
    Some(name.to_string())
}

/// The marker parameter of aggregation value scripts standing for a runtime field.
pub const AGG_MARKER: &str = "__roll_runtime_field";

/// `field: <runtime field>` of an aggregation as a value script (with the value type the
/// field's values have), for search-aggs' script values sources.
pub fn agg_script(f: &RtField, def: &Value) -> Value {
    let value_type = match f.ty {
        RtType::Keyword | RtType::Ip => "string",
        RtType::Long => "long",
        RtType::Double => "double",
        RtType::Date => "date",
        RtType::Boolean => "boolean",
        RtType::GeoPoint => "geo_point",
    };
    crate::obj! {
        "script" => crate::obj! { "source" => "", "params" => crate::obj! { AGG_MARKER => f.name.as_str(), "def" => def.clone() } },
        "value_type" => value_type,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn predicates_over_values() {
        let vals = vec![RtValue::Long(3), RtValue::Long(7)];
        assert!(Predicate::Terms(vec![RtValue::Long(7)]).matches(&vals));
        assert!(!Predicate::Terms(vec![RtValue::Long(5)]).matches(&vals));
        assert!(Predicate::Range { lo: Some(RtValue::Long(4)), hi: Some(RtValue::Long(7)), include_lower: true, include_upper: true }.matches(&vals));
        assert!(!Predicate::Range { lo: Some(RtValue::Long(7)), hi: None, include_lower: false, include_upper: false }.matches(&vals));
        assert!(Predicate::Range { lo: Some(RtValue::Double(6.5)), hi: None, include_lower: false, include_upper: false }.matches(&vals));
        assert!(!Predicate::Exists.matches(&[]));
        assert!(Predicate::Prefix("ab".into()).matches(&[RtValue::Str("abc".into())]));
    }

    fn field(ty: RtType) -> RtField {
        RtField { name: "f".into(), ty, script: None, composite_key: None, format: Arc::new(crate::time::DateFormatter::new(crate::time::DateFormatter::DEFAULT).unwrap()), lenient: false }
    }

    #[test]
    fn query_descriptions_match_script_field_queries() {
        let range = |lo: i64, hi: i64, il: bool, iu: bool| Predicate::Range { lo: Some(RtValue::Long(lo)), hi: Some(RtValue::Long(hi)), include_lower: il, include_upper: iu };
        assert_eq!(Predicate::Terms(vec![RtValue::Str("Monday".into())]).describe(&field(RtType::Keyword)), "f:Monday");
        assert_eq!(range(50, 60, true, false).describe(&field(RtType::Long)), "f:[50 TO 59]");
        assert_eq!(range(50, 60, false, true).describe(&field(RtType::Date)), "f:[51 TO 60]");
        let open = Predicate::Range { lo: None, hi: Some(RtValue::Double(1.0)), include_lower: true, include_upper: false };
        assert_eq!(open.describe(&field(RtType::Double)), format!("f:[-Infinity TO {}]", crate::json::write::java_double(1.0f64.next_down())));
        assert_eq!(Predicate::Exists.describe(&field(RtType::Ip)), "f:*");
    }

    #[test]
    fn doc_values_are_sorted_like_script_doc_values() {
        let mut dv = search_painless::DocValues::default();
        fill_doc_values(RtType::Keyword, &[RtValue::Str("b".into()), RtValue::Str("B".into()), RtValue::Str("a".into()), RtValue::Str("b".into())], &mut dv);
        assert_eq!(dv.strings()[..dv.len()].to_vec(), vec!["B", "a", "b", "b"]);
        fill_doc_values(RtType::Boolean, &[RtValue::Bool(false), RtValue::Bool(true)], &mut dv);
        assert_eq!(dv.longs, vec![0, 1]);
        fill_doc_values(RtType::GeoPoint, &[RtValue::Geo(10.0, 5.0), RtValue::Geo(-10.0, 5.0)], &mut dv);
        assert!(dv.geo[0][0] < 0.0 && (dv.geo[1][0] - 10.0).abs() < 1e-6);
        let mut vals = vec![RtValue::Str("é".into()), RtValue::Str("z".into()), RtValue::Str("Z".into())];
        sort_as_doc_values(&mut vals);
        assert_eq!(vals, vec![RtValue::Str("Z".into()), RtValue::Str("z".into()), RtValue::Str("é".into())]);
    }

    #[test]
    fn innermost_reason_walks_causes() {
        let e = EsError::new(400, "script_exception", "runtime error").caused_by(EsError::illegal_argument("Cyclic dependency detected while resolving runtime fields: a -> a"));
        assert_eq!(innermost_reason(&e), "Cyclic dependency detected while resolving runtime fields: a -> a");
    }

    #[test]
    fn source_values_parse_per_type() {
        let mut out = Vec::new();
        from_json_value(RtType::Long, &Value::Arr(vec![Value::from("5"), Value::from(3i64), Value::from("x")]), &mut out);
        assert_eq!(out, vec![RtValue::Long(5), RtValue::Long(3)]);
        out.clear();
        from_json_value(RtType::Date, &Value::from("2021-01-01T00:00:01Z"), &mut out);
        assert_eq!(out, vec![RtValue::Date(1_609_459_201_000)]);
        out.clear();
        from_json_value(RtType::Boolean, &Value::from("true"), &mut out);
        assert_eq!(out, vec![RtValue::Bool(true)]);
    }
}
