//! Index mappings with Elasticsearch's semantics.
//!
//! A [`Mapping`] is the root object (`_doc`): its fields, objects and aliases, the root-only
//! settings (`dynamic_templates`, `date_detection`, `dynamic_date_formats`,
//! `numeric_detection`, `runtime`) and the metadata fields (`_source`, `_routing`, `_meta`,
//! `_data_stream_timestamp`).
//!
//! - [`Mapping::parse`] turns mapping JSON into the model, failing with Elasticsearch's
//!   exceptions and texts (callers wrap them as `Failed to parse mapping: …`).
//! - [`Mapping::merge`] applies a mapping update (or composes templates), reporting every
//!   non-updatable parameter as `Mapper for [f] conflicts with existing mapper: …`.
//! - [`Mapping::validate`] enforces the cross-field rules and `index.mapping.*` limits.
//! - [`Mapping::to_value`] prints the mapping as `GET _mapping` does: only configured parameters,
//!   `properties` sorted by name, each type's own parameter order.
//! - [`dynamic`] infers the mapping update a document needs (dynamic mapping).
//!
//! Field types and their parameters are tables in [`types`] interpreted by [`params`], so the
//! parse/merge/print logic is shared by every type.

pub mod dynamic;
pub mod params;
pub mod types;

use crate::cluster::Dialect;
use crate::error::EsError;
use crate::json::{Map, Value, java_map_string};
use crate::meta::settings::Settings;
use crate::obj;
use params::{Dv, Merge, ParamDef, Ser, SettingDefault, conflict_string, node_bool, node_bool_named, parse_value};
use std::cell::RefCell;
use std::collections::{BTreeMap, HashSet};
use std::sync::Arc;
use types::TypeDef;

/// Built-in analyzers (Elasticsearch core and `analysis-common`).
pub const BUILT_IN_ANALYZERS: &[&str] = &[
    "arabic", "armenian", "basque", "bengali", "brazilian", "bulgarian", "catalan", "chinese", "cjk", "czech", "danish", "default",
    "dutch", "english", "estonian", "fingerprint", "finnish", "french", "galician", "german", "greek", "hindi", "hungarian",
    "indonesian", "irish", "italian", "keyword", "latvian", "lithuanian", "norwegian", "pattern", "persian", "portuguese",
    "romanian", "russian", "serbian", "simple", "snowball", "sorani", "spanish", "standard", "stop", "swedish", "thai", "turkish",
    "whitespace",
];

/// Settings and analysis names a mapping is parsed against.
#[derive(Debug, Default)]
pub struct MappingCtx {
    /// `index.mapping.coerce` when set (field types default to `true` otherwise).
    pub coerce: Option<bool>,
    pub ignore_malformed: bool,
    pub ignore_above: i64,
    pub analyzers: HashSet<String>,
    pub normalizers: HashSet<String>,
    pub similarities: HashSet<String>,
    pub total_fields_limit: i64,
    pub depth_limit: i64,
    pub nested_fields_limit: i64,
    pub nested_objects_limit: i64,
    pub field_name_length_limit: i64,
    /// The index mode when it is a strict columnar one (`columnar`, `logsdb_columnar`): objects
    /// flatten into dotted field names and their `dynamic`/`enabled` move to `prefix_properties`.
    pub columnar: Option<&'static str>,
    /// `index.mapping.doc_values.multi_value` / `.nullability`: the column defaults.
    pub dv_multi_value: bool,
    pub dv_nullability: bool,
    /// `index.mapping.index_disabled_by_default`.
    pub index_disabled_by_default: bool,
    /// `index.mode: vectordb_document`: vectors default to `bfloat16` elements.
    pub vectordb: bool,
    /// Deprecation warnings raised while parsing.
    pub warnings: RefCell<Vec<String>>,
    /// Whose mapping rules apply: OpenSearch has its own field types (`flat_object`,
    /// `xy_point`, …), parameters (`boost`, `disable_objects`) and `dynamic` values.
    pub dialect: Dialect,
}

impl MappingCtx {
    pub fn from_settings(s: &Settings) -> MappingCtx {
        MappingCtx::from_settings_in(Dialect::Elasticsearch, s)
    }

    pub fn from_settings_in(dialect: Dialect, s: &Settings) -> MappingCtx {
        let b = |k: &str, d: bool| s.get(k).map(|v| v == "true").unwrap_or(d);
        let n = |k: &str, d: i64| s.get(k).and_then(|v| v.parse::<i64>().ok()).unwrap_or(d);
        let names = |prefix: &str| -> HashSet<String> {
            s.keys().filter_map(|k| k.strip_prefix(prefix)).filter_map(|rest| rest.split('.').next()).map(str::to_string).collect()
        };
        // Logs indices ignore malformed values and long keywords by default.
        let logs = crate::meta::index_mode::IndexMode::of(s).is_logs();
        MappingCtx {
            coerce: s.get("index.mapping.coerce").map(|v| v == "true"),
            ignore_malformed: b("index.mapping.ignore_malformed", logs),
            ignore_above: n("index.mapping.ignore_above", if logs { 8191 } else { i32::MAX as i64 }),
            analyzers: names("index.analysis.analyzer."),
            normalizers: names("index.analysis.normalizer."),
            similarities: names("index.similarity."),
            total_fields_limit: n("index.mapping.total_fields.limit", 1000),
            depth_limit: n("index.mapping.depth.limit", 20),
            nested_fields_limit: n("index.mapping.nested_fields.limit", 100),
            nested_objects_limit: n("index.mapping.nested_objects.limit", 10000),
            field_name_length_limit: n("index.mapping.field_name_length.limit", i64::MAX),
            columnar: {
                let mode = crate::meta::index_mode::IndexMode::of(s);
                mode.is_strict_columnar().then(|| mode.name())
            },
            dv_multi_value: b("index.mapping.doc_values.multi_value", true),
            dv_nullability: b("index.mapping.doc_values.nullability", true),
            index_disabled_by_default: b(
                "index.mapping.index_disabled_by_default",
                crate::meta::index_mode::IndexMode::of(s).is_strict_columnar(),
            ),
            vectordb: crate::meta::index_mode::IndexMode::of(s) == crate::meta::index_mode::IndexMode::VectordbDocument,
            warnings: RefCell::new(Vec::new()),
            dialect,
        }
    }

    pub fn has_analyzer(&self, name: &str) -> bool {
        BUILT_IN_ANALYZERS.contains(&name) || self.analyzers.contains(name)
    }

    pub fn has_normalizer(&self, name: &str) -> bool {
        name == "lowercase" || self.normalizers.contains(name)
    }

    pub fn has_similarity(&self, name: &str) -> bool {
        matches!(name, "BM25" | "boolean") || self.similarities.contains(name)
    }

    pub fn warn(&self, msg: impl Into<String>) {
        self.warnings.borrow_mut().push(msg.into());
    }

