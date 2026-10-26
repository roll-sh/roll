//! The bridge to `search-painless`: compiling a request's [`Script`] (inline or stored, painless
//! or mustache) for a script context, its errors as [`EsError`]s, and conversions between
//! roll's ordered JSON and the `serde_json` / Painless values the script engine speaks.
//!
//! One `search_painless::Engine` per cluster holds the compiled-script cache, so a script
//! compiled for one request is reused by the next (Elasticsearch's `ScriptService` cache).

use crate::error::{EsError, EsResult};
use crate::json::{Map, Number, Value};
use crate::meta::ClusterState;
use crate::query::script::{Script, ScriptType};
use search_painless::{Compiled, Context, Lang, ScriptError};
use std::sync::Arc;

/// The cluster's script engine.
pub struct ScriptService {
    pub engine: search_painless::Engine,
}

impl Default for ScriptService {
    fn default() -> Self {
        // Elasticsearch's YAML test clusters (and roll's tenants) compile many distinct scripts;
        // the compilation rate limit is off, the cache bounds memory.
        let settings = search_painless::Settings { max_compilations_rate: None, ..Default::default() };
        ScriptService { engine: search_painless::Engine::new(settings) }
    }
}

impl std::fmt::Debug for ScriptService {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("ScriptService")
    }
}

/// A compiled script with its parameters.
#[derive(Clone)]
pub struct Bound {
    pub compiled: Compiled,
    pub params: serde_json::Value,
    /// How Elasticsearch prints the script (`Script{type=inline, lang='painless', …}`).
    pub describe: String,
}

impl Bound {
    pub fn painless(&self) -> Option<&Arc<search_painless::Script>> {
        match &self.compiled {
            Compiled::Painless(s) => Some(s),
            _ => None,
        }
    }
}

/// `Script.toString()`.
pub fn describe(s: &Script) -> String {
    let ty = match s.ty {
        ScriptType::Inline => "inline",
        ScriptType::Stored => "stored",
    };
    let lang = match &s.lang {
        Some(l) => format!("'{l}'"),
        None => "null".into(),
    };
    let opts = java_map(&s.options);
    let params = java_map(&s.params);
    format!("Script{{type={ty}, lang={lang}, idOrCode='{}', options={opts}, params={params}}}", s.id_or_code)
}

fn java_map(m: &Map) -> String {
    let parts: Vec<String> = m.iter().map(|(k, v)| format!("{k}={}", java_value(v))).collect();
    format!("{{{}}}", parts.join(", "))
}

fn java_value(v: &Value) -> String {
    match v {
        Value::Null => "null".into(),
        Value::Str(s) => s.clone(),
        Value::Obj(m) => java_map(m),
        Value::Arr(a) => format!("[{}]", a.iter().map(java_value).collect::<Vec<_>>().join(", ")),
        other => other.to_es_string(),
    }
}

impl ScriptService {
    /// Compiles `s` for `ctx` (stored scripts looked up in `st`).
    pub fn compile(&self, st: &ClusterState, s: &Script, ctx: Context) -> EsResult<Bound> {
        let (lang, source, options) = match s.ty {
            ScriptType::Inline => (s.lang.clone().unwrap_or_else(|| "painless".into()), s.id_or_code.clone(), s.options.clone()),
            ScriptType::Stored => {
                let Some(stored) = st.metadata.scripts.get(&s.id_or_code) else {
                    return Err(EsError::resource_not_found(format!("unable to find script [{}] in cluster state", s.id_or_code)));
                };
                let src = match &stored.source {
                    Value::Str(x) => x.clone(),
                    other => other.to_json(),
                };
                (stored.lang.clone(), src, stored.options.clone().unwrap_or_default())
            }
        };
        let l = match lang.as_str() {
            "painless" => Lang::Painless,
            "mustache" => Lang::Mustache,
            "expression" => Lang::Expression,
            other => return Err(EsError::illegal_argument(format!("script_lang not supported [{other}]"))),
        };
        let opts: Vec<(String, String)> = options.iter().map(|(k, v)| (k.clone(), v.as_str().map(str::to_string).unwrap_or_else(|| v.to_json()))).collect();
        let compiled = match s.ty {
            ScriptType::Inline => self.engine.compile_lang(l, &source, ctx, &opts),
            ScriptType::Stored => self.engine.compile_stored(l, &source, ctx, &opts),
        }
        .map_err(|e| {
            // An engine that rejects the context throws a plain IllegalArgumentException, which
            // `ScriptService.compile` wraps.
            if l == Lang::Expression && e.plain && e.cause_type == "illegal_argument_exception" {
                let ty = if s.ty == ScriptType::Inline { "inline" } else { "stored" };
                let what = if s.ty == ScriptType::Inline { source.as_str() } else { s.id_or_code.as_str() };
                let cause = script_error(e);
                return EsError::new(cause.status, "general_script_exception", format!("Failed to compile {ty} script [{what}] using lang [expression]")).caused_by(cause);
            }
            script_error(e)
        })?;
        Ok(Bound { compiled, params: to_serde(&Value::Obj(s.params.clone())), describe: describe(s) })
    }
}

