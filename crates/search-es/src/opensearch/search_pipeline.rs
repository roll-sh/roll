//! OpenSearch search pipelines: `PUT/GET/DELETE /_search/pipeline/{id}`, and running them on
//! searches (`?search_pipeline=`, an inline `search_pipeline` object in the body, or the
//! index's `index.search.default_pipeline`).
//!
//! A pipeline is stored as the JSON it was put with (the get API returns it unchanged) and
//! compiled when it is put — so a bad definition fails there, with the texts of OpenSearch's
//! `ConfigurationUtils` and processor factories — and again for each search that uses it.
//! Compiling is a handful of map reads and at most one query parse, small next to a search.
//!
//! Request processors rewrite the parsed request: `filter_query` wraps the query in a `bool`
//! filter, `oversample` raises `size` (remembering the original for `truncate_hits`), `script`
//! runs Painless over the request's scalar properties. Response processors rewrite the
//! rendered response: `rename_field`, `truncate_hits`, `collapse`, `sort` and `split` work on
//! each hit's `_source` and `fields` the way their Java counterparts work on `SearchHit`s. The
//! routing processors (`acl_routing_search`, `hierarchical_routing_search`,
//! `temporal_routing_search`) are validated like OpenSearch validates them and then search
//! every shard: they only narrow which shards a search visits, and the query itself still
//! decides which documents match.
//!
//! As in OpenSearch, a processor that fails stops the search unless it set `ignore_failure`.

use super::java_hashmap_order;
use crate::error::{EsError, EsResult};
use crate::json::{Map, Number, Value};
use crate::obj;
use crate::query::Query;
use crate::rest::{Ctx, Reply};
use crate::xcontent::filter::glob_match;
use std::collections::HashMap;

/// `SearchPipelineService.NOOP_PIPELINE_ID`.
pub const NOOP: &str = "_none";

// ---- ConfigurationUtils ----------------------------------------------------------------

/// The Java class `XContentHelper.convertToMap` gives a JSON value.
fn java_class(v: &Value) -> &'static str {
    match v {
        Value::Obj(_) => "java.util.HashMap",
        Value::Arr(_) => "java.util.ArrayList",
        Value::Str(_) => "java.lang.String",
        Value::Bool(_) => "java.lang.Boolean",
        Value::Num(Number::Int(i)) if i32::try_from(*i).is_ok() => "java.lang.Integer",
        Value::Num(Number::Int(_)) => "java.lang.Long",
        Value::Num(Number::UInt(_) | Number::Big(_)) => "java.math.BigInteger",
        Value::Num(Number::Float(_)) => "java.lang.Double",
        Value::Null => "null",
    }
}

/// `ConfigurationUtils.newConfigurationException`: an `OpenSearchParseException` whose
/// metadata (`processor_type`, `processor_tag`, `property_name`, those that are set) prints in
/// the order of the exception's metadata `HashMap`.
fn config_error(ptype: Option<&str>, tag: Option<&str>, prop: Option<&str>, reason: &str) -> EsError {
    let reason = match prop {
        Some(p) => format!("[{p}] {reason}"),
        None => reason.to_string(),
    };
    let meta: Vec<(&str, &str)> = [("opensearch.processor_type", ptype), ("opensearch.processor_tag", tag), ("opensearch.property_name", prop)]
        .into_iter()
        .filter_map(|(k, v)| v.map(|v| (k, v)))
        .collect();
    let keys: Vec<&str> = meta.iter().map(|(k, _)| *k).collect();
    let mut e = EsError::new(400, "parse_exception", reason);
    for k in java_hashmap_order(&keys, keys.len()) {
        let v = meta.iter().find(|(m, _)| *m == k).map(|(_, v)| *v).unwrap_or_default();
        e = e.with_meta(k.trim_start_matches("opensearch.").to_string(), v.to_string());
    }
    e
}

/// The configuration of one pipeline or processor, consumed as it is read (OpenSearch removes
/// every property it reads, then rejects what is left).
struct Config<'a> {
    ptype: Option<&'a str>,
    tag: Option<String>,
    map: Map,
    /// Entries the map was parsed with (its `HashMap` capacity, for the leftovers' order).
    entries: usize,
}

