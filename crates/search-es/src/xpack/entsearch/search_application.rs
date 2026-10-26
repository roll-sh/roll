//! Search applications (`_application/search_application`) and behavioral analytics
//! collections (`_application/analytics`).
//!
//! A search application is a stored search template over an alias: putting one points an
//! alias named after the application at its indices (as Elasticsearch does), `_search` renders
//! the template with the request's params over the template's defaults, checks them against the
//! application's dictionary (a JSON schema: `type`, `enum`, `required`,
//! `additionalProperties`, `properties`, `items`, with the validator's messages; `_search`
//! checks the request's params, `_render_query` the merged ones) and runs the
//! search on the alias. `_render_query` returns the rendered search body with the query in its
//! canonical form.
//!
//! An analytics collection *is* its event data stream (`behavioral_analytics-events-<name>`),
//! created from the built-in `behavioral_analytics-events-default` template (installed on first
//! use); events are validated and indexed into it.

use crate::error::{EsError, EsResult};
use crate::json::{Map, Value};
use crate::obj;
use crate::rest::{Ctx, Reply};
use crate::xpack::{enc, internal, store};

pub const APPS: &str = "search_application";

const DEFAULT_TEMPLATE: &str = "{\n  \"query\": {\n    \"query_string\": {\n        \"query\": \"{{query_string}}\",\n        \"default_field\": \"{{default_field}}\"\n        }\n    }\n}\n";

fn not_found(name: &str) -> EsError {
    EsError::resource_not_found(name.to_string())
}

fn normalize_template(t: Option<&Value>) -> Value {
    let Some(Value::Obj(t)) = t else {
        return obj! { "script" => obj! { "source" => DEFAULT_TEMPLATE, "lang" => "mustache", "params" => obj! { "query_string" => "*", "default_field" => "*" } } };
    };
    let mut out = Map::new();
    if let Some(Value::Obj(script)) = t.get("script") {
        let mut s = Map::new();
        let source = script.get("source").cloned().unwrap_or(Value::Null);
        let is_obj = matches!(source, Value::Obj(_));
        s.insert("source".into(), source);
        s.insert("lang".into(), script.get("lang").cloned().unwrap_or_else(|| Value::from("mustache")));
        if let Some(o) = script.get("options") {
            s.insert("options".into(), o.clone());
        } else if is_obj {
            s.insert("options".into(), obj! { "content_type" => "application/json;charset=utf-8" });
        }
        if let Some(p) = script.get("params") {
            s.insert("params".into(), p.clone());
        }
        out.insert("script".into(), Value::Obj(s));
    }
    if let Some(d) = t.get("dictionary") {
        out.insert("dictionary".into(), d.clone());
    }
    Value::Obj(out)
}

