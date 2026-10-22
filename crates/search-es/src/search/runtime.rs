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

    fn context(self) -> search_painless::Context {
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
}

/// The runtime fields a search sees.
#[derive(Debug, Default)]
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
            let p = rf.params.get("format").and_then(Value::as_str).unwrap_or(crate::time::DateFormatter::DEFAULT);
            crate::time::DateFormatter::new(p).map(Arc::new).map_err(EsError::illegal_argument)
        };
        if rf.ty == "composite" {
            let script = script_of(name, rf)?;
            if let Some(Value::Obj(subs)) = rf.params.get("fields") {
                for (sub, def) in subs {
                    let ty = def.get("type").and_then(Value::as_str).and_then(RtType::of).unwrap_or(RtType::Keyword);
                    let sub_rf = RuntimeField::parse_def(sub, def).unwrap_or(RuntimeField { ty: ty.name().into(), params: Map::new() });
                    let full = format!("{name}.{sub}");
                    self.fields.insert(full.clone(), Arc::new(RtField { name: full, ty, script: script.clone(), composite_key: Some(sub.clone()), format: format(&sub_rf)? }));
                }
            }
            return Ok(());
        }
        // `lookup` fields fetch from another index; they are not evaluated here.
        let Some(ty) = RtType::of(&rf.ty) else { return Ok(()) };
        self.fields.insert(name.to_string(), Arc::new(RtField { name: name.to_string(), ty, script: script_of(name, rf)?, composite_key: None, format: format(rf)? }));
        Ok(())
    }

    pub fn is_empty(&self) -> bool {
        self.fields.is_empty()
    }

    pub fn get(&self, name: &str) -> Option<&Arc<RtField>> {
        self.fields.get(name)
    }

    /// Names matching a `fields` pattern, sorted.
    pub fn matching(&self, pattern: &str) -> Vec<String> {
        self.fields.keys().filter(|k| crate::xcontent::filter::glob_match(pattern, k)).cloned().collect()
    }
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
}

/// A painless value as a runtime field value of `ty`.
fn from_painless(ty: RtType, v: &search_painless::Value, out: &mut Vec<RtValue>) {
    let j = v.to_json();
    from_json_value(ty, &crate::scripting::from_serde(&j), out);
}

/// A (script or `_source`) value parsed as the field type parses it; unparsable values are
/// skipped (`on_script_error` aside, that is what runtime fields reading `_source` do).
fn from_json_value(ty: RtType, v: &Value, out: &mut Vec<RtValue>) {
    if let Value::Arr(a) = v {
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
        Ok(Evaluator { field, bound, table: table.clone(), shard, index: index.to_string() })
    }

    /// An evaluator for one segment.
    pub fn leaf(&self, seg: &SegmentContext) -> EsResult<EvalLeaf> {
        let leaf = match self.bound.as_ref().and_then(|b| b.painless().map(|s| (s, b))) {
            Some((s, b)) => {
                let mut l = Leaf::new(s, &b.params, DocHost::new(self.table.clone(), seg.clone(), self.shard, self.index.clone()));
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

/// [`Evaluator`] on one segment.
pub struct EvalLeaf {
    field: Arc<RtField>,
    leaf: Option<Leaf>,
    seg: SegmentContext,
}

impl EvalLeaf {
    /// The values of segment doc `doc`, in emission (or `_source`) order.
    pub fn values(&mut self, doc: u32) -> EsResult<Vec<RtValue>> {
        let f = &self.field;
        let mut out = Vec::new();
        match &mut self.leaf {
            Some(l) => {
                l.run_emit(doc).map_err(crate::scripting::script_error)?;
                match &f.composite_key {
                    Some(k) => {
                        if let Some((_, vals)) = l.emitted_fields().iter().find(|(n, _)| n == k) {
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
                        crate::engine::source::extract_values(m, &f.name, &mut raw);
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
            _ => out.sort_by(|a, b| a.cmp(b).unwrap_or(std::cmp::Ordering::Equal)),
        }
        Ok(out)
    }
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
}

impl Predicate {
    fn matches(&self, values: &[RtValue]) -> bool {
        use std::cmp::Ordering;
        match self {
            Predicate::Exists => !values.is_empty(),
            Predicate::Terms(ts) => values.iter().any(|v| ts.iter().any(|t| v.cmp(t) == Some(Ordering::Equal))),
            Predicate::Prefix(p) => values.iter().any(|v| matches!(v, RtValue::Str(s) if s.starts_with(p.as_str()))),
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
        let vals = self.leaf.values(doc).map_err(|e| search_index::Error::illegal(e.reason().to_string()))?;
        Ok(self.pred.matches(&vals))
    }
}

impl search_index::query::FilterScript for RuntimeQuery {
    fn leaf(&self, seg: &SegmentContext) -> search_index::Result<Box<dyn search_index::query::LeafFilterScript>> {
        let leaf = self.ev.leaf(seg).map_err(|e| search_index::Error::illegal(e.reason().to_string()))?;
        Ok(Box::new(RuntimeQueryLeaf { leaf, pred: self.pred.clone() }))
    }
    fn describe(&self) -> String {
        format!("{}:{:?}", self.ev.field.name, self.pred)
    }
}

/// The runtime field a marked aggregation script stands for, rebuilt from its parameters
/// (`def`: the field's definition, a composite's for its sub-fields).
pub fn from_agg_params(params: &Value) -> Option<Arc<RtField>> {
    let name = params.get(AGG_MARKER)?.as_str()?;
    let def = params.get("def")?;
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