    pub fn opensearch(&self) -> bool {
        self.dialect == Dialect::OpenSearch
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Dynamic {
    True,
    False,
    Strict,
    Runtime,
    /// OpenSearch: new fields only through a matching dynamic template, otherwise an error.
    StrictAllowTemplates,
    /// OpenSearch: new fields only through a matching dynamic template, otherwise ignored.
    FalseAllowTemplates,
}

impl Dynamic {
    pub fn as_str(self) -> &'static str {
        match self {
            Dynamic::True => "true",
            Dynamic::False => "false",
            Dynamic::Strict => "strict",
            Dynamic::Runtime => "runtime",
            Dynamic::StrictAllowTemplates => "strict_allow_templates",
            Dynamic::FalseAllowTemplates => "false_allow_templates",
        }
    }

    fn parse(v: &Value, name: &str, dialect: Dialect) -> Result<Dynamic, EsError> {
        let s = v.to_es_string();
        if s.eq_ignore_ascii_case("strict") {
            return Ok(Dynamic::Strict);
        }
        if dialect == Dialect::OpenSearch {
            if s.eq_ignore_ascii_case("strict_allow_templates") {
                return Ok(Dynamic::StrictAllowTemplates);
            }
            if s.eq_ignore_ascii_case("false_allow_templates") {
                return Ok(Dynamic::FalseAllowTemplates);
            }
        } else if s.eq_ignore_ascii_case("runtime") {
            return Ok(Dynamic::Runtime);
        }
        Ok(if node_bool_named(v, &format!("{name}.dynamic"))? { Dynamic::True } else { Dynamic::False })
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MergeReason {
    /// `PUT _mapping`, dynamic updates: non-updatable parameters must not change.
    MappingUpdate,
    /// Composing templates: later definitions replace earlier ones.
    IndexTemplate,
}

/// A leaf field.
#[derive(Clone, Debug)]
pub struct FieldMapper {
    pub name: String,
    pub ty: &'static str,
    def: &'static TypeDef,
    /// Explicitly set parameter values, aligned with `def.params`.
    values: Vec<Option<Value>>,
    pub fields: BTreeMap<String, FieldMapper>,
    pub copy_to: Vec<String>,
    pub source_keep: Option<String>,
    /// `flattened` only: explicitly mapped sub-fields (`properties`), by dotted key.
    pub props: BTreeMap<String, FieldMapper>,
    /// OpenSearch's deprecated index-time `boost`, kept for the types that print it.
    pub boost: Option<f64>,
    /// A date field's compiled `format` and `locale`, built on first use: documents must not
    /// recompile the pattern for every value.
    date_format: std::sync::OnceLock<Result<Arc<crate::time::DateFormatter>, String>>,
    /// The full path documents reach this field by, shared by every value parsed into it.
    pub(crate) full_name: std::sync::OnceLock<Arc<str>>,
}

impl PartialEq for FieldMapper {
    fn eq(&self, o: &Self) -> bool {
        self.name == o.name && self.ty == o.ty && self.values == o.values && self.fields == o.fields && self.copy_to == o.copy_to && self.props == o.props
    }
}

impl FieldMapper {
    pub fn new(name: &str, def: &'static TypeDef) -> FieldMapper {
        FieldMapper {
            name: name.to_string(),
            ty: def.name,
            def,
            values: vec![None; def.params.len()],
            fields: BTreeMap::new(),
            copy_to: Vec::new(),
            source_keep: None,
            props: BTreeMap::new(),
            boost: None,
            date_format: std::sync::OnceLock::new(),
            full_name: std::sync::OnceLock::new(),
        }
    }

    /// `full` (this field's path in a document) as a shared string, allocated once per mapper
    /// rather than once per value.
    pub fn full_name(&self, full: &str) -> Arc<str> {
        match self.full_name.get() {
            Some(n) if **n == *full => n.clone(),
            // Reached by another path (pass-through objects): not cached.
            Some(_) => Arc::from(full),
            None => self.full_name.get_or_init(|| Arc::from(full)).clone(),
        }
    }

    /// The formatter of a `date`/`date_nanos` field (its `format` with its `locale`).
    pub fn date_formatter(&self, ctx: &MappingCtx) -> Result<Arc<crate::time::DateFormatter>, EsError> {
        self.date_format
            .get_or_init(|| {
                let format = self.effective("format", ctx);
                let format = format.as_str().unwrap_or(crate::time::DateFormatter::DEFAULT);
                crate::time::DateFormatter::new(format).map(|f| Arc::new(f.with_locale(date_locale(self.raw("locale")))))
            })
            .clone()
            .map_err(EsError::illegal_argument)
    }

    pub fn def(&self) -> &'static TypeDef {
        self.def
    }

    pub fn is_set(&self, name: &str) -> bool {
        self.def.param_index(name).is_some_and(|i| self.values[i].is_some())
    }

    /// The explicitly set value.
    pub fn raw(&self, name: &str) -> Option<&Value> {
        self.def.param_index(name).and_then(|i| self.values[i].as_ref())
    }

    pub fn set(&mut self, name: &str, v: Value) {
        if let Some(i) = self.def.param_index(name) {
            self.values[i] = Some(v);
            self.date_format = std::sync::OnceLock::new();
        }
    }

    fn default_at(&self, i: usize, ctx: &MappingCtx) -> Value {
        match self.def.params[i].default {
            Dv::Null => Value::Null,
            Dv::Bool(b) => Value::Bool(b),
            Dv::Int(n) => Value::from(n),
            Dv::Float(f) => Value::from(f),
            Dv::Str(s) => Value::from(s),
            Dv::EmptyObj => Value::object(),
            Dv::EmptyList => Value::Arr(Vec::new()),
            Dv::Param(other) => self.effective(other, ctx),
            Dv::SettingBool(SettingDefault::Coerce) => Value::Bool(ctx.coerce.unwrap_or(true)),
            Dv::SettingBool(SettingDefault::IgnoreMalformed) => Value::Bool(ctx.ignore_malformed),
            Dv::IgnoreAbove => Value::from(ctx.ignore_above),
            Dv::Fn(f) => f(self, ctx),
        }
    }

    /// The value in effect: the set value or the default.
    pub fn effective(&self, name: &str, ctx: &MappingCtx) -> Value {
        match self.def.param_index(name) {
            Some(i) => match &self.values[i] {
                Some(v) => v.clone(),
                None => self.default_at(i, ctx),
            },
            None => Value::Null,
        }
    }

    /// Set to a value different from the default.
    pub fn is_configured(&self, name: &str, ctx: &MappingCtx) -> bool {
        match self.def.param_index(name) {
            Some(i) => match &self.values[i] {
                Some(v) => *v != self.default_at(i, ctx),
                None => false,
            },
            None => false,
        }
    }

    pub fn bool_param(&self, name: &str, ctx: &MappingCtx) -> bool {
        match self.effective(name, ctx) {
            Value::Bool(b) => b,
            // A column configuration (`doc_values: {multi_value, nullability}`) is enabled.
            Value::Obj(_) => true,
            _ => false,
        }
    }

    fn should_print(&self, i: usize, ctx: &MappingCtx, include_defaults: bool) -> bool {
        let def: &ParamDef = &self.def.params[i];
        match def.ser {
            Ser::NonNull => return !self.effective(def.name, ctx).is_null(),
            // Accepted for compatibility, never printed (even with `include_defaults`).
            Ser::Never => return false,
            _ => {}
        }
        if include_defaults {
            return true;
        }
        match def.ser {
            Ser::Configured => self.values[i].as_ref().is_some_and(|v| *v != self.default_at(i, ctx)),
            Ser::Always => !self.effective(def.name, ctx).is_null() || self.values[i].is_some(),
            Ser::WhenSet => self.values[i].is_some(),
            Ser::Never => false,
            Ser::NonNull => false,
            Ser::Fn(f) => f(self, ctx),
        }
    }

    /// The field as `GET _mapping` prints it.
    pub fn to_value(&self, ctx: &MappingCtx, include_defaults: bool) -> Value {
        let mut m = Map::new();
        m.insert("type".into(), Value::from(self.ty));
        if let Some(b) = self.boost {
            m.insert("boost".into(), Value::from(b));
        } else if include_defaults && ctx.opensearch() && types::os_prints_boost(self.ty) {
            m.insert("boost".into(), Value::from(1.0));
        }
        let n = self.def.params.len();
        let fields_at = self.def.fields_at.min(n);
        for i in 0..=n {
            if i == fields_at {
                if !self.fields.is_empty() {
                    let mut fm = Map::new();
                    for (k, f) in &self.fields {
                        fm.insert(k.clone(), f.to_value(ctx, include_defaults));
                    }
                    m.insert("fields".into(), Value::Obj(fm));
                }
                if !self.copy_to.is_empty() {
                    m.insert("copy_to".into(), Value::from(self.copy_to.clone()));
                }
                if let Some(k) = &self.source_keep {
                    m.insert("synthetic_source_keep".into(), Value::from(k.clone()));
                }
            }
            if i == n {
                break;
            }
            if self.should_print(i, ctx, include_defaults) {
                let name = self.def.params[i].name;
                m.insert(name.into(), self.effective(name, ctx));
            }
        }
        if !self.props.is_empty() {
            let mut pm = Map::new();
            for (k, f) in &self.props {
                pm.insert(k.clone(), f.to_value(ctx, include_defaults));
            }
            m.insert("properties".into(), Value::Obj(pm));
        }
        Value::Obj(m)
    }

    /// Parses `{"type": …, params…}` for a field named `name` (its leaf name).
    pub fn parse(name: &str, def: &'static TypeDef, node: &Map, ctx: &MappingCtx, in_multi_field: bool) -> Result<FieldMapper, EsError> {
        let mut f = FieldMapper::new(name, def);
        for (k, v) in node {
            match k.as_str() {
                "type" => continue,
                "fields" => {
                    if in_multi_field {
                        return Err(EsError::illegal_argument(format!(
                            "Encountered a multi-field [{name}] which itself contains a multi-field. Defining chained multi-fields is not supported."
                        )));
                    }
                    parse_multi_fields(&mut f, name, v, ctx)?;
                    continue;
                }
                "copy_to" => {
                    let targets = match v {
                        Value::Arr(a) => a.iter().map(Value::to_es_string).collect(),
                        other => vec![other.to_es_string()],
                    };
                    f.copy_to.extend(targets);
                    continue;
                }
                "boost" if ctx.opensearch() => {
                    let b = params::node_double(v)?;
                    if types::os_prints_boost(def.name) && b != 1.0 {
                        f.boost = Some(b);
                    }
                    continue;
                }
                "boost" => return Err(EsError::mapper_parsing(format!("Unknown parameter [boost] on mapper [{name}]"))),
                // OpenSearch's text mappers still accept (and ignore) `doc_values`.
                "doc_values" if ctx.opensearch() && def.name == "text" => continue,
                "time_series_dimension" | "time_series_metric" | "script" | "on_script_error" | "synthetic_source_keep" if ctx.opensearch() => {
                    return Err(EsError::mapper_parsing(format!("unknown parameter [{k}] on mapper [{name}] of type [{}]", def.name)));
                }
                "properties" if def.name == "flattened" => {
                    f.props = parse_flattened_properties(name, v, ctx)?;
                    continue;
                }
                "synthetic_source_keep" => {
                    let s = v.to_es_string();
                    if !matches!(s.as_str(), "none" | "arrays" | "all") {
                        return Err(EsError::illegal_argument(format!("Unknown synthetic_source_keep value [{s}], accepted values are [none,arrays,all]")));
                    }
                    f.source_keep = Some(s);
                    continue;
                }
                _ => {}
            }
            let Some(i) = def.param_index(k) else {
                return Err(EsError::mapper_parsing(format!("unknown parameter [{k}] on mapper [{name}] of type [{}]", def.name)));
            };
            let pd = &def.params[i];
            if v.is_null() && !pd.accepts_null {
                return Err(EsError::mapper_parsing(format!("[{k}] on mapper [{name}] of type [{}] must not have a [null] value", def.name)));
            }
            let parsed = parse_value(pd, name, def.name, v, ctx)?;
            f.values[i] = Some(parsed);
        }
        f.post_parse(ctx)?;
        if f.ty == "aggregate_metric_double" && node.contains_key("default_metric") {
            ctx.warn("Parameter [default_metric] is deprecated and will be removed in a future version");
        }
        if let Some(mode) = ctx.columnar {
            // Strict columnar modes keep values only in doc values.
            if f.raw("store").and_then(Value::as_bool) == Some(true) {
                return Err(EsError::illegal_argument(format!("[store] cannot be enabled on field [{name}] in [{mode}] index mode")));
            }
            if !f.copy_to.is_empty() {
                return Err(EsError::illegal_argument(format!("[copy_to] is not allowed on field [{name}] in [{mode}] index mode")));
            }
        }
        Ok(f)
    }

    /// Type-specific normalization and validation after parsing.
    fn post_parse(&mut self, ctx: &MappingCtx) -> Result<(), EsError> {
        if self.ty == "dense_vector"
            && let Some(Value::Obj(o)) = self.raw("index_options")
        {
            let normalized = types::normalize_vector_index_options(&self.name, &o.clone(), ctx)?;
            self.set("index_options", normalized);
        }
        if self.ty == "flattened" && self.props.is_empty() && self.raw("passthrough").is_some_and(|v| !v.is_null()) {
            return Err(EsError::mapper_parsing(format!("Flattened field [{}] has [passthrough] set but no [properties] defined", self.name)));
        }
        // A `null` for a nullable parameter is the same as not setting it.
        for v in self.values.iter_mut() {
            if matches!(v, Some(Value::Null)) {
                *v = None;
            }
        }
        if (self.ty == "date" || self.ty == "date_nanos")
            && let Some(Value::Str(nv)) = self.raw("null_value").cloned()
        {
            let fmt_s = self.effective("format", ctx);
            let fmt_s = fmt_s.as_str().unwrap_or(crate::time::DateFormatter::DEFAULT);
            if let Ok(fmt) = crate::time::DateFormatter::new(fmt_s).map(|f| f.with_locale(date_locale(self.raw("locale")))) {
                let wrap = |reason: String, cause: EsError| EsError::mapper_parsing(format!("Error parsing [null_value] on field [{}]: {reason}", self.name)).caused_by(cause);
                match fmt.parse_detailed(&nv, None, false) {
                    Err(e) => return Err(wrap(e.reason(), e.to_es())),
                    Ok(i) if self.ty == "date_nanos" => {
                        if let Err(msg) = crate::time::to_epoch_nanos(i) {
                            return Err(wrap(msg.clone(), EsError::illegal_argument(msg)));
                        }
                    }
                    Ok(_) => {}
                }
            }
        }
        if self.ty == "completion"
            && let Some(contexts) = self.raw("contexts").cloned()
            && !contexts.is_null()
        {
            self.set("contexts", completion_contexts(&contexts)?);
        }
        if self.ty == "geo_point"
            && let Some(nv) = self.raw("null_value").cloned()
        {
            let p = parse_geo_point(&nv).map_err(|e| EsError::mapper_parsing(format!("Error parsing [null_value] on field [{}]: {e}", self.name)))?;
            self.set("null_value", p);
        }
        if let Some(v) = self.def.validate {
            v(self, ctx)?;
        }
        Ok(())
    }

    /// Merges an update into this field (same full path). Collects every conflict.
    fn merge(&self, incoming: &FieldMapper, full_path: &str, ctx: &MappingCtx, reason: MergeReason) -> Result<FieldMapper, EsError> {
        if incoming.ty != self.ty {
            if reason == MergeReason::IndexTemplate {
                return Ok(incoming.clone());
            }
            return Err(EsError::illegal_argument(format!("mapper [{full_path}] cannot be changed from type [{}] to [{}]", self.ty, incoming.ty)));
        }
        let mut out = self.clone();
        let mut conflicts = Vec::new();
        for (i, pd) in self.def.params.iter().enumerate() {
            let cur = self.effective(pd.name, ctx);
            // The incoming builder's value: what it set, or its default.
            let new = incoming.effective(pd.name, ctx);
            let ok = reason == MergeReason::IndexTemplate
                || match pd.merge {
                    Merge::Updatable => true,
                    Merge::Fixed => cur == new,
                    Merge::TrueToFalse => cur == new || (cur == Value::Bool(true) && new == Value::Bool(false)),
                    Merge::Custom { allowed, .. } => cur.is_null() || new.is_null() || cur == new || allowed(&cur, &new),
                    Merge::Check { check, .. } => check(&cur, &new, &mut conflicts),
                };
            if ok {
                out.values[i] = incoming.values[i].clone();
                if out.values[i].is_none() && self.values[i].is_some() && pd.merge == Merge::Fixed {
                    // Equal values: keep what was set so serialization doesn't change.
                    out.values[i] = self.values[i].clone();
                }
            } else if let Merge::Custom { show, .. } | Merge::Check { show, .. } = pd.merge {
                conflicts.push(format!("Cannot update parameter [{}] from [{}] to [{}]", pd.name, show(&cur), show(&new)));
            } else {
                conflicts.push(conflict_text(pd.name, &cur, &new));
            }
        }
        if !conflicts.is_empty() {
            return Err(EsError::illegal_argument(format!(
                "Mapper for [{full_path}] conflicts with existing mapper:\n\t{}",
                conflicts.join("\n\t")
            )));
        }
        // Mapped sub-fields of flattened fields merge by key, like multi-fields.
        for (k, pf) in &incoming.props {
            let merged = match self.props.get(k) {
                Some(existing) => existing.merge(pf, &format!("{full_path}.{k}"), ctx, reason)?,
                None => pf.clone(),
            };
            out.props.insert(k.clone(), merged);
        }
        // Multi-fields merge by name; existing ones not in the update are kept.
        for (k, mf) in &incoming.fields {
            let merged = match self.fields.get(k) {
                Some(existing) => existing.merge(mf, &format!("{full_path}.{k}"), ctx, reason)?,
                None => mf.clone(),
            };
            out.fields.insert(k.clone(), merged);
        }
        out.copy_to = incoming.copy_to.clone();
        out.boost = incoming.boost;
        if incoming.source_keep.is_some() {
            out.source_keep = incoming.source_keep.clone();
        }
        if let Some(v) = out.def.validate {
            v(&out, ctx)?;
        }
        Ok(out)
    }
}

fn conflict_text(param: &str, cur: &Value, new: &Value) -> String {
    let show = |v: &Value| {
        if param == "doc_values" {
            let enabled = v.as_bool().unwrap_or(true);
            return format!("Values[enabled={enabled}, cardinality=low, multiValue=true, nullability=true]");
        }
        conflict_string(v)
    };
    format!("Cannot update parameter [{param}] from [{}] to [{}]", show(cur), show(new))
}

/// A completion field's `contexts` (`ContextMappings.load`): one object or a list, each with a
/// `name` and a `type` (`category` or `geo`, any case), printed as Elasticsearch does: the type
/// upper-cased, a geo context's `precision` (default 6) always.
fn completion_contexts(v: &Value) -> Result<Value, EsError> {
    let list: Vec<&Value> = match v {
        Value::Arr(a) => a.iter().collect(),
        other => vec![other],
    };
    let mut out = Vec::with_capacity(list.len());
    for c in list {
        let Value::Obj(m) = c else {
            return Err(EsError::new(400, "parse_exception", "expected a map for context mapping"));
        };
        let name = m.get("name").map(Value::to_es_string).ok_or_else(|| EsError::new(400, "parse_exception", "missing [name] in context mapping"))?;
        let ty = m.get("type").map(Value::to_es_string).ok_or_else(|| EsError::new(400, "parse_exception", "missing [type] in context mapping"))?;
        let geo = match ty.to_ascii_lowercase().as_str() {
            "category" => false,
            "geo" => true,
            _ => return Err(EsError::illegal_argument(format!("No context type for [{ty}]"))),
        };
        let mut o = obj! { "name" => name.clone(), "type" => if geo { "GEO" } else { "CATEGORY" } };
        let extra: Vec<String> = m
            .iter()
            .filter(|(k, _)| !matches!(k.as_str(), "name" | "type" | "path") && !(geo && k.as_str() == "precision"))
            .map(|(k, v)| format!("{k} : {}", java_value(v)))
            .collect();
        if !extra.is_empty() {
            return Err(EsError::mapper_parsing(format!("Mapping definition for [{name}] has unsupported parameters:  [{}]", extra.join(", "))));
        }
        if geo {
            let precision = match m.get("precision") {
                None => 6,
                Some(Value::Num(n)) => n.as_f64() as i64,
                Some(other) => other.to_es_string().trim().parse::<i64>().map_err(|_| EsError::illegal_argument(format!("failed to parse [precision] in context [{name}]")))?,
            };
            if let Value::Obj(om) = &mut o {
                om.insert("precision".into(), Value::from(precision));
            }
        }
        if let (Some(path), Value::Obj(om)) = (m.get("path"), &mut o) {
            om.insert("path".into(), Value::from(path.to_es_string()));
        }
        out.push(o);
    }
    Ok(Value::Arr(out))
}

/// `geo_point` values in their accepted forms, normalized to `{lat, lon}`.
pub fn parse_geo_point(v: &Value) -> Result<Value, String> {
    let (lat, lon) = match v {
        Value::Obj(m) => {
            let lat = m.get("lat").and_then(Value::as_f64).ok_or("field [lat] missing")?;
            let lon = m.get("lon").and_then(Value::as_f64).ok_or("field [lon] missing")?;
            (lat, lon)
        }
        Value::Arr(a) if a.len() >= 2 => {
            let lon = a[0].as_f64().ok_or("numeric value expected")?;
            let lat = a[1].as_f64().ok_or("numeric value expected")?;
            (lat, lon)
        }
        // `lat,lon`, WKT `POINT` or a geohash (its cell's south-west corner), as documents.
        Value::Str(s) => {
            let p = crate::query::geo::point_from_string(s).map_err(|e| e.reason().to_string())?;
            (p.lat, p.lon)
        }
        other => return Err(format!("geo_point expected, got {}", other.to_es_string())),
    };
    Ok(obj! { "lat" => lat, "lon" => lon })
}

fn parse_multi_fields(f: &mut FieldMapper, name: &str, v: &Value, ctx: &MappingCtx) -> Result<(), EsError> {
    let m = match v {
        Value::Arr(a) if a.is_empty() => return Ok(()),
        Value::Obj(m) => m,
        other => {
            return Err(EsError::mapper_parsing(format!(
                "expected map for property [fields] on field [{}] or [fields] but got a class java.lang.{}",
                other.to_es_string(),
                params::java_simple_class(other)
            )));
        }
    };
    for (mf_name, node) in m {
        if mf_name.contains('.') {
            return Err(EsError::mapper_parsing(format!("Field name [{mf_name}] which is a multi field of [{name}] cannot contain '.'")));
        }
        let Value::Obj(node) = node else {
            return Err(EsError::mapper_parsing(format!("illegal field [{mf_name}], only fields can be specified inside fields")));
        };
        let Some(ty) = node.get("type") else {
            return Err(EsError::mapper_parsing(format!("no type specified for property [{mf_name}]")));
        };
        let ty = ty.to_es_string();
        let def = match types::lookup_in(ctx.dialect, &ty) {
            Some(d) => d,
            None if matches!(ty.as_str(), "object" | "nested" | "passthrough" | "alias") => {
                return Err(EsError::mapper_parsing(format!("Type [{ty}] cannot be used in multi field")));
            }
            None => return Err(unknown_type(&ty, mf_name, ctx)),
        };
        if def.name == "join" {
            return Err(EsError::illegal_argument(format!("join field [{name}.{mf_name}] cannot be added inside an object or in a multi-field")));
        }
        if !def.in_multi_field {
            return Err(EsError::mapper_parsing(format!("Field [{mf_name}] of type [{ty}] can't be used in multifields")));
        }
        let sub = FieldMapper::parse(mf_name, def, node, ctx, true)?;
        f.fields.insert(mf_name.clone(), sub);
    }
    Ok(())
}

fn unknown_type(ty: &str, field: &str, ctx: &MappingCtx) -> EsError {
    if ctx.opensearch() {
        return EsError::mapper_parsing(format!("No handler for type [{ty}] declared on field [{field}]"));
    }
    if let Some(why) = types::unsupported(ty) {
        return EsError::mapper_parsing(format!("The mapper type [{ty}] declared on field [{field}] is not supported: {why}."));
    }
    EsError::mapper_parsing(format!(
        "The mapper type [{ty}] declared on field [{field}] does not exist. It might have been created within a future version or requires a plugin to be installed. Check the documentation."
    ))
}

#[derive(Clone, Debug, PartialEq)]
pub enum ObjectKind {
    Object,
    Nested { include_in_parent: Option<bool>, include_in_root: Option<bool> },
    /// `passthrough`: an object without subobjects whose leaf fields also resolve at the root
    /// (the highest `priority` wins), optionally making them all time series dimensions.
    PassThrough { priority: i64, dimensions: Option<bool> },
}

#[derive(Clone, Debug, PartialEq)]
pub struct ObjectMapper {
    pub name: String,
    pub kind: ObjectKind,
    pub dynamic: Option<Dynamic>,
    pub enabled: Option<bool>,
    pub subobjects: Option<bool>,
    pub source_keep: Option<String>,
    /// OpenSearch's `disable_objects`: dotted names stay literal leaf names, and object
    /// children are refused.
    pub disable_objects: Option<bool>,
    pub children: BTreeMap<String, Mapper>,
}

impl ObjectMapper {
    /// `NestedObjectMapper.Builder.build`: `include_in_root` is dropped where it adds nothing —
    /// on a top-level nested object that is included in its parent (the root), and on a nested
    /// object included in a parent that is itself included in the root. `parent_in_root` is
    /// `None` outside nested objects.
    fn normalize_nested(&mut self, parent_in_root: Option<bool>) {
        for child in self.children.values_mut() {
            let Mapper::Object(o) = child else { continue };
            let inner = match &mut o.kind {
                ObjectKind::Nested { include_in_parent, include_in_root } => {
                    let ip = include_in_parent.unwrap_or(false);
                    let ir = include_in_root.unwrap_or(false);
                    let redundant = match parent_in_root {
                        None => ip,
                        Some(p) => p && ip,
                    };
                    if redundant {
                        *include_in_root = None;
                    }
                    Some(if parent_in_root.is_none() { ir || ip } else { ir })
                }
                _ => parent_in_root,
            };
            crate::stack::maybe_grow(|| o.normalize_nested(inner));
        }
    }

    pub fn new(name: &str) -> ObjectMapper {
        ObjectMapper {
            name: name.to_string(),
            kind: ObjectKind::Object,
            dynamic: None,
            enabled: None,
            subobjects: None,
            source_keep: None,
            disable_objects: None,
            children: BTreeMap::new(),
        }
    }

    /// Dotted names in `properties` are leaf names rather than object paths.
    pub fn literal_dots(&self) -> bool {
        !self.subobjects_enabled() || self.disable_objects == Some(true)
    }

    pub fn is_nested(&self) -> bool {
        matches!(self.kind, ObjectKind::Nested { .. })
    }

    pub fn is_enabled(&self) -> bool {
        self.enabled.unwrap_or(true)
    }

    /// A pass-through object with `time_series_dimension: true` makes its leaf fields
    /// dimensions (where the type supports it).
    fn inherit_dimensions(&mut self) {
        if !matches!(self.kind, ObjectKind::PassThrough { dimensions: Some(true), .. }) {
            return;
        }
        for c in self.children.values_mut() {
            if let Mapper::Field(f) = c
                && f.def().param_index("time_series_dimension").is_some()
                && f.raw("time_series_dimension").is_none()
            {
                f.set("time_series_dimension", Value::Bool(true));
            }
        }
    }

    pub fn subobjects_enabled(&self) -> bool {
        !matches!(self.kind, ObjectKind::PassThrough { .. }) && self.subobjects.unwrap_or(true)
    }

    fn type_name(&self) -> &'static str {
        match self.kind {
            ObjectKind::Object => "object",
            ObjectKind::Nested { .. } => "nested",
            ObjectKind::PassThrough { .. } => "passthrough",
        }
    }

    /// Prints the object (not the root).
    fn to_value(&self, ctx: &MappingCtx, include_defaults: bool) -> Value {
        let mut m = Map::new();
        match &self.kind {
            ObjectKind::Nested { include_in_parent, include_in_root } => {
                m.insert("type".into(), "nested".into());
                if let Some(s) = self.subobjects {
                    m.insert("subobjects".into(), s.into());
                }
                if *include_in_parent == Some(true) {
                    m.insert("include_in_parent".into(), true.into());
                }
                if *include_in_root == Some(true) {
                    m.insert("include_in_root".into(), true.into());
                }
                if let Some(d) = self.dynamic {
                    m.insert("dynamic".into(), d.as_str().into());
                }
                if !self.is_enabled() {
                    m.insert("enabled".into(), false.into());
                }
                if let Some(d) = self.disable_objects {
                    m.insert("disable_objects".into(), d.into());
                }
                if let Some(k) = &self.source_keep {
                    m.insert("synthetic_source_keep".into(), k.clone().into());
                }
            }
            ObjectKind::Object => {
                if self.children.is_empty() {
                    m.insert("type".into(), "object".into());
                }
                self.common_to_value(&mut m);
            }
            ObjectKind::PassThrough { priority, dimensions } => {
                m.insert("type".into(), "passthrough".into());
                if let Some(d) = dimensions {
                    m.insert("time_series_dimension".into(), (*d).into());
                }
                if *priority >= 0 {
                    m.insert("priority".into(), (*priority).into());
                }
                if let Some(d) = self.dynamic {
                    m.insert("dynamic".into(), d.as_str().into());
                }
                if !self.is_enabled() {
                    m.insert("enabled".into(), false.into());
                }
            }
        }
        self.properties_into(&mut m, ctx, include_defaults);
        Value::Obj(m)
    }

    fn common_to_value(&self, m: &mut Map) {
        if let Some(d) = self.dynamic {
            m.insert("dynamic".into(), d.as_str().into());
        }
        if !self.is_enabled() {
            m.insert("enabled".into(), false.into());
        }
        if let Some(d) = self.disable_objects {
            m.insert("disable_objects".into(), d.into());
        }
        if let Some(s) = self.subobjects {
            m.insert("subobjects".into(), s.into());
        }
        if let Some(k) = &self.source_keep {
            m.insert("synthetic_source_keep".into(), k.clone().into());
        }
    }

    fn properties_into(&self, m: &mut Map, ctx: &MappingCtx, include_defaults: bool) {
        if self.children.is_empty() {
            return;
        }
        let mut props = Map::new();
        for (k, c) in &self.children {
            props.insert(k.clone(), c.to_value(ctx, include_defaults));
        }
        m.insert("properties".into(), Value::Obj(props));
    }

    /// Parses the object parameters and `properties`. Unknown keys are left for the caller.
    fn parse_into(&mut self, node: &Map, ctx: &MappingCtx, depth: usize, consumed: &mut Vec<String>) -> Result<(), EsError> {
        if ctx.opensearch() {
            // Read first: it decides how dotted names in `properties` are taken.
            if let Some(v) = node.get("disable_objects") {
                let b = node_bool_named(v, "disable_objects.disable_objects").map_err(|e| {
                    EsError::illegal_argument(format!(
                        "Configuration error: Parameter [disable_objects] must be a boolean value (true or false), but got [{}]. {}",
                        java_value(v),
                        e.reason()
                    ))
                })?;
                self.disable_objects = Some(b);
                consumed.push("disable_objects".into());
            }
        } else if let Some(v) = node.get("subobjects") {
            self.subobjects = Some(parse_subobjects(v)?);
            consumed.push("subobjects".into());
        }
        for (k, v) in node {
            match k.as_str() {
                "store_array_source" | "synthetic_source_keep" if ctx.opensearch() => continue,
                "dynamic" => self.dynamic = Some(Dynamic::parse(v, "dynamic", ctx.dialect)?),
                "enabled" => self.enabled = Some(node_bool_named(v, "enabled.enabled")?),
                "store_array_source" => self.source_keep = Some("arrays".into()),
                "synthetic_source_keep" => {
                    let s = v.to_es_string();
                    if !matches!(s.as_str(), "none" | "arrays" | "all") {
                        return Err(EsError::illegal_argument(format!("Unknown synthetic_source_keep value [{s}], accepted values are [none,arrays,all]")));
                    }
                    self.source_keep = Some(s);
                }
                "properties" => match v {
                    Value::Arr(a) if a.is_empty() => {}
                    Value::Obj(props) => self.parse_properties(props, ctx, depth)?,
                    _ => return Err(EsError::parse("properties must be a map type")),
                },
                "include_in_all" => ctx.warn("[include_in_all] is deprecated, the _all field have been removed in this version"),
                _ => continue,
            }
            consumed.push(k.clone());
        }
        Ok(())
    }

    fn parse_properties(&mut self, props: &Map, ctx: &MappingCtx, depth: usize) -> Result<(), EsError> {
        // Elasticsearch reads `properties` into a `HashMap`, so its fields are built in hash
        // order: that decides which error is reported first and which of two definitions of the
        // same field (`a.b` and `a: {properties: {b}}`) wins.
        let entries: Vec<(&String, &Value)> = props.iter().collect();
        for i in java_hash_map_order(entries.iter().map(|(k, _)| k.as_str())) {
            let (field_name, node) = entries[i];
            validate_field_name(field_name)?;
            let node = match node {
                Value::Obj(m) => m,
                Value::Arr(a) if a.is_empty() => continue,
                other => {
                    let _ = other;
                    return Err(EsError::mapper_parsing(format!(
                        "Expected map for property [fields] on field [{field_name}] but got a class java.lang.String"
                    )));
                }
            };
            let ty = match node.get("type") {
                Some(t) => t.to_es_string(),
                None if node.get("properties").is_some_and(|p| !p.is_null()) => "object".into(),
                None if node.len() == 1 && node.contains_key("enabled") => "object".into(),
                None => return Err(EsError::mapper_parsing(format!("No type specified for field [{field_name}]"))),
            };
            let (leaf, parents): (String, Vec<String>) = if !self.literal_dots() {
                let parts: Vec<&str> = field_name.split('.').collect();
                // Java's split drops trailing empty strings; only dots means nothing is left.
                let mut parts: Vec<&str> = parts;
                while parts.last().is_some_and(|p| p.is_empty()) {
                    parts.pop();
                }
                if parts.is_empty() {
                    return Err(EsError::illegal_argument("field name cannot contain only dots"));
                }
                let leaf = parts[parts.len() - 1].to_string();
                validate_field_name(&leaf)?;
                for p in &parts[..parts.len() - 1] {
                    validate_field_name(p)?;
                }
                (leaf, parts[..parts.len() - 1].iter().map(|s| s.to_string()).collect())
            } else {
                (field_name.clone(), Vec::new())
            };
            // Dotted names build their parent objects directly, without counting towards the
            // depth limit (as Elasticsearch's builders do).
            let mut mapper = crate::stack::maybe_grow(|| parse_mapper(&leaf, &ty, node, ctx, depth))?;
            for p in parents.iter().rev() {
                let mut o = ObjectMapper::new(p);
                o.children.insert(mapper.name().to_string(), mapper);
                mapper = Mapper::Object(o);
            }
            let key = mapper.name().to_string();
            match self.children.remove(&key) {
                Some(existing) => {
                    // The same field given twice (object and dotted notation): later definitions win.
                    let merged = existing.merge(&mapper, &key, ctx, MergeReason::IndexTemplate)?;
                    self.children.insert(key, merged);
                }
                None => {
                    self.children.insert(key, mapper);
                }
            }
        }
        Ok(())
    }

    fn merge(&self, incoming: &ObjectMapper, full_path: &str, ctx: &MappingCtx, reason: MergeReason) -> Result<ObjectMapper, EsError> {
        match (&self.kind, &incoming.kind) {
            // Composing templates, a later definition of a different kind replaces the earlier one.
            (ObjectKind::Object, ObjectKind::Nested { .. }) | (ObjectKind::Nested { .. }, ObjectKind::Object) if reason == MergeReason::IndexTemplate => {
                return Ok(incoming.clone());
            }
            (ObjectKind::Object, ObjectKind::Nested { .. }) | (ObjectKind::Nested { .. }, ObjectKind::Object) => {
                return Err(EsError::illegal_argument(format!("can't merge a non-nested mapping [{full_path}] with a nested mapping")));
            }
            (ObjectKind::PassThrough { .. }, ObjectKind::Nested { .. }) | (ObjectKind::Nested { .. }, ObjectKind::PassThrough { .. }) => {
                return Err(EsError::illegal_argument(format!("can't merge a non-nested mapping [{full_path}] with a nested mapping")));
            }
            (ObjectKind::PassThrough { .. }, ObjectKind::Object) if incoming.subobjects == Some(true) => {
                return Err(EsError::illegal_argument(format!(
                    "can't merge a passthrough mapping [{full_path}] with an object mapping that is either root or has subobjects enabled"
                )));
            }
            _ => {}
        }
        let mut out = self.clone();
        // An object becomes a pass-through object when one is merged in.
        if matches!((&self.kind, &incoming.kind), (ObjectKind::Object, ObjectKind::PassThrough { .. })) {
            out.kind = incoming.kind.clone();
            out.subobjects = None;
        }
        if let (ObjectKind::PassThrough { priority: a, dimensions: ad }, ObjectKind::PassThrough { priority: b, dimensions: bd }) = (&self.kind, &incoming.kind) {
            out.kind = ObjectKind::PassThrough { priority: (*a).max(*b), dimensions: bd.or(*ad) };
        }
        if let (ObjectKind::Nested { include_in_parent: a_p, include_in_root: a_r }, ObjectKind::Nested { include_in_parent: b_p, include_in_root: b_r }) =
            (&self.kind, &incoming.kind)
        {
            if reason == MergeReason::IndexTemplate {
                out.kind = ObjectKind::Nested { include_in_parent: b_p.or(*a_p), include_in_root: b_r.or(*a_r) };
            } else {
                if a_p.unwrap_or(false) != b_p.unwrap_or(false) {
                    return Err(EsError::new(500, "mapper_exception", "the [include_in_parent] parameter can't be updated on a nested object mapping"));
                }
                if a_r.unwrap_or(false) != b_r.unwrap_or(false) {
                    return Err(EsError::new(500, "mapper_exception", "the [include_in_root] parameter can't be updated on a nested object mapping"));
                }
            }
        }
        if let Some(e) = incoming.enabled {
            if reason == MergeReason::IndexTemplate {
                out.enabled = Some(e);
            } else if self.is_enabled() != e {
                return Err(EsError::new(500, "mapper_exception", format!("the [enabled] parameter can't be updated for the object mapping [{full_path}]")));
            }
        }
        if let Some(s) = incoming.subobjects {
            if reason == MergeReason::IndexTemplate {
                out.subobjects = Some(s);
            } else if self.subobjects_enabled() != s {
                return Err(EsError::new(500, "mapper_exception", format!("the [subobjects] parameter can't be updated for the object mapping [{full_path}]")));
            }
        }
        if let Some(k) = &incoming.source_keep {
            if reason == MergeReason::IndexTemplate {
                out.source_keep = Some(k.clone());
            } else if self.source_keep.as_ref() != Some(k) {
                return Err(EsError::new(500, "mapper_exception", format!("the [ synthetic_source_keep ] parameter can't be updated for the object mapping [{full_path}]")));
            }
        }
        if incoming.dynamic.is_some() {
            out.dynamic = incoming.dynamic;
        }
        let own_name = if full_path.is_empty() { "_doc" } else { full_path };
        if let Some(b) = incoming.disable_objects {
            match self.disable_objects {
                Some(a) if a != b && reason != MergeReason::IndexTemplate => {
                    return Err(EsError::new(500, "mapper_exception", format!(
                        "Cannot update parameter [disable_objects] from [{a}] to [{b}] for object mapping [{own_name}]. The disable_objects setting is immutable after index creation."
                    )));
                }
                _ => out.disable_objects = Some(b),
            }
        }
        // Under `subobjects: false`, an update's objects are dotted leaf names: flatten them
        // first so `user: {id}` meets the existing `user.id` (and its conflicts).
        let flattened;
        let incoming_children = if out.subobjects_enabled() {
            &incoming.children
        } else {
            let mut c = incoming.clone();
            c.flatten_subobjects();
            flattened = c;
            &flattened.children
        };
        for (k, child) in incoming_children {
            let path = if full_path.is_empty() { k.clone() } else { format!("{full_path}.{k}") };
            let merged = match self.children.get(k) {
                Some(existing) => existing.merge(child, &path, ctx, reason)?,
                None if out.disable_objects == Some(true) && reason != MergeReason::IndexTemplate && matches!(child, Mapper::Object(_)) => {
                    return Err(EsError::new(500, "mapper_exception", disable_objects_conflict(&path, own_name)));
                }
                None => child.clone(),
            };
            out.children.insert(k.clone(), merged);
        }
        if !out.subobjects_enabled() {
            out.flatten_subobjects();
        }
        out.inherit_dimensions();
        Ok(out)
    }

    /// With `subobjects: false`, plain object children (merged in from a template parsed on
    /// its own) are flattened into dotted leaf names, as Elasticsearch auto-flattens them.
    fn flatten_subobjects(&mut self) {
        let names: Vec<String> = self
            .children
            .iter()
            .filter(|(_, c)| matches!(c, Mapper::Object(o) if !o.is_nested() && o.enabled.is_none() && o.dynamic.is_none() && o.subobjects.is_none()))
            .map(|(k, _)| k.clone())
            .collect();
        for k in names {
            let Some(Mapper::Object(mut o)) = self.children.remove(&k) else { continue };
            crate::stack::maybe_grow(|| o.flatten_subobjects_all());
            for (ck, child) in std::mem::take(&mut o.children) {
                self.children.insert(format!("{k}.{ck}"), child);
            }
        }
    }

    /// Flattens every plain object below this one (used under a `subobjects: false` parent).
    fn flatten_subobjects_all(&mut self) {
        let names: Vec<String> = self.children.iter().filter(|(_, c)| matches!(c, Mapper::Object(o) if !o.is_nested())).map(|(k, _)| k.clone()).collect();
        for k in names {
            let Some(Mapper::Object(mut o)) = self.children.remove(&k) else { continue };
            crate::stack::maybe_grow(|| o.flatten_subobjects_all());
            for (ck, child) in std::mem::take(&mut o.children) {
                self.children.insert(format!("{k}.{ck}"), child);
            }
        }
    }
}

/// OpenSearch's text for an object below a `disable_objects: true` object.
fn disable_objects_conflict(child: &str, parent: &str) -> String {
    format!(
        "Field mapping conflict: Cannot add nested object field [{child}] when disable_objects is enabled for [{parent}]. With disable_objects=true, all fields must use flat field notation (e.g., 'field.name' as a single field) rather than nested object structures."
    )
}

/// OpenSearch refuses object children under `disable_objects: true` when the mapping is built.
fn check_disable_objects(o: &ObjectMapper, path: &str) -> Result<(), EsError> {
    for (k, c) in &o.children {
        let Mapper::Object(child) = c else { continue };
        let child_path = if path.is_empty() { k.clone() } else { format!("{path}.{k}") };
        if o.disable_objects == Some(true) {
            let parent = if path.is_empty() { "_doc" } else { o.name.as_str() };
            return Err(EsError::mapper_parsing(disable_objects_conflict(&child_path, parent)));
        }
        crate::stack::maybe_grow(|| check_disable_objects(child, &child_path))?;
    }
    Ok(())
}

fn parse_subobjects(v: &Value) -> Result<bool, EsError> {
    match v {
        Value::Bool(b) => Ok(*b),
        Value::Str(s) if s.eq_ignore_ascii_case("true") => Ok(true),
        Value::Str(s) if s.eq_ignore_ascii_case("false") => Ok(false),
        other => Err(EsError::parse(format!("unknown subobjects value: {}", other.to_es_string()))),
    }
}

fn validate_field_name(name: &str) -> Result<(), EsError> {
    if name.is_empty() {
        return Err(EsError::illegal_argument("field name cannot be an empty string"));
    }
    if name.trim().is_empty() {
        return Err(EsError::illegal_argument("field name cannot contain only whitespaces"));
    }
    Ok(())
}

/// A field alias.
#[derive(Clone, Debug, PartialEq)]
pub struct AliasMapper {
    pub name: String,
    pub path: String,
}

#[derive(Clone, Debug, PartialEq)]
pub enum Mapper {
    Object(ObjectMapper),
    Field(FieldMapper),
    Alias(AliasMapper),
}

impl Mapper {
    pub fn name(&self) -> &str {
        match self {
            Mapper::Object(o) => &o.name,
            Mapper::Field(f) => &f.name,
            Mapper::Alias(a) => &a.name,
        }
    }