pub async fn put(ctx: &Ctx<'_>) -> EsResult<Reply> {
    let name = ctx.path_param("name").unwrap_or_default().to_string();
    let create = ctx.bool_param("create", false)?;
    let body = ctx.body_map()?.unwrap_or_default();
    for k in body.keys() {
        if !matches!(k.as_str(), "indices" | "analytics_collection_name" | "template" | "updated_at_millis" | "name") {
            return Err(crate::xpack::unknown_field(ctx, "search_application", k));
        }
    }
    let indices: Vec<String> = match body.get("indices") {
        Some(Value::Arr(a)) if !a.is_empty() => a.iter().map(Value::to_es_string).collect(),
        Some(Value::Str(s)) => vec![s.clone()],
        _ => return Err(EsError::action_request_validation(&["indices are missing".into()])),
    };
    let st = ctx.state();
    for i in &indices {
        let exists = st.metadata.indices.contains_key(i) || st.metadata.data_streams.contains_key(i) || st.metadata.indices.values().any(|x| x.aliases.contains_key(i))
            || (i.contains('*') && st.metadata.indices.keys().any(|n| crate::xcontent::filter::glob_match(i, n)));
        if !exists {
            return Err(EsError::illegal_argument(format!("no such index [{i}]")).caused_by(EsError::index_not_found(i)));
        }
    }
    if create && st.metadata.xpack.get(APPS, &name).is_some() {
        let uuid = st.metadata.indices.get(".search-app-1").map(|i| i.uuid.clone()).unwrap_or_default();
        return Err(EsError::new(409, "version_conflict_engine_exception", format!("[{name}]: version conflict, document already exists (current version [1])"))
            .with_meta("index_uuid", uuid)
            .with_meta("shard", "0")
            .with_meta("index", ".search-app-1"));
    }
    let previous: Vec<String> = st.metadata.indices.values().filter(|i| i.aliases.contains_key(&name)).map(|i| i.name.clone()).collect();
    drop(st);
    let mut app = Map::new();
    app.insert("name".into(), name.clone().into());
    app.insert("indices".into(), Value::Arr(indices.iter().map(|i| Value::from(i.as_str())).collect()));
    if let Some(a) = body.get("analytics_collection_name") {
        app.insert("analytics_collection_name".into(), a.clone());
    }
    app.insert("updated_at_millis".into(), ctx.now_ms.into());
    app.insert("template".into(), normalize_template(body.get("template")));
    // The alias named after the application points at exactly its indices.
    let mut actions: Vec<Value> = previous.iter().filter(|p| !indices.contains(p)).map(|p| obj! { "remove" => obj! { "index" => p.clone(), "alias" => name.clone() } }).collect();
    actions.push(obj! { "add" => obj! { "indices" => Value::Arr(indices.iter().map(|i| Value::from(i.as_str())).collect()), "alias" => name.clone() } });
    internal::call(ctx.cluster, "POST", "/_aliases", Some(&obj! { "actions" => Value::Arr(actions) })).await?;
    let now = ctx.now_ms;
    let created = ctx.cluster.update(move |st| store::put(st, APPS, &name, Value::Obj(app), now)).await?;
    Ok(Reply::Body(if created { 201 } else { 200 }, obj! { "result" => if created { "created" } else { "updated" } }))
}

pub fn get(ctx: &Ctx) -> EsResult<Reply> {
    let name = ctx.path_param("name").unwrap_or_default();
    ctx.state().metadata.xpack.get(APPS, name).cloned().map_or_else(|| Err(not_found(name)), Reply::ok)
}

pub fn list(ctx: &Ctx) -> EsResult<Reply> {
    let from = ctx.int_param("from", 0)?.max(0) as usize;
    let size = ctx.int_param("size", 100)?.max(0) as usize;
    let q = ctx.query("q").unwrap_or("*");
    let st = ctx.state();
    let all: Vec<Value> = st
        .metadata
        .xpack
        .iter(APPS)
        .filter(|(k, _)| crate::xcontent::filter::glob_match(q, k))
        .map(|(_, a)| {
            let mut m = Map::new();
            for k in ["name", "analytics_collection_name", "updated_at_millis"] {
                if let Some(v) = a.get(k) {
                    m.insert(k.into(), v.clone());
                }
            }
            Value::Obj(m)
        })
        .collect();
    let count = all.len();
    Reply::ok(obj! { "count" => count, "results" => Value::Arr(all.into_iter().skip(from).take(size).collect()) })
}

pub async fn delete(ctx: &Ctx<'_>) -> EsResult<Reply> {
    let name = ctx.path_param("name").unwrap_or_default().to_string();
    if ctx.state().metadata.xpack.get(APPS, &name).is_none() {
        return Err(not_found(&name));
    }
    let _ = internal::call(ctx.cluster, "DELETE", &format!("/_all/_alias/{}", enc(&name)), None).await;
    ctx.cluster
        .update(move |st| {
            store::remove(st, APPS, &name);
            Ok(())
        })
        .await?;
    Reply::acknowledged()
}

// --- rendering ---------------------------------------------------------------------------------