/// A script engine error as Elasticsearch renders it (`script_exception` with its stack and
/// cause, or a plain exception).
pub fn script_error(e: ScriptError) -> EsError {
    let status = e.status;
    let roots: Vec<EsError> = e.root_causes().iter().map(|r| from_json_error(r, status)).collect();
    let mut out = from_json_error(&e.to_json(), status);
    // An expression's cause is a `java.text.ParseException` (`parse_exception`, which reads like
    // Elasticsearch's own parse exception): the script exception itself is the root cause.
    if !e.plain && e.lang == "expression" && !e.cause_is_root {
        let full = out.clone();
        out = out.with_root_causes(vec![full]);
    } else if roots.len() == 1 && !e.plain && e.cause_is_root {
        out = out.with_root_causes(roots);
    }
    out
}

/// Builds an [`EsError`] from a rendered exception object (`type`, `reason`, metadata,
/// `caused_by`).
pub fn from_json_error(j: &serde_json::Value, status: u16) -> EsError {
    let kind = j.get("type").and_then(|t| t.as_str()).unwrap_or("exception").to_string();
    let reason = j.get("reason").and_then(|r| r.as_str()).map(str::to_string);
    let mut e = if search_analysis::is_java_exception(&kind) || JAVA.contains(&kind.as_str()) {
        EsError::java(status, kind, reason.clone().unwrap_or_default())
    } else {
        EsError::new(status, kind, reason.clone().unwrap_or_default())
    };
    if reason.is_none() {
        e = e.with_null_reason();
    }
    if let Some(o) = j.as_object() {
        // `serde_json` maps may come back sorted: metadata goes out in the order
        // `ScriptException.metadataToXContent` writes it, other keys after.
        let rank = |k: &str| ["script_stack", "script", "lang", "position"].iter().position(|x| *x == k).unwrap_or(usize::MAX);
        let mut keys: Vec<&String> = o.keys().filter(|k| !matches!(k.as_str(), "type" | "reason" | "caused_by" | "root_cause")).collect();
        keys.sort_by_key(|k| rank(k));
        for k in keys {
            let v = &o[k.as_str()];
            let mut val = from_serde(v);
            if k == "position"
                && let Value::Obj(m) = &val
            {
                let mut pm = Map::new();
                for f in ["offset", "start", "end"] {
                    if let Some(x) = m.get(f) {
                        pm.insert(f.into(), x.clone());
                    }
                }
                for (f, x) in m {
                    if !pm.contains_key(f) {
                        pm.insert(f.clone(), x.clone());
                    }
                }
                val = Value::Obj(pm);
            }
            // A host that threw with OpenSearch's frames (the opensearch dialect) owns the whole
            // stack: the Elasticsearch lookup frames the interpreter appends don't exist there.
            if k == "script_stack"
                && let Value::Arr(a) = &mut val
                && a.iter().any(|f| f.as_str().is_some_and(|f| f.starts_with("org.opensearch.")))
            {
                a.retain(|f| !f.as_str().is_some_and(|f| f.starts_with(search_painless::es::SERVER)));
            }
            e = e.with_meta(k.clone(), val);
        }
        if let Some(c) = o.get("caused_by") {
            e = e.caused_by(from_json_error(c, status));
        }
    }
    e
}

const JAVA: &[&str] = &[
    "illegal_argument_exception",
    "illegal_state_exception",
    "null_pointer_exception",
    "class_cast_exception",
    "arithmetic_exception",
    "array_index_out_of_bounds_exception",
    "index_out_of_bounds_exception",
    "string_index_out_of_bounds_exception",
    "number_format_exception",
    "unsupported_operation_exception",
    "concurrent_modification_exception",
    "no_such_element_exception",
    "stack_overflow_error",
    "out_of_memory_error",
    "pattern_syntax_exception",
    "date_time_exception",
    "date_time_parse_exception",
    "runtime_exception",
];