    pub fn type_name(&self) -> &str {
        match self {
            Mapper::Object(o) => o.type_name(),
            Mapper::Field(f) => f.ty,
            Mapper::Alias(_) => "alias",
        }
    }

    fn to_value(&self, ctx: &MappingCtx, include_defaults: bool) -> Value {
        match self {
            Mapper::Object(o) => crate::stack::maybe_grow(|| o.to_value(ctx, include_defaults)),
            Mapper::Field(f) => f.to_value(ctx, include_defaults),
            Mapper::Alias(a) => obj! { "type" => "alias", "path" => a.path.clone() },
        }
    }

    fn merge(&self, incoming: &Mapper, full_path: &str, ctx: &MappingCtx, reason: MergeReason) -> Result<Mapper, EsError> {
        match (self, incoming) {
            (Mapper::Object(a), Mapper::Object(b)) => Ok(Mapper::Object(crate::stack::maybe_grow(|| a.merge(b, full_path, ctx, reason))?)),
            (Mapper::Field(a), Mapper::Field(b)) => {
                if reason == MergeReason::IndexTemplate && a.ty != b.ty {
                    return Ok(incoming.clone());
                }
                Ok(Mapper::Field(a.merge(b, full_path, ctx, reason)?))
            }
            (Mapper::Alias(_), Mapper::Alias(b)) => Ok(Mapper::Alias(b.clone())),
            (Mapper::Field(_), Mapper::Alias(_)) | (Mapper::Alias(_), Mapper::Field(_)) if reason == MergeReason::IndexTemplate => Ok(incoming.clone()),
            (Mapper::Alias(_), _) => Err(EsError::illegal_argument(format!(
                "Cannot merge a field alias mapping [{full_path}] with a mapping that is not for a field alias."
            ))),
            (_, Mapper::Alias(_)) => Err(EsError::illegal_argument(format!(
                "Cannot merge a non-alias mapping [{full_path}] with a field alias mapping"
            ))),
            (Mapper::Object(_), _) | (_, Mapper::Object(_)) => Err(EsError::illegal_argument(format!(
                "can't merge a non object mapping [{full_path}] with an object mapping"
            ))),
        }
    }
}

fn parse_mapper(name: &str, ty: &str, node: &Map, ctx: &MappingCtx, depth: usize) -> Result<Mapper, EsError> {
    match ty {
        "passthrough" => {
            let mut o = ObjectMapper::new(name);
            let mut consumed = vec!["type".to_string()];
            let mut dimensions = None;
            let mut priority = -1;
            if let Some(v) = node.get("time_series_dimension") {
                dimensions = Some(node_bool_named(v, &format!("{name}time_series_dimension"))?);
                consumed.push("time_series_dimension".into());
            }
            if let Some(v) = node.get("priority") {
                priority = params::node_int(v)?;
                consumed.push("priority".into());
            }
            o.kind = ObjectKind::PassThrough { priority, dimensions };
            // Subobjects can't be configured: `subobjects` is left as an unsupported parameter.
            let mut rest = node.clone();
            rest.shift_remove("subobjects");
            o.parse_into(&rest, ctx, depth, &mut consumed)?;
            crate::stack::maybe_grow(|| o.flatten_subobjects_all());
            check_remaining(name, node, &consumed)?;
            o.inherit_dimensions();
            if priority < 0 {
                return Err(EsError::new(500, "mapper_exception", format!("Pass-through object [{name}] is missing a non-negative value for parameter [priority]")));
            }
            Ok(Mapper::Object(o))
        }
        "object" | "nested" => {
            let mut o = ObjectMapper::new(name);
            let mut consumed = vec!["type".to_string()];
            if ty == "nested" {
                let mut ip = None;
                let mut ir = None;
                if let Some(v) = node.get("include_in_parent") {
                    ip = Some(node_bool_named(v, &format!("{name}.include_in_parent"))?);
                    consumed.push("include_in_parent".into());
                }
                if let Some(v) = node.get("include_in_root") {
                    ir = Some(node_bool_named(v, &format!("{name}.include_in_root"))?);
                    consumed.push("include_in_root".into());
                }
                o.kind = ObjectKind::Nested { include_in_parent: ip, include_in_root: ir };
            }
            // `MappingParserContext.incrementMappingObjectDepth`: only plain objects count, and the
            // check runs while parsing, so arbitrarily deep mappings fail before being built.
            let depth = if ty == "object" { depth + 1 } else { depth };
            if ty == "object" && depth as i64 > ctx.depth_limit {
                return Err(EsError::mapper_parsing(format!("Limit of mapping depth [{}] has been exceeded", ctx.depth_limit)));
            }
            o.parse_into(node, ctx, depth, &mut consumed)?;
            check_remaining(name, node, &consumed)?;
            Ok(Mapper::Object(o))
        }
        "alias" => {
            let mut path = None;
            let mut rest = Vec::new();
            for (k, v) in node {
                match k.as_str() {
                    "type" => {}
                    "path" => path = Some(v.to_es_string()),
                    _ => rest.push(k.clone()),
                }
            }
            if !rest.is_empty() {
                let consumed: Vec<String> = vec!["type".into(), "path".into()];
                check_remaining(name, node, &consumed)?;
            }
            let Some(path) = path else {
                return Err(EsError::mapper_parsing(format!("The [path] property must be specified for field [{name}].")));
            };
            Ok(Mapper::Alias(AliasMapper { name: name.to_string(), path }))
        }
        _ => {
            let def = types::lookup_in(ctx.dialect, ty).ok_or_else(|| unknown_type(ty, name, ctx))?;
            Ok(Mapper::Field(FieldMapper::parse(name, def, node, ctx, false)?))
        }
    }
}

/// `Mapping definition for [x] has unsupported parameters:  [k : v]`.
fn check_remaining(name: &str, node: &Map, consumed: &[String]) -> Result<(), EsError> {
    let rest: Vec<(&String, &Value)> = node.iter().filter(|(k, _)| !consumed.iter().any(|c| c == *k)).collect();
    if rest.is_empty() {
        return Ok(());
    }
    Err(EsError::mapper_parsing(format!("Mapping definition for [{name}] has unsupported parameters: {}", remaining_text(&rest))))
}

fn remaining_text(rest: &[(&String, &Value)]) -> String {
    let mut s = String::new();
    for (k, v) in rest {
        s.push_str(&format!(" [{k} : {}]", java_value(v)));
    }
    s
}

/// Java's `toString()` of a value parsed into a map.
fn java_value(v: &Value) -> String {
    match v {
        Value::Obj(m) => java_map_string(m),
        other => other.to_es_string(),
    }
}

#[derive(Clone, Debug, PartialEq)]
pub enum MatchType {
    Default,
    Simple,
    Regex,
}

/// One entry of `dynamic_templates`.
#[derive(Clone, Debug, PartialEq)]
pub struct DynamicTemplate {
    pub name: String,
    pub match_: Vec<String>,
    pub path_match: Vec<String>,
    pub unmatch: Vec<String>,
    pub path_unmatch: Vec<String>,
    pub match_mapping_type: Vec<String>,
    pub unmatch_mapping_type: Vec<String>,
    pub match_pattern: MatchType,
    pub mapping: Map,
    pub runtime: bool,
    /// The JSON types this template applies to.
    pub xcontent_types: Vec<&'static str>,
    /// `match_pattern: regex`: the patterns compiled once, at mapping time.
    regexes: Option<Arc<TemplateRegexes>>,
}

/// Compiled `match_pattern: regex` patterns (Java's `String.matches`: anchored, whole name).
/// The `regex` crate runs in linear time, so no field name can make matching blow up. Java
/// constructs it lacks (look-around, backreferences) are accepted, as Elasticsearch accepts
/// them, but never match (`None`).
#[derive(Debug)]
struct TemplateRegexes {
    match_: Vec<Option<regex::Regex>>,
    path_match: Vec<Option<regex::Regex>>,
    unmatch: Vec<Option<regex::Regex>>,
    path_unmatch: Vec<Option<regex::Regex>>,
}

/// Java's `\d`, `\w`, `\s` (and negations) and `\b`/`\B` are ASCII-only, where the `regex`
/// crate's are Unicode: rewrites them to the ASCII classes, inside and outside brackets.
fn java_classes(p: &str) -> std::borrow::Cow<'_, str> {
    if !p.contains('\\') {
        return std::borrow::Cow::Borrowed(p);
    }
    let mut out = String::with_capacity(p.len() + 16);
    let mut depth = 0usize;
    let mut it = p.chars().peekable();
    while let Some(c) = it.next() {
        match c {
            '\\' => {
                let Some(n) = it.next() else {
                    out.push(c);
                    break;
                };
                let class = match n {
                    'd' => Some("digit"),
                    'D' => Some("^digit"),
                    'w' => Some("word"),
                    'W' => Some("^word"),
                    's' => Some("space"),
                    'S' => Some("^space"),
                    _ => None,
                };
                match (class, depth) {
                    (Some(cl), 0) => {
                        out.push_str("[[:");
                        out.push_str(cl);
                        out.push_str(":]]");
                    }
                    (Some(cl), _) => {
                        out.push_str("[:");
                        out.push_str(cl);
                        out.push_str(":]");
                    }
                    (None, 0) if n == 'b' || n == 'B' => {
                        out.push_str("(?-u:\\");
                        out.push(n);
                        out.push(')');
                    }
                    _ => {
                        out.push(c);
                        out.push(n);
                    }
                }
            }
            '[' => {
                depth += 1;
                out.push(c);
                // A leading `]` (or `^]`) is literal.
                if it.peek() == Some(&'^') {
                    out.push(it.next().unwrap_or('^'));
                }
                if it.peek() == Some(&']') {
                    out.push(it.next().unwrap_or(']'));
                }
            }
            ']' if depth > 0 => {
                depth -= 1;
                out.push(c);
            }
            _ => out.push(c),
        }
    }
    std::borrow::Cow::Owned(out)
}