/// The template's defaults overlaid with the request's params.
fn merged_params(app: &Value, req: &Map) -> Map {
    let mut p = app.pointer(&["template", "script", "params"]).and_then(Value::as_object).cloned().unwrap_or_default();
    if let Some(Value::Obj(rp)) = req.get("params") {
        for (k, v) in rp {
            p.insert(k.clone(), v.clone());
        }
    }
    p
}

fn json_type(v: &Value) -> &'static str {
    match v {
        Value::Null => "null",
        Value::Bool(_) => "boolean",
        Value::Num(n) if n.is_integer() => "integer",
        Value::Num(_) => "number",
        Value::Str(_) => "string",
        Value::Arr(_) => "array",
        Value::Obj(_) => "object",
    }
}

/// A JSON schema check of the subset dictionaries use, with networknt's messages, in its
/// order (validators sorted by keyword: `additionalProperties`, `enum`, `items`, `properties`,
/// `required`, `type`).
fn validate(schema: &Value, v: &Value, path: &str, errors: &mut Vec<String>) {
    let props = schema.get("properties").and_then(Value::as_object);
    if let (Value::Obj(m), Some(Value::Bool(false))) = (v, schema.get("additionalProperties")) {
        for k in m.keys() {
            if props.is_none_or(|p| !p.contains_key(k)) {
                errors.push(format!("{path}.{k}: is not defined in the schema and the schema does not allow additional properties"));
            }
        }
    }
    if let Some(Value::Arr(options)) = schema.get("enum")
        && !options.contains(v)
    {
        let list: Vec<String> = options.iter().map(Value::to_es_string).collect();
        errors.push(format!("{path}: does not have a value in the enumeration [{}]", list.join(", ")));
    }
    if let (Value::Arr(items), Some(item_schema @ Value::Obj(_))) = (v, schema.get("items")) {
        for (i, it) in items.iter().enumerate() {
            validate(item_schema, it, &format!("{path}[{i}]"), errors);
        }
    }
    if let (Value::Obj(m), Some(p)) = (v, props) {
        for (k, sub) in p {
            match (m.get(k), sub) {
                (Some(val), Value::Obj(_)) => validate(sub, val, &format!("{path}.{k}"), errors),
                // A `false` schema rejects any value.
                (Some(_), Value::Bool(false)) => errors.push(format!("{path}.{k}: schema for '{k}' is false")),
                _ => {}
            }
        }
    }
    if let (Value::Obj(m), Some(Value::Arr(req))) = (v, schema.get("required")) {
        for r in req {
            let r = r.to_es_string();
            if !m.contains_key(&r) {
                errors.push(format!("{path}.{r}: is missing but it is required"));
            }
        }
    }
    if let Some(t) = schema.get("type") {
        let types: Vec<String> = match t {
            Value::Arr(a) => a.iter().map(Value::to_es_string).collect(),
            other => vec![other.to_es_string()],
        };
        let actual = json_type(v);
        if !types.iter().any(|t| t == actual || (t == "number" && actual == "integer")) {
            errors.push(format!("{path}: {actual} found, {} expected", types.join(", ")));
        }
    }
}

fn check(dict: Option<&Value>, params: &Map) -> EsResult<()> {
    let Some(dict) = dict else { return Ok(()) };
    let mut errors = Vec::new();
    validate(dict, &Value::Obj(params.clone()), "$", &mut errors);
    if errors.is_empty() {
        return Ok(());
    }
    let numbered: String = errors.iter().enumerate().map(|(i, e)| format!("{}: {e};", i + 1)).collect();
    Err(EsError::new(400, "validation_exception", format!("Validation Failed: {numbered}")))
}