impl<'a> Config<'a> {
    fn new(ptype: Option<&'a str>, map: Map) -> Config<'a> {
        let entries = map.len();
        Config { ptype, tag: None, map, entries }
    }

    fn err(&self, prop: &str, reason: &str) -> EsError {
        config_error(self.ptype, self.tag.as_deref(), Some(prop), reason)
    }

    fn take(&mut self, key: &str) -> Option<Value> {
        self.map.shift_remove(key).filter(|v| !v.is_null())
    }

    fn optional_string(&mut self, key: &str) -> EsResult<Option<String>> {
        match self.take(key) {
            None => Ok(None),
            Some(Value::Str(s)) => Ok(Some(s)),
            Some(v) => Err(self.err(key, &format!("property isn't a string, but of type [{}]", java_class(&v)))),
        }
    }

    fn string(&mut self, key: &str) -> EsResult<String> {
        self.optional_string(key)?.ok_or_else(|| self.err(key, "required property is missing"))
    }

    fn string_or(&mut self, key: &str, default: &str) -> EsResult<String> {
        Ok(self.optional_string(key)?.unwrap_or_else(|| default.to_string()))
    }

    fn boolean(&mut self, key: &str, default: bool) -> EsResult<bool> {
        match self.take(key) {
            None => Ok(default),
            Some(Value::Bool(b)) => Ok(b),
            Some(v) => Err(self.err(key, &format!("property isn't a boolean, but of type [{}]", java_class(&v)))),
        }
    }

    /// `readIntProperty`: `Integer.parseInt(value.toString())`.
    fn int(&mut self, key: &str) -> EsResult<Option<i32>> {
        match self.take(key) {
            None => Ok(None),
            Some(v) => {
                let s = v.to_es_string();
                crate::units::parse_java_int(&s).map(Some).ok_or_else(|| self.err(key, &format!("property cannot be converted to an int [{s}]")))
            }
        }
    }

    /// `readDoubleProperty` (required): `Double.parseDouble(value.toString())`.
    fn double(&mut self, key: &str) -> EsResult<f64> {
        let Some(v) = self.take(key) else { return Err(self.err(key, "required property is missing")) };
        let s = v.to_es_string();
        crate::units::parse_java_double(s.trim()).ok_or_else(|| self.err(key, &format!("property cannot be converted to a double [{s}]")))
    }

    fn optional_map(&mut self, key: &str) -> EsResult<Option<Map>> {
        match self.take(key) {
            None => Ok(None),
            Some(Value::Obj(m)) => Ok(Some(m)),
            Some(v) => Err(self.err(key, &format!("property isn't a map, but of type [{}]", java_class(&v)))),
        }
    }

    fn optional_list(&mut self, key: &str) -> EsResult<Option<Vec<Value>>> {
        match self.take(key) {
            None => Ok(None),
            Some(Value::Arr(a)) => Ok(Some(a)),
            Some(v) => Err(self.err(key, &format!("property isn't a list, but of type [{}]", java_class(&v)))),
        }
    }

    /// The properties nobody read, as `Arrays.toString(config.keySet().toArray())`.
    fn leftovers(&self) -> Option<String> {
        if self.map.is_empty() {
            return None;
        }
        let keys: Vec<&str> = self.map.keys().map(String::as_str).collect();
        Some(format!("[{}]", java_hashmap_order(&keys, self.entries).join(", ")))
    }
}

// ---- processors ------------------------------------------------------------------------

#[derive(Clone)]
enum RequestKind {
    FilterQuery(Query),
    Oversample { factor: f64, prefix: Option<String> },
    Script { bound: crate::scripting::Bound },
    /// The routing processors: validated, then a no-op (see the module documentation).
    Routing,
}

#[derive(Clone)]
enum ResponseKind {
    Rename { field: String, target: String, ignore_missing: bool },
    Truncate { size: i32, prefix: Option<String> },
    Collapse { field: String },
    Sort { field: String, target: String, desc: bool },
    Split { field: String, separator: String, preserve_trailing: bool, target: String },
    /// The neural-search plugin's `hybrid_score_explanation`: applied by the hybrid search
    /// itself (it reads the fusion's explain pass), a no-op here.
    HybridExplanation,
}

#[derive(Clone)]
struct Proc<K> {
    kind: K,
    ignore_failure: bool,
    /// The processor type and tag `verbose_pipeline` reports.
    name: String,
    tag: Option<String>,
}

/// A compiled pipeline.
#[derive(Clone)]
pub struct Pipeline {
    pub id: String,
    request: Vec<Proc<RequestKind>>,
    response: Vec<Proc<ResponseKind>>,
    /// The neural-search plugin's phase-results processor (`normalization-processor`,
    /// `score-ranker-processor`): how a `hybrid` query's sub-query results are fused.
    pub phase: Option<std::sync::Arc<super::hybrid::Phase>>,
    /// An inline definition (`search_pipeline` in the body), which the request still prints.
    inline: Option<Value>,
}

const REQUEST_TYPES: &[&str] = &["filter_query", "script", "oversample", "hierarchical_routing_search", "temporal_routing_search", "acl_routing_search"];
const RESPONSE_TYPES: &[&str] = &["rename_field", "truncate_hits", "collapse", "split", "sort", "hybrid_score_explanation"];
const PHASE_TYPES: &[&str] = &["normalization-processor", "score-ranker-processor"];

/// `NormalizationProcessorFactory` / `RRFProcessorFactory`. The technique clauses are read
/// (and their properties removed) like any configuration; what else they hold is not checked.
fn phase_processor(ptype: &str, c: &mut Config) -> EsResult<super::hybrid::Phase> {
    use super::hybrid;
    fn nested<'a>(c: &Config<'a>, m: Map) -> Config<'a> {
        let mut n = Config::new(c.ptype, m);
        n.tag = c.tag.clone();
        n
    }
    if ptype == "score-ranker-processor" {
        let mut norm = hybrid::Normalization::Rrf { rank_constant: 60 };
        let mut combination = (hybrid::CombinationKind::Rrf, Vec::new());
        if let Some(clause) = c.optional_map("combination")? {
            let rank_constant = clause.get("rank_constant").cloned();
            let mut cc = nested(c, clause);
            let technique = cc.string_or("technique", "rrf")?;
            if technique != "rrf" {
                return Err(EsError::illegal_argument(format!(
                    "provided combination technique is not supported by [{ptype}], supported technique is [rrf], got [{technique}]"
                )));
            }
            if let Some(rc) = rank_constant {
                norm = hybrid::Normalization::Rrf { rank_constant: hybrid::rank_constant(Some(&rc))? };
            }
            let params = cc.optional_map("parameters")?;
            combination = hybrid::combination(&technique, params.as_ref())?;
        }
        return Ok(hybrid::Phase { normalization: norm, combination: combination.0, weights: combination.1, explain_response: false });
    }
    let mut norm = hybrid::Normalization::MinMax { lower: None, upper: None };
    if let Some(clause) = c.optional_map("normalization")? {
        let mut nc = nested(c, clause);
        let technique = nc.string_or("technique", "min_max")?;
        let params = nc.optional_map("parameters")?;
        norm = hybrid::normalization(&technique, params.as_ref())?;
    }
    let mut combination = (hybrid::CombinationKind::ArithmeticMean, Vec::new());
    if let Some(clause) = c.optional_map("combination")? {
        let mut cc = nested(c, clause);
        let technique = cc.string_or("technique", "arithmetic_mean")?;
        let params = cc.optional_map("parameters")?;
        combination = hybrid::combination(&technique, params.as_ref())?;
    }
    hybrid::check_compatible(&norm, combination.0)?;
    Ok(hybrid::Phase { normalization: norm, combination: combination.0, weights: combination.1, explain_response: false })
}