impl PartialEq for TemplateRegexes {
    /// Derived from the pattern strings, which the template compares already.
    fn eq(&self, _: &Self) -> bool {
        true
    }
}

impl TemplateRegexes {
    fn compile(t: &DynamicTemplate) -> Result<TemplateRegexes, EsError> {
        let all = |list: &[String]| -> Result<Vec<Option<regex::Regex>>, EsError> {
            list.iter()
                .map(|p| match regex::Regex::new(&format!("^(?:{})$", java_classes(p))) {
                    Ok(r) => Ok(Some(r)),
                    Err(err) => {
                        use regex_syntax::ast::ErrorKind;
                        match regex_syntax::ast::parse::Parser::new().parse(p) {
                            Err(e) if matches!(e.kind(), ErrorKind::UnsupportedLookAround | ErrorKind::UnsupportedBackreference) => Ok(None),
                            // Java's `PatternSyntaxException` texts differ; the type is the same.
                            _ => Err(EsError::mapper_parsing(format!(
                                "Pattern [{p}] of type [regex] is invalid. Cannot create dynamic template [{}].",
                                t.name
                            ))
                            .caused_by(EsError::java(400, "pattern_syntax_exception", err.to_string()))),
                        }
                    }
                })
                .collect()
        };
        Ok(TemplateRegexes { match_: all(&t.match_)?, path_match: all(&t.path_match)?, unmatch: all(&t.unmatch)?, path_unmatch: all(&t.path_unmatch)? })
    }
}

