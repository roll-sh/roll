//! The OpenSearch SQL plugin's PPL endpoints (`POST _plugins/_ppl`, `_plugins/_ppl/_explain`,
//! `GET _plugins/_ppl/_grammar`) and its settings API (`PUT _plugins/_query/settings`), as the
//! plugin 3.9.0 answers them with its Calcite engine.
//!
//! [`parser`] turns the query into commands, [`exec`] resolves the source indices' mappings into
//! a schema, scans the documents (the `search` command's terms and time modifiers become the
//! scan's query) and runs the commands over rows, with [`func`] holding the scalar functions.
//! Responses are built here: the default `jdbc` format (`schema`, `datarows`, `total`, `size`,
//! pretty-printed the way Gson prints it), `csv`, `raw` and `viz`.
//!
//! Errors keep the plugin's own shape (`{"error": {"reason", "details", "type", ...},
//! "status"}` with Java `HashMap` key order and `org.json`'s indentation, where a one-entry
//! array or object stays on one line), except request-level failures (a body without `query`,
//! an unknown format), which the core REST layer renders as usual.

mod brain;
pub mod exec;
mod exec_more;
pub mod func;
pub mod parser;
pub mod value;

use crate::error::{EsError, EsResult};
use crate::json::{Map, Number, Value};
use crate::rest::{Ctx, Reply};

/// The hidden key carrying a plugin-format error body (see [`render_error`]).
const PPL_ERROR: &str = "ppl_error";

/// A plugin error: `type` is the Java exception's simple name; the body's other fields
/// (`code`, `context`, `location`, `suggestion`) come in `extra`.
pub(crate) fn error(status: u16, ty: &'static str, reason: impl Into<String>, extra: Vec<(&'static str, Value)>) -> EsError {
    let reason = reason.into();
    let mut m = Map::new();
    m.insert("reason".into(), Value::Str(reason.clone()));
    m.insert("details".into(), Value::Str(reason.clone()));
    m.insert("type".into(), Value::Str(ty.into()));
    for (k, v) in extra {
        m.insert(k.into(), v);
    }
    EsError::java(status, ty, reason).with_hidden(PPL_ERROR, Value::Obj(m))
}