/// roll JSON → `serde_json` (key order kept by the map type in use).
pub fn to_serde(v: &Value) -> serde_json::Value {
    match v {
        Value::Null => serde_json::Value::Null,
        Value::Bool(b) => serde_json::Value::Bool(*b),
        Value::Num(Number::Int(i)) => serde_json::Value::from(*i),
        Value::Num(Number::UInt(u)) => serde_json::Value::from(*u),
        Value::Num(Number::Big(s)) => s.parse::<f64>().ok().and_then(serde_json::Number::from_f64).map(serde_json::Value::Number).unwrap_or(serde_json::Value::Null),
        Value::Num(Number::Float(f)) => serde_json::Number::from_f64(*f).map(serde_json::Value::Number).unwrap_or(serde_json::Value::Null),
        Value::Str(s) => serde_json::Value::String(s.clone()),
        Value::Arr(a) => serde_json::Value::Array(a.iter().map(|x| crate::stack::maybe_grow(|| to_serde(x))).collect()),
        Value::Obj(m) => {
            let mut o = serde_json::Map::new();
            for (k, x) in m {
                o.insert(k.clone(), crate::stack::maybe_grow(|| to_serde(x)));
            }
            serde_json::Value::Object(o)
        }
    }
}

/// `serde_json` → roll JSON.
pub fn from_serde(v: &serde_json::Value) -> Value {
    match v {
        serde_json::Value::Null => Value::Null,
        serde_json::Value::Bool(b) => Value::Bool(*b),
        serde_json::Value::Number(n) => {
            if let Some(i) = n.as_i64() {
                Value::Num(Number::Int(i))
            } else if let Some(u) = n.as_u64() {
                Value::Num(Number::UInt(u))
            } else {
                Value::Num(Number::Float(n.as_f64().unwrap_or(f64::NAN)))
            }
        }
        serde_json::Value::String(s) => Value::Str(s.clone()),
        serde_json::Value::Array(a) => Value::Arr(a.iter().map(|x| crate::stack::maybe_grow(|| from_serde(x))).collect()),
        serde_json::Value::Object(o) => {
            let mut m = Map::with_capacity(o.len());
            for (k, x) in o {
                m.insert(k.clone(), crate::stack::maybe_grow(|| from_serde(x)));
            }
            Value::Obj(m)
        }
    }
}

/// Scripts of `condition` and `predicate_token_filter` token filters (the `analysis`
/// context). Analysis chains are built outside any request, so they share one engine.
struct AnalysisScripts {
    engine: search_painless::Engine,
}

struct TokenScript {
    script: Arc<search_painless::Script>,
    params: serde_json::Value,
}

fn analysis_error(e: ScriptError) -> search_analysis::AnalysisError {
    let es = script_error(e);
    search_analysis::AnalysisError { kind: "script_exception", reason: es.reason().to_string(), status: es.status, caused_by: None }
}

impl search_analysis::ScriptHost for AnalysisScripts {
    fn compile_predicate(&self, script: &serde_json::Value) -> Result<Arc<dyn search_analysis::TokenPredicate>, search_analysis::AnalysisError> {
        let (source, params) = match script {
            serde_json::Value::String(s) => (s.clone(), serde_json::Value::Object(Default::default())),
            serde_json::Value::Object(m) => {
                if let Some(l) = m.get("lang").and_then(|l| l.as_str())
                    && l != "painless"
                {
                    return Err(search_analysis::AnalysisError::illegal(format!("script_lang not supported [{l}]")));
                }
                let src = m.get("source").or_else(|| m.get("inline")).and_then(|s| s.as_str()).unwrap_or_default().to_string();
                (src, m.get("params").cloned().unwrap_or(serde_json::Value::Object(Default::default())))
            }
            _ => return Err(search_analysis::AnalysisError::illegal("[script] must be an object or a string")),
        };
        let script = self.engine.compile(&source, Context::Analysis).map_err(analysis_error)?;
        Ok(Arc::new(TokenScript { script, params }))
    }
}

impl search_analysis::TokenPredicate for TokenScript {
    fn test(&self, t: &search_analysis::ScriptToken<'_>) -> Result<bool, search_analysis::AnalysisError> {
        let token = search_painless::es::misc::Token {
            term: t.term.to_string(),
            position: t.position as i32,
            position_increment: t.position_increment as i32,
            position_length: t.position_length as i32,
            start_offset: t.start_offset as i32,
            end_offset: t.end_offset as i32,
            type_: t.kind.to_string(),
            keyword: t.keyword,
        };
        let mut inst = self.script.instance();
        inst.set_params(&self.params);
        inst.set_var("token", search_painless::es::misc::analysis_token(token));
        inst.execute_bool(&mut search_painless::NoHost).map_err(analysis_error)
    }
}

static ANALYSIS_SCRIPTS: std::sync::LazyLock<Arc<AnalysisScripts>> = std::sync::LazyLock::new(|| {
    let settings = search_painless::Settings { max_compilations_rate: None, ..Default::default() };
    Arc::new(AnalysisScripts { engine: search_painless::Engine::new(settings) })
});

/// The script host analysis chains compile their predicate scripts with.
pub fn analysis_scripts() -> Arc<dyn search_analysis::ScriptHost> {
    ANALYSIS_SCRIPTS.clone()
}