fn context_key(prefix: &Option<String>, key: &str) -> String {
    match prefix {
        Some(p) => format!("{p}.{key}"),
        None => key.to_string(),
    }
}

fn request_processor(ctx: &Ctx, ptype: &str, c: &mut Config) -> EsResult<RequestKind> {
    Ok(match ptype {
        "filter_query" => {
            let Some(q) = c.optional_map("query")? else {
                return Err(EsError::illegal_argument("Did not specify the query property in processor of type filter_query"));
            };
            RequestKind::FilterQuery(crate::query::parse_query(&Value::Obj(q), None, &[])?.0)
        }
        "oversample" => {
            let factor = c.double("sample_factor")?;
            if factor < 1.0 {
                return Err(c.err("sample_factor", "Value must be >= 1.0"));
            }
            RequestKind::Oversample { factor, prefix: c.optional_string("context_prefix")? }
        }
        "script" => {
            let mut script = Map::new();
            for key in ["id", "source", "inline", "lang", "params", "options"] {
                if let Some(v) = c.take(key) {
                    script.insert(key.into(), v);
                }
            }
            let v = Value::Obj(script);
            let mut p = crate::query::xparser::XParser::new(&v, None, Vec::new());
            let parsed = crate::query::script::Script::parse(&mut p)?;
            // A script that doesn't compile fails the processor's creation
            // (`newConfigurationException(TYPE, tag, null, e)`), its cause the root cause.
            let bound = ctx
                .cluster
                .scripts
                .compile(&ctx.state(), &parsed, search_painless::Context::Ingest)
                .map_err(|e| config_error(Some("script"), c.tag.as_deref(), None, e.reason()).caused_by(e.clone()).with_root_causes(vec![e]))?;
            RequestKind::Script { bound }
        }
        "acl_routing_search" => {
            c.string("acl_field")?;
            c.boolean("extract_from_query", true)?;
            RequestKind::Routing
        }
        "hierarchical_routing_search" => {
            let path = c.string("path_field")?;
            let depth = c.int("anchor_depth")?.unwrap_or(2);
            let sep = c.optional_string("path_separator")?.unwrap_or_else(|| "/".into());
            c.boolean("enable_auto_detection", true)?;
            if depth <= 0 {
                return Err(c.err("anchor_depth", "must be greater than 0"));
            }
            if sep.is_empty() {
                return Err(c.err("path_separator", "cannot be null or empty"));
            }
            if path.is_empty() {
                return Err(c.err("path_field", "cannot be null or empty"));
            }
            RequestKind::Routing
        }
        "temporal_routing_search" => {
            let field = c.string("timestamp_field")?;
            let granularity = c.string("granularity")?;
            let format = c.optional_string("format")?.unwrap_or_else(|| "strict_date_optional_time".into());
            c.boolean("enable_auto_detection", true)?;
            c.boolean("hash_bucket", false)?;
            if field.is_empty() {
                return Err(c.err("timestamp_field", "cannot be null or empty"));
            }
            if granularity.is_empty() {
                return Err(c.err("granularity", "cannot be null or empty"));
            }
            if !["hour", "day", "week", "month"].contains(&granularity.to_ascii_lowercase().as_str()) {
                return Err(c.err("granularity", &format!("Invalid granularity: {granularity}. Supported values are: hour, day, week, month")));
            }
            if let Err(e) = crate::time::DateFormatter::new(&format) {
                return Err(c.err("format", &format!("invalid date format: {e}")));
            }
            RequestKind::Routing
        }
        other => return Err(EsError::illegal_argument(format!("Invalid processor type {other}"))),
    })
}

fn response_processor(ptype: &str, c: &mut Config) -> EsResult<ResponseKind> {
    Ok(match ptype {
        "hybrid_score_explanation" => ResponseKind::HybridExplanation,
        "rename_field" => {
            let field = c.string("field")?;
            let target = c.string("target_field")?;
            ResponseKind::Rename { field, target, ignore_missing: c.boolean("ignore_missing", false)? }
        }
        "truncate_hits" => {
            let size = match c.int("target_size")? {
                Some(n) if n < 0 => return Err(c.err("target_size", "Value must be >= 0")),
                Some(n) => n,
                None => -1,
            };
            ResponseKind::Truncate { size, prefix: c.optional_string("context_prefix")? }
        }
        "collapse" => ResponseKind::Collapse { field: c.string("field")? },
        "sort" => {
            let field = c.string("field")?;
            let target = c.string_or("target_field", &field)?;
            let order = c.string_or("order", "asc")?;
            let desc = match order.as_str() {
                "asc" => false,
                "desc" => true,
                other => return Err(c.err("order", &format!("Sort direction [{other}] not recognized. Valid values are: [asc, desc]"))),
            };
            ResponseKind::Sort { field, target, desc }
        }
        "split" => {
            let field = c.string("field")?;
            let separator = c.string("separator")?;
            let preserve_trailing = c.boolean("preserve_trailing", false)?;
            let target = c.string_or("target_field", &field)?;
            ResponseKind::Split { field, separator, preserve_trailing, target }
        }
        other => return Err(EsError::illegal_argument(format!("Invalid processor type {other}"))),
    })
}