const XCONTENT_TYPES: [&str; 7] = ["object", "string", "long", "double", "boolean", "date", "binary"];

impl DynamicTemplate {
    fn parse(name: &str, conf: &Map) -> Result<DynamicTemplate, EsError> {
        let mut t = DynamicTemplate {
            name: name.to_string(),
            match_: Vec::new(),
            path_match: Vec::new(),
            unmatch: Vec::new(),
            path_unmatch: Vec::new(),
            match_mapping_type: Vec::new(),
            unmatch_mapping_type: Vec::new(),
            match_pattern: MatchType::Default,
            mapping: Map::new(),
            runtime: false,
            xcontent_types: Vec::new(),
            regexes: None,
        };
        let mut mapping_set = false;
        for (k, v) in conf {
            let list = |v: &Value| -> Result<Vec<String>, EsError> {
                match v {
                    Value::Arr(a) => a
                        .iter()
                        .map(|x| match x {
                            Value::Str(s) => Ok(s.clone()),
                            _ => Err(EsError::mapper_parsing(format!(
                                "[{k}] values must either be a string or list of strings, but was [{}]",
                                v.to_es_string()
                            ))),
                        })
                        .collect(),
                    other => Ok(vec![other.to_es_string()]),
                }
            };
            match k.as_str() {
                "match" => t.match_.extend(list(v)?),
                "path_match" => t.path_match.extend(list(v)?),
                "unmatch" => t.unmatch.extend(list(v)?),
                "path_unmatch" => t.path_unmatch.extend(list(v)?),
                "match_mapping_type" => t.match_mapping_type.extend(list(v)?),
                "unmatch_mapping_type" => t.unmatch_mapping_type.extend(list(v)?),
                "match_pattern" => {
                    t.match_pattern = match v.to_es_string().as_str() {
                        "default" => MatchType::Default,
                        "simple" => MatchType::Simple,
                        "regex" => MatchType::Regex,
                        other => return Err(EsError::illegal_argument(format!("No matching pattern matched on [{other}]"))),
                    }
                }
                "mapping" | "runtime" => {
                    if mapping_set {
                        return Err(EsError::mapper_parsing(format!("mapping and runtime cannot be both specified in the same dynamic template [{name}]")));
                    }
                    mapping_set = true;
                    t.runtime = k == "runtime";
                    t.mapping = v.as_object().cloned().unwrap_or_default();
                }
                other => return Err(EsError::illegal_argument(format!("Illegal dynamic template parameter: [{other}]"))),
            }
        }
        if !mapping_set {
            return Err(EsError::mapper_parsing(format!("template [{name}] must have either mapping or runtime set")));
        }
        let wildcard = (t.match_mapping_type.is_empty() && (!t.match_.is_empty() || !t.path_match.is_empty() || !t.unmatch_mapping_type.is_empty()))
            || (t.match_mapping_type.len() == 1 && t.match_mapping_type[0] == "*");
        let check = |s: &str| -> Result<&'static str, EsError> {
            XCONTENT_TYPES.iter().copied().find(|x| *x == s).ok_or_else(|| {
                EsError::illegal_argument(format!(
                    "No field type matched on [{s}], possible values are [object, string, long, double, boolean, date, binary]"
                ))
            })
        };
        let mut types: Vec<&'static str> = if wildcard {
            XCONTENT_TYPES.iter().copied().filter(|x| !t.runtime || !matches!(*x, "object" | "binary")).collect()
        } else {
            let mut v = Vec::new();
            for s in &t.match_mapping_type {
                v.push(check(s)?);
            }
            if t.runtime {
                let unsupported: Vec<&str> = v.iter().copied().filter(|x| matches!(*x, "object" | "binary")).collect();
                if !unsupported.is_empty() {
                    let n = unsupported.len();
                    return Err(EsError::mapper_parsing(format!(
                        "Dynamic template [{name}] defines a runtime field but type{} [{}] {} not supported as runtime field",
                        if n == 1 { "" } else { "s" },
                        unsupported.join(", "),
                        if n == 1 { "is" } else { "are" }
                    )));
                }
            }
            v
        };
        let mut unmatch = Vec::new();
        for s in &t.unmatch_mapping_type {
            unmatch.push(check(s)?);
        }
        types.retain(|x| !unmatch.contains(x));
        t.xcontent_types = types;
        if t.match_pattern == MatchType::Regex {
            t.regexes = Some(Arc::new(TemplateRegexes::compile(&t)?));
        }
        Ok(t)
    }

    fn validate_patterns(&self, ctx: &MappingCtx) {
        if self.match_pattern != MatchType::Default {
            return;
        }
        for p in self.match_.iter().chain(&self.unmatch).chain(&self.path_match).chain(&self.path_unmatch) {
            if p.starts_with('*') {
                continue;
            }
            let candidate = p.starts_with(".*")
                || p.contains('^')
                || p.contains('$')
                || p.contains('|')
                || (p.contains('[') && p.contains(']'))
                || (p.contains('{') && p.contains('}'));
            if candidate {
                ctx.warn(format!(
                    "Pattern [{p}] in dynamic template [{}] appears to be a regular expression, not a simple wildcard pattern. To remove this warning, set [match_pattern] in the dynamic template.",
                    self.name
                ));
            }
        }
    }

    pub fn to_value(&self) -> Value {
        let mut m = Map::new();
        let put = |m: &mut Map, k: &str, l: &Vec<String>| match l.len() {
            0 => {}
            1 => {
                m.insert(k.into(), Value::from(l[0].clone()));
            }
            _ => {
                m.insert(k.into(), Value::from(l.clone()));
            }
        };
        put(&mut m, "match", &self.match_);
        put(&mut m, "path_match", &self.path_match);
        put(&mut m, "unmatch", &self.unmatch);
        put(&mut m, "path_unmatch", &self.path_unmatch);
        put(&mut m, "match_mapping_type", &self.match_mapping_type);
        put(&mut m, "unmatch_mapping_type", &self.unmatch_mapping_type);
        match self.match_pattern {
            MatchType::Default => {}
            MatchType::Simple => {
                m.insert("match_pattern".into(), "simple".into());
            }
            MatchType::Regex => {
                m.insert("match_pattern".into(), "regex".into());
            }
        }
        let mut sorted: Vec<(&String, &Value)> = self.mapping.iter().collect();
        sorted.sort_by(|a, b| a.0.cmp(b.0));
        let mut mm = Map::new();
        for (k, v) in sorted {
            mm.insert(k.clone(), v.clone());
        }
        m.insert(if self.runtime { "runtime" } else { "mapping" }.into(), Value::Obj(mm));
        Value::Obj(m)
    }

    /// The mapping type for a dynamic type (`{dynamic_type}` substitution, `index: not_analyzed`).
    pub fn mapping_type(&self, dynamic_type: &str) -> String {
        let ty = match self.mapping.get("type") {
            Some(t) => t
                .to_es_string()
                .replace("{{dynamic_type}}", dynamic_type)
                .replace("{dynamic_type}", dynamic_type)
                .replace("{{dynamicType}}", dynamic_type)
                .replace("{dynamicType}", dynamic_type),
            None => dynamic_type.to_string(),
        };
        let raw = self.mapping.get("type").map(Value::to_es_string);
        if raw.as_deref() != Some(ty.as_str()) && ty == "text" {
            let index = self.mapping.get("index").map(Value::to_es_string);
            if matches!(index.as_deref(), Some("not_analyzed") | Some("no")) {
                return "keyword".into();
            }
        }
        ty
    }

    /// The mapping for a concrete field, with `{name}` and `{dynamic_type}` substituted.
    pub fn mapping_for(&self, name: &str, dynamic_type: &str) -> Map {
        fn subst(s: &str, name: &str, dt: &str) -> String {
            let mut s = s.to_string();
            if s.contains("{{") {
                s = s.replace("{{name}}", name).replace("{{dynamic_type}}", dt).replace("{{dynamicType}}", dt);
            }
            if s.contains('{') {
                s = s.replace("{name}", name).replace("{dynamic_type}", dt).replace("{dynamicType}", dt);
            }
            s
        }
        fn walk(v: &Value, name: &str, dt: &str) -> Value {
            match v {
                Value::Obj(m) => Value::Obj(m.iter().map(|(k, v)| (subst(k, name, dt), crate::stack::maybe_grow(|| walk(v, name, dt)))).collect()),
                Value::Arr(a) => Value::Arr(a.iter().map(|v| crate::stack::maybe_grow(|| walk(v, name, dt))).collect()),
                Value::Str(s) => Value::Str(subst(s, name, dt)),
                other => other.clone(),
            }
        }
        match walk(&Value::Obj(self.mapping.clone()), name, dynamic_type) {
            Value::Obj(m) => m,
            _ => Map::new(),
        }
    }

    /// `DynamicTemplate.match`.
    pub fn matches(&self, path: &str, field: &str, xtype: &str) -> bool {
        if !self.xcontent_types.contains(&xtype) {
            return false;
        }
        if let Some(rx) = &self.regexes {
            let any = |list: &[Option<regex::Regex>], v: &str| list.iter().flatten().any(|r| r.is_match(v));
            return (rx.path_match.is_empty() || any(&rx.path_match, path))
                && (rx.match_.is_empty() || any(&rx.match_, field))
                && !any(&rx.path_unmatch, path)
                && !any(&rx.unmatch, field);
        }
        let m = crate::xcontent::filter::glob_match;
        if !self.path_match.is_empty() && !self.path_match.iter().any(|p| m(p, path)) {
            return false;
        }
        if !self.match_.is_empty() && !self.match_.iter().any(|p| m(p, field)) {
            return false;
        }
        if self.path_unmatch.iter().any(|p| m(p, path)) || self.unmatch.iter().any(|p| m(p, field)) {
            return false;
        }
        true
    }
}

/// A runtime field.
#[derive(Clone, Debug, PartialEq)]
pub struct RuntimeField {
    pub ty: String,
    /// Parameters other than `type`, in Elasticsearch's print order.
    pub params: Map,
}

const RUNTIME_TYPES: &[&str] = &["boolean", "composite", "date", "double", "geo_point", "geo_shape", "ip", "keyword", "long", "lookup"];

impl RuntimeField {
    fn parse(name: &str, v: &Value) -> Result<RuntimeField, EsError> {
        let Value::Obj(m) = v else {
            return Err(EsError::mapper_parsing(format!("Expected map for runtime field [{name}] definition but got a {}", v.to_es_string())));
        };
        let Some(ty) = m.get("type") else {
            return Err(EsError::mapper_parsing(format!("No type specified for runtime field [{name}]")));
        };
        let ty = ty.to_es_string();
        if !RUNTIME_TYPES.contains(&ty.as_str()) {
            return Err(EsError::mapper_parsing(format!(
                "The mapper type [{ty}] declared on runtime field [{name}] does not exist. It might have been created within a future version or requires a plugin to be installed. Check the documentation."
            )));
        }
        let allowed: &[&str] = match ty.as_str() {
            "date" => &["script", "format", "locale", "on_script_error", "meta"],
            "composite" => &["script", "fields", "on_script_error", "meta"],
            "lookup" => &["target_index", "input_field", "target_field", "fetch_fields", "meta"],
            _ => &["script", "on_script_error", "meta"],
        };
        let mut params = Map::new();
        for key in ["script", "format", "locale", "fields", "target_index", "input_field", "target_field", "fetch_fields", "on_script_error", "meta"] {
            if let Some(v) = m.get(key) {
                let v = match key {
                    "script" => params::parse_script(name, v)?,
                    "meta" => params::parse_meta(name, v)?,
                    "format" => {
                        let s = v.to_es_string();
                        crate::time::validate_format(&s).map_err(|e| EsError::illegal_argument(format!("Error parsing [format] on field [{name}]: {e}")))?;
                        Value::Str(s)
                    }
                    _ => v.clone(),
                };
                params.insert(key.to_string(), v);
            }
        }
        for k in m.keys() {
            if k != "type" && !allowed.contains(&k.as_str()) {
                return Err(EsError::mapper_parsing(format!("unknown parameter [{k}] on runtime field [{name}] of type [{ty}]")));
            }
        }
        if ty == "composite" && params.get("script").is_none_or(Value::is_null) {
            return Err(EsError::illegal_argument(format!("composite runtime field [{name}] must declare a [script]")));
        }
        if ty == "lookup" {
            for req in ["target_index", "input_field", "target_field", "fetch_fields"] {
                if !params.contains_key(req) {
                    return Err(EsError::mapper_parsing(format!("[{req}] parameter must be specified for lookup runtime field [{name}]")));
                }
            }
        }
        Ok(RuntimeField { ty, params })
    }

    fn to_value(&self) -> Value {
        let mut m = Map::new();
        m.insert("type".into(), Value::from(self.ty.clone()));
        for (k, v) in &self.params {
            m.insert(k.clone(), v.clone());
        }
        Value::Obj(m)
    }
}

/// `_source` metadata field configuration (only what was set).
#[derive(Clone, Debug, Default, PartialEq)]
pub struct SourceConfig {
    pub enabled: Option<bool>,
    pub mode: Option<String>,
    pub includes: Option<Vec<String>>,
    pub excludes: Option<Vec<String>>,
}

impl SourceConfig {
    /// The `_source` mode name `_cluster/stats` reports (`stored`, `synthetic`, `disabled`).
    pub fn mode_name(&self) -> String {
        match (&self.mode, self.enabled) {
            (Some(m), _) => m.to_ascii_lowercase(),
            (None, Some(false)) => "disabled".into(),
            _ => "stored".into(),
        }
    }

    fn is_empty(&self) -> bool {
        self.enabled.is_none() && self.includes.is_none() && self.excludes.is_none()
    }
}

/// A complete index mapping (the `_doc` root).
#[derive(Clone, Debug, PartialEq)]
pub struct Mapping {
    pub root: ObjectMapper,
    pub dynamic_templates: Option<Vec<DynamicTemplate>>,
    pub dynamic_date_formats: Option<Vec<String>>,
    pub date_detection: Option<bool>,
    pub numeric_detection: Option<bool>,
    pub runtime: BTreeMap<String, RuntimeField>,
    /// Runtime fields removed by an update (`"r": null`).
    removed_runtime: Vec<String>,
    pub source: SourceConfig,
    pub routing_required: Option<bool>,
    /// `_routing.doc_values` (routing kept as doc values, for index sorting and splits).
    pub routing_doc_values: Option<bool>,
    /// `_id.mode`: `columnar` keeps ids in doc values only; `None` is `document` (or the strict
    /// columnar modes' default).
    pub id_mode: Option<String>,
    /// Whether the parsed source had a `_routing` object (an update naming `_routing` restates
    /// both of its parameters, defaults included).
    routing_present: bool,
    pub meta: Option<Map>,
    pub data_stream_timestamp: Option<bool>,
    /// `prefix_properties`: `dynamic`/`enabled` of object paths whose fields are flattened
    /// (strict columnar modes), by dotted path.
    pub prefix_properties: BTreeMap<String, PrefixProps>,
}

/// The object-level parameters a strict columnar mapping keeps per field prefix.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct PrefixProps {
    pub dynamic: Option<Dynamic>,
    pub enabled: Option<bool>,
}

impl PrefixProps {
    fn to_value(&self) -> Value {
        let mut m = Map::new();
        if let Some(d) = self.dynamic {
            m.insert("dynamic".into(), d.as_str().into());
        }
        if let Some(e) = self.enabled {
            m.insert("enabled".into(), e.into());
        }
        Value::Obj(m)
    }

