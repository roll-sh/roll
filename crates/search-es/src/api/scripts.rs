//! Stored scripts (`_scripts/{id}`), script languages and contexts.
//!
//! Scripts are stored as Elasticsearch stores them: `lang` and `source`, with mustache sources
//! given as objects kept as their JSON text (plus the `content_type` option Elasticsearch adds).
//! Painless sources are compiled only when a `context` is given, as in Elasticsearch; until the
//! Painless engine is wired in, any source is accepted.

use crate::error::{EsError, EsResult};
use crate::json::{Map, Value};
use crate::meta::StoredScript;
use crate::obj;
use crate::rest::{Ctx, Reply};

pub const LANGS: &[&str] = &["expression", "mustache", "painless"];

/// Script contexts Elasticsearch 9 registers (for `context` validation and `_script_context`).
pub const CONTEXTS: &[&str] = &[
    "aggregation_selector", "aggs", "aggs_combine", "aggs_init", "aggs_map", "aggs_reduce", "analysis", "boolean_field",
    "bucket_aggregation", "composite_field", "date_field", "double_field", "field", "filter", "geo_point_field", "ingest",
    "ingest_template", "interval", "ip_field", "keyword_field", "long_field", "moving-function", "number_sort", "painless_test",
    "processor_conditional", "score", "script_heuristic", "similarity", "similarity_weight", "string_sort", "template", "terms_set",
    "update", "xpack_template",
];

fn parsing(line: usize, col: usize, msg: impl Into<String>) -> EsError {
    EsError::new(400, "parsing_exception", msg).with_meta("line", line as i64).with_meta("col", col as i64)
}

pub async fn put(ctx: &Ctx<'_>) -> EsResult<Reply> {
    ctx.validate_timeouts()?;
    let id = ctx.path_param("id").unwrap_or_default().to_string();
    if let Some(c) = ctx.param("context")
        && !CONTEXTS.contains(&c)
    {
        return Err(EsError::illegal_argument(format!("failed to parse/compile stored script [{id}]"))
            .caused_by(EsError::illegal_argument(format!("Unknown context [{c}]"))));
    }
    let body = ctx.body()?.unwrap_or_else(|| obj! {});
    let raw = ctx.raw_json();
    let script = match body.get("script") {
        Some(s) => s,
        None => {
            let (l, c) = raw.map(end_of_object).unwrap_or((1, 2));
            return Err(parsing(l, c, "unexpected token [END_OBJECT, expected [script]"));
        }
    };
    let Value::Obj(sm) = script else {
        let (l, c) = raw
            .and_then(|r| crate::json::locate::locate(r, &[crate::json::locate::Seg::Key("script")], crate::json::locate::Target::Value))
            .unwrap_or((1, 1));
        return Err(parsing(l, c, format!("unexpected token [{}], expected [{{, <source>]", script.kind_name())));
    };
    let lang = sm.get("lang").map(Value::to_es_string);
    let source = sm.get("source").or_else(|| sm.get("code"));
    let options = sm.get("options").and_then(Value::as_object).cloned();
    let Some(lang) = lang else {
        return Err(EsError::illegal_argument("must specify lang for stored script"));
    };
    if lang.is_empty() {
        return Err(EsError::illegal_argument("lang cannot be empty"));
    }
    let Some(source) = source else {
        return Err(EsError::illegal_argument("must specify source for stored script"));
    };
    let (source, mut options) = match source {
        Value::Obj(_) | Value::Arr(_) if lang == "mustache" => {
            let mut o = options.unwrap_or_default();
            o.insert("content_type".into(), Value::from("application/json;charset=utf-8"));
            (Value::Str(source.to_json()), Some(o))
        }
        Value::Str(s) => (Value::Str(s.clone()), options),
        other => (Value::Str(other.to_es_string()), options),
    };
    if source.as_str().is_some_and(str::is_empty) {
        return Err(EsError::illegal_argument("source cannot be empty"));
    }
    if let Some(o) = &options
        && lang != "mustache"
    {
        let extra: Map = o.iter().filter(|(k, _)| k.as_str() != "content_type").map(|(k, v)| (k.clone(), v.clone())).collect();
        if !extra.is_empty() {
            return Err(EsError::illegal_argument(format!("illegal compiler options [{}] specified", crate::json::java_map_string(&extra))));
        }
    }
    if !LANGS.contains(&lang.as_str()) {
        return Err(EsError::illegal_argument(format!("unable to put stored script with unsupported lang [{lang}]")));
    }
    if options.as_ref().is_some_and(Map::is_empty) {
        options = None;
    }
    let script = StoredScript { lang, source, options };
    ctx.cluster
        .update(move |st| {
            st.metadata.scripts.insert(id, script);
            Ok(())
        })
        .await?;
    Reply::acknowledged()
}

/// The location just past the closing brace of a one-object body (where Jackson reports
/// `END_OBJECT`).
fn end_of_object(raw: &[u8]) -> (usize, usize) {
    let mut line = 1;
    let mut col = 1;
    let mut last = (1, 2);
    for &b in raw {
        if b == b'}' {
            last = (line, col);
        }
        if b == b'\n' {
            line += 1;
            col = 1;
        } else {
            col += 1;
        }
    }
    last
}

pub fn get(ctx: &Ctx) -> EsResult<Reply> {
    ctx.validate_timeouts()?;
    let id = ctx.path_param("id").unwrap_or_default();
    let st = ctx.state();
    match st.metadata.scripts.get(id) {
        Some(s) => Reply::ok(obj! { "_id" => id, "found" => true, "script" => s.to_value() }),
        None => Ok(Reply::Body(404, obj! { "_id" => id, "found" => false })),
    }
}

pub async fn delete(ctx: &Ctx<'_>) -> EsResult<Reply> {
    ctx.validate_timeouts()?;
    let id = ctx.path_param("id").unwrap_or_default().to_string();
    ctx.cluster
        .update(move |st| match st.metadata.scripts.remove(&id) {
            Some(_) => Ok(()),
            None => Err(EsError::resource_not_found(format!("stored script [{id}] does not exist and cannot be deleted"))),
        })
        .await?;
    Reply::acknowledged()
}

fn asset(raw: &'static [u8]) -> Value {
    crate::json::parse(raw).ok().flatten().unwrap_or_default()
}

/// `GET _script_language`, as Elasticsearch 9.5.5 lists its languages and contexts.
pub fn languages(_ctx: &Ctx) -> EsResult<Reply> {
    Reply::ok(asset(include_bytes!("../../assets/script_language.json")))
}

/// `GET _script_context`, with each context's methods as Elasticsearch 9.5.5 describes them.
pub fn contexts(_ctx: &Ctx) -> EsResult<Reply> {
    Reply::ok(asset(include_bytes!("../../assets/script_context.json")))
}
