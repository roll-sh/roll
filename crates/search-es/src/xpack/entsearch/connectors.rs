//! Connectors (`_connector`) and connector sync jobs (`_connector/_sync_job`), plus the
//! internal connector secrets API (`_connector/_secret`).
//!
//! A connector is the record an Elastic connector service reads and updates: its target index,
//! configuration, filtering (a draft that becomes active once validated), scheduling, status
//! (Elasticsearch's `ConnectorStateMachine`) and sync bookkeeping. Sync jobs carry a snapshot
//! of their connector and move through `ConnectorSyncJobStateMachine`; invalid transitions fail
//! with Elasticsearch's `status_exception`. Deletes are soft unless `hard=true`.

use super::now_nanos_string;
use crate::error::{EsError, EsResult};
use crate::json::{Map, Value};
use crate::obj;
use crate::rest::{Ctx, Reply};
use crate::json::locate::{Seg, Target, locate};
use crate::xpack::{store, unknown_field};

pub const CONNECTORS: &str = "connector";
pub const SYNC_JOBS: &str = "connector.sync_job";
pub const SECRETS: &str = "connector_secret";

const CONNECTOR_STATUSES: [&str; 5] = ["created", "needs_configuration", "configured", "connected", "error"];

fn not_found(id: &str) -> EsError {
    EsError::resource_not_found(format!("connector [{id}] not found"))
}

fn updated() -> EsResult<Reply> {
    Reply::ok(obj! { "result" => "updated" })
}

fn default_filtering(now: &str) -> Value {
    let rule = obj! {
        "created_at" => now, "field" => "_", "id" => "DEFAULT", "order" => 0, "policy" => "include", "rule" => "regex", "updated_at" => now, "value" => ".*",
    };
    let rules = obj! {
        "advanced_snippet" => obj! { "created_at" => now, "updated_at" => now, "value" => obj! {} },
        "rules" => crate::arr![rule],
        "validation" => obj! { "errors" => Value::Arr(vec![]), "state" => "valid" },
    };
    crate::arr![obj! { "active" => rules.clone(), "domain" => "DEFAULT", "draft" => rules }]
}

fn default_scheduling() -> Value {
    let s = obj! { "enabled" => false, "interval" => "0 0 0 * * ?" };
    obj! { "access_control" => s.clone(), "full" => s.clone(), "incremental" => s }
}

fn new_connector(id: &str, body: &Map) -> Value {
    let now = now_nanos_string();
    let mut m = Map::new();
    m.insert("id".into(), id.into());
    if let Some(v) = body.get("api_key_id") {
        m.insert("api_key_id".into(), v.clone());
    }
    m.insert("configuration".into(), Value::object());
    m.insert("custom_scheduling".into(), Value::object());
    if let Some(d) = body.get("description") {
        m.insert("description".into(), d.clone());
    }
    m.insert("filtering".into(), default_filtering(&now));
    m.insert("index_name".into(), body.get("index_name").cloned().unwrap_or(Value::Null));
    m.insert("is_native".into(), body.get("is_native").cloned().unwrap_or(Value::Bool(false)));
    if let Some(l) = body.get("language") {
        m.insert("language".into(), l.clone());
    }
    m.insert("name".into(), body.get("name").cloned().unwrap_or_else(|| Value::from("")));
    m.insert("scheduling".into(), default_scheduling());
    if let Some(s) = body.get("service_type") {
        m.insert("service_type".into(), s.clone());
    }
    // An Elastic-managed connector needs its configuration first.
    let native = body.get("is_native").and_then(Value::as_bool) == Some(true);
    m.insert("status".into(), (if native { "needs_configuration" } else { "created" }).into());
    m.insert("sync_now".into(), false.into());
    m.insert("deleted".into(), false.into());
    if m.get("index_name").is_some_and(Value::is_null) {
        m.shift_remove("index_name");
    }
    Value::Obj(m)
}

const PUT_FIELDS: [&str; 6] = ["description", "index_name", "is_native", "language", "name", "service_type"];

fn check_index_name(st: &crate::meta::ClusterState, id: &str, index: Option<&Value>) -> EsResult<()> {
    let Some(Value::Str(index)) = index else { return Ok(()) };
    crate::meta::names::validate_index_name(index)?;
    if st.metadata.xpack.iter(CONNECTORS).any(|(k, c)| k != id && c.get("deleted") != Some(&Value::Bool(true)) && c.get("index_name").and_then(Value::as_str) == Some(index)) {
        return Err(EsError::illegal_argument(format!("Index name [{index}] is used by another connector.")));
    }
    Ok(())
}

async fn create(ctx: &Ctx<'_>, id: String) -> EsResult<(bool, String)> {
    let mut body = ctx.body_map()?.unwrap_or_default();
    let parser = if ctx.method == crate::routes::Method::Post { "connector_post_request" } else { "connector_put_request" };
    for (k, v) in body.iter_mut() {
        if !PUT_FIELDS.contains(&k.as_str()) {
            return Err(unknown_field(ctx, parser, k));
        }
        let kind = if k == "is_native" { Kind::Bool } else { Kind::StrOrNull };
        *v = typed(ctx, parser, &[Seg::Key(k.as_str())], v, kind)?;
    }
    // PutConnectorAction.Request.validate.
    let index = body.get("index_name").and_then(Value::as_str);
    let mut errors: Vec<String> = index.and_then(invalid_index_name).into_iter().collect();
    if body.get("is_native").and_then(Value::as_bool) == Some(true)
        && let Some(i) = index
        && !i.starts_with("content-")
    {
        errors.push(format!("Index [{i}] is invalid. Index attached to an Elastic-managed connector must start with the prefix: [content-]"));
    }
    if !errors.is_empty() {
        return Err(EsError::action_request_validation(&errors));
    }
    let now = ctx.now_ms;
    let created = ctx
        .cluster
        .update(move |st| {
            check_index_name(st, &id, body.get("index_name"))?;
            let c = new_connector(&id, &body);
            store::put(st, CONNECTORS, &id, c, now)
        })
        .await?;
    Ok((created, String::new()))
}

pub async fn put(ctx: &Ctx<'_>) -> EsResult<Reply> {
    // Without an id (`PUT _connector`) the connector gets a generated one, like a post.
    let id = ctx.path_param("connector_id").filter(|i| !i.is_empty()).map_or_else(crate::meta::names::time_based_id, str::to_string);
    let (created, _) = create(ctx, id.clone()).await?;
    Ok(Reply::Body(if created { 201 } else { 200 }, obj! { "id" => id, "result" => if created { "created" } else { "updated" } }))
}

pub async fn post(ctx: &Ctx<'_>) -> EsResult<Reply> {
    let id = crate::meta::names::time_based_id();
    create(ctx, id.clone()).await?;
    Ok(Reply::Body(201, obj! { "id" => id, "result" => "created" }))
}

fn live(st: &crate::meta::ClusterState, id: &str, include_deleted: bool) -> EsResult<Value> {
    match st.metadata.xpack.get(CONNECTORS, id) {
        Some(c) if include_deleted || c.get("deleted") != Some(&Value::Bool(true)) => Ok(c.clone()),
        _ => Err(not_found(id)),
    }
}

pub fn get(ctx: &Ctx) -> EsResult<Reply> {
    let id = ctx.path_param("connector_id").unwrap_or_default();
    let include_deleted = ctx.bool_param("include_deleted", false)?;
    Reply::ok(live(&ctx.state(), id, include_deleted)?)
}

pub fn list(ctx: &Ctx) -> EsResult<Reply> {
    let st = ctx.state();
    let from = ctx.int_param("from", 0)?;
    let size = ctx.int_param("size", 100)?;
    if from < 0 {
        return Err(EsError::action_request_validation(&["[from] parameter cannot be negative, found [".to_string() + &from.to_string() + "]"]));
    }
    if size < 0 {
        return Err(EsError::action_request_validation(&["[size] parameter cannot be negative, found [".to_string() + &size.to_string() + "]"]));
    }
    let include_deleted = ctx.bool_param("include_deleted", false)?;
    let filter = |name: &str| ctx.list_param(name).unwrap_or_default();
    let (indices, names, types) = (filter("index_name"), filter("connector_name"), filter("service_type"));
    let invalid: Vec<String> = indices.iter().filter_map(|i| invalid_index_name(i)).collect();
    if !invalid.is_empty() {
        return Err(EsError::action_request_validation(&invalid));
    }
    let query = ctx.query("query").map(str::to_lowercase);
    let mut all: Vec<&Value> = st
        .metadata
        .xpack
        .iter(CONNECTORS)
        .map(|(_, c)| c)
        .filter(|c| include_deleted || c.get("deleted") != Some(&Value::Bool(true)))
        .filter(|c| indices.is_empty() || c.get("index_name").and_then(Value::as_str).is_some_and(|i| indices.iter().any(|p| crate::xcontent::filter::glob_match(p, i))))
        .filter(|c| names.is_empty() || c.get("name").and_then(Value::as_str).is_some_and(|n| names.iter().any(|p| p == n)))
        .filter(|c| types.is_empty() || c.get("service_type").and_then(Value::as_str).is_some_and(|n| types.iter().any(|p| p == n)))
        .filter(|c| {
            query.as_ref().is_none_or(|q| {
                ["name", "description", "index_name"].iter().any(|f| c.get(f).and_then(Value::as_str).is_some_and(|s| s.to_lowercase().contains(q.as_str())))
            })
        })
        .collect();
    all.sort_by(|a, b| {
        let k = |c: &Value| c.get("index_name").and_then(Value::as_str).unwrap_or("").to_string();
        k(a).cmp(&k(b))
    });
    let count = all.len();
    let results: Vec<Value> = all.into_iter().skip(from as usize).take(size as usize).cloned().collect();
    Reply::ok(obj! { "count" => count, "results" => Value::Arr(results) })
}