/// Renders the template. `_render_query` validates the merged params against the dictionary,
/// `_search` only the request's (as Elasticsearch does).
fn render(ctx: &Ctx, app: &Value, req: &Map, validate_merged: bool) -> EsResult<Value> {
    let params = merged_params(app, req);
    let dict = app.pointer(&["template", "dictionary"]);
    if validate_merged {
        check(dict, &params)?;
    } else {
        check(dict, &req.get("params").and_then(Value::as_object).cloned().unwrap_or_default())?;
    }
    let mut tb = Map::new();
    tb.insert("source".into(), app.pointer(&["template", "script", "source"]).cloned().unwrap_or(Value::Null));
    tb.insert("params".into(), Value::Obj(params));
    crate::search::misc::render_template(ctx, &tb, None)
}

/// The search body in `SearchSourceBuilder`'s field order, with the query in canonical form.
fn canonical(body: Value) -> EsResult<Value> {
    let Value::Obj(mut m) = body else { return Ok(body) };
    if let Some(q) = m.get("query").cloned() {
        let (parsed, _) = crate::query::parse_query(&q, None, &[crate::json::locate::Seg::Key("query")])?;
        m.insert("query".into(), parsed.to_value());
    }
    if let Some(a) = m.shift_remove("aggs") {
        m.insert("aggregations".into(), a);
    }
    if let Some(s) = m.get("sort").cloned() {
        let list = match s {
            Value::Arr(a) => a,
            other => vec![other],
        };
        let norm: Vec<Value> = list
            .into_iter()
            .map(|e| match e {
                Value::Str(f) => {
                    let order = if f == "_score" { "desc" } else { "asc" };
                    let mut o = Map::new();
                    o.insert(f, obj! { "order" => order });
                    Value::Obj(o)
                }
                Value::Obj(o) => Value::Obj(
                    o.into_iter()
                        .map(|(k, v)| match v {
                            Value::Str(order) => (k, obj! { "order" => order }),
                            other => (k, other),
                        })
                        .collect(),
                ),
                other => other,
            })
            .collect();
        m.insert("sort".into(), Value::Arr(norm));
    }
    const ORDER: &[&str] = &[
        "from", "size", "timeout", "terminate_after", "query", "post_filter", "knn", "min_score", "version", "seq_no_primary_term", "explain", "_source", "stored_fields",
        "docvalue_fields", "script_fields", "fields", "sort", "track_scores", "track_total_hits", "search_after", "indices_boost", "aggregations", "highlight", "suggest", "rescore",
        "ext", "collapse", "slice", "point_in_time", "runtime_mappings", "retriever", "rank",
    ];
    let mut out = Map::new();
    for k in ORDER {
        if let Some(v) = m.shift_remove(*k) {
            out.insert(k.to_string(), v);
        }
    }
    out.extend(m);
    Ok(Value::Obj(out))
}

fn app_of(ctx: &Ctx) -> EsResult<Value> {
    let name = ctx.path_param("name").unwrap_or_default();
    ctx.state().metadata.xpack.get(APPS, name).cloned().ok_or_else(|| not_found(name))
}

pub fn render_query(ctx: &Ctx) -> EsResult<Reply> {
    let app = app_of(ctx)?;
    let req = ctx.body_map()?.unwrap_or_default();
    let body = render(ctx, &app, &req, true)?;
    Reply::ok(canonical(body)?)
}

pub async fn search(ctx: &Ctx<'_>) -> EsResult<Reply> {
    let app = app_of(ctx)?;
    let req = ctx.body_map()?.unwrap_or_default();
    let body = render(ctx, &app, &req, false)?;
    let name = ctx.path_param("name").unwrap_or_default();
    let mut uri = format!("/{}/_search", enc(name));
    if ctx.bool_param("typed_keys", false)? {
        uri.push_str("?typed_keys=true");
    }
    Reply::ok(internal::call(ctx.cluster, "POST", &uri, Some(&body)).await?)
}

// --- behavioral analytics ----------------------------------------------------------------------

const EVENTS_PREFIX: &str = "behavioral_analytics-events-";
const EVENTS_TEMPLATE: &str = "behavioral_analytics-events-default";