fn class_cast(v: &Value) -> EsError {
    EsError::new(500, "class_cast_exception", format!("class {} cannot be cast to class java.util.Map", java_class(v)))
}

/// `PipelineWithMetrics.readProcessors`.
fn read_processors<K>(list: Option<Vec<Value>>, known: &[&str], mut build: impl FnMut(&str, &mut Config) -> EsResult<K>) -> EsResult<Vec<Proc<K>>> {
    let mut out = Vec::new();
    for item in list.unwrap_or_default() {
        let Value::Obj(entry) = &item else { return Err(class_cast(&item)) };
        let keys: Vec<&str> = entry.keys().map(String::as_str).collect();
        for ptype in java_hashmap_order(&keys, keys.len()) {
            if !known.contains(&ptype) {
                return Err(EsError::illegal_argument(format!("Invalid processor type {ptype}")));
            }
            let cfg = match entry.get(ptype) {
                Some(Value::Obj(m)) => m.clone(),
                Some(other) => return Err(class_cast(other)),
                None => Map::new(),
            };
            let mut c = Config::new(Some(ptype), cfg);
            // `tag`, `ignore_failure` and `description` are read without the processor type.
            c.tag = match c.take("tag") {
                None => None,
                Some(Value::Str(s)) => Some(s),
                Some(v) => return Err(config_error(None, None, Some("tag"), &format!("property isn't a string, but of type [{}]", java_class(&v)))),
            };
            let ignore_failure = match c.take("ignore_failure") {
                None => false,
                Some(Value::Bool(b)) => b,
                Some(v) => {
                    return Err(config_error(None, None, Some("ignore_failure"), &format!("property isn't a boolean, but of type [{}]", java_class(&v))));
                }
            };
            match c.take("description") {
                None | Some(Value::Str(_)) => {}
                Some(v) => {
                    return Err(config_error(None, c.tag.as_deref(), Some("description"), &format!("property isn't a string, but of type [{}]", java_class(&v))));
                }
            }
            let kind = build(ptype, &mut c)?;
            if let Some(left) = c.leftovers() {
                let name = match &c.tag {
                    Some(t) => format!("{ptype}:{t}"),
                    None => ptype.to_string(),
                };
                return Err(EsError::new(400, "parse_exception", format!("processor [{name}] doesn't support one or more provided configuration parameters: {left}")));
            }
            out.push(Proc { kind, ignore_failure, name: ptype.to_string(), tag: c.tag.clone() });
        }
    }
    Ok(out)
}

/// `PipelineWithMetrics.create`.
pub fn compile(ctx: &Ctx, id: &str, def: &Map) -> EsResult<Pipeline> {
    let mut c = Config::new(None, def.clone());
    c.optional_string("description")?;
    c.int("version")?;
    let request = c.optional_list("request_processors")?;
    let request = read_processors(request, REQUEST_TYPES, |t, pc| request_processor(ctx, t, pc))?;
    let response = c.optional_list("response_processors")?;
    let response = read_processors(response, RESPONSE_TYPES, response_processor)?;
    // The bundled neural-search plugin's processors.
    let phase = c.optional_list("phase_results_processors")?;
    let phase = read_processors(phase, PHASE_TYPES, phase_processor)?;
    if let Some(left) = c.leftovers() {
        return Err(EsError::new(400, "parse_exception", format!("pipeline [{id}] doesn't support one or more provided configuration parameters {left}")));
    }
    Ok(Pipeline { id: id.to_string(), request, response, phase: phase.into_iter().next().map(|p| std::sync::Arc::new(p.kind)), inline: None })
}

// ---- the CRUD APIs ---------------------------------------------------------------------

pub async fn put(ctx: &Ctx<'_>) -> EsResult<Reply> {
    let id = ctx.path_param("id").unwrap_or_default().to_string();
    let body = ctx.body_map()?.ok_or_else(|| EsError::action_request_validation(&["request body or source parameter is required".into()]))?;
    compile(ctx, &id, &body)?;
    ctx.cluster
        .update(move |st| {
            st.metadata.search_pipelines.insert(id, Value::Obj(body));
            Ok(())
        })
        .await?;
    Reply::acknowledged()
}

/// `Regex.simpleMatch` over the requested ids (none: every pipeline). `_all` is a plain id.
fn selected(patterns: &[String], id: &str) -> bool {
    patterns.is_empty() || patterns.iter().any(|p| glob_match(p, id))
}

pub fn get(ctx: &Ctx) -> EsResult<Reply> {
    let patterns = ctx.list_param("id").unwrap_or_default();
    let st = ctx.state();
    let mut out = Map::new();
    for (id, p) in &st.metadata.search_pipelines {
        if selected(&patterns, id) {
            out.insert(id.clone(), p.clone());
        }
    }
    if out.is_empty() && !patterns.is_empty() {
        return Ok(Reply::Body(404, obj! {}));
    }
    Reply::ok(Value::Obj(out))
}