    fn parse(path: &str, v: &Value) -> Result<PrefixProps, EsError> {
        let Value::Obj(m) = v else {
            return Err(EsError::mapper_parsing(format!("[prefix_properties.{path}] must be an object")));
        };
        let mut p = PrefixProps::default();
        for (k, v) in m {
            match k.as_str() {
                "dynamic" => {
                    let Value::Str(s) = v else {
                        return Err(EsError::mapper_parsing(format!("[prefix_properties.{path}.dynamic] must be a string value")));
                    };
                    p.dynamic = Some(match s.to_ascii_lowercase().as_str() {
                        "true" => Dynamic::True,
                        "false" => Dynamic::False,
                        "strict" => Dynamic::Strict,
                        "runtime" => return Err(EsError::mapper_parsing(format!("[prefix_properties.{path}.dynamic] does not support [runtime]"))),
                        _ => return Err(EsError::mapper_parsing(format!("unknown value [{s}] for [prefix_properties.{path}.dynamic]"))),
                    });
                }
                "enabled" => {
                    p.enabled = Some(match v {
                        Value::Bool(b) => *b,
                        Value::Str(s) if s == "true" => true,
                        Value::Str(s) if s == "false" => false,
                        other => {
                            return Err(EsError::illegal_argument(format!("Could not convert [prefix_properties.{path}.enabled] to boolean")).caused_by(
                                EsError::illegal_argument(format!("Failed to parse value [{}] as only [true] or [false] are allowed.", other.to_es_string())),
                            ));
                        }
                    })
                }
                // Unknown parameters are dropped.
                _ => {}
            }
        }
        Ok(p)
    }
}

/// The types a `flattened` field's `properties` may map its keys to.
const FLATTENED_SUB_FIELD_TYPES: &[&str] = &[
    "keyword", "constant_keyword", "wildcard", "text", "long", "integer", "short", "byte", "double", "float", "half_float", "scaled_float",
    "unsigned_long", "date", "date_nanos", "boolean", "ip",
];

/// `FlattenedFieldMapper.Builder.parseProperties`: a flat map of dotted keys to leaf fields of
/// an allowed type, without `copy_to` or multi-fields.
fn parse_flattened_properties(flattened: &str, v: &Value, ctx: &MappingCtx) -> Result<BTreeMap<String, FieldMapper>, EsError> {
    let mut out = BTreeMap::new();
    let props = match v {
        Value::Arr(a) if a.is_empty() => return Ok(out),
        Value::Obj(o) => o,
        _ => return Err(EsError::mapper_parsing(format!("[properties] on flattened field [{flattened}] must be a map"))),
    };
    for (name, node) in props {
        let Value::Obj(node) = node else {
            return Err(EsError::mapper_parsing(format!("Expected map for property [{name}] in flattened field [{flattened}]")));
        };
        let Some(ty) = node.get("type") else {
            return Err(EsError::mapper_parsing(format!("No type specified for property [{name}] in flattened field [{flattened}]")));
        };
        let ty = ty.to_es_string();
        if !FLATTENED_SUB_FIELD_TYPES.contains(&ty.as_str()) {
            return Err(EsError::mapper_parsing(format!(
                "Type [{ty}] is not supported as a mapped sub-field of flattened field [{flattened}]. Supported types: [{}]",
                FLATTENED_SUB_FIELD_TYPES.join(", ")
            )));
        }
        if node.contains_key("copy_to") {
            return Err(EsError::mapper_parsing(format!("[copy_to] is not supported on properties of flattened field [{flattened}]")));
        }
        if node.contains_key("fields") {
            return Err(EsError::mapper_parsing(format!("[fields] (multi-fields) is not supported on properties of flattened field [{flattened}]")));
        }
        let Some(def) = types::lookup(&ty) else {
            return Err(EsError::mapper_parsing(format!("No handler for type [{ty}] declared on property [{name}]")));
        };
        out.insert(name.clone(), FieldMapper::parse(name, def, node, ctx, false)?);
    }
    Ok(out)
}

pub const METADATA_FIELDS: &[&str] = &[
    "_id", "_routing", "_index", "_source", "_seq_no", "_version", "_ignored", "_ignored_source", "_nested_path", "_field_names",
    "_doc_count", "_data_stream_timestamp", "_tier", "_feature", "_primary_term", "_tsid", "_ts_routing_hash",
];

impl Default for Mapping {
    fn default() -> Self {
        Mapping::empty()
    }
}

impl Mapping {
    pub fn empty() -> Mapping {
        Mapping {
            root: ObjectMapper::new("_doc"),
            dynamic_templates: None,
            dynamic_date_formats: None,
            date_detection: None,
            numeric_detection: None,
            runtime: BTreeMap::new(),
            removed_runtime: Vec::new(),
            source: SourceConfig::default(),
            routing_required: None,
            routing_doc_values: None,
            id_mode: None,
            routing_present: false,
            meta: None,
            data_stream_timestamp: None,
            prefix_properties: BTreeMap::new(),
        }
    }

    pub fn is_empty(&self) -> bool {
        let mut m = self.clone();
        // `_routing` at its defaults doesn't make a mapping.
        m.routing_present = false;
        if m.routing_required == Some(false) {
            m.routing_required = None;
        }
        if m.routing_doc_values == Some(false) {
            m.routing_doc_values = None;
        }
        if m.id_mode.as_deref() == Some("document") {
            m.id_mode = None;
        }
        m == Mapping::empty()
    }

    /// `_routing` as `GET _mapping` prints it (only non-default parameters), if anything.
    pub fn routing_value(&self, include_defaults: bool) -> Option<Value> {
        let required = self.routing_required.unwrap_or(false);
        let doc_values = self.routing_doc_values.unwrap_or(false);
        if !required && !doc_values {
            return None;
        }
        let mut m = Map::new();
        if required || include_defaults {
            m.insert("required".into(), required.into());
        }
        if doc_values || include_defaults {
            m.insert("doc_values".into(), doc_values.into());
        }
        (!m.is_empty()).then_some(Value::Obj(m))
    }

    /// Parses mapping JSON (without the type wrapper; see [`unwrap_type`]).
    pub fn parse(src: &Map, ctx: &MappingCtx) -> Result<Mapping, EsError> {
        let mut m = Mapping::empty();
        let mut consumed: Vec<String> = Vec::new();
        m.root.parse_into(src, ctx, 0, &mut consumed)?;
        m.root.normalize_nested(None);
        for (k, v) in src {
            if consumed.contains(k) {
                continue;
            }
            match k.as_str() {
                "date_formats" | "dynamic_date_formats" => {
                    let formats: Vec<String> = match v {
                        Value::Arr(a) => {
                            let mut out = Vec::new();
                            for f in a {
                                let s = f.to_es_string();
                                if s.starts_with("epoch_") {
                                    return Err(EsError::mapper_parsing(format!("Epoch [{s}] is not supported as dynamic date format")));
                                }
                                crate::time::validate_format(&s).map_err(EsError::illegal_argument)?;
                                out.push(s);
                            }
                            out
                        }
                        Value::Str(s) if s == "none" => Vec::new(),
                        other => {
                            let s = match other {
                                Value::Str(s) => s.clone(),
                                o => return Err(EsError::illegal_argument(format!("Invalid format: [{}]: expected string value", o.to_es_string()))),
                            };
                            crate::time::validate_format(&s).map_err(EsError::illegal_argument)?;
                            vec![s]
                        }
                    };
                    m.dynamic_date_formats = Some(formats);
                }
                "dynamic_templates" => {
                    let Value::Arr(items) = v else {
                        return Err(EsError::mapper_parsing("Dynamic template syntax error. An array of named objects is expected."));
                    };
                    let mut templates = Vec::new();
                    for item in items {
                        let Value::Obj(t) = item else {
                            return Err(EsError::mapper_parsing("Dynamic template syntax error. An array of named objects is expected."));
                        };
                        if t.len() != 1 {
                            return Err(EsError::mapper_parsing("A dynamic template must be defined with a name"));
                        }
                        let (name, conf) = t.iter().next().unwrap();
                        let conf = conf.as_object().cloned().unwrap_or_default();
                        let tpl = DynamicTemplate::parse(name, &conf)?;
                        tpl.validate_patterns(ctx);
                        validate_dynamic_template(&tpl, ctx)?;
                        templates.push(tpl);
                    }
                    m.dynamic_templates = Some(templates);
                }
                // Elasticsearch-only root parameters.
                "runtime" | "prefix_properties" | "_id" if ctx.opensearch() => continue,
                "date_detection" => m.date_detection = Some(node_bool_named(v, "date_detection")?),
                "numeric_detection" => m.numeric_detection = Some(node_bool_named(v, "numeric_detection")?),
                "runtime" => {
                    let Value::Obj(fields) = v else {
                        return Err(EsError::parse("runtime must be a map type"));
                    };
                    for (name, def) in fields {
                        if def.is_null() {
                            m.removed_runtime.push(name.clone());
                            continue;
                        }
                        m.runtime.insert(name.clone(), RuntimeField::parse(name, def)?);
                    }
                }
                "_source" if ctx.opensearch() => {
                    let obj = meta_object("_source", v)?;
                    if let Some(other) = obj.keys().find(|k| !matches!(k.as_str(), "enabled" | "includes" | "excludes")) {
                        return Err(EsError::mapper_parsing(format!("unknown parameter [{other}] on mapper [_source] of type [null]")));
                    }
                    m.source = parse_source(v)?;
                }
                "_source" => m.source = parse_source(v)?,
                "_routing" => {
                    let obj = meta_object("_routing", v)?;
                    // An empty `_routing` object changes nothing.
                    m.routing_present = !obj.is_empty();
                    for (rk, rv) in obj {
                        match rk.as_str() {
                            "required" => m.routing_required = Some(node_bool(rv)?),
                            "doc_values" => m.routing_doc_values = Some(node_bool(rv)?),
                            other => return Err(EsError::mapper_parsing(format!("unknown parameter [{other}] on metadata field [_routing]"))),
                        }
                    }
                }
                "prefix_properties" => {
                    let Value::Obj(pp) = v else {
                        return Err(EsError::mapper_parsing("[prefix_properties] must be an object"));
                    };
                    for (path, props) in pp {
                        m.prefix_properties.insert(path.clone(), PrefixProps::parse(path, props)?);
                    }
                }
                "_meta" => match v {
                    Value::Null => {}
                    Value::Obj(meta) => m.meta = Some(meta.clone()),
                    other => {
                        return Err(EsError::java(400, "class_cast_exception", format!(
                            "class java.lang.{} cannot be cast to class java.util.Map (java.lang.{} and java.util.Map are in module java.base of loader 'bootstrap')",
                            params::java_simple_class(other),
                            params::java_simple_class(other)
                        )));
                    }
                },
                "_data_stream_timestamp" => {
                    let obj = meta_object("_data_stream_timestamp", v)?;
                    for (rk, rv) in obj {
                        match rk.as_str() {
                            "enabled" => m.data_stream_timestamp = Some(node_bool(rv)?),
                            other => return Err(EsError::mapper_parsing(format!("unknown parameter [{other}] on metadata field [_data_stream_timestamp]"))),
                        }
                    }
                }
                "_field_names" => {
                    let obj = meta_object("_field_names", v)?;
                    if obj.contains_key("enabled") {
                        return Err(EsError::mapper_parsing(
                            "The `enabled` setting for the `_field_names` field has been deprecated and removed. Please remove it from your mappings and templates.",
                        ));
                    }
                    if let Some(other) = obj.keys().next() {
                        return Err(EsError::mapper_parsing(format!("unknown parameter [{other}] on metadata field [_field_names]")));
                    }
                }
                "_index" | "_doc_count" | "_seq_no" | "_version" | "_nested_path" | "_ignored" | "_ignored_source" | "_tier" | "_feature" => {
                    meta_object(k, v)?;
                    return Err(EsError::mapper_parsing(format!("{k} is not configurable")));
                }
                "_id" => {
                    let obj = meta_object("_id", v)?;
                    for (ik, iv) in obj {
                        match ik.as_str() {
                            "mode" => match iv {
                                Value::Null => {}
                                Value::Str(s) if s == "document" || s == "columnar" => m.id_mode = Some(s.clone()),
                                other => {
                                    return Err(EsError::mapper_parsing(format!(
                                        "Unknown value [{}] for field [mode] - accepted values are [document, columnar]",
                                        other.to_es_string()
                                    )));
                                }
                            },
                            other => return Err(EsError::mapper_parsing(format!("unknown parameter [{other}] on metadata field [_id]"))),
                        }
                    }
                }
                _ => continue,
            }
            consumed.push(k.clone());
        }
        let rest: Vec<(&String, &Value)> = src.iter().filter(|(k, _)| !consumed.contains(k)).collect();
        if !rest.is_empty() {
            let mut sorted = rest;
            sorted.sort_by(|a, b| a.0.cmp(b.0));
            return Err(EsError::mapper_parsing(format!("Root mapping definition has unsupported parameters: {}", remaining_text(&sorted))));
        }
        if let Some(mode) = ctx.columnar {
            m.columnarize(mode)?;
        }
        if ctx.opensearch() {
            check_disable_objects(&m.root, "")?;
        }
        Ok(m)
    }

    /// Strict columnar modes: object hierarchies become dotted field names under the root, and
    /// the `dynamic`/`enabled` of each object move to `prefix_properties` (explicit
    /// `prefix_properties` win). Nested objects stay; inside them objects auto-flatten as under
    /// `subobjects: false`.
    fn columnarize(&mut self, mode: &str) -> Result<(), EsError> {
        if self.root.subobjects == Some(true) {
            return Err(EsError::mapper_parsing(format!("subobjects [true] is not supported in {mode} mode")));
        }
        if self.root.dynamic == Some(Dynamic::Runtime) {
            return Err(EsError::mapper_parsing("dynamic [runtime] is not supported in strict columnar mode"));
        }
        // Implied, never printed.
        self.root.subobjects = None;
        let mut captured: BTreeMap<String, PrefixProps> = BTreeMap::new();
        let mut flat: BTreeMap<String, Mapper> = BTreeMap::new();
        fn walk(
            children: BTreeMap<String, Mapper>,
            prefix: &str,
            flat: &mut BTreeMap<String, Mapper>,
            captured: &mut BTreeMap<String, PrefixProps>,
            mode: &str,
        ) -> Result<(), EsError> {
            for (k, child) in children {
                let path = if prefix.is_empty() { k } else { format!("{prefix}.{k}") };
                match child {
                    Mapper::Object(mut o) if !o.is_nested() => {
                        if o.subobjects == Some(true) {
                            return Err(EsError::mapper_parsing(format!("subobjects [true] is not supported in {mode} mode")));
                        }
                        if o.dynamic == Some(Dynamic::Runtime) {
                            return Err(EsError::mapper_parsing("dynamic [runtime] is not supported in strict columnar mode"));
                        }
                        if o.dynamic.is_some() || o.enabled.is_some() {
                            captured.insert(path.clone(), PrefixProps { dynamic: o.dynamic, enabled: o.enabled });
                        }
                        // Fields declared inside a disabled object are dropped with it.
                        if o.enabled != Some(false) {
                            let children = std::mem::take(&mut o.children);
                            crate::stack::maybe_grow(|| walk(children, &path, flat, captured, mode))?;
                        }
                    }
                    Mapper::Object(mut o) => {
                        let parent = o.dynamic.unwrap_or(Dynamic::True);
                        flatten_nested(&mut o, &path, parent)?;
                        let mut renamed = o;
                        renamed.name = path.clone();
                        flat.insert(path, Mapper::Object(renamed));
                    }
                    other => {
                        let other = match other {
                            Mapper::Field(mut f) => {
                                f.name = path.clone();
                                Mapper::Field(f)
                            }
                            Mapper::Alias(mut a) => {
                                a.name = path.clone();
                                Mapper::Alias(a)
                            }
                            o => o,
                        };
                        flat.insert(path, other);
                    }
                }
            }
            Ok(())
        }
        /// Inside a nested object the fields flatten as under `subobjects: false`, failing for
        /// objects whose `dynamic` differs from their context's.
        fn flatten_nested(o: &mut ObjectMapper, path: &str, context: Dynamic) -> Result<(), EsError> {
            let mut out = BTreeMap::new();
            fn go(children: BTreeMap<String, Mapper>, prefix: &str, base: &str, context: Dynamic, out: &mut BTreeMap<String, Mapper>) -> Result<(), EsError> {
                for (k, child) in children {
                    let name = if prefix.is_empty() { k } else { format!("{prefix}.{k}") };
                    match child {
                        Mapper::Object(mut c) if !c.is_nested() => {
                            if let Some(d) = c.dynamic
                                && d != context
                            {
                                return Err(EsError::mapper_parsing(format!(
                                    "Object mapper [{base}.{name}] was found in a context where subobjects is set to false. Auto-flattening [{base}.{name}] failed because the value of [dynamic] ({}) is not compatible with the value from its parent context ({})",
                                    d.as_str().to_ascii_uppercase(),
                                    context.as_str().to_ascii_uppercase()
                                )));
                            }
                            let children = std::mem::take(&mut c.children);
                            crate::stack::maybe_grow(|| go(children, &name, base, context, out))?;
                        }
                        other => {
                            out.insert(name, other);
                        }
                    }
                }
                Ok(())
            }
            go(std::mem::take(&mut o.children), "", path, context, &mut out)?;
            o.children = out;
            Ok(())
        }
        walk(std::mem::take(&mut self.root.children), "", &mut flat, &mut captured, mode)?;
        self.root.children = flat;
        // Explicit `prefix_properties` override what objects declared.
        for (k, v) in std::mem::take(&mut self.prefix_properties) {
            captured.insert(k, v);
        }
        self.prefix_properties = captured;
        Ok(())
    }