pub async fn delete(ctx: &Ctx<'_>) -> EsResult<Reply> {
    let id = ctx.path_param("connector_id").unwrap_or_default().to_string();
    let hard = ctx.bool_param("hard", false)?;
    let delete_jobs = ctx.bool_param("delete_sync_jobs", false)?;
    let now = ctx.now_ms;
    ctx.cluster
        .update(move |st| {
            let c = live(st, &id, hard)?;
            if hard {
                store::remove(st, CONNECTORS, &id);
            } else {
                let mut c = c;
                if let Value::Obj(m) = &mut c {
                    m.insert("deleted".into(), true.into());
                }
                store::put(st, CONNECTORS, &id, c, now)?;
            }
            if delete_jobs {
                let jobs: Vec<String> = st.metadata.xpack.iter(SYNC_JOBS).filter(|(_, j)| j.pointer(&["connector", "id"]).and_then(Value::as_str) == Some(&id)).map(|(k, _)| k.clone()).collect();
                for j in jobs {
                    store::remove(st, SYNC_JOBS, &j);
                }
            }
            Ok(())
        })
        .await?;
    Reply::acknowledged()
}

/// Applies `f` to a live connector and stores it.
async fn modify(ctx: &Ctx<'_>, f: impl FnOnce(&crate::meta::ClusterState, &mut Map) -> EsResult<()> + Send) -> EsResult<Reply> {
    let id = ctx.path_param("connector_id").unwrap_or_default().to_string();
    let now = ctx.now_ms;
    ctx.cluster
        .update(move |st| {
            let c = live(st, &id, false)?;
            let Value::Obj(mut m) = c else { return Err(not_found(&id)) };
            f(st, &mut m)?;
            store::put(st, CONNECTORS, &id, hash_ordered(Value::Obj(m)), now).map(|_| ())
        })
        .await?;
    updated()
}

fn required<'a>(body: &'a Map, key: &str) -> EsResult<&'a Value> {
    body.get(key).ok_or_else(|| EsError::illegal_argument(format!("Required [{key}]")))
}

fn body_fields(ctx: &Ctx, parser: &str, allowed: &[&str]) -> EsResult<Map> {
    let body = ctx.body_map()?.unwrap_or_default();
    for k in body.keys() {
        if !allowed.contains(&k.as_str()) {
            return Err(unknown_field(ctx, parser, k));
        }
    }
    Ok(body)
}

pub async fn check_in(ctx: &Ctx<'_>) -> EsResult<Reply> {
    validate_id(ctx, "connector_id", EMPTY_ID)?;
    modify(ctx, |_, m| {
        m.insert("last_seen".into(), now_nanos_string().into());
        Ok(())
    })
    .await
}

// --- request parsing ---------------------------------------------------------------------------
//
// Update requests parse like Elasticsearch's ObjectParsers: a value of the wrong JSON type fails
// at the value ("[parser] field doesn't support values of type: TOKEN"), a nested object's
// failure is wrapped in "failed to parse field [...]" of each enclosing parser, and the request
// is validated (empty connector id, both fields null, ...) only after it parsed.

/// What an ObjectParser field accepts.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Kind {
    Str,
    StrOrNull,
    Bool,
    Long,
    Obj,
    ObjOrNull,
}

/// The XContent token Elasticsearch names for a JSON value.
fn token(v: &Value) -> &'static str {
    match v {
        Value::Null => "VALUE_NULL",
        Value::Bool(_) => "VALUE_BOOLEAN",
        Value::Num(_) => "VALUE_NUMBER",
        Value::Str(_) => "VALUE_STRING",
        Value::Arr(_) => "START_ARRAY",
        Value::Obj(_) => "START_OBJECT",
    }
}

/// Where the value at `path` starts in the request body.
fn at(ctx: &Ctx, path: &[Seg<'_>]) -> (usize, usize) {
    ctx.raw_json().and_then(|s| locate(s, path, Target::Value)).unwrap_or((1, 1))
}

/// Where the object at `path` ends (Jackson's location once an object's last field arrived).
fn end_of(ctx: &Ctx, path: &[Seg<'_>]) -> (usize, usize) {
    ctx.raw_json().and_then(|s| crate::json::locate::value_end(s, path)).unwrap_or((1, 1))
}

fn last_key<'a>(path: &[Seg<'a>]) -> &'a str {
    path.iter().rev().find_map(|s| if let Seg::Key(k) = s { Some(*k) } else { None }).unwrap_or("")
}