pub async fn delete(ctx: &Ctx<'_>) -> EsResult<Reply> {
    // `DeleteSearchPipelineRequest` takes the id whole (no comma lists).
    let raw = ctx.path_param("id").unwrap_or_default().to_string();
    let patterns = vec![raw.clone()];
    ctx.cluster
        .update(move |st| {
            let matched: Vec<String> = st.metadata.search_pipelines.keys().filter(|id| selected(&patterns, id)).cloned().collect();
            if matched.is_empty() {
                return Err(EsError::resource_not_found(format!("pipeline [{raw}] is missing")));
            }
            for m in matched {
                st.metadata.search_pipelines.remove(&m);
            }
            Ok(())
        })
        .await?;
    Reply::acknowledged()
}

// ---- running pipelines -----------------------------------------------------------------

/// State the processors of one search share (`PipelineProcessingContext`).
#[derive(Default, Debug)]
pub struct RunContext {
    attributes: HashMap<String, Value>,
    /// `verbose_pipeline`: record each processor's run (`processor_results`).
    pub verbose: bool,
    results: Vec<Value>,
}

/// `SearchPipelineService.resolvePipeline`: the pipeline a search runs, if any. Takes the
/// inline `search_pipeline` object out of the body (it is not part of the search source).
pub fn resolve(ctx: &Ctx, body: &mut Option<Value>, indices: &[String]) -> EsResult<Option<Pipeline>> {
    let inline = match body {
        Some(Value::Obj(m)) => m.shift_remove("search_pipeline"),
        _ => None,
    };
    let named = ctx.query("search_pipeline").map(str::to_string);
    if let Some(def) = inline {
        if named.is_some() {
            return Err(EsError::illegal_argument("Both named and inline search pipeline were specified. Please only specify one or the other."));
        }
        let Value::Obj(def) = def else {
            return Err(EsError::x_content_parse_bare(format!("[search_pipeline] expected an object, got {}", def.kind_name())));
        };
        let mut p = compile(ctx, "_ad_hoc_pipeline", &def)?;
        p.inline = Some(Value::Obj(def));
        return Ok(Some(p));
    }
    let st = ctx.state();
    let id = match named {
        Some(n) => n,
        None if !indices.is_empty() => {
            // The indices' common default pipeline; indices that disagree run none.
            let mut id = NOOP.to_string();
            let opts = crate::meta::resolve::IndicesOptions::STRICT_EXPAND_OPEN_FORBID_CLOSED;
            if let Ok(names) = crate::meta::resolve::resolve(&st, indices, &opts, ctx.now_ms) {
                for n in names {
                    let Some(idx) = st.metadata.indices.get(&n) else { continue };
                    let Some(p) = idx.settings.get("index.search.default_pipeline") else { continue };
                    if id == NOOP {
                        id = p.to_string();
                    } else if id != p {
                        id = NOOP.to_string();
                        break;
                    }
                }
            }
            id
        }
        None => NOOP.to_string(),
    };
    if id == NOOP {
        return Ok(None);
    }
    let Some(Value::Obj(def)) = st.metadata.search_pipelines.get(&id) else {
        return Err(EsError::illegal_argument(format!("Pipeline {id} is not defined")));
    };
    compile(ctx, &id, def).map(Some)
}

impl Pipeline {
    /// The phase-results processor a hybrid search runs with (knowing whether the pipeline
    /// also explains hybrid scores).
    pub fn hybrid_phase(&self) -> Option<std::sync::Arc<super::hybrid::Phase>> {
        let phase = self.phase.as_ref()?;
        let explain = self.response.iter().any(|p| matches!(p.kind, ResponseKind::HybridExplanation));
        if !explain {
            return Some(phase.clone());
        }
        let mut p = (**phase).clone();
        p.explain_response = true;
        Some(std::sync::Arc::new(p))
    }

    /// `Pipeline.transformRequest`. With `verbose_pipeline` each processor's run is recorded
    /// in `rc` (the request as `SearchSourceBuilder` prints it before and after, converted to
    /// a `HashMap` like the plugin's `ProcessorExecutionDetail` does).
    pub fn transform_request(&self, req: &mut crate::search::SearchRequest, rc: &mut RunContext) -> EsResult<()> {
        let source = |req: &crate::search::SearchRequest| {
            let mut v = crate::tasks::source_value(&req.source, crate::cluster::Dialect::OpenSearch);
            if let Value::Obj(m) = &mut v {
                m.shift_insert(0, "verbose_pipeline".into(), Value::Bool(true));
                if let Some(def) = &self.inline {
                    m.insert("search_pipeline".into(), def.clone());
                }
            }
            super::hash_ordered(v)
        };
        for p in &self.request {
            let started = rc.verbose.then(|| (std::time::Instant::now(), source(req)));
            let r = run_request(&p.kind, req, rc);
            if let Some((t, input)) = started {
                rc.results.push(detail(p, t, input, source(req), r.as_ref().err()));
            }
            match r {
                Err(e) if !p.ignore_failure => return Err(e),
                _ => {}
            }
        }
        Ok(())
    }

    /// `Pipeline.transformResponse`, over the rendered response; with `verbose_pipeline` each
    /// run records the hits before and after, and the response gets `processor_results`.
    pub fn transform_response(&self, resp: &mut Value, rc: &mut RunContext) -> EsResult<()> {
        let hits = |resp: &mut Value| hits_mut(resp).map_or(Value::Arr(Vec::new()), |h| Value::Arr(h.clone()));
        for p in &self.response {
            let started = rc.verbose.then(|| (std::time::Instant::now(), hits(resp)));
            let r = run_response(&p.kind, resp, rc);
            if let Some((t, input)) = started {
                let output = hits(resp);
                rc.results.push(detail(p, t, input, output, r.as_ref().err()));
            }
            match r {
                Err(e) if !p.ignore_failure => return Err(e),
                _ => {}
            }
        }
        if rc.verbose
            && let Value::Obj(m) = resp
        {
            m.insert("processor_results".into(), Value::Arr(std::mem::take(&mut rc.results)));
        }
        Ok(())
    }
}