fn events_stream(name: &str) -> String {
    format!("{EVENTS_PREFIX}{name}")
}

/// Installs the built-in template event data streams are created from.
async fn ensure_events_template(ctx: &Ctx<'_>) -> EsResult<()> {
    if ctx.state().metadata.index_templates.contains_key(EVENTS_TEMPLATE) {
        return Ok(());
    }
    let keyword = obj! { "type" => "keyword" };
    let t = obj! {
        "index_patterns" => crate::arr!["behavioral_analytics-events-*"],
        "priority" => 100,
        "version" => 3,
        "_meta" => obj! { "managed" => true, "description" => "Built-in template applied by default to behavioral analytics event data streams." },
        "data_stream" => obj! { "hidden" => false, "allow_custom_routing" => true },
        "allow_auto_create" => false,
        "template" => obj! {
            "settings" => obj! { "index.hidden" => true },
            "mappings" => obj! {
                "dynamic" => "false",
                "properties" => obj! {
                    "@timestamp" => obj! { "type" => "date" },
                    "event" => obj! { "properties" => obj! { "action" => keyword.clone() } },
                    "session" => obj! { "properties" => obj! { "id" => keyword.clone() } },
                    "user" => obj! { "properties" => obj! { "id" => keyword.clone() } },
                    "data_stream" => obj! { "properties" => obj! {
                        "type" => obj! { "type" => "constant_keyword", "value" => "behavioral_analytics" },
                        "dataset" => obj! { "type" => "constant_keyword", "value" => "events" },
                        "namespace" => obj! { "type" => "constant_keyword" },
                    } },
                },
            },
        },
    };
    internal::call(ctx.cluster, "PUT", &format!("/_index_template/{EVENTS_TEMPLATE}"), Some(&t)).await.map(|_| ())
}

pub async fn put_analytics(ctx: &Ctx<'_>) -> EsResult<Reply> {
    let name = ctx.path_param("name").unwrap_or_default().to_string();
    if ctx.state().metadata.data_streams.contains_key(&events_stream(&name)) {
        return Err(EsError::resource_already_exists(format!("analytics collection [{name}] already exists")));
    }
    ensure_events_template(ctx).await?;
    internal::call(ctx.cluster, "PUT", &format!("/_data_stream/{}", enc(&events_stream(&name))), None).await?;
    Ok(Reply::Body(201, obj! { "acknowledged" => true, "name" => name }))
}

pub fn get_analytics(ctx: &Ctx) -> EsResult<Reply> {
    let st = ctx.state();
    let names = ctx.list_param("name").unwrap_or_default();
    let mut out = Map::new();
    for ds in st.metadata.data_streams.keys() {
        let Some(n) = ds.strip_prefix(EVENTS_PREFIX) else { continue };
        if names.is_empty() || names.iter().any(|p| crate::xcontent::filter::glob_match(p, n)) {
            out.insert(n.to_string(), obj! { "event_data_stream" => obj! { "name" => ds.clone() } });
        }
    }
    for n in &names {
        if !n.contains('*') && !out.contains_key(n) {
            return Err(EsError::resource_not_found(format!("no such analytics collection [{n}]")));
        }
    }
    Reply::ok(Value::Obj(out))
}

pub async fn delete_analytics(ctx: &Ctx<'_>) -> EsResult<Reply> {
    let name = ctx.path_param("name").unwrap_or_default().to_string();
    if !ctx.state().metadata.data_streams.contains_key(&events_stream(&name)) {
        return Err(EsError::resource_not_found(format!("analytics collection [{name}] does not exists")));
    }
    internal::call(ctx.cluster, "DELETE", &format!("/_data_stream/{}", enc(&events_stream(&name))), None).await?;
    Reply::acknowledged()
}

/// The fields each event type requires (`AnalyticsEventParser`s).
fn required_fields(event_type: &str) -> &'static [&'static str] {
    match event_type {
        "page_view" => &["session", "user", "page"],
        "search" => &["session", "user", "search"],
        "search_click" => &["session", "user", "search"],
        _ => &[],
    }
}