    /// Merges `incoming` into this mapping.
    pub fn merge(&self, incoming: &Mapping, ctx: &MappingCtx, reason: MergeReason) -> Result<Mapping, EsError> {
        let mut out = self.clone();
        // Metadata fields.
        if let Some(e) = incoming.source.enabled {
            let cur = self.source.enabled.unwrap_or(true);
            // `_source` can be disabled later but never re-enabled.
            if reason == MergeReason::MappingUpdate && cur != e && !(cur && !e) {
                return Err(EsError::illegal_argument(format!(
                    "Mapper for [_source] conflicts with existing mapper:\n\tCannot update parameter [enabled] from [{cur}] to [{e}]"
                )));
            }
            out.source.enabled = Some(e);
        }
        if incoming.source.includes.is_some() {
            out.source.includes = incoming.source.includes.clone();
        }
        if incoming.source.excludes.is_some() {
            out.source.excludes = incoming.source.excludes.clone();
        }
        if incoming.source.mode.is_some() {
            out.source.mode = incoming.source.mode.clone();
        }
        if let Some(new) = &incoming.id_mode {
            let cur = self.id_mode.as_deref().unwrap_or(if ctx.columnar.is_some() { "columnar" } else { "document" });
            if reason == MergeReason::MappingUpdate && cur != new {
                return Err(EsError::illegal_argument(format!(
                    "Mapper for [_id] conflicts with existing mapper:\n\tCannot update parameter [mode] from [{cur}] to [{new}]"
                )));
            }
            out.id_mode = Some(new.clone());
        }
        if reason == MergeReason::MappingUpdate {
            if incoming.routing_present {
                // The update's `_routing` is a whole new mapper: unset parameters are defaults.
                let mut conflicts = String::new();
                for (name, cur, new) in [
                    ("required", self.routing_required.unwrap_or(false), incoming.routing_required.unwrap_or(false)),
                    ("doc_values", self.routing_doc_values.unwrap_or(false), incoming.routing_doc_values.unwrap_or(false)),
                ] {
                    if cur != new {
                        conflicts.push_str(&format!("\n\tCannot update parameter [{name}] from [{cur}] to [{new}]"));
                    }
                }
                if !conflicts.is_empty() {
                    return Err(EsError::illegal_argument(format!("Mapper for [_routing] conflicts with existing mapper:{conflicts}")));
                }
            }
        } else {
            if let (Some(a), Some(b)) = (self.routing_required, incoming.routing_required)
                && a != b
            {
                return Err(EsError::mapper_parsing("contradicting `_routing.required` settings"));
            }
            if incoming.routing_required.is_some() {
                out.routing_required = incoming.routing_required;
            }
            if incoming.routing_doc_values.is_some() {
                out.routing_doc_values = incoming.routing_doc_values;
            }
            out.routing_present |= incoming.routing_present;
        }
        if incoming.meta.is_some() {
            out.meta = incoming.meta.clone();
        }
        for (k, v) in &incoming.prefix_properties {
            let e = out.prefix_properties.entry(k.clone()).or_default();
            if v.dynamic.is_some() {
                e.dynamic = v.dynamic;
            }
            if v.enabled.is_some() {
                e.enabled = v.enabled;
            }
        }
        if incoming.data_stream_timestamp.is_some() {
            out.data_stream_timestamp = incoming.data_stream_timestamp;
        }
        if incoming.date_detection.is_some() {
            out.date_detection = incoming.date_detection;
        }
        if incoming.numeric_detection.is_some() {
            out.numeric_detection = incoming.numeric_detection;
        }
        if incoming.dynamic_date_formats.is_some() {
            out.dynamic_date_formats = incoming.dynamic_date_formats.clone();
        }
        if let Some(t) = &incoming.dynamic_templates {
            if reason == MergeReason::IndexTemplate {
                let mut merged: Vec<DynamicTemplate> = self.dynamic_templates.clone().unwrap_or_default();
                for nt in t {
                    match merged.iter_mut().find(|x| x.name == nt.name) {
                        Some(x) => *x = nt.clone(),
                        None => merged.push(nt.clone()),
                    }
                }
                out.dynamic_templates = Some(merged);
            } else {
                out.dynamic_templates = Some(t.clone());
            }
        }
        for r in &incoming.removed_runtime {
            out.runtime.remove(r);
        }
        for (k, v) in &incoming.runtime {
            out.runtime.insert(k.clone(), v.clone());
        }
        out.root = self.root.merge(&incoming.root, "", ctx, reason)?;
        out.root.normalize_nested(None);
        out.removed_runtime.clear();
        Ok(out)
    }

    /// Cross-field validation and limits (`MappingLookup` checks).
    pub fn validate(&self, ctx: &MappingCtx) -> Result<(), EsError> {
        if self.is_empty() {
            return Ok(());
        }
        let mut fields: Vec<(String, &Mapper, Option<String>)> = Vec::new();
        collect(&self.root, "", None, &mut fields);
        // Metadata field names can't be used as fields.
        for (path, m, _) in &fields {
            if METADATA_FIELDS.contains(&path.as_str()) && !matches!(m, Mapper::Object(_)) {
                return Err(EsError::mapper_parsing(format!("Field [{path}] is defined more than once")));
            }
        }
        // Field name length.
        if ctx.field_name_length_limit < i64::MAX {
            for name in ["_id", "_routing", "_index", "_source", "_seq_no", "_version", "_ignored", "_nested_path", "_field_names", "_doc_count", "_tier"]
                .iter()
                .map(|s| s.to_string())
                .chain(fields.iter().map(|f| f.0.clone()))
            {
                if name.chars().count() as i64 > ctx.field_name_length_limit {
                    return Err(EsError::illegal_argument(format!(
                        "Field name [{name}] is longer than the limit of [{}] characters",
                        ctx.field_name_length_limit
                    )));
                }
            }
        }
        // Total fields: objects, fields, multi-fields, aliases and runtime fields.
        let total = self.total_fields() as i64;
        if total > ctx.total_fields_limit {
            return Err(EsError::illegal_argument(format!("Limit of total fields [{}] has been exceeded", ctx.total_fields_limit)));
        }
        // `ParentJoinFieldMapper.doValidate`.
        if !ctx.opensearch() {
            let joins: Vec<&str> = fields.iter().filter(|(_, m, _)| matches!(m, Mapper::Field(f) if f.ty == "join")).map(|(p, _, _)| p.as_str()).collect();
            if joins.len() > 1 {
                return Err(EsError::illegal_argument(format!("Only one [parent-join] field can be defined per index, got [{}]", joins.join(", "))));
            }
        }
        // Depth.
        for (path, m, _) in &fields {
            if matches!(m, Mapper::Object(_)) {
                let depth = path.matches('.').count() as i64 + 2;
                if depth > ctx.depth_limit {
                    return Err(EsError::illegal_argument(format!(
                        "Limit of mapping depth [{}] has been exceeded due to object field [{path}]",
                        ctx.depth_limit
                    )));
                }
            }
        }
        // Nested.
        let nested: Vec<&String> = fields.iter().filter(|(_, m, _)| matches!(m, Mapper::Object(o) if o.is_nested())).map(|(p, _, _)| p).collect();
        if nested.len() as i64 > ctx.nested_fields_limit {
            return Err(EsError::illegal_argument(format!("Limit of nested fields [{}] has been exceeded", ctx.nested_fields_limit)));
        }
        // Pass-through sources (objects and flattened fields) resolve fields at the root by
        // priority, so priorities must be distinct.
        let mut seen: Vec<(i64, &str)> = Vec::new();
        for (path, m, _) in &fields {
            let priority = match m {
                Mapper::Object(o) => match o.kind {
                    ObjectKind::PassThrough { priority, .. } => Some(priority),
                    _ => None,
                },
                Mapper::Field(f) if f.ty == "flattened" => f.raw("passthrough").and_then(|p| p.get("priority")).and_then(Value::as_i64),
                _ => None,
            };
            let Some(priority) = priority else { continue };
            if let Some((_, other)) = seen.iter().find(|(p, _)| *p == priority) {
                return Err(EsError::new(500, "mapper_exception", format!(
                    "Pass-through source [{path}] has a conflicting param [priority={priority}] with source [{other}]"
                )));
            }
            seen.push((priority, path.as_str()));
        }
        // Dimensions identify a time series; a nested document belongs to none.
        for (path, m, scope) in &fields {
            if scope.is_some()
                && let Mapper::Field(f) = m
                && f.raw("time_series_dimension").and_then(Value::as_bool) == Some(true)
            {
                return Err(EsError::illegal_argument(format!("time_series_dimension can't be configured in nested field [{path}]")));
            }
        }
        let lookup: BTreeMap<&str, (&Mapper, &Option<String>)> = fields.iter().map(|(p, m, n)| (p.as_str(), (*m, n))).collect();
        let multi_fields: HashSet<String> = fields
            .iter()
            .filter_map(|(p, m, _)| match m {
                Mapper::Field(f) => Some(f.fields.keys().map(move |k| format!("{p}.{k}"))),
                _ => None,
            })
            .flatten()
            .collect();
        // Aliases.
        for (path, m, alias_scope) in &fields {
            let Mapper::Alias(a) = m else { continue };
            match lookup.get(a.path.as_str()) {
                // Metadata fields can be aliased (the time series ones only exist in TSDB).
                None if METADATA_FIELDS.contains(&a.path.as_str()) && !matches!(a.path.as_str(), "_tsid" | "_ts_routing_hash") => {}
                None if !multi_fields.contains(&a.path) => {
                    return Err(EsError::mapper_parsing(format!(
                        "Invalid [path] value [{}] for field alias [{path}]: an alias must refer to an existing field in the mappings.",
                        a.path
                    )));
                }
                Some((Mapper::Object(_), _)) => {
                    return Err(EsError::mapper_parsing(format!(
                        "Invalid [path] value [{}] for field alias [{path}]: an alias must refer to an existing field in the mappings.",
                        a.path
                    )));
                }
                Some((Mapper::Alias(_), _)) => {
                    let what = if *path == a.path { "itself" } else { "another alias" };
                    return Err(EsError::mapper_parsing(format!("Invalid [path] value [{}] for field alias [{path}]: an alias cannot refer to {what}.", a.path)));
                }
                Some((_, target_scope)) => {
                    if *path == a.path {
                        return Err(EsError::mapper_parsing(format!(
                            "Invalid [path] value [{}] for field alias [{path}]: an alias cannot refer to itself.",
                            a.path
                        )));
                    }
                    if let Some(scope) = target_scope.as_ref()
                        && alias_scope.as_ref() != Some(scope)
                        && !alias_scope.as_ref().is_some_and(|s| s.starts_with(&format!("{scope}.")))
                    {
                        return Err(EsError::mapper_parsing(format!(
                            "Invalid [path] value [{}] for field alias [{path}]: an alias must have the same nested scope as its target. The alias is not nested, but the target's nested scope is [{scope}].",
                            a.path
                        )));
                    }
                }
                None => {}
            }
        }
        // copy_to.
        for (path, m, scope) in &fields {
            let Mapper::Field(f) = m else { continue };
            for mf in f.fields.values() {
                if !mf.copy_to.is_empty() {
                    return Err(EsError::illegal_argument(format!("[copy_to] may not be used to copy from a multi-field: [{path}.{}]", mf.name)));
                }
            }
            for target in &f.copy_to {
                if let Some((Mapper::Object(_), _)) = lookup.get(target.as_str()) {
                    return Err(EsError::illegal_argument(format!("Cannot copy to field [{target}] since it is mapped as an object")));
                }
                if multi_fields.contains(target) {
                    return Err(EsError::illegal_argument(format!("[copy_to] may not be used to copy to a multi-field: [{target}]")));
                }
                // The target's nested scope must be this field's scope or one of its parents.
                let target_scope = nested_scope_of(target, &nested);
                if let Some(ts) = &target_scope {
                    let ok = scope.as_ref().is_some_and(|s| s == ts || s.starts_with(&format!("{ts}.")));
                    if !ok {
                        return Err(EsError::illegal_argument(format!(
                            "Illegal combination of [copy_to] and [nested] mappings: [copy_to] may only copy data to the current nested document or any of its parents, however one [copy_to] directive is trying to copy data from nested object [{}] to [{ts}]",
                            scope.clone().unwrap_or_else(|| "null".into())
                        )));
                    }
                }
            }
        }
        if self.data_stream_timestamp == Some(true) {
            match lookup.get("@timestamp") {
                Some((Mapper::Field(f), _)) if f.ty == "date" || f.ty == "date_nanos" => {}
                Some((m, _)) => {
                    return Err(EsError::illegal_argument(format!(
                        "data stream timestamp field [@timestamp] is of type [{}], but [date,date_nanos] is expected",
                        m.type_name()
                    )));
                }
                None => return Err(EsError::illegal_argument("data stream timestamp field [@timestamp] does not exist")),
            }
        }
        Ok(())
    }

    /// Fields counted against `index.mapping.total_fields.limit`.
    pub fn total_fields(&self) -> usize {
        fn count(o: &ObjectMapper) -> usize {
            o.children
                .values()
                .map(|c| match c {
                    Mapper::Object(o) => 1 + crate::stack::maybe_grow(|| count(o)),
                    Mapper::Field(f) => 1 + f.fields.len(),
                    Mapper::Alias(_) => 1,
                })
                .sum()
        }
        count(&self.root) + self.runtime.len()
    }