/// `ProcessorExecutionDetail.toXContent`.
fn detail<K>(p: &Proc<K>, started: std::time::Instant, input: Value, output: Value, error: Option<&EsError>) -> Value {
    let mut m = Map::new();
    m.insert("processor_name".into(), Value::Str(p.name.clone()));
    if let Some(t) = &p.tag {
        m.insert("tag".into(), Value::Str(t.clone()));
    }
    m.insert("duration_millis".into(), Value::from(started.elapsed().as_millis() as i64));
    m.insert("status".into(), Value::from(if error.is_some() { "fail" } else { "success" }));
    m.insert("input_data".into(), input);
    match error {
        Some(e) => {
            m.insert("error".into(), Value::Str(e.reason().to_string()));
        }
        None => {
            m.insert("output_data".into(), output);
        }
    }
    Value::Obj(m)
}

fn run_request(k: &RequestKind, req: &mut crate::search::SearchRequest, rc: &mut RunContext) -> EsResult<()> {
    let src = &mut req.source;
    match k {
        RequestKind::FilterQuery(f) => {
            let original = src.query.take();
            let b = crate::query::compound::Bool { filter: vec![f.clone()], must: original.into_iter().collect(), ..Default::default() };
            src.query = Some(Query::Bool(Box::new(b)));
        }
        RequestKind::Oversample { factor, prefix } => {
            let original = src.size.unwrap_or(10);
            rc.attributes.insert(context_key(prefix, "original_size"), Value::from(i64::from(original)));
            src.size = Some((f64::from(original) * factor).ceil() as i32);
        }
        RequestKind::Script { bound } => run_script(bound, src)?,
        RequestKind::Routing => {}
    }
    Ok(())
}

fn map_error(msg: String) -> EsError {
    EsError::new(500, "search_request_map_processing_exception", msg)
}

/// The `script` request processor: `ctx._source` is the request's scalar properties
/// (`SearchRequestMap`); what the script changes there is written back.
fn run_script(bound: &crate::scripting::Bound, src: &mut crate::search_source::SearchSource) -> EsResult<()> {
    use crate::search_source::TrackTotalHits;
    let opt_bool = |b: Option<bool>| b.map(Value::Bool).unwrap_or(Value::Null);
    let tth = match src.track_total_hits {
        Some(TrackTotalHits::UpTo(n)) => Value::from(i64::from(n)),
        Some(TrackTotalHits::Accurate) => Value::from(i64::from(i32::MAX)),
        Some(TrackTotalHits::Disabled) => Value::from(-1i64),
        None => Value::Null,
    };
    let map = obj! {
        "from" => i64::from(src.from.unwrap_or(-1)),
        "size" => i64::from(src.size.unwrap_or(-1)),
        "explain" => opt_bool(src.explain),
        "version" => opt_bool(src.version),
        "seq_no_primary_term" => opt_bool(src.seq_no_primary_term),
        "track_scores" => src.track_scores,
        "track_total_hits" => tth,
        "min_score" => src.min_score.map(|f| Value::from(f64::from(f))).unwrap_or(Value::Null),
        "terminate_after" => i64::from(src.terminate_after.unwrap_or(0)),
        "profile" => src.profile,
    };
    let Some(sc) = bound.painless() else { return Err(EsError::illegal_argument("only painless scripts run in search pipelines")) };
    let source = search_painless::xcontent::parse_source_unordered(&map.to_json()).map_err(|e| EsError::parse(format!("{e:?}")))?;
    let ctx = search_painless::es::write::ctx_value(search_painless::es::write::CtxKind::Ingest, source, &[], None).map_err(EsError::illegal_argument)?;
    let mut inst = sc.instance();
    inst.set_params(&bound.params);
    inst.set_var("ctx", ctx.clone());
    inst.execute(&mut search_painless::NoHost).map_err(crate::scripting::script_error)?;
    let c = search_painless::es::write::ctx_of(&ctx).ok_or_else(|| EsError::illegal_state("script lost its ctx"))?;
    let json = search_painless::xcontent::to_xcontent(&c.source()).map_err(|e| EsError::illegal_argument(e.reason.clone()))?;
    let Ok(Some(Value::Obj(out))) = crate::json::parse(json.as_bytes()) else { return Ok(()) };
    for (key, v) in &out {
        if map.get(key) == Some(v) {
            continue;
        }
        let set_err = || map_error(format!("Error while setting value for SearchRequest source property: {key}"));
        let int = || v.as_i64().and_then(|i| i32::try_from(i).ok()).ok_or_else(set_err);
        let boolean = || v.as_bool().ok_or_else(set_err);
        match key.as_str() {
            "from" => src.from = Some(int()?),
            "size" => src.size = Some(int()?),
            "explain" => src.explain = Some(boolean()?),
            "version" => src.version = Some(boolean()?),
            "seq_no_primary_term" => src.seq_no_primary_term = Some(boolean()?),
            "track_scores" => src.track_scores = boolean()?,
            "track_total_hits" => {
                src.track_total_hits = Some(match int()? {
                    i32::MAX => TrackTotalHits::Accurate,
                    -1 => TrackTotalHits::Disabled,
                    n => TrackTotalHits::UpTo(n),
                })
            }
            "min_score" => src.min_score = Some(v.as_f64().ok_or_else(set_err)? as f32),
            "terminate_after" => src.terminate_after = Some(int()?),
            "profile" => src.profile = boolean()?,
            other => return Err(set_err().caused_by(map_error(format!("Unsupported SearchRequest source property: {other}")))),
        }
    }
    Ok(())
}