pub async fn post_event(ctx: &Ctx<'_>) -> EsResult<Reply> {
    let name = ctx.path_param("collection_name").unwrap_or_default().to_string();
    let ty = ctx.path_param("event_type").unwrap_or_default().to_string();
    let debug = ctx.bool_param("debug", false)?;
    if !ctx.state().metadata.data_streams.contains_key(&events_stream(&name)) {
        return Err(EsError::resource_not_found(format!("no such analytics collection [{name}]")));
    }
    if !matches!(ty.as_str(), "page_view" | "search" | "search_click") {
        return Err(EsError::illegal_argument(format!("{ty} is not a valid event type")));
    }
    let body = ctx.body_map()?.unwrap_or_default();
    let missing: Vec<String> = required_fields(&ty)
        .iter()
        .filter(|f| **f != "session" && !body.contains_key(**f))
        .map(|f| format!("Required one of fields [{f}], but none were specified."))
        .collect();
    if body.get("session").and_then(|s| s.get("id")).is_none() {
        return Err(EsError::illegal_argument("Required one of fields [session], but none were specified."));
    }
    if !missing.is_empty() {
        return Err(EsError::illegal_argument(missing.join(" ")));
    }
    let mut session = body.get("session").and_then(Value::as_object).cloned().unwrap_or_default();
    if let Some(ua) = ctx.request.header("user-agent") {
        session.insert("user_agent".into(), ua.into());
    }
    let mut ev = Map::new();
    ev.insert("@timestamp".into(), ctx.now_ms.into());
    ev.insert("event".into(), obj! { "action" => ty.clone() });
    ev.insert("data_stream".into(), obj! { "type" => "behavioral_analytics", "dataset" => "events", "namespace" => name.clone() });
    ev.insert("session".into(), Value::Obj(session));
    for (k, v) in &body {
        if k != "session" {
            ev.insert(k.clone(), v.clone());
        }
    }
    let ev = Value::Obj(ev);
    // Event data streams require routing: events are routed by session.
    let session_id = ev.pointer(&["session", "id"]).map(Value::to_es_string).unwrap_or_default();
    internal::call(ctx.cluster, "POST", &format!("/{}/_doc?op_type=create&routing={}", enc(&events_stream(&name)), enc(&session_id)), Some(&ev)).await?;
    let mut out = Map::new();
    out.insert("accepted".into(), true.into());
    if debug {
        out.insert("event".into(), ev);
    }
    Ok(Reply::Body(202, Value::Obj(out)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dictionary_validation() {
        let schema = obj! {
            "properties" => obj! { "f" => obj! { "type" => "string", "enum" => crate::arr!["a", "b"] }, "v" => obj! { "type" => "string" } },
            "required" => crate::arr!["f"],
            "additionalProperties" => false,
        };
        let mut e = Vec::new();
        validate(&schema, &obj! { "f" => "c" }, "$", &mut e);
        assert_eq!(e, ["$.f: does not have a value in the enumeration [a, b]"]);
        e.clear();
        validate(&schema, &obj! { "f" => "a", "zz" => 1 }, "$", &mut e);
        assert_eq!(e, ["$.zz: is not defined in the schema and the schema does not allow additional properties"]);
        e.clear();
        validate(&schema, &obj! { "f" => "a", "v" => 1 }, "$", &mut e);
        assert_eq!(e, ["$.v: integer found, string expected"]);
        e.clear();
        validate(&schema, &obj! {}, "$", &mut e);
        assert_eq!(e, ["$.f: is missing but it is required"]);
    }

    #[test]
    fn canonical_sort_and_order() {
        let v = canonical(obj! { "sort" => crate::arr!["n"], "size" => 5 }).unwrap();
        assert_eq!(v.to_json(), r#"{"size":5,"sort":[{"n":{"order":"asc"}}]}"#);
    }
}