/// `[parser] failed to parse field [key]` at the value, caused by `cause`.
fn failed_field(ctx: &Ctx, parser: &str, path: &[Seg<'_>], cause: EsError) -> EsError {
    let (l, c) = at(ctx, path);
    EsError::x_content_parse(l, c, format!("[{parser}] failed to parse field [{}]", last_key(path))).caused_by(cause)
}

/// Checks the value at `path` against `kind`; booleans and numbers given as strings become
/// their values, as ObjectParser's `booleanValue()` / `longValue()` read them.
fn typed(ctx: &Ctx, parser: &str, path: &[Seg<'_>], v: &Value, kind: Kind) -> EsResult<Value> {
    let ok = match (kind, v) {
        (Kind::Str, Value::Str(_)) | (Kind::StrOrNull, Value::Str(_) | Value::Null) => true,
        (Kind::Obj, Value::Obj(_)) | (Kind::ObjOrNull, Value::Obj(_) | Value::Null) => true,
        (Kind::Bool, Value::Bool(_)) | (Kind::Long, Value::Num(_)) => true,
        (Kind::Bool, Value::Str(s)) => {
            return match s.as_str() {
                "true" => Ok(Value::Bool(true)),
                "false" => Ok(Value::Bool(false)),
                _ => Err(failed_field(ctx, parser, path, EsError::illegal_argument(format!("Failed to parse value [{s}] as only [true] or [false] are allowed.")))),
            };
        }
        (Kind::Long, Value::Str(s)) => {
            return s
                .trim()
                .parse::<i64>()
                .map(Value::from)
                .map_err(|_| failed_field(ctx, parser, path, EsError::new(400, "number_format_exception", format!("For input string: \"{s}\""))));
        }
        _ => false,
    };
    if ok {
        return Ok(v.clone());
    }
    let (l, c) = at(ctx, path);
    Err(EsError::x_content_parse(l, c, format!("[{parser}] {} doesn't support values of type: {}", last_key(path), token(v))))
}

/// `ConnectorUtils.parseInstant`: an ISO-8601 or epoch-millis string, printed back the way
/// `XContentBuilder` prints an `Instant` (at least milliseconds, up to nanoseconds).
fn instant(ctx: &Ctx, parser: &str, path: &[Seg<'_>], v: &Value, nullable: bool) -> EsResult<Value> {
    if nullable && v.is_null() {
        return Ok(Value::Null);
    }
    let Value::Str(s) = v else {
        let (l, c) = at(ctx, path);
        return Err(EsError::x_content_parse(l, c, format!("[{parser}] {} doesn't support values of type: {}", last_key(path), token(v))));
    };
    let f = crate::time::DateFormatter::new(crate::time::DateFormatter::DEFAULT).map_err(EsError::illegal_argument)?;
    match f.parse(s, None, false) {
        Ok(i) => Ok(Value::from(instant_string(i))),
        Err(_) => Err(failed_field(
            ctx,
            parser,
            path,
            EsError::illegal_argument(format!("failed to parse date field [{s}] with format [strict_date_optional_time||epoch_millis]"))
                .caused_by(EsError::new(400, "date_time_parse_exception", "Failed to parse with all enclosed parsers")),
        )),
    }
}

/// `strict_date_optional_time_nanos` printing: the fraction has 3 to 9 digits.
fn instant_string(i: crate::time::Instant) -> String {
    let mut out = crate::time::java_instant_string(crate::time::Instant { secs: i.secs, nanos: 0 }.millis());
    out.pop();
    let digits = format!("{:09}", i.nanos);
    let keep = digits.trim_end_matches('0').len().max(3);
    out.push('.');
    out.push_str(&digits[..keep]);
    out.push('Z');
    out
}

/// A ConstructingObjectParser's error for missing required fields (names sorted).
fn missing(names: &mut [&str]) -> EsError {
    names.sort_unstable();
    EsError::illegal_argument(format!("Required [{}]", names.join(", ")))
}

/// ES|`Strings.isNullOrEmpty(connectorId)` of every connector request's `validate()`.
fn validate_id(ctx: &Ctx, param: &str, message: &str) -> EsResult<()> {
    if ctx.path_param(param).is_none_or(str::is_empty) {
        return Err(EsError::action_request_validation(&[message.to_string()]));
    }
    Ok(())
}

const EMPTY_ID: &str = "[connector_id] cannot be [null] or [\"\"].";

/// `validateIndexOrAliasName` as connector requests report it.
fn invalid_index_name(name: &str) -> Option<String> {
    crate::meta::names::name_problem(name).map(|p| format!("[{name}] org.elasticsearch.indices.InvalidIndexNameException: Invalid index name [{name}], {p}"))
}

/// A stored document after Elasticsearch's update API merged a partial doc into it: every
/// object becomes a `HashMap` (its keys in Java's bucket order).
fn hash_ordered(v: Value) -> Value {
    match v {
        Value::Obj(m) => {
            let m: Map = m.into_iter().map(|(k, v)| (k, hash_ordered(v))).collect();
            Value::Obj(crate::json::java_hash_map(m, 16))
        }
        Value::Arr(a) => Value::Arr(a.into_iter().map(hash_ordered).collect()),
        other => other,
    }
}

/// Parses one `FilteringRule` (lenient: unknown fields are ignored).
fn filtering_rule(ctx: &Ctx, path: &[Seg<'_>], v: &Value, now: &str) -> EsResult<Map> {
    const P: &str = "connector_filtering_rule";
    let Value::Obj(r) = v else { return Err(typed(ctx, P, path, v, Kind::Obj).err().unwrap_or_else(|| EsError::illegal_argument("[rules] must be objects"))) };
    let sub = |k: &'static str| {
        let mut p = path.to_vec();
        p.push(Seg::Key(k));
        p
    };
    let mut req: Vec<&str> = ["field", "id", "order", "policy", "rule", "value"].into_iter().filter(|k| !r.contains_key(*k)).collect();
    let mut out = Map::new();
    let created = match r.get("created_at") {
        Some(x) => instant(ctx, P, &sub("created_at"), x, false)?,
        None => Value::from(now),
    };
    out.insert("created_at".into(), created);
    for k in ["field", "id"] {
        if let Some(x) = r.get(k) {
            out.insert(k.into(), typed(ctx, P, &sub(k), x, Kind::Str)?);
        }
    }
    if let Some(x) = r.get("order") {
        out.insert("order".into(), typed(ctx, P, &sub("order"), x, Kind::Long)?);
    }
    for (k, allowed, class) in [("policy", &["exclude", "include"][..], "FilteringPolicy"), ("rule", &["contains", "ends_with", "equals", "regex", ">", "<", "starts_with"][..], "FilteringRuleCondition")] {
        if let Some(x) = r.get(k) {
            let s = typed(ctx, P, &sub(k), x, Kind::Str)?.to_es_string();
            if !allowed.contains(&s.as_str()) {
                return Err(failed_field(ctx, P, &sub(k), EsError::illegal_argument(format!("Unknown {class} [{s}]."))));
            }
            out.insert(k.into(), s.into());
        }
    }
    let updated = match r.get("updated_at") {
        Some(x) => instant(ctx, P, &sub("updated_at"), x, false)?,
        None => Value::from(now),
    };
    out.insert("updated_at".into(), updated);
    if let Some(x) = r.get("value") {
        out.insert("value".into(), typed(ctx, P, &sub("value"), x, Kind::Str)?);
    }
    if !req.is_empty() {
        let (l, c) = end_of(ctx, path);
        return Err(EsError::x_content_parse(l, c, format!("Failed to build [{P}] after last required field arrived")).caused_by(missing(&mut req)));
    }
    Ok(out)
}

fn is_default_rule(r: &Map) -> bool {
    let s = |k: &str| r.get(k).map(Value::to_es_string).unwrap_or_default();
    s("field") == "_" && s("id") == "DEFAULT" && s("policy") == "include" && s("rule") == "regex" && s("value") == ".*"
}

/// `ConnectorFiltering.sortFilteringRulesByOrder`: by order, the default rule last with the next
/// order (its creation time kept).
fn sort_rules(rules: Vec<Map>, now: &str) -> Vec<Value> {
    let default_created = rules.iter().find(|r| is_default_rule(r)).and_then(|r| r.get("created_at").cloned());
    let mut sorted: Vec<Map> = rules.into_iter().filter(|r| !is_default_rule(r)).collect();
    sorted.sort_by_key(|r| r.get("order").and_then(Value::as_i64).unwrap_or(0));
    let order = sorted.last().and_then(|r| r.get("order").and_then(Value::as_i64)).map_or(0, |o| o + 1);
    let created = default_created.unwrap_or_else(|| Value::from(now));
    let default = obj! {
        "created_at" => created, "field" => "_", "id" => "DEFAULT", "order" => order, "policy" => "include", "rule" => "regex", "updated_at" => now, "value" => ".*",
    };
    let mut out: Vec<Value> = sorted.into_iter().map(Value::Obj).collect();
    out.push(default);
    out
}

/// Parses a `FilteringAdvancedSnippet`.
fn advanced_snippet(ctx: &Ctx, path: &[Seg<'_>], v: &Value, now: &str) -> EsResult<Value> {
    const P: &str = "connector_filtering_advanced_snippet";
    let Value::Obj(s) = v else { return typed(ctx, P, path, v, Kind::Obj) };
    let sub = |k: &'static str| {
        let mut p = path.to_vec();
        p.push(Seg::Key(k));
        p
    };
    let mut out = Map::new();
    for k in ["created_at", "updated_at"] {
        let t = match s.get(k) {
            Some(x) => instant(ctx, P, &sub(k), x, false)?,
            None => Value::from(now),
        };
        out.insert(k.into(), t);
    }
    match s.get("value") {
        Some(x @ (Value::Obj(_) | Value::Arr(_))) => {
            out.insert("value".into(), x.clone());
        }
        Some(x) => {
            let (l, c) = at(ctx, &sub("value"));
            return Err(failed_field(
                ctx,
                P,
                &sub("value"),
                EsError::x_content_parse_bare(format!("Unsupported token [{}]. Expected an array or an object.", token(x))),
            )
            .caused_by(EsError::x_content_parse(l, c, "")));
        }
        None => {
            let (l, c) = end_of(ctx, path);
            return Err(EsError::x_content_parse(l, c, format!("Failed to build [{P}] after last required field arrived")).caused_by(EsError::illegal_argument("Required [value]")));
        }
    }
    Ok(Value::Obj(out))
}

/// Parses a `FilteringValidationInfo` (`errors` and `state` required).
fn validation_info(ctx: &Ctx, path: &[Seg<'_>], v: &Value) -> EsResult<Value> {
    const P: &str = "filtering_validation_info";
    let Value::Obj(m) = v else { return typed(ctx, P, path, v, Kind::Obj) };
    let mut req: Vec<&str> = ["errors", "state"].into_iter().filter(|k| !m.contains_key(*k)).collect();
    if !req.is_empty() {
        let (l, c) = end_of(ctx, path);
        return Err(EsError::x_content_parse(l, c, format!("Failed to build [{P}] after last required field arrived")).caused_by(missing(&mut req)));
    }
    let state = m.get("state").map(Value::to_es_string).unwrap_or_default();
    if !["edited", "invalid", "valid"].contains(&state.as_str()) {
        let mut p = path.to_vec();
        p.push(Seg::Key("state"));
        return Err(failed_field(ctx, P, &p, EsError::illegal_argument(format!("Unknown FilteringValidationState [{state}]."))));
    }
    let mut errors = Vec::new();
    for (i, e) in m.get("errors").and_then(Value::as_array).map(Vec::as_slice).unwrap_or_default().iter().enumerate() {
        let mut ep = path.to_vec();
        ep.push(Seg::Key("errors"));
        ep.push(Seg::Index(i));
        let Value::Obj(em) = e else { return Err(typed(ctx, "filtering_validation", &ep, e, Kind::Obj).err().unwrap_or_else(|| EsError::illegal_argument("bad error"))) };
        let mut req: Vec<&str> = ["ids", "messages"].into_iter().filter(|k| !em.contains_key(*k)).collect();
        if !req.is_empty() {
            let (l, c) = end_of(ctx, &ep);
            return Err(EsError::x_content_parse(l, c, "Failed to build [filtering_validation] after last required field arrived").caused_by(missing(&mut req)));
        }
        errors.push(obj! { "ids" => em.get("ids").cloned().unwrap_or(Value::Null), "messages" => em.get("messages").cloned().unwrap_or(Value::Null) });
    }
    Ok(obj! { "errors" => Value::Arr(errors), "state" => state })
}

/// `ConnectorScheduling.ScheduleConfig`: `enabled` and a valid cron `interval`.
fn schedule_config(ctx: &Ctx, path: &[Seg<'_>], v: &Value) -> EsResult<Value> {
    const P: &str = "schedule_config";
    let Value::Obj(m) = v else { return typed(ctx, P, path, v, Kind::Obj) };
    let sub = |k: &'static str| {
        let mut p = path.to_vec();
        p.push(Seg::Key(k));
        p
    };
    let enabled = m.get("enabled").map(|x| typed(ctx, P, &sub("enabled"), x, Kind::Bool)).transpose()?;
    let interval = m.get("interval").map(|x| typed(ctx, P, &sub("interval"), x, Kind::Str)).transpose()?;
    let mut req: Vec<&str> = [("enabled", enabled.is_none()), ("interval", interval.is_none())].into_iter().filter(|(_, m)| *m).map(|(k, _)| k).collect();
    if !req.is_empty() {
        let (l, c) = end_of(ctx, path);
        return Err(EsError::x_content_parse(l, c, format!("Failed to build [{P}] after last required field arrived")).caused_by(missing(&mut req)));
    }
    let interval = interval.map(|i| i.to_es_string()).unwrap_or_default();
    if let Some(e) = crate::xpack::watcher::cron_problem(&interval) {
        let (l, c) = at(ctx, &sub("interval"));
        let built = EsError::x_content_parse_bare(format!("Failed to build [{P}] after last required field arrived")).caused_by(
            EsError::illegal_argument(format!("invalid cron expression [{interval}]"))
                .caused_by(EsError::illegal_argument(format!("illegal cron expression format [java.lang.IllegalArgumentException: {e}]"))),
        );
        return Err(EsError::x_content_parse(l, c, format!("[{P}] failed to parse field [interval]")).caused_by(built));
    }
    Ok(obj! { "enabled" => enabled.unwrap_or(Value::Bool(false)), "interval" => interval })
}

pub async fn update(ctx: &Ctx<'_>) -> EsResult<Reply> {
    use crate::routes::Api;
    let api = ctx.api;
    let now = now_nanos_string();
    // Parse the body into the update it makes: `(field, value)` pairs merged into the
    // connector, or a closure needing the stored connector.
    let mut set: Vec<(String, Value)> = Vec::new();
    let body = match api {
        Api::ConnectorUpdateActiveFiltering => Map::new(),
        Api::ConnectorUpdateName => body_fields(ctx, "connector_update_name_request", &["name", "description"])?,
        Api::ConnectorUpdateFiltering => body_fields(ctx, "connector_update_filtering_request", &["filtering", "advanced_snippet", "rules"])?,
        Api::ConnectorUpdateFilteringValidation => body_fields(ctx, "connector_update_filtering_validation_request", &["validation"])?,
        Api::ConnectorUpdateError => body_fields(ctx, "connector_update_error_request", &["error"])?,
        Api::ConnectorUpdateConfiguration => body_fields(ctx, "connector_update_configuration_request", &["configuration", "values"])?,
        Api::ConnectorUpdateIndexName => body_fields(ctx, "connector_update_index_name_request", &["index_name"])?,
        Api::ConnectorUpdateNative => body_fields(ctx, "connector_update_native_request", &["is_native"])?,
        Api::ConnectorUpdatePipeline => body_fields(ctx, "connector_update_pipeline_request", &["pipeline"])?,
        Api::ConnectorUpdateScheduling => body_fields(ctx, "connector_update_scheduling_request", &["scheduling"])?,
        Api::ConnectorUpdateStatus => body_fields(ctx, "connector_update_status_request", &["status"])?,
        Api::ConnectorUpdateServiceType => body_fields(ctx, "connector_update_service_type_request", &["service_type"])?,
        Api::ConnectorUpdateApiKeyId => body_fields(ctx, "connector_update_api_key_id_request", &["api_key_id", "api_key_secret_id"])?,
        Api::ConnectorUpdateFeatures => body_fields(ctx, "connector_update_features_request", &["features"])?,
        Api::ConnectorLastSync => body_fields(ctx, "connector_update_last_sync_stats_request", LAST_SYNC_FIELDS)?,
        _ => ctx.body_map()?.unwrap_or_default(),
    };
    let field = |parser: &str, key: &'static str, kind: Kind| -> EsResult<Option<Value>> {
        body.get(key).map(|v| typed(ctx, parser, &[Seg::Key(key)], v, kind)).transpose()
    };
    let required_field = |parser: &str, key: &'static str, kind: Kind| -> EsResult<Value> { field(parser, key, kind)?.ok_or_else(|| missing(&mut [key])) };
    // Draft filtering updates replace these parts of the first filtering entry's draft.
    let mut draft_rules: Option<Vec<Map>> = None;
    let mut draft_snippet: Option<Value> = None;
    let mut validation: Option<Value> = None;
    let mut schedules: Vec<(&str, Value)> = Vec::new();
    let mut index_name: Option<Value> = None;
    let mut native: Option<bool> = None;
    match api {
        Api::ConnectorUpdateName => {
            const P: &str = "connector_update_name_request";
            let (name, description) = (field(P, "name", Kind::StrOrNull)?, field(P, "description", Kind::StrOrNull)?);
            validate_id(ctx, "connector_id", EMPTY_ID)?;
            if name.is_none() && description.is_none() {
                return Err(EsError::action_request_validation(&["[name] and [description] cannot both be [null]. Please provide a value for at least one of them.".into()]));
            }
            set.extend(name.map(|v| ("name".to_string(), v)));
            set.extend(description.map(|v| ("description".to_string(), v)));
        }
        Api::ConnectorUpdateError => {
            let e = required_field("connector_update_error_request", "error", Kind::StrOrNull)?;
            validate_id(ctx, "connector_id", EMPTY_ID)?;
            // Clearing the error marks the connector connected again.
            set.push(("status".into(), (if e.is_null() { "connected" } else { "error" }).into()));
            set.push(("error".into(), e));
        }
        Api::ConnectorUpdateServiceType => {
            let t = required_field("connector_update_service_type_request", "service_type", Kind::Str)?;
            validate_id(ctx, "connector_id", EMPTY_ID)?;
            if t.as_str() == Some("") {
                return Err(EsError::action_request_validation(&["[service_type] cannot be [null] or [\"\"].".into()]));
            }
            set.push(("service_type".into(), t));
        }
        Api::ConnectorUpdateIndexName => {
            let idx = required_field("connector_update_index_name_request", "index_name", Kind::StrOrNull)?;
            validate_id(ctx, "connector_id", EMPTY_ID)?;
            if let Some(p) = idx.as_str().and_then(invalid_index_name) {
                return Err(EsError::action_request_validation(&[p]));
            }
            index_name = Some(idx);
        }
        Api::ConnectorUpdateNative => {
            let n = required_field("connector_update_native_request", "is_native", Kind::Bool)?;
            validate_id(ctx, "connector_id", EMPTY_ID)?;
            native = n.as_bool();
        }
        Api::ConnectorUpdateApiKeyId => {
            const P: &str = "connector_update_api_key_id_request";
            let (id, secret) = (field(P, "api_key_id", Kind::StrOrNull)?, field(P, "api_key_secret_id", Kind::StrOrNull)?);
            validate_id(ctx, "connector_id", EMPTY_ID)?;
            if id.as_ref().is_none_or(Value::is_null) && secret.as_ref().is_none_or(Value::is_null) {
                return Err(EsError::action_request_validation(&["[api_key_id] and [api_key_secret_id] cannot both be [null]. Please provide a value for at least one of them.".into()]));
            }
            set.extend(id.map(|v| ("api_key_id".to_string(), v)));
            set.extend(secret.map(|v| ("api_key_secret_id".to_string(), v)));
        }
        Api::ConnectorUpdatePipeline => {
            const P: &str = "connector_update_pipeline_request";
            let path = [Seg::Key("pipeline")];
            let v = required_field(P, "pipeline", Kind::Obj)?;
            let m = v.as_object().cloned().unwrap_or_default();
            let mut out = Map::new();
            let mut req = Vec::new();
            for (k, kind) in [("extract_binary_content", Kind::Bool), ("name", Kind::Str), ("reduce_whitespace", Kind::Bool), ("run_ml_inference", Kind::Bool)] {
                match m.get(k) {
                    Some(x) => {
                        let fp = [Seg::Key("pipeline"), Seg::Key(k)];
                        let x = typed(ctx, "ingest_pipeline_params", &fp, x, kind).map_err(|e| failed_field(ctx, P, &path, e))?;
                        out.insert(k.into(), x);
                    }
                    None => req.push(k),
                }
            }
            if !req.is_empty() {
                let (l, c) = end_of(ctx, &path);
                return Err(EsError::x_content_parse(l, c, format!("[{P}] failed to parse field [pipeline]")).caused_by(missing(&mut req)));
            }
            validate_id(ctx, "connector_id", EMPTY_ID)?;
            set.push(("pipeline".into(), Value::Obj(out)));
        }
        Api::ConnectorUpdateScheduling => {
            const P: &str = "connector_update_scheduling_request";
            let v = required_field(P, "scheduling", Kind::Obj)?;
            let m = v.as_object().cloned().unwrap_or_default();
            for (k, _) in &m {
                if !["access_control", "full", "incremental"].contains(&k.as_str()) {
                    let (l, c) = ctx.raw_json().and_then(|s| locate(s, &[Seg::Key("scheduling"), Seg::Key(k)], Target::Name)).unwrap_or((1, 1));
                    return Err(failed_field(ctx, P, &[Seg::Key("scheduling")], EsError::x_content_parse(l, c, format!("[connector_scheduling] unknown field [{k}]"))));
                }
            }
            for k in ["access_control", "full", "incremental"] {
                if let Some(x) = m.get(k) {
                    let s = schedule_config(ctx, &[Seg::Key("scheduling"), Seg::Key(k)], x).map_err(|e| {
                        let (l, c) = at(ctx, &[Seg::Key("scheduling"), Seg::Key(k)]);
                        failed_field(ctx, P, &[Seg::Key("scheduling")], EsError::x_content_parse(l, c, format!("[connector_scheduling] failed to parse field [{k}]")).caused_by(e))
                    })?;
                    schedules.push((k, s));
                }
            }
            validate_id(ctx, "connector_id", EMPTY_ID)?;
            if schedules.is_empty() {
                return Err(EsError::action_request_validation(&["[scheduling] object needs to define at least one schedule type: [full | incremental | access_control]".into()]));
            }
        }
        Api::ConnectorUpdateFiltering => {
            const P: &str = "connector_update_filtering_request";
            if let Some(f) = body.get("filtering") {
                let Value::Arr(entries) = f else { return Err(typed(ctx, P, &[Seg::Key("filtering")], f, Kind::Obj).err().unwrap_or_else(|| EsError::illegal_argument("[filtering] must be an array"))) };
                let mut out = Vec::new();
                for (i, e) in entries.iter().enumerate() {
                    let ep = [Seg::Key("filtering"), Seg::Index(i)];
                    let Value::Obj(em) = e else { return Err(typed(ctx, "connector_filtering", &ep, e, Kind::Obj).err().unwrap_or_else(|| EsError::illegal_argument("bad filtering"))) };
                    let mut entry = Map::new();
                    for part in ["active", "draft"] {
                        let Some(Value::Obj(rules)) = em.get(part) else { continue };
                        let pp = [Seg::Key("filtering"), Seg::Index(i), Seg::Key(part)];
                        let mut r = Map::new();
                        if let Some(s) = rules.get("advanced_snippet") {
                            let sp = [Seg::Key("filtering"), Seg::Index(i), Seg::Key(part), Seg::Key("advanced_snippet")];
                            r.insert("advanced_snippet".into(), advanced_snippet(ctx, &sp, s, &now)?);
                        }
                        let mut list = Vec::new();
                        for (j, rule) in rules.get("rules").and_then(Value::as_array).map(Vec::as_slice).unwrap_or_default().iter().enumerate() {
                            let rp = [Seg::Key("filtering"), Seg::Index(i), Seg::Key(part), Seg::Key("rules"), Seg::Index(j)];
                            list.push(Value::Obj(filtering_rule(ctx, &rp, rule, &now)?));
                        }
                        r.insert("rules".into(), Value::Arr(list));
                        if let Some(v) = rules.get("validation") {
                            let vp = [Seg::Key("filtering"), Seg::Index(i), Seg::Key(part), Seg::Key("validation")];
                            r.insert("validation".into(), validation_info(ctx, &vp, v)?);
                        }
                        let _ = pp;
                        entry.insert(part.into(), Value::Obj(r));
                    }
                    entry.insert("domain".into(), em.get("domain").cloned().unwrap_or_else(|| Value::from("DEFAULT")));
                    out.push(Value::Obj(entry));
                }
                validate_id(ctx, "connector_id", EMPTY_ID)?;
                set.push(("filtering".into(), Value::Arr(out)));
            } else {
                if let Some(s) = body.get("advanced_snippet") {
                    draft_snippet = Some(advanced_snippet(ctx, &[Seg::Key("advanced_snippet")], s, &now).map_err(|e| failed_field(ctx, P, &[Seg::Key("advanced_snippet")], e))?);
                }
                if let Some(r) = body.get("rules") {
                    let Value::Arr(list) = r else { return Err(typed(ctx, P, &[Seg::Key("rules")], r, Kind::Obj).err().unwrap_or_else(|| EsError::illegal_argument("[rules] must be an array"))) };
                    let mut rules = Vec::new();
                    for (j, rule) in list.iter().enumerate() {
                        rules.push(filtering_rule(ctx, &[Seg::Key("rules"), Seg::Index(j)], rule, &now).map_err(|e| failed_field(ctx, P, &[Seg::Key("rules")], e))?);
                    }
                    draft_rules = Some(rules);
                }
                validate_id(ctx, "connector_id", EMPTY_ID)?;
                if draft_rules.is_none() && draft_snippet.is_none() {
                    return Err(EsError::action_request_validation(&["[advanced_snippet] and [rules] cannot be both [null]. Please provide a value for at least one of them.".into()]));
                }
            }
        }
        Api::ConnectorUpdateFilteringValidation => {
            const P: &str = "connector_update_filtering_validation_request";
            let v = required_field(P, "validation", Kind::Obj)?;
            validation = Some(validation_info(ctx, &[Seg::Key("validation")], &v).map_err(|e| failed_field(ctx, P, &[Seg::Key("validation")], e))?);
            validate_id(ctx, "connector_id", EMPTY_ID)?;
        }
        Api::ConnectorUpdateStatus => {
            const P: &str = "connector_update_status_request";
            let v = required_field(P, "status", Kind::StrOrNull)?;
            if !v.is_null() && !CONNECTOR_STATUSES.contains(&v.to_es_string().as_str()) {
                return Err(failed_field(ctx, P, &[Seg::Key("status")], EsError::illegal_argument(format!("Unknown ConnectorStatus [{}].", v.to_es_string()))));
            }
            validate_id(ctx, "connector_id", EMPTY_ID)?;
            if v.is_null() {
                return Err(EsError::action_request_validation(&["[status] cannot be [null].".into()]));
            }
            set.push(("status".into(), v));
        }
        Api::ConnectorUpdateConfiguration => {
            const P: &str = "connector_update_configuration_request";
            let configuration = match body.get("configuration") {
                Some(c @ Value::Obj(_)) => Some(normalize_configuration(ctx, c)?),
                Some(Value::Null) | None => None,
                Some(c) => return Err(typed(ctx, P, &[Seg::Key("configuration")], c, Kind::Obj).err().unwrap_or_else(|| EsError::illegal_argument("bad configuration"))),
            };
            let values = field(P, "values", Kind::ObjOrNull)?.filter(|v| !v.is_null());
            validate_id(ctx, "connector_id", EMPTY_ID)?;
            match (&configuration, &values) {
                (None, None) => return Err(EsError::action_request_validation(&["[configuration] and [values] cannot both be null.".into()])),
                (Some(_), Some(_)) => return Err(EsError::action_request_validation(&["[configuration] and [values] cannot both be provided in the same request.".into()])),
                _ => {}
            }
        }
        Api::ConnectorUpdateFeatures => {
            required_field("connector_update_features_request", "features", Kind::Obj)?;
            validate_id(ctx, "connector_id", EMPTY_ID)?;
        }
        Api::ConnectorLastSync => {
            const P: &str = "connector_update_last_sync_stats_request";
            for (k, v) in &body {
                let path = [Seg::Key(k.as_str())];
                let v = match k.as_str() {
                    "last_access_control_sync_error" | "last_sync_error" => typed(ctx, P, &path, v, Kind::StrOrNull)?,
                    "last_deleted_document_count" | "last_indexed_document_count" => typed(ctx, P, &path, v, Kind::Long)?,
                    "sync_cursor" => typed(ctx, P, &path, v, Kind::ObjOrNull)?,
                    "last_access_control_sync_status" | "last_sync_status" => {
                        let s = typed(ctx, P, &path, v, Kind::StrOrNull)?;
                        if let Value::Str(st) = &s
                            && !SYNC_STATUSES.contains(&st.as_str())
                        {
                            return Err(failed_field(ctx, P, &path, EsError::illegal_argument(format!("Unknown ConnectorSyncStatus [{st}]."))));
                        }
                        s
                    }
                    _ => instant(ctx, P, &path, v, true)?,
                };
                set.push((k.clone(), v));
            }
            validate_id(ctx, "connector_id", EMPTY_ID)?;
        }
        Api::ConnectorUpdateActiveFiltering => validate_id(ctx, "connector_id", EMPTY_ID)?,
        _ => {}
    }
    let id = ctx.path_param("connector_id").unwrap_or_default().to_string();
    let now_ms = ctx.now_ms;
    let noop = ctx
        .cluster
        .update(move |st| {
            let c = live(st, &id, false)?;
            let Value::Obj(mut m) = c else { return Err(not_found(&id)) };
            let before = m.clone();
            for (k, v) in set {
                m.insert(k, v);
            }
            match api {
                Api::ConnectorUpdateIndexName => {
                    let idx = index_name.unwrap_or(Value::Null);
                    if m.get("index_name").unwrap_or(&Value::Null) == &idx {
                        return Ok(true);
                    }
                    if m.get("is_native").and_then(Value::as_bool) == Some(true)
                        && let Some(i) = idx.as_str()
                        && !i.starts_with("content-")
                    {
                        return Err(EsError::new(400, "status_exception", format!(
                            "Index attached to an Elastic-managed connector must start with the prefix: [content-]. The index name in the payload [{i}] doesn't comply with this requirement."
                        )));
                    }
                    check_index_name(st, &id, Some(&idx))?;
                    m.insert("index_name".into(), idx);
                }
                Api::ConnectorUpdateNative => {
                    let native = native.unwrap_or(false);
                    if native {
                        let index = m.get("index_name").and_then(Value::as_str).unwrap_or("");
                        if !index.starts_with("content-") {
                            return Err(EsError::new(400, "status_exception", format!(
                                "The index name [{index}] attached to the connector [{id}] must start with the required prefix: [content-] to be Elastic-managed. Please update the attached index first to comply with this requirement."
                            )));
                        }
                    }
                    m.insert("is_native".into(), native.into());
                }
                Api::ConnectorUpdateScheduling => {
                    let mut cur = m.get("scheduling").and_then(Value::as_object).cloned().unwrap_or_default();
                    for (k, s) in schedules {
                        cur.insert(k.into(), s);
                    }
                    m.insert("scheduling".into(), Value::Obj(cur));
                }
                Api::ConnectorUpdateFiltering if draft_rules.is_some() || draft_snippet.is_some() => {
                    let mut filtering = m.get("filtering").and_then(Value::as_array).cloned().unwrap_or_default();
                    if filtering.is_empty() {
                        filtering = default_filtering(&now).as_array().cloned().unwrap_or_default();
                    }
                    if let Some(Value::Obj(entry)) = filtering.first_mut() {
                        let mut draft = entry.get("draft").and_then(Value::as_object).cloned().unwrap_or_default();
                        if let Some(s) = draft_snippet {
                            draft.insert("advanced_snippet".into(), s);
                        }
                        if let Some(r) = draft_rules {
                            draft.insert("rules".into(), Value::Arr(sort_rules(r, &now)));
                        }
                        draft.insert("validation".into(), obj! { "errors" => Value::Arr(vec![]), "state" => "edited" });
                        entry.insert("draft".into(), Value::Obj(draft));
                    }
                    m.insert("filtering".into(), Value::Arr(filtering));
                }
                Api::ConnectorUpdateFilteringValidation => {
                    let mut filtering = m.get("filtering").and_then(Value::as_array).cloned().unwrap_or_default();
                    if let Some(Value::Obj(entry)) = filtering.first_mut()
                        && let Some(Value::Obj(draft)) = entry.get_mut("draft")
                    {
                        draft.insert("validation".into(), validation.unwrap_or(Value::Null));
                    }
                    m.insert("filtering".into(), Value::Arr(filtering));
                }
                Api::ConnectorUpdateActiveFiltering => {
                    let mut filtering = m.get("filtering").and_then(Value::as_array).cloned().unwrap_or_default();
                    if let Some(Value::Obj(entry)) = filtering.first_mut() {
                        let draft = entry.get("draft").cloned().unwrap_or(Value::Null);
                        let state = draft.pointer(&["validation", "state"]).and_then(Value::as_str).unwrap_or("");
                        if state != "valid" {
                            return Err(EsError::new(
                                400,
                                "status_exception",
                                format!("Filtering draft needs to be validated by the connector service before activation. Current filtering draft validation state [{state}]"),
                            ));
                        }
                        entry.insert("active".into(), draft);
                    }
                    m.insert("filtering".into(), Value::Arr(filtering));
                }
                Api::ConnectorUpdateConfiguration => match (body.get("configuration"), body.get("values")) {
                    (Some(c @ Value::Obj(_)), _) => {
                        // Parsed above; normalizing again can't fail.
                        let cfg = c.as_object().map(|e| Value::Obj(e.iter().map(|(k, v)| (k.clone(), config_defaults(v))).collect())).unwrap_or(Value::Null);
                        m.insert("configuration".into(), cfg);
                        m.insert("status".into(), "configured".into());
                    }
                    (_, Some(Value::Obj(vals))) => {
                        let mut cfg = m.get("configuration").and_then(Value::as_object).cloned().unwrap_or_default();
                        let unknown: Vec<&String> = vals.keys().filter(|k| !cfg.contains_key(k.as_str())).collect();
                        if !unknown.is_empty() {
                            let list: Vec<&str> = unknown.iter().map(|s| s.as_str()).collect();
                            return Err(EsError::new(
                                400,
                                "status_exception",
                                format!("Unknown [configuration] fields in the request payload: [{}]. Remove them from request or register their schema first.", list.join(", ")),
                            ));
                        }
                        for (k, v) in vals {
                            if let Some(Value::Obj(entry)) = cfg.get_mut(k) {
                                entry.insert("value".into(), v.clone());
                            }
                        }
                        m.insert("configuration".into(), Value::Obj(cfg));
                        m.insert("status".into(), "configured".into());
                    }
                    _ => {}
                },
                Api::ConnectorUpdateStatus => {
                    let to = m.get("status").map(Value::to_es_string).unwrap_or_default();
                    let from = before.get("status").and_then(Value::as_str).unwrap_or("created").to_string();
                    if !connector_transition(&from, &to) {
                        let msg = format!("Invalid transition attempt from [{from}] to [{to}]. Such a status transition is not supported by the Connector Protocol.");
                        return Err(EsError::new(400, "status_exception", msg.clone()).caused_by(EsError::new(400, "connector_invalid_status_transition_exception", msg)));
                    }
                }
                Api::ConnectorUpdateFeatures => {
                    let mut cur = m.get("features").and_then(Value::as_object).cloned().unwrap_or_default();
                    if let Some(Value::Obj(fm)) = body.get("features") {
                        for (k, v) in fm {
                            cur.insert(k.clone(), v.clone());
                        }
                    }
                    m.insert("features".into(), Value::Obj(cur));
                }
                _ => {}
            }
            store::put(st, CONNECTORS, &id, hash_ordered(Value::Obj(m)), now_ms).map(|_| false)
        })
        .await?;
    if noop { Reply::ok(obj! { "result" => "noop" }) } else { updated() }
}

/// The fields of `connector_update_last_sync_stats_request`.
const LAST_SYNC_FIELDS: &[&str] = &[
    "last_access_control_sync_error",
    "last_access_control_sync_scheduled_at",
    "last_access_control_sync_status",
    "last_deleted_document_count",
    "last_incremental_sync_scheduled_at",
    "last_indexed_document_count",
    "last_sync_error",
    "last_sync_scheduled_at",
    "last_sync_status",
    "last_synced",
    "sync_cursor",
];

const SYNC_STATUSES: [&str; 7] = ["canceling", "canceled", "completed", "error", "in_progress", "pending", "suspended"];

/// A configuration entry with ConnectorConfiguration's defaults filled in.
fn config_defaults(e: &Value) -> Value {
    let mut n = e.as_object().cloned().unwrap_or_default();
    n.entry("default_value".to_string()).or_insert(Value::Null);
    n.entry("required".to_string()).or_insert(Value::Bool(false));
    n.entry("sensitive".to_string()).or_insert(Value::Bool(false));
    Value::Obj(n)
}

/// Checks `ConnectorConfiguration` entries: `label` is required, `type`, `display` and each
/// validation's `type` must be known values.
fn normalize_configuration(ctx: &Ctx, c: &Value) -> EsResult<Value> {
    const P: &str = "connector_update_configuration_request";
    let Value::Obj(entries) = c else { return Err(EsError::illegal_argument("[configuration] must be an object")) };
    let wrap = |cause: EsError| failed_field(ctx, P, &[Seg::Key("configuration")], cause);
    let mut out = Map::new();
    for (k, e) in entries {
        let Value::Obj(e) = e else { return Err(wrap(EsError::illegal_argument("Required [label]"))) };
        let enums: [(&str, &[&str], &str); 2] = [
            ("type", &["str", "int", "list", "bool"], "ConfigurationFieldType"),
            ("display", &["text", "textbox", "textarea", "numeric", "toggle", "dropdown"], "ConfigurationDisplayType"),
        ];
        for (f, allowed, class) in enums {
            if let Some(Value::Str(s)) = e.get(f)
                && !allowed.contains(&s.as_str())
            {
                return Err(wrap(EsError::illegal_argument(format!("Unknown {class} [{s}]."))));
            }
        }
        for v in e.get("validations").and_then(Value::as_array).map(Vec::as_slice).unwrap_or_default() {
            if let Some(Value::Str(s)) = v.get("type")
                && !["less_than", "greater_than", "list_type", "included_in", "regex"].contains(&s.as_str())
            {
                return Err(wrap(EsError::illegal_argument(format!("Unknown ConfigurationValidationType [{s}]."))));
            }
        }
        if !e.contains_key("label") {
            return Err(wrap(EsError::illegal_argument("Required [label]")));
        }
        out.insert(k.clone(), config_defaults(&Value::Obj(e.clone())));
    }
    Ok(Value::Obj(out))
}

/// `ConnectorStateMachine`.
fn connector_transition(from: &str, to: &str) -> bool {
    matches!(
        (from, to),
        ("created", "needs_configuration" | "error")
            | ("needs_configuration", "configured" | "error")
            | ("configured", "needs_configuration" | "configured" | "connected" | "error")
            | ("connected", "connected" | "configured" | "error")
            | ("error", "connected" | "configured" | "error")
    )
}

// --- sync jobs ---------------------------------------------------------------------------------

/// `ConnectorSyncJobStateMachine`.
fn sync_transition(from: &str, to: &str) -> bool {
    matches!(
        (from, to),
        ("pending", "in_progress" | "canceled")
            | ("in_progress", "canceling" | "completed" | "suspended" | "error")
            | ("suspended", "in_progress" | "canceled")
            | ("canceling", "canceled" | "error")
    )
}

fn sync_not_found(id: &str) -> EsError {
    EsError::resource_not_found(id.to_string())
}

fn invalid_sync(from: &str, to: &str) -> EsError {
    let msg = format!("Invalid transition attempt from [{from}] to [{to}]. Such a ConnectorSyncStatus transition is not supported by the Connector Protocol.");
    EsError::new(400, "status_exception", msg.clone()).caused_by(EsError::new(400, "connector_sync_job_invalid_status_transition_exception", msg))
}

pub async fn sync_job_post(ctx: &Ctx<'_>) -> EsResult<Reply> {
    let body = body_fields(ctx, "connector_sync_job_post_request", &["id", "job_type", "trigger_method"])?;
    let Some(cid) = body.get("id").map(Value::to_es_string) else { return Err(EsError::illegal_argument("Required [id]")) };
    let job_type = body.get("job_type").map(Value::to_es_string).unwrap_or_else(|| "full".into());
    if !matches!(job_type.as_str(), "full" | "incremental" | "access_control") {
        return Err(EsError::x_content_parse_bare("Failed to build [connector_sync_job_post_request] after last required field arrived")
            .caused_by(EsError::illegal_argument(format!("Unknown ConnectorSyncJobType [{job_type}]."))));
    }
    let trigger = body.get("trigger_method").map(Value::to_es_string).unwrap_or_else(|| "on_demand".into());
    if !matches!(trigger.as_str(), "on_demand" | "scheduled") {
        return Err(EsError::x_content_parse_bare("Failed to build [connector_sync_job_post_request] after last required field arrived")
            .caused_by(EsError::illegal_argument(format!("Unknown ConnectorSyncJobTriggerMethod [{trigger}]."))));
    }
    let id = crate::meta::names::time_based_id();
    let out = id.clone();
    let now_ms = ctx.now_ms;
    ctx.cluster
        .update(move |st| {
            let Some(c) = st.metadata.xpack.get(CONNECTORS, &cid).filter(|c| c.get("deleted") != Some(&Value::Bool(true))).cloned() else {
                return Err(EsError::resource_not_found(format!("Connector with id '{cid}' does not exist.")));
            };
            let now = now_nanos_string();
            let mut snap = Map::new();
            snap.insert("id".into(), cid.clone().into());
            let filtering = c.get("filtering").and_then(Value::as_array).and_then(|f| f.first()).and_then(|f| f.get("active")).cloned().unwrap_or(Value::Null);
            snap.insert("filtering".into(), filtering);
            for k in ["index_name", "language", "pipeline", "service_type"] {
                if let Some(v) = c.get(k) {
                    snap.insert(k.into(), v.clone());
                }
            }
            // Access control syncs write the connector's ACL index.
            if job_type == "access_control"
                && let Some(Value::Str(i)) = snap.get_mut("index_name")
            {
                *i = format!(".search-acl-filter-{i}");
            }
            snap.insert("configuration".into(), c.get("configuration").cloned().unwrap_or_else(Value::object));
            let job = obj! {
                "id" => id.clone(), "connector" => Value::Obj(snap), "created_at" => now.clone(), "deleted_document_count" => 0,
                "indexed_document_count" => 0, "indexed_document_volume" => 0, "job_type" => job_type, "last_seen" => now,
                "metadata" => obj! {}, "status" => "pending", "total_document_count" => 0, "trigger_method" => trigger,
            };
            store::put(st, SYNC_JOBS, &id, job, now_ms).map(|_| ())
        })
        .await?;
    Ok(Reply::Body(201, obj! { "id" => out }))
}

pub fn sync_job_get(ctx: &Ctx) -> EsResult<Reply> {
    let id = ctx.path_param("connector_sync_job_id").unwrap_or_default();
    ctx.state().metadata.xpack.get(SYNC_JOBS, id).cloned().map_or_else(|| Err(sync_not_found(id)), Reply::ok)
}

pub fn sync_job_list(ctx: &Ctx) -> EsResult<Reply> {
    let st = ctx.state();
    let from = ctx.int_param("from", 0)?.max(0) as usize;
    let size = ctx.int_param("size", 100)?.max(0) as usize;
    let cid = ctx.query("connector_id");
    let status = ctx.query("status");
    let types = ctx.list_param("job_type").unwrap_or_default();
    if let Some(s) = status
        && !SYNC_STATUSES.contains(&s)
    {
        return Err(EsError::illegal_argument(format!("Unknown ConnectorSyncStatus [{s}].")));
    }
    if let Some(t) = types.iter().find(|t| !matches!(t.as_str(), "full" | "incremental" | "access_control")) {
        return Err(EsError::illegal_argument(format!("Unknown ConnectorSyncJobType [{t}].")));
    }
    let mut jobs: Vec<&Value> = st
        .metadata
        .xpack
        .iter(SYNC_JOBS)
        .map(|(_, j)| j)
        .filter(|j| cid.is_none_or(|c| j.pointer(&["connector", "id"]).and_then(Value::as_str) == Some(c)))
        .filter(|j| status.is_none_or(|s| j.get("status").and_then(Value::as_str) == Some(s)))
        .filter(|j| types.is_empty() || j.get("job_type").and_then(Value::as_str).is_some_and(|t| types.iter().any(|x| x == t)))
        .collect();
    // Newest first, as Elasticsearch sorts by `created_at`.
    jobs.sort_by(|a, b| b.get("created_at").map(Value::to_es_string).cmp(&a.get("created_at").map(Value::to_es_string)));
    let count = jobs.len();
    Reply::ok(obj! { "count" => count, "results" => Value::Arr(jobs.into_iter().skip(from).take(size).cloned().collect()) })
}

async fn modify_job(ctx: &Ctx<'_>, f: impl FnOnce(&mut Map) -> EsResult<()> + Send) -> EsResult<Reply> {
    let id = ctx.path_param("connector_sync_job_id").unwrap_or_default().to_string();
    let now = ctx.now_ms;
    ctx.cluster
        .update(move |st| {
            let Some(Value::Obj(mut m)) = st.metadata.xpack.get(SYNC_JOBS, &id).cloned() else { return Err(sync_not_found(&id)) };
            f(&mut m)?;
            store::put(st, SYNC_JOBS, &id, hash_ordered(Value::Obj(m)), now).map(|_| ())
        })
        .await?;
    updated()
}

fn status_of(m: &Map) -> String {
    m.get("status").and_then(Value::as_str).unwrap_or("pending").to_string()
}

pub async fn sync_job_check_in(ctx: &Ctx<'_>) -> EsResult<Reply> {
    modify_job(ctx, |m| {
        m.insert("last_seen".into(), now_nanos_string().into());
        Ok(())
    })
    .await
}

pub async fn sync_job_cancel(ctx: &Ctx<'_>) -> EsResult<Reply> {
    modify_job(ctx, |m| {
        let from = status_of(m);
        let now = now_nanos_string();
        if from == "pending" || from == "suspended" {
            m.insert("status".into(), "canceled".into());
            m.insert("completed_at".into(), now.clone().into());
            m.insert("cancelation_requested_at".into(), now.clone().into());
            m.insert("canceled_at".into(), now.into());
        } else if sync_transition(&from, "canceling") {
            m.insert("status".into(), "canceling".into());
            m.insert("cancelation_requested_at".into(), now.into());
        } else {
            return Err(invalid_sync(&from, "canceling"));
        }
        Ok(())
    })
    .await
}

pub async fn sync_job_error(ctx: &Ctx<'_>) -> EsResult<Reply> {
    let body = body_fields(ctx, "connector_sync_job_error_request", &["error"])?;
    let err = required(&body, "error")?.clone();
    modify_job(ctx, move |m| {
        let from = status_of(m);
        if !sync_transition(&from, "error") {
            return Err(invalid_sync(&from, "error"));
        }
        m.insert("status".into(), "error".into());
        m.insert("error".into(), err);
        Ok(())
    })
    .await
}

pub async fn sync_job_update_stats(ctx: &Ctx<'_>) -> EsResult<Reply> {
    let body = body_fields(
        ctx,
        "connector_sync_job_update_ingestion_stats",
        &["deleted_document_count", "indexed_document_count", "indexed_document_volume", "total_document_count", "last_seen", "metadata"],
    )?;
    const P: &str = "connector_sync_job_update_ingestion_stats";
    let mut body = body;
    for (k, v) in body.iter_mut() {
        let path = [Seg::Key(k.as_str())];
        *v = match k.as_str() {
            "metadata" => typed(ctx, P, &path, v, Kind::Obj)?,
            "last_seen" => instant(ctx, P, &path, v, true)?,
            _ => typed(ctx, P, &path, v, Kind::Long)?,
        };
    }
    let mut absent: Vec<&str> = ["deleted_document_count", "indexed_document_count", "indexed_document_volume"].into_iter().filter(|k| !body.contains_key(*k)).collect();
    if !absent.is_empty() {
        return Err(missing(&mut absent));
    }
    let mut errors = Vec::new();
    if ctx.path_param("connector_sync_job_id").is_none_or(str::is_empty) {
        errors.push("[connector_sync_job_id] of the connector sync job cannot be null or empty.".to_string());
    }
    for k in ["deleted_document_count", "indexed_document_count", "indexed_document_volume", "total_document_count"] {
        if body.get(k).and_then(Value::as_i64).is_some_and(|n| n < 0) {
            errors.push(format!("[{k}] cannot be negative."));
        }
    }
    if !errors.is_empty() {
        return Err(EsError::action_request_validation(&errors));
    }
    modify_job(ctx, move |m| {
        for (k, v) in body {
            m.insert(k, v);
        }
        if !m.contains_key("last_seen") {
            m.insert("last_seen".into(), now_nanos_string().into());
        }
        Ok(())
    })
    .await
}

pub async fn sync_job_claim(ctx: &Ctx<'_>) -> EsResult<Reply> {
    const P: &str = "connector_sync_job_claim_request";
    let body = body_fields(ctx, P, &["worker_hostname", "sync_cursor"])?;
    let host = match body.get("worker_hostname") {
        Some(v) => typed(ctx, P, &[Seg::Key("worker_hostname")], v, Kind::Str)?,
        None => return Err(missing(&mut ["worker_hostname"])),
    };
    let cursor = body.get("sync_cursor").cloned();
    modify_job(ctx, move |m| {
        let now = now_nanos_string();
        m.insert("last_seen".into(), now.clone().into());
        m.insert("status".into(), "in_progress".into());
        m.insert("worker_hostname".into(), host);
        m.insert("started_at".into(), now.into());
        if let Some(c) = cursor
            && let Some(Value::Obj(conn)) = m.get_mut("connector")
        {
            conn.insert("sync_cursor".into(), c);
        }
        Ok(())
    })
    .await
}

pub async fn sync_job_delete(ctx: &Ctx<'_>) -> EsResult<Reply> {
    let id = ctx.path_param("connector_sync_job_id").unwrap_or_default().to_string();
    ctx.cluster.update(move |st| store::remove(st, SYNC_JOBS, &id).map(|_| ()).ok_or_else(|| sync_not_found(&id))).await?;
    Reply::acknowledged()
}

// --- secrets -----------------------------------------------------------------------------------

pub async fn secret_post(ctx: &Ctx<'_>) -> EsResult<Reply> {
    let body = ctx.body_map()?.unwrap_or_default();
    let Some(v) = body.get("value").filter(|v| !v.is_null()) else {
        return Err(EsError::action_request_validation(&["[value] of the connector secret cannot be [null] or [\"\"]".into()]));
    };
    let id = crate::meta::names::time_based_id();
    let out = id.clone();
    let v = v.clone();
    let now = ctx.now_ms;
    ctx.cluster.update(move |st| store::put(st, SECRETS, &id, obj! { "value" => v }, now).map(|_| ())).await?;
    Reply::ok(obj! { "id" => out })
}

pub async fn secret_put(ctx: &Ctx<'_>) -> EsResult<Reply> {
    let id = ctx.path_param("id").unwrap_or_default().to_string();
    let body = ctx.body_map()?.unwrap_or_default();
    let Some(v) = body.get("value").filter(|v| !v.is_null()).cloned() else {
        return Err(EsError::action_request_validation(&["[value] of the connector secret cannot be [null] or [\"\"]".into()]));
    };
    let now = ctx.now_ms;
    let created = ctx.cluster.update(move |st| store::put(st, SECRETS, &id, obj! { "value" => v }, now)).await?;
    Ok(Reply::Body(if created { 201 } else { 200 }, obj! { "result" => if created { "created" } else { "updated" } }))
}

pub fn secret_get(ctx: &Ctx) -> EsResult<Reply> {
    let id = ctx.path_param("id").unwrap_or_default();
    match ctx.state().metadata.xpack.get(SECRETS, id) {
        Some(s) => Reply::ok(obj! { "id" => id, "value" => s.get("value").cloned().unwrap_or(Value::Null) }),
        None => Err(EsError::resource_not_found(format!("No secret with id [{id}]"))),
    }
}

pub async fn secret_delete(ctx: &Ctx<'_>) -> EsResult<Reply> {
    let id = ctx.path_param("id").unwrap_or_default().to_string();
    ctx.cluster
        .update(move |st| store::remove(st, SECRETS, &id).map(|_| ()).ok_or_else(|| EsError::resource_not_found(format!("No secret with id [{id}]"))))
        .await?;
    Reply::ok(obj! { "deleted" => true })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn state_machines() {
        assert!(connector_transition("created", "needs_configuration"));
        assert!(!connector_transition("error", "needs_configuration"));
        assert!(sync_transition("pending", "canceled"));
        assert!(!sync_transition("pending", "error"));
        assert!(!sync_transition("canceled", "canceling"));
        assert!(sync_transition("in_progress", "error"));
    }

    #[test]
    fn new_connector_shape() {
        let mut b = Map::new();
        b.insert("index_name".into(), "x".into());
        let c = new_connector("c", &b);
        let keys: Vec<&str> = c.as_object().unwrap().keys().map(String::as_str).collect();
        assert_eq!(keys, ["id", "configuration", "custom_scheduling", "filtering", "index_name", "is_native", "name", "scheduling", "status", "sync_now", "deleted"]);
    }
}