/// An analysis-stage error (`SemanticCheckException`, `IllegalArgumentException`, ...) with the
/// plugin's `context`/`location`/`code`.
pub(crate) fn analysis_error(ty: &'static str, code: &'static str, reason: impl Into<String>, context: Vec<(&'static str, Value)>) -> EsError {
    let mut ctx = Map::new();
    ctx.insert("stage".into(), Value::Str("analyzing".into()));
    ctx.insert("stage_description".into(), Value::Str("Parsing and validating the query".into()));
    for (k, v) in context {
        ctx.insert(k.into(), v);
    }
    error(
        400,
        ty,
        reason,
        vec![("context", Value::Obj(ctx)), ("location", Value::Arr(vec![Value::Str("while preparing and validating the query plan".into())])), ("code", Value::Str(code.into()))],
    )
}

pub(crate) fn semantic(reason: impl Into<String>) -> EsError {
    analysis_error("SemanticCheckException", "UNKNOWN", reason, Vec::new())
}

/// A failure outside the analysis stage (`ArithmeticException` overflow, invalid spans):
/// the plugin reports `Invalid Query` with the exception's message as the details.
pub(crate) fn invalid_query(ty: &'static str, details: impl Into<String>) -> EsError {
    let details = details.into();
    let mut m = Map::new();
    m.insert("reason".into(), Value::Str("Invalid Query".into()));
    m.insert("details".into(), Value::Str(details.clone()));
    m.insert("type".into(), Value::Str(ty.into()));
    EsError::java(400, ty, details).with_hidden(PPL_ERROR, Value::Obj(m))
}

/// `SyntaxCheckException`: the plugin reports `Invalid Query` with the parser's message as
/// the details.
pub(crate) fn syntax_error(details: String) -> EsError {
    invalid_query("SyntaxCheckException", details)
}

/// The response for a failed query: the plugin's error body, or (for errors from the core
/// APIs the engine called) the core error.
fn render_error(e: EsError) -> EsResult<Reply> {
    let Some(Value::Obj(inner)) = e.hidden(PPL_ERROR).cloned() else {
        return Err(e);
    };
    let inner = hash_deep(Value::Obj(inner));
    let mut outer = Map::new();
    outer.insert("error".into(), inner);
    outer.insert("status".into(), Value::Num(Number::Int(i64::from(e.status))));
    let outer = Value::Obj(crate::json::java_hash_map(outer, 2));
    let mut s = String::new();
    org_json(&mut s, &outer, 0);
    // The plugin sends its error bodies as plain text.
    Ok(Reply::Raw(e.status, "text/plain; charset=UTF-8".into(), bytes::Bytes::from(s)))
}

/// The error maps are `HashMap`s: the plain `Invalid Query` form a default-sized one, the
/// analysis-stage errors (with a `context`) copies sized to their entry count
/// (`new HashMap<>(map.size())`), so the order depends on the entry count. `query_pos` keeps
/// its `column`, `line` order.
fn hash_deep(v: Value) -> Value {
    match v {
        Value::Obj(m) => {
            let cap = if m.contains_key("context") || m.contains_key("stage") { m.len() } else { 16 };
            let m: Map = m.into_iter().map(|(k, v)| if k == "query_pos" { (k, v) } else { (k, hash_deep(v)) }).collect();
            Value::Obj(crate::json::java_hash_map(m, cap))
        }
        other => other,
    }
}

/// `JSONObject.toString(2)`: two-space indents, `"key": value`, and a container with one
/// entry kept on its line.
fn org_json(out: &mut String, v: &Value, indent: usize) {
    match v {
        Value::Obj(m) if m.len() == 1 => {
            let (k, v) = m.iter().next().expect("one");
            out.push('{');
            out.push_str(&Value::Str(k.clone()).to_json());
            out.push_str(": ");
            org_json(out, v, indent);
            out.push('}');
        }
        Value::Obj(m) if !m.is_empty() => {
            out.push_str("{\n");
            for (i, (k, v)) in m.iter().enumerate() {
                if i > 0 {
                    out.push_str(",\n");
                }
                pad(out, indent + 2);
                out.push_str(&Value::Str(k.clone()).to_json());
                out.push_str(": ");
                org_json(out, v, indent + 2);
            }
            out.push('\n');
            pad(out, indent);
            out.push('}');
        }
        Value::Arr(a) if a.len() == 1 => {
            out.push('[');
            org_json(out, &a[0], indent);
            out.push(']');
        }
        Value::Arr(a) if !a.is_empty() => {
            out.push_str("[\n");
            for (i, v) in a.iter().enumerate() {
                if i > 0 {
                    out.push_str(",\n");
                }
                pad(out, indent + 2);
                org_json(out, v, indent + 2);
            }
            out.push('\n');
            pad(out, indent);
            out.push(']');
        }
        other => out.push_str(&other.to_json()),
    }
}

fn pad(out: &mut String, n: usize) {
    out.extend(std::iter::repeat_n(' ', n));
}

/// Gson's pretty printing (the `jdbc` format): two-space indents, `"key": value`, `[]` and
/// `{}` for empty containers.
pub(crate) fn gson_pretty(out: &mut String, v: &Value, indent: usize) {
    match v {
        Value::Obj(m) if !m.is_empty() => {
            out.push_str("{\n");
            for (i, (k, v)) in m.iter().enumerate() {
                if i > 0 {
                    out.push_str(",\n");
                }
                pad(out, indent + 2);
                out.push_str(&Value::Str(k.clone()).to_json());
                out.push_str(": ");
                gson_pretty(out, v, indent + 2);
            }
            out.push('\n');
            pad(out, indent);
            out.push('}');
        }
        Value::Arr(a) if !a.is_empty() => {
            out.push_str("[\n");
            for (i, v) in a.iter().enumerate() {
                if i > 0 {
                    out.push_str(",\n");
                }
                pad(out, indent + 2);
                gson_pretty(out, v, indent + 2);
            }
            out.push('\n');
            pad(out, indent);
            out.push(']');
        }
        other => out.push_str(&other.to_json()),
    }
}

fn json_reply(v: &Value, pretty: bool) -> Reply {
    let body = if pretty {
        let mut s = String::new();
        gson_pretty(&mut s, v, 0);
        s
    } else {
        v.to_json()
    };
    Reply::Raw(200, "application/json; charset=UTF-8".into(), bytes::Bytes::from(body))
}

/// The request body: `query` (required) and the optional `profile` and `analyze` flags.
struct Request {
    query: String,
    profile: bool,
    analyze: bool,
}

fn request(ctx: &Ctx<'_>) -> EsResult<Request> {
    let body = ctx.body().ok().flatten().unwrap_or(Value::Null);
    let payload = || EsError::illegal_argument("Failed to parse request payload");
    let Some(q) = body.get("query") else {
        return Err(payload().caused_by(EsError::java(400, "j_s_o_n_exception", "JSONObject[\"query\"] not found.")));
    };
    let query = match q {
        Value::Str(s) => s.clone(),
        other => other.to_es_string(),
    };
    let flag = |k: &str| body.get(k).and_then(Value::as_bool).unwrap_or(false);
    Ok(Request { query, profile: flag("profile"), analyze: flag("analyze") })
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Format {
    Jdbc,
    Csv,
    Raw,
    Viz,
}

fn format(ctx: &Ctx<'_>) -> EsResult<Format> {
    match ctx.query("format").map(str::to_ascii_lowercase).as_deref() {
        None | Some("jdbc") | Some("json") => Ok(Format::Jdbc),
        Some("csv") => Ok(Format::Csv),
        Some("raw") => Ok(Format::Raw),
        Some("viz") => Ok(Format::Viz),
        Some(other) => Err(EsError::illegal_argument(format!("Failed to create executor due to unknown response format: {other}"))),
    }
}

/// `POST _plugins/_ppl`.
pub async fn ppl(ctx: &Ctx<'_>) -> EsResult<Reply> {
    let req = request(ctx)?;
    let fmt = format(ctx)?;
    if ctx.cluster.state().metadata.cluster_setting("plugins.ppl.enabled") == Some("false") {
        return render_error(error(400, "IllegalAccessException", "Either plugins.ppl.enabled or rest.action.multi.allow_explicit_index setting is false", Vec::new()));
    }
    let started = std::time::Instant::now();
    let q = match parser::parse(&req.query) {
        Ok(q) => q,
        Err(e) => return render_error(e),
    };
    if q.explain {
        return match exec::explain(ctx.cluster, &q, ctx.now_ms).await {
            Ok(v) => Ok(json_reply(&v, true)),
            Err(e) => render_error(e),
        };
    }
    let mut prof = exec::Profile::default();
    let table = match exec::run(ctx.cluster, &q, ctx.now_ms, &mut prof).await {
        Ok(t) => t,
        Err(e) => return render_error(e),
    };
    let pretty = ctx.query("pretty").is_none_or(|p| p != "false");
    Ok(match fmt {
        Format::Jdbc if req.analyze => {
            // `analyze`: the plans with Calcite's cost estimates, the profile and a
            // recommendation, then the result with `ExprType` names in its schema.
            prof.total = started.elapsed();
            let (logical, physical) = exec::plan_lines(ctx.cluster, &q, ctx.now_ms).await.unwrap_or_default();
            let mut m = Map::new();
            m.insert("logicalPlan".into(), Value::Arr(logical.into_iter().map(Value::Str).collect()));
            m.insert("physicalPlan".into(), Value::Arr(physical.into_iter().map(Value::Str).collect()));
            m.insert("profile".into(), prof.to_value(table.rows.len()));
            let ms = (prof.execute.as_secs_f64() * 1000.0 * 100.0).round() / 100.0;
            m.insert(
                "recommendations".into(),
                crate::arr![crate::obj! {
                    "severity" => "INFO",
                    "rule" => "Bottleneck Stage",
                    "message" => format!("CalciteEnumerableIndexScan took {ms} ms (100% of execution)"),
                    "affected_node" => "CalciteEnumerableIndexScan"
                }],
            );
            if let Value::Obj(body) = jdbc_with(&table, |t| t.upper()) {
                m.extend(body);
            }
            json_reply(&Value::Obj(m), pretty)
        }
        Format::Jdbc => {
            let v = jdbc(&table);
            if req.profile {
                prof.total = started.elapsed();
                let mut m = Map::new();
                m.insert("profile".into(), prof.to_value(table.rows.len()));
                if let Value::Obj(body) = v {
                    m.extend(body);
                }
                json_reply(&Value::Obj(m), pretty)
            } else {
                json_reply(&v, pretty)
            }
        }
        Format::Viz => json_reply(&viz(&table), false),
        Format::Csv | Format::Raw => {
            let sanitize = fmt == Format::Csv && ctx.query("sanitize").is_none_or(|s| s != "false");
            // The plugin labels csv and raw output as JSON.
            Reply::Raw(200, "application/json; charset=UTF-8".into(), bytes::Bytes::from(delimited(&table, if fmt == Format::Csv { ',' } else { '|' }, sanitize)))
        }
    })
}

/// `POST _plugins/_ppl/_explain`.
pub async fn explain(ctx: &Ctx<'_>) -> EsResult<Reply> {
    let req = request(ctx)?;
    let q = match parser::parse(&req.query) {
        Ok(q) => q,
        Err(e) => return render_error(e),
    };
    match exec::explain(ctx.cluster, &q, ctx.now_ms).await {
        Ok(v) => Ok(json_reply(&v, true)),
        Err(e) => render_error(e),
    }
}

/// `GET _plugins/_ppl/_grammar`: the plugin's grammar bundle (ANTLR's serialized ATNs, rule
/// and token names of `OpenSearchPPLLexer.g4`/`OpenSearchPPLParser.g4`, Apache-2.0), as
/// OpenSearch 3.9.0 serves it.
pub fn grammar(_ctx: &Ctx<'_>) -> EsResult<Reply> {
    use std::io::Read;
    static GRAMMAR: std::sync::OnceLock<bytes::Bytes> = std::sync::OnceLock::new();
    let body = GRAMMAR.get_or_init(|| {
        let mut out = Vec::new();
        let gz: &[u8] = include_bytes!("../../../assets/ppl_grammar.json.gz");
        flate2::read::GzDecoder::new(gz).read_to_end(&mut out).expect("bundled grammar");
        bytes::Bytes::from(out)
    });
    Ok(Reply::Raw(200, "application/json; charset=UTF-8".into(), body.clone()))
}

/// The settings prefixes `_plugins/_query/settings` passes on (`RestQuerySettingsAction`).
const SETTING_PREFIXES: &[&str] = &["plugins.sql.", "plugins.ppl.", "plugins.query.", "plugins.calcite.", "opendistro.sql.", "opendistro.ppl.", "opendistro.query."];

/// `PUT _plugins/_query/settings`: the plugin's own keys of the body, as a cluster settings
/// update.
pub async fn settings(ctx: &Ctx<'_>) -> EsResult<Reply> {
    let body = ctx.body()?.unwrap_or(Value::Null);
    let mut out = Map::new();
    for scope in ["transient", "persistent"] {
        let mut kept = Map::new();
        if let Some(Value::Obj(m)) = body.get(scope) {
            let mut flat = Vec::new();
            flatten("", &Value::Obj(m.clone()), &mut flat);
            for (k, v) in flat {
                if SETTING_PREFIXES.iter().any(|p| k.starts_with(p)) {
                    kept.insert(k, v);
                }
            }
        }
        out.insert(scope.into(), Value::Obj(kept));
    }
    let empty = out.values().all(|v| v.as_object().is_some_and(Map::is_empty));
    if empty {
        return Err(EsError::action_request_validation(&["no settings to update".into()]));
    }
    let v = crate::xpack::internal::call(ctx.cluster, "PUT", "/_cluster/settings", Some(&Value::Obj(out))).await?;
    Reply::ok(v)
}

fn flatten(prefix: &str, v: &Value, out: &mut Vec<(String, Value)>) {
    match v {
        Value::Obj(m) => {
            for (k, v) in m {
                let key = if prefix.is_empty() { k.clone() } else { format!("{prefix}.{k}") };
                flatten(&key, v, out);
            }
        }
        other => out.push((prefix.to_string(), other.clone())),
    }
}

/// The `jdbc` body.
fn jdbc(t: &exec::Table) -> Value {
    jdbc_with(t, value::Ty::name)
}

fn jdbc_with(t: &exec::Table, type_name: fn(value::Ty) -> &'static str) -> Value {
    let schema: Vec<Value> = t.cols.iter().map(|c| crate::obj! { "name" => c.name.clone(), "type" => type_name(c.ty) }).collect();
    let rows: Vec<Value> = t.rows.iter().map(|r| Value::Arr(r.iter().zip(&t.cols).map(|(v, c)| v.to_json(c.ty)).collect())).collect();
    let n = rows.len() as i64;
    crate::obj! { "schema" => Value::Arr(schema), "datarows" => Value::Arr(rows), "total" => n, "size" => n }
}

/// The `viz` body: one array per column, with the mapping-style type names.
fn viz(t: &exec::Table) -> Value {
    let mut data = Map::new();
    for (i, c) in t.cols.iter().enumerate() {
        data.insert(c.name.clone(), Value::Arr(t.rows.iter().map(|r| r[i].to_json(c.ty)).collect()));
    }
    let fields: Vec<Value> = t.cols.iter().map(|c| crate::obj! { "name" => c.name.clone(), "type" => legacy_type(c.ty) }).collect();
    crate::obj! { "data" => Value::Obj(data), "metadata" => crate::obj! { "fields" => Value::Arr(fields) }, "size" => t.rows.len() as i64, "status" => 200 }
}

/// The legacy (`ExprCoreType` → OpenSearch) type names the `viz` format prints.
fn legacy_type(t: value::Ty) -> &'static str {
    use value::Ty;
    match t {
        Ty::Int => "integer",
        Ty::Long => "long",
        Ty::Short => "short",
        Ty::Byte => "byte",
        Ty::Str => "keyword",
        Ty::Struct => "object",
        Ty::Array => "nested",
        other => other.name(),
    }
}

/// `csv` (comma-separated, with formula sanitizing) and `raw` (`|`-separated).
fn delimited(t: &exec::Table, sep: char, sanitize: bool) -> String {
    let mut out = String::new();
    let cell = |s: String| -> String {
        let mut s = s;
        if sanitize && s.starts_with(['=', '+', '-', '@']) {
            s.insert(0, '\'');
        }
        if sep == ',' && (s.contains(',') || s.contains('"') || s.contains('\n')) {
            format!("\"{}\"", s.replace('"', "\"\""))
        } else {
            s
        }
    };
    let header: Vec<String> = t.cols.iter().map(|c| cell(c.name.clone())).collect();
    out.push_str(&header.join(&sep.to_string()));
    for r in &t.rows {
        out.push('\n');
        let cells: Vec<String> = r.iter().zip(&t.cols).map(|(v, c)| cell(v.to_text(c.ty).unwrap_or_default())).collect();
        out.push_str(&cells.join(&sep.to_string()));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn errors_print_like_org_json() {
        let e = analysis_error("SemanticCheckException", "UNKNOWN", "bad", Vec::new());
        let Ok(Reply::Raw(400, _, body)) = render_error(e) else { panic!() };
        let s = std::str::from_utf8(&body).unwrap();
        // HashMap order of the plugin's keys, one-entry arrays on one line.
        assert_eq!(
            s,
            "{\n  \"error\": {\n    \"context\": {\n      \"stage\": \"analyzing\",\n      \"stage_description\": \"Parsing and validating the query\"\n    },\n    \"reason\": \"bad\",\n    \"details\": \"bad\",\n    \"location\": [\"while preparing and validating the query plan\"],\n    \"code\": \"UNKNOWN\",\n    \"type\": \"SemanticCheckException\"\n  },\n  \"status\": 400\n}"
        );
    }
}