fn hits_mut(resp: &mut Value) -> Option<&mut Vec<Value>> {
    match resp.as_object_mut()?.get_mut("hits")?.as_object_mut()?.get_mut("hits")? {
        Value::Arr(a) => Some(a),
        _ => None,
    }
}

fn hit_id(h: &Map) -> String {
    h.get("_id").map(Value::to_es_string).unwrap_or_else(|| "null".into())
}

/// A hit whose `_source` a processor rewrote through `getSourceAsMap`: its keys come back in
/// `HashMap` order.
fn rehash_source(hit: &mut Map) {
    if let Some(src) = hit.get_mut("_source") {
        *src = super::hash_ordered(std::mem::take(src));
    }
}

fn run_response(k: &ResponseKind, resp: &mut Value, rc: &RunContext) -> EsResult<()> {
    let mut truncated = false;
    let Some(hits) = hits_mut(resp) else { return Ok(()) };
    match k {
        ResponseKind::HybridExplanation => {}
        ResponseKind::Rename { field, target, ignore_missing } => {
            // `foundField` is not reset between hits, as in `RenameFieldResponseProcessor`.
            let mut found = false;
            for h in hits.iter_mut() {
                let Some(hm) = h.as_object_mut() else { continue };
                if let Some(Value::Obj(fields)) = hm.get_mut("fields")
                    && let Some(v) = fields.shift_remove(field)
                {
                    found = true;
                    fields.insert(target.clone(), v);
                }
                // `getSourceAsMap` (a `HashMap`), `remove` and `put`, written back: the source
                // comes out in `HashMap` order.
                if let Some(Value::Obj(source)) = hm.get_mut("_source")
                    && let Some(v) = source.shift_remove(field)
                {
                    found = true;
                    source.insert(target.clone(), v);
                    rehash_source(hm);
                }
                if !found && !ignore_missing {
                    return Err(EsError::illegal_argument(format!("Document with id {} is missing field {field}", hit_id(hm))));
                }
            }
        }
        ResponseKind::Truncate { size, prefix } => {
            let size = if *size < 0 {
                let key = context_key(prefix, "original_size");
                match rc.attributes.get(&key).and_then(Value::as_i64) {
                    Some(n) => n.max(0) as usize,
                    None => return Err(EsError::illegal_state(format!("Must specify target_size unless an earlier processor set {key}"))),
                }
            } else {
                *size as usize
            };
            truncated = hits.len() > size;
            hits.truncate(size);
        }
        ResponseKind::Collapse { field } => {
            let mut seen: Vec<String> = Vec::new();
            let mut kept = Vec::with_capacity(hits.len());
            for h in hits.drain(..) {
                let value = match h.get("fields").and_then(|f| f.get(field)) {
                    Some(Value::Arr(vals)) => {
                        if vals.len() > 1 {
                            let id = h.as_object().map(hit_id).unwrap_or_default();
                            return Err(EsError::illegal_state(format!("Failed to collapse {id}: doc has multiple values for field {field}")));
                        }
                        vals.first().cloned()
                    }
                    Some(v) => Some(v.clone()),
                    None => h.get("_source").and_then(|s| s.get(field)).cloned(),
                };
                let key = match value.filter(|v| !v.is_null()) {
                    Some(v) => v.to_es_string(),
                    None => "__missing__".to_string(),
                };
                if !seen.contains(&key) {
                    seen.push(key);
                    kept.push(h);
                }
            }
            *hits = kept;
        }
        ResponseKind::Sort { field, target, desc } => {
            for h in hits.iter_mut() {
                let Some(hm) = h.as_object_mut() else { continue };
                if let Some(Value::Obj(fields)) = hm.get_mut("fields")
                    && let Some(Value::Arr(vals)) = fields.get(field)
                {
                    let sorted = sorted_values(field, vals, *desc)?;
                    fields.insert(target.clone(), Value::Arr(sorted));
                }
                if let Some(Value::Obj(source)) = hm.get_mut("_source")
                    && source.contains_key(field.as_str())
                {
                    if let Some(Value::Arr(vals)) = source.get(field) {
                        let sorted = sorted_values(field, vals, *desc)?;
                        source.insert(target.clone(), Value::Arr(sorted));
                    }
                    rehash_source(hm);
                }
            }
        }
        ResponseKind::Split { field, separator, preserve_trailing, target } => {
            let pattern = crate::ingest::jregex::JavaPattern::compile(separator).map_err(crate::analysis::es_error)?;
            let limit = if *preserve_trailing { -1 } else { 0 };
            for h in hits.iter_mut() {
                let Some(hm) = h.as_object_mut() else { continue };
                if let Some(Value::Obj(fields)) = hm.get_mut("fields")
                    && let Some(vals) = fields.get(field)
                {
                    // `DocumentField.getValue()`: the first value.
                    let first = match vals {
                        Value::Arr(a) => a.first(),
                        v => Some(v),
                    };
                    let Some(Value::Str(s)) = first else {
                        return Err(EsError::illegal_argument(format!("field [{field}] is not a string, cannot split")));
                    };
                    let parts = pattern.split(s, limit)?;
                    fields.insert(target.clone(), Value::Arr(parts.into_iter().map(Value::Str).collect()));
                }
                if let Some(Value::Obj(source)) = hm.get_mut("_source")
                    && let Some(Value::Str(s)) = source.get(field)
                {
                    let parts = pattern.split(s, limit)?;
                    source.insert(target.clone(), Value::Arr(parts.into_iter().map(Value::Str).collect()));
                    rehash_source(hm);
                }
            }
        }
    }
    // A truncating processor rebuilds the response with an empty profile
    // (`SearchProfileShardResults`), which prints.
    if truncated
        && let Value::Obj(m) = resp
        && !m.contains_key("profile")
    {
        m.insert("profile".into(), crate::obj! { "shards" => Value::Arr(Vec::new()) });
    }
    Ok(())
}