    /// The mapping as `GET _mapping` prints it (without the type wrapper).
    pub fn to_value(&self, ctx: &MappingCtx, include_defaults: bool) -> Value {
        let mut m = Map::new();
        self.root.common_to_value(&mut m);
        if let Some(t) = self.data_stream_timestamp {
            m.insert("_data_stream_timestamp".into(), obj! { "enabled" => t });
        }
        let id_mode = self.id_mode.as_deref().unwrap_or(if ctx.columnar.is_some() { "columnar" } else { "document" });
        let default_id_mode = if ctx.columnar.is_some() { "columnar" } else { "document" };
        if id_mode != default_id_mode || include_defaults {
            m.insert("_id".into(), obj! { "mode" => id_mode });
        }
        if let Some(meta) = &self.meta {
            m.insert("_meta".into(), Value::Obj(meta.clone()));
        }
        if let Some(r) = self.routing_value(include_defaults) {
            m.insert("_routing".into(), r);
        }
        if !self.source.is_empty() {
            let mut s = Map::new();
            if let Some(e) = self.source.enabled
                && !e
            {
                s.insert("enabled".into(), false.into());
            }
            if let Some(i) = &self.source.includes {
                s.insert("includes".into(), Value::from(i.clone()));
            }
            if let Some(e) = &self.source.excludes {
                s.insert("excludes".into(), Value::from(e.clone()));
            }
            if !s.is_empty() {
                m.insert("_source".into(), Value::Obj(s));
            }
        }
        if let Some(f) = &self.dynamic_date_formats {
            m.insert("dynamic_date_formats".into(), Value::from(f.clone()));
        }
        if let Some(t) = &self.dynamic_templates {
            let items = t.iter().map(|t| obj! { t.name.clone() => t.to_value() }).collect::<Vec<_>>();
            m.insert("dynamic_templates".into(), Value::Arr(items));
        }
        if let Some(d) = self.date_detection {
            m.insert("date_detection".into(), d.into());
        }
        if let Some(n) = self.numeric_detection {
            m.insert("numeric_detection".into(), n.into());
        }
        if !self.runtime.is_empty() {
            let mut r = Map::new();
            for (k, f) in &self.runtime {
                r.insert(k.clone(), f.to_value());
            }
            m.insert("runtime".into(), Value::Obj(r));
        }
        if !self.prefix_properties.is_empty() {
            let mut pp = Map::new();
            for (k, v) in &self.prefix_properties {
                pp.insert(k.clone(), v.to_value());
            }
            m.insert("prefix_properties".into(), Value::Obj(pp));
        }
        self.root.properties_into(&mut m, ctx, include_defaults);
        Value::Obj(m)
    }

    /// Finds a mapper by full dotted path (objects and fields; multi-fields as `field.sub`).
    pub fn lookup(&self, path: &str) -> Option<MapperRef<'_>> {
        lookup_in(&self.root, path)
    }

    /// Every concrete field path with its mapper, in path order (multi-fields included).
    pub fn field_paths(&self) -> Vec<(String, MapperRef<'_>)> {
        let mut out = Vec::new();
        fn walk<'a>(o: &'a ObjectMapper, prefix: &str, out: &mut Vec<(String, MapperRef<'a>)>) {
            for (k, c) in &o.children {
                let path = if prefix.is_empty() { k.clone() } else { format!("{prefix}.{k}") };
                match c {
                    Mapper::Object(child) => {
                        out.push((path.clone(), MapperRef::Object(child)));
                        crate::stack::maybe_grow(|| walk(child, &path, out));
                    }
                    Mapper::Field(f) => {
                        out.push((path.clone(), MapperRef::Field(f)));
                        for (mk, mf) in &f.fields {
                            out.push((format!("{path}.{mk}"), MapperRef::Field(mf)));
                        }
                    }
                    Mapper::Alias(a) => out.push((path, MapperRef::Alias(a))),
                }
            }
        }
        walk(&self.root, "", &mut out);
        out
    }

    /// The dimensions a time series index routes on (`findDimensionFields`): `path_match`
    /// patterns of dynamic templates mapping dimensions, then dimension fields (in the hash
    /// order Elasticsearch's field mapper map iterates them) not already covered by a pattern.
    /// The flag is false when dynamic templates make dimensions, whose patterns may also match
    /// fields that aren't dimensions.
    pub fn dimension_fields(&self, ctx: &MappingCtx) -> (Vec<String>, bool) {
        let mut out: Vec<String> = Vec::new();
        let mut all = true;
        for t in self.dynamic_templates.iter().flatten() {
            if t.mapping.get("time_series_dimension").and_then(Value::as_bool) != Some(true) {
                continue;
            }
            all = false;
            out.extend(t.path_match.iter().cloned());
        }
        let paths = self.field_paths();
        let fields: Vec<&String> = paths.iter().filter(|(_, m)| matches!(m, MapperRef::Field(_))).map(|(p, _)| p).collect();
        // Field mappers (metadata ones included) live in a `HashMap` sized for all of them.
        let order = crate::json::java_hash_order(&fields, (fields.len() + METADATA_FIELDS.len()) * 4 / 3 + 1);
        for i in order {
            let path = fields[i];
            let Some((_, MapperRef::Field(f))) = paths.iter().find(|(p, _)| p == path) else { continue };
            if f.bool_param("time_series_dimension", ctx) && !out.iter().any(|p| crate::xcontent::filter::glob_match(p, path)) {
                out.push(path.clone());
            }
        }
        (out, all)
    }

    /// The effective `dynamic` for an object path (inherited from parents, default `true`).
    pub fn dynamic_for(&self, object_path: &str) -> Dynamic {
        let mut d = self.root.dynamic.unwrap_or(Dynamic::True);
        if object_path.is_empty() {
            return d;
        }
        // Columnar mappings keep object settings by prefix: the longest one applies.
        if !self.prefix_properties.is_empty() {
            let mut path = object_path;
            loop {
                if let Some(pd) = self.prefix_properties.get(path).and_then(|p| p.dynamic) {
                    return pd;
                }
                match path.rsplit_once('.') {
                    Some((parent, _)) => path = parent,
                    None => return d,
                }
            }
        }
        let mut cur = &self.root;
        for part in object_path.split('.') {
            match cur.children.get(part) {
                Some(Mapper::Object(o)) => {
                    if let Some(od) = o.dynamic {
                        d = od;
                    }
                    cur = o;
                }
                _ => break,
            }
        }
        d
    }
}

/// A borrowed mapper found by path.
#[derive(Clone, Copy, Debug)]
pub enum MapperRef<'a> {
    Object(&'a ObjectMapper),
    Field(&'a FieldMapper),
    Alias(&'a AliasMapper),
}

impl MapperRef<'_> {
    pub fn type_name(&self) -> &str {
        match self {
            MapperRef::Object(o) => o.type_name(),
            MapperRef::Field(f) => f.ty,
            MapperRef::Alias(_) => "alias",
        }
    }
}

pub(crate) fn lookup_in<'a>(o: &'a ObjectMapper, path: &str) -> Option<MapperRef<'a>> {
    if let Some(c) = o.children.get(path) {
        return Some(match c {
            Mapper::Object(o) => MapperRef::Object(o),
            Mapper::Field(f) => MapperRef::Field(f),
            Mapper::Alias(a) => MapperRef::Alias(a),
        });
    }
    // Try every split point (`a.b.c` may be object `a` → `b.c`, or field `a.b` with multi-field `c`).
    for (i, _) in path.match_indices('.') {
        let (head, tail) = (&path[..i], &path[i + 1..]);
        match o.children.get(head) {
            Some(Mapper::Object(child)) => {
                if let Some(r) = crate::stack::maybe_grow(|| lookup_in(child, tail)) {
                    return Some(r);
                }
            }
            Some(Mapper::Field(f)) => {
                if let Some(mf) = f.fields.get(tail).or_else(|| f.props.get(tail)) {
                    return Some(MapperRef::Field(mf));
                }
            }
            _ => {}
        }
    }
    None
}

fn nested_scope_of(path: &str, nested: &[&String]) -> Option<String> {
    nested.iter().filter(|n| path.starts_with(&format!("{n}."))).max_by_key(|n| n.len()).map(|n| n.to_string())
}

/// Collects (full path, mapper, enclosing nested scope).
fn collect<'a>(o: &'a ObjectMapper, prefix: &str, scope: Option<String>, out: &mut Vec<(String, &'a Mapper, Option<String>)>) {
    for (k, c) in &o.children {
        let path = if prefix.is_empty() { k.clone() } else { format!("{prefix}.{k}") };
        out.push((path.clone(), c, scope.clone()));
        if let Mapper::Object(child) = c {
            let child_scope = if child.is_nested() { Some(path.clone()) } else { scope.clone() };
            crate::stack::maybe_grow(|| collect(child, &path, child_scope, out));
        }
    }
}

fn meta_object<'a>(name: &str, v: &'a Value) -> Result<&'a Map, EsError> {
    match v {
        Value::Obj(m) => Ok(m),
        _ => Err(EsError::illegal_argument(format!("[{name}] config must be an object"))),
    }
}

fn parse_source(v: &Value) -> Result<SourceConfig, EsError> {
    let obj = meta_object("_source", v)?;
    let mut s = SourceConfig::default();
    for (k, v) in obj {
        match k.as_str() {
            "enabled" => s.enabled = Some(node_bool(v)?),
            "mode" => {
                let m = v.to_es_string().to_uppercase();
                if !matches!(m.as_str(), "STORED" | "SYNTHETIC" | "DISABLED") {
                    return Err(EsError::illegal_argument(format!("No enum constant org.elasticsearch.index.mapper.SourceFieldMapper.Mode.{m}")));
                }
                s.mode = Some(m.to_lowercase());
            }
            "includes" | "excludes" => {
                let Value::Arr(a) = v else {
                    return Err(EsError::java(
                        400,
                        "class_cast_exception",
                        format!(
                            "class java.lang.{} cannot be cast to class java.util.List (java.lang.{} and java.util.List are in module java.base of loader 'bootstrap')",
                            params::java_simple_class(v),
                            params::java_simple_class(v)
                        ),
                    ));
                };
                let l: Vec<String> = a.iter().map(Value::to_es_string).collect();
                if k == "includes" {
                    s.includes = Some(l);
                } else {
                    s.excludes = Some(l);
                }
            }
            other => return Err(EsError::mapper_parsing(format!("unknown parameter [{other}] on metadata field [_source]"))),
        }
    }
    Ok(s)
}

fn validate_dynamic_template(t: &DynamicTemplate, ctx: &MappingCtx) -> Result<(), EsError> {
    if contains_snippet(&Value::Obj(t.mapping.clone()), "{name}") {
        return Ok(());
    }
    let mut last: Option<EsError> = None;
    for xt in &t.xcontent_types {
        let dynamic_type = if t.runtime {
            match *xt {
                "string" => "keyword",
                other => other,
            }
        } else {
            match *xt {
                "string" => "text",
                "double" => "float",
                other => other,
            }
        };
        let ty = t.mapping_type(dynamic_type);
        let name = format!("__dynamic__{}", t.name);
        let mut conf = t.mapping_for(&name, dynamic_type);
        let res = if t.runtime {
            if !RUNTIME_TYPES.contains(&ty.as_str()) {
                Err(EsError::illegal_argument(format!("No runtime field found for type [{ty}]")))
            } else {
                conf.insert("type".into(), Value::from(ty.clone()));
                RuntimeField::parse(&name, &Value::Obj(conf)).map(|_| ())
            }
        } else if matches!(ty.as_str(), "object" | "nested") {
            parse_mapper(&name, &ty, &conf, ctx, 1).map(|_| ())
        } else {
            match types::lookup_in(ctx.dialect, &ty) {
                None => Err(EsError::illegal_argument(format!("No mapper found for type [{ty}]"))),
                Some(def) => FieldMapper::parse(&name, def, &conf, ctx, false).map(|_| ()),
            }
        };
        match res {
            Ok(()) => {
                last = None;
                break;
            }
            Err(e) => last = Some(e),
        }
    }
    if let Some(e) = last {
        let types: Vec<&str> = t.xcontent_types.to_vec();
        let msg = format!(
            "dynamic template [{}] has invalid content [{}], attempted to validate it with the following match_mapping_type: [{}]",
            t.name,
            t.to_value().to_json(),
            types.join(", ")
        );
        return Err(EsError::illegal_argument(msg).caused_by(e));
    }
    Ok(())
}

fn contains_snippet(v: &Value, snippet: &str) -> bool {
    match v {
        Value::Obj(m) => m.iter().any(|(k, v)| k.contains(snippet) || contains_snippet(v, snippet)),
        Value::Arr(a) => a.iter().any(|v| contains_snippet(v, snippet)),
        Value::Str(s) => s.contains(snippet),
        _ => false,
    }
}

/// Removes the `_doc` type wrapper of a `PUT _mapping` body (`{"_doc": {...}}`), as
/// `MappingParser` does when the single root key is the type name.
pub fn unwrap_type(src: &Map) -> Map {
    if src.len() == 1
        && let Some((k, Value::Obj(inner))) = src.iter().next()
        && k == "_doc"
    {
        return inner.clone();
    }
    src.clone()
}

/// Wraps a parse failure as `mapper_parsing_exception: Failed to parse mapping: …`.
pub fn wrap_parse_error(e: EsError) -> EsError {
    let reason = e.reason().to_string();
    EsError::mapper_parsing(format!("Failed to parse mapping: {reason}")).caused_by(e)
}

impl MappingCtx {
    /// [`wrap_parse_error`] in the ctx's dialect (OpenSearch names the type: `Failed to parse
    /// mapping [_doc]: …`).
    pub fn wrap_parse_error(&self, e: EsError) -> EsError {
        if self.opensearch() {
            let reason = e.reason().to_string();
            return EsError::mapper_parsing(format!("Failed to parse mapping [_doc]: {reason}")).caused_by(e);
        }
        wrap_parse_error(e)
    }
}

/// Parses, wraps parse errors like Elasticsearch, and validates.
pub fn parse_and_validate(src: &Map, ctx: &MappingCtx) -> Result<Mapping, EsError> {
    let m = Mapping::parse(src, ctx).map_err(|e| ctx.wrap_parse_error(e))?;
    m.validate(ctx)?;
    Ok(m)
}

/// The order a `java.util.HashMap` filled with `keys` (in this order) iterates them in: by bucket
/// (`hash ^ hash >>> 16` masked by the table size, which doubles past 3/4 full from 16), then by
/// insertion.
pub fn java_hash_map_order<'a>(keys: impl Iterator<Item = &'a str>) -> Vec<usize> {
    let hashes: Vec<u32> = keys
        .map(|k| {
            let h = k.encode_utf16().fold(0u32, |h, c| h.wrapping_mul(31).wrapping_add(c as u32));
            h ^ (h >> 16)
        })
        .collect();
    let mut cap = 16usize;
    while hashes.len() * 4 > cap * 3 {
        cap *= 2;
    }
    let mut order: Vec<usize> = (0..hashes.len()).collect();
    order.sort_by_key(|&i| hashes[i] as usize & (cap - 1));
    order
}

/// The texts a date field's `locale` parses and prints with: `Locale.ROOT` for an empty or
/// `root` locale, English otherwise (the default, `en`, and every locale whose texts roll does
/// not carry).
pub fn date_locale(locale: Option<&Value>) -> crate::time::DateLocale {
    match locale.and_then(Value::as_str) {
        Some(s) if s.is_empty() || s.eq_ignore_ascii_case("root") => crate::time::DateLocale::Root,
        _ => crate::time::DateLocale::English,
    }
}

#[cfg(test)]
mod tests;

/// The mapping of a create-index request (`"mappings": {...}`).
pub fn for_create(src: &Map, ctx: &MappingCtx) -> Result<Mapping, EsError> {
    if src.len() == 1
        && let Some((k, Value::Obj(_))) = src.iter().next()
        && k == "_doc"
    {
        return Err(EsError::illegal_argument("The mapping definition cannot be nested under a type [_doc]."));
    }
    parse_and_validate(src, ctx)
}

/// Applies a `PUT _mapping` body to an index's mapping.
pub fn for_update(existing: &Mapping, src: &Map, ctx: &MappingCtx) -> Result<Mapping, EsError> {
    let src = unwrap_type(src);
    let incoming = Mapping::parse(&src, ctx).map_err(|e| ctx.wrap_parse_error(e))?;
    let merged = existing.merge(&incoming, ctx, MergeReason::MappingUpdate)?;
    merged.validate(ctx)?;
    Ok(merged)
}