/// `SortResponseProcessor.getSortedValues`: natural order of comparable values of one kind.
fn sorted_values(field: &str, vals: &[Value], desc: bool) -> EsResult<Vec<Value>> {
    if vals.iter().any(Value::is_null) {
        return Err(EsError::illegal_argument(format!("field [{field}] contains a null value.]")));
    }
    if let Some(v) = vals.iter().find(|v| matches!(v, Value::Obj(_) | Value::Arr(_))) {
        return Err(EsError::illegal_argument(format!("field [{field}] of type [{}] is not comparable.]", java_class(v))));
    }
    let kind = |v: &Value| match v {
        Value::Num(_) => 0,
        Value::Str(_) => 1,
        _ => 2,
    };
    if vals.windows(2).any(|w| kind(&w[0]) != kind(&w[1])) {
        let (a, b) = vals.windows(2).find(|w| kind(&w[0]) != kind(&w[1])).map(|w| (java_class(&w[0]), java_class(&w[1]))).unwrap_or_default();
        return Err(EsError::new(500, "class_cast_exception", format!("class {b} cannot be cast to class {a}")));
    }
    let mut out = vals.to_vec();
    out.sort_by(|a, b| {
        let o = match (a, b) {
            (Value::Num(x), Value::Num(y)) => x.as_f64().total_cmp(&y.as_f64()),
            (Value::Str(x), Value::Str(y)) => crate::api::cat::java_str_cmp(x, y),
            (Value::Bool(x), Value::Bool(y)) => x.cmp(y),
            _ => std::cmp::Ordering::Equal,
        };
        if desc { o.reverse() } else { o }
    });
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn config_errors_like_opensearch() {
        // OpenSearch 3.9.0 prints processor_type before property_name.
        let e = config_error(Some("rename_field"), None, Some("target_field"), "required property is missing");
        assert_eq!(
            e.to_value(false).to_json(),
            r#"{"type":"parse_exception","reason":"[target_field] required property is missing","processor_type":"rename_field","property_name":"target_field"}"#
        );
        let e = config_error(None, None, Some("version"), "property cannot be converted to an int [x]");
        assert_eq!(e.to_value(false).to_json(), r#"{"type":"parse_exception","reason":"[version] property cannot be converted to an int [x]","property_name":"version"}"#);
    }

    #[test]
    fn values_sort_naturally() {
        let v = |s: &str| crate::json::parse(s.as_bytes()).unwrap().unwrap();
        let Value::Arr(a) = v("[3, 1, 2]") else { panic!() };
        assert_eq!(Value::Arr(sorted_values("f", &a, false).unwrap()).to_json(), "[1,2,3]");
        assert_eq!(Value::Arr(sorted_values("f", &a, true).unwrap()).to_json(), "[3,2,1]");
        let Value::Arr(a) = v(r#"["b", null]"#) else { panic!() };
        assert_eq!(sorted_values("f", &a, false).unwrap_err().reason(), "field [f] contains a null value.]");
    }

    #[test]
    fn response_processors_rewrite_hits() {
        let mut resp = crate::json::parse(br#"{"hits":{"hits":[{"_id":"1","_source":{"a":"x,y","b":1},"fields":{"a":["x,y"]}},{"_id":"2","_source":{"a":"x,y"}},{"_id":"3","_source":{"c":2}}]}}"#).unwrap().unwrap();
        let rc = RunContext::default();
        run_response(&ResponseKind::Collapse { field: "a".into() }, &mut resp, &rc).unwrap();
        assert_eq!(hits_mut(&mut resp).unwrap().len(), 2);
        run_response(&ResponseKind::Split { field: "a".into(), separator: ",".into(), preserve_trailing: false, target: "s".into() }, &mut resp, &rc).unwrap();
        assert_eq!(resp.get("hits").unwrap().get("hits").unwrap().as_array().unwrap()[0].get("_source").unwrap().to_json(), r#"{"a":"x,y","b":1,"s":["x","y"]}"#);
        run_response(&ResponseKind::Rename { field: "a".into(), target: "r".into(), ignore_missing: false }, &mut resp, &rc).unwrap();
        let first = resp.get("hits").unwrap().get("hits").unwrap().as_array().unwrap()[0].clone();
        // Rewritten through `getSourceAsMap`: `HashMap` order (`b` and `r` share a bucket).
        assert_eq!(first.get("_source").unwrap().to_json(), r#"{"b":1,"r":"x,y","s":["x","y"]}"#);
        assert_eq!(first.get("fields").unwrap().to_json(), r#"{"s":["x","y"],"r":["x,y"]}"#);
        run_response(&ResponseKind::Truncate { size: 1, prefix: None }, &mut resp, &rc).unwrap();
        assert_eq!(hits_mut(&mut resp).unwrap().len(), 1);
        let e = run_response(&ResponseKind::Truncate { size: -1, prefix: Some("p".into()) }, &mut resp, &rc).unwrap_err();
        assert_eq!(e.reason(), "Must specify target_size unless an earlier processor set p.original_size");
    }
}
