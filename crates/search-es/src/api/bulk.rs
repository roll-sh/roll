//! `_bulk`: NDJSON action/metadata lines and sources (`BulkRequestParser`), every action
//! (`index`, `create`, `update`, `delete`) run through the shared write path in request order,
//! per-item results and errors exactly as Elasticsearch renders them (`errors` first, then
//! `took`, then `items`).

use crate::api::document;
use crate::api::update::{self, UpdateBody};
use crate::api::Reply;
use crate::error::{EsError, EsResult};
use crate::fetch::FetchSource;
use crate::json::{Map, Value};
use crate::rest::Ctx;
use crate::write::{self, Action, RefreshPolicy, WriteArgs};
use bytes::Bytes;

/// One parsed bulk item.
pub struct Item {
    pub action: Action,
    pub index: Option<String>,
    pub args: WriteArgs,
    pub retry_on_conflict: i64,
    pub source: Option<FetchSource>,
    /// The source line (index/create) or update body.
    pub data: Bytes,
}

fn token_name(v: &Value) -> &'static str {
    match v {
        Value::Null => "VALUE_NULL",
        Value::Bool(_) => "VALUE_BOOLEAN",
        Value::Num(_) => "VALUE_NUMBER",
        Value::Str(_) => "VALUE_STRING",
        Value::Arr(_) => "START_ARRAY",
        Value::Obj(_) => "START_OBJECT",
    }
}

fn trim_cr(b: &[u8]) -> &[u8] {
    if b.last() == Some(&b'\r') { &b[..b.len() - 1] } else { b }
}

/// Parses a bulk body with the request-level defaults.
pub fn parse(data: &Bytes, defaults: &WriteArgs, default_index: Option<&str>, default_source: Option<&FetchSource>, allow_explicit_index: bool) -> EsResult<Vec<Item>> {
    let mut items = Vec::new();
    let mut from = 0usize;
    let mut line = 0usize;
    let n = data.len();
    let next_marker = |from: usize| memchr::memchr(b'\n', &data[from..]).map(|i| from + i);
    loop {
        let Some(nl) = next_marker(from) else {
            if data[from..].iter().any(|b| !b.is_ascii_whitespace()) {
                return Err(EsError::illegal_argument("The bulk request must be terminated by a newline [\\n]"));
            }
            break;
        };
        line += 1;
        let action_line = trim_cr(&data[from..nl]);
        from = nl + 1;
        let v = match crate::json::parse(action_line) {
            Ok(Some(v)) => v,
            Ok(None) => continue,
            Err(e) => return Err(e.into()),
        };
        let Value::Obj(m) = &v else {
            return Err(EsError::illegal_argument(format!("Malformed action/metadata line [{line}], expected START_OBJECT but found [{}]", token_name(&v))));
        };
        let Some((action_name, meta)) = m.iter().next() else {
            return Err(EsError::illegal_argument(format!("Malformed action/metadata line [{line}], expected FIELD_NAME but found [END_OBJECT]")));
        };
        if m.len() > 1 {
            let second = m.keys().nth(1).map(String::as_str).unwrap_or("");
            let _ = second;
            return Err(EsError::illegal_argument(format!("Malformed action/metadata line [{line}], expected END_OBJECT but found [FIELD_NAME]")));
        }
        let action = match action_name.as_str() {
            "index" => Action::Index,
            "create" => Action::Create,
            "update" => Action::Update,
            "delete" => Action::Delete,
            other => {
                return Err(EsError::illegal_argument(format!(
                    "Malformed action/metadata line [{line}], expected field [create], [delete], [index] or [update] but found [{other}]"
                )));
            }
        };
        let mut args = defaults.clone();
        args.id = None;
        let mut index = default_index.map(str::to_string);
        let mut retry = 0i64;
        let mut source = default_source.cloned();
        match meta {
            Value::Obj(meta) => {
                for (k, x) in meta {
                    if let Value::Arr(_) = x {
                        return Err(EsError::illegal_argument(format!("Malformed action/metadata line [{line}], expected a simple value for field [{k}] but found [START_ARRAY]")));
                    }
                    if let Value::Obj(_) = x
                        && k != "_source"
                        && k != "dynamic_templates"
                    {
                        return Err(EsError::illegal_argument(format!("Malformed action/metadata line [{line}], expected a simple value for field [{k}] but found [START_OBJECT]")));
                    }
                    if x.is_null() {
                        continue;
                    }
                    let text = || x.as_str().map(str::to_string).unwrap_or_else(|| x.to_es_string());
                    let long = || -> EsResult<i64> {
                        match x {
                            Value::Num(n) => n.as_i64().ok_or_else(|| EsError::illegal_argument(format!("Value [{}] is out of range for a long", x.to_es_string()))),
                            Value::Str(s) => s.parse::<i64>().map_err(|_| EsError::number_format(s)),
                            _ => Err(EsError::illegal_argument(format!("Current token ({}) not numeric", token_name(x)))),
                        }
                    };
                    match k.as_str() {
                        "_index" => {
                            if !allow_explicit_index {
                                return Err(EsError::illegal_argument("explicit index in bulk is not allowed"));
                            }
                            index = Some(text());
                        }
                        "_id" => args.id = Some(text()),
                        "routing" => args.routing = Some(text()),
                        "op_type" => match text().as_str() {
                            "create" => args.create = true,
                            "index" => args.create = false,
                            o => return Err(EsError::illegal_argument(format!("opType must be 'create' or 'index', found: [{o}]"))),
                        },
                        "version" => args.version = Some(long()?),
                        "version_type" => args.version_type = Some(write::parse_version_type(&text())?),
                        "if_seq_no" => args.if_seq_no = Some(long()?),
                        "if_primary_term" => args.if_primary_term = Some(long()?),
                        "retry_on_conflict" => retry = long()?,
                        "pipeline" => args.pipeline = Some(text()),
                        "_source" => source = Some(FetchSource::from_value(x)?),
                        "require_alias" => args.require_alias = crate::units::parse_bool(&text())?,
                        "require_data_stream" => args.require_data_stream = crate::units::parse_bool(&text())?,
                        "list_executed_pipelines" => {}
                        "dynamic_templates" => {
                            if let Value::Obj(dt) = x {
                                args.dynamic_templates = Some(dt.clone());
                            }
                        }
                        "_type" => {}
                        other => return Err(EsError::illegal_argument(format!("Action/metadata line [{line}] contains an unknown parameter [{other}]"))),
                    }
                }
            }
            other => {
                return Err(EsError::illegal_argument(format!(
                    "Malformed action/metadata line [{line}], expected START_OBJECT or END_OBJECT but found [{}]",
                    token_name(other)
                )));
            }
        }
        if action == Action::Create {
            args.create = true;
        }
        // An index action with `op_type: create` is a create (`IndexRequest.opType`).
        let action = if action == Action::Index && args.create { Action::Create } else { action };
        if action == Action::Delete {
            items.push(Item { action, index, args, retry_on_conflict: retry, source, data: Bytes::new() });
            continue;
        }
        let Some(nl) = next_marker(from) else {
            if from < n && data[from..].iter().any(|b| !b.is_ascii_whitespace()) {
                return Err(EsError::illegal_argument("The bulk request must be terminated by a newline [\\n]"));
            }
            break;
        };
        line += 1;
        let body = data.slice(from..from + trim_cr(&data[from..nl]).len());
        from = nl + 1;
        items.push(Item { action, index, args, retry_on_conflict: retry, source, data: body });
    }
    Ok(items)
}

/// `POST /_bulk`, `POST /{index}/_bulk`.
pub async fn bulk(ctx: &Ctx<'_>) -> EsResult<Reply> {
    let start = std::time::Instant::now();
    let refresh = RefreshPolicy::parse(ctx.query("refresh"))?;
    let defaults = WriteArgs {
        routing: ctx.query("routing").filter(|r| !r.is_empty()).map(str::to_string),
        pipeline: ctx.query("pipeline").map(str::to_string),
        require_alias: ctx.bool_param("require_alias", false)?,
        require_data_stream: ctx.bool_param("require_data_stream", false)?,
        ..Default::default()
    };
    let default_source = FetchSource::from_params(ctx.query("_source"), ctx.query("_source_includes"), ctx.query("_source_excludes"))?;
    let default_index = ctx.path_param("index").map(str::to_string);
    let items = parse(&ctx.body, &defaults, default_index.as_deref(), default_source.as_ref(), true)?;
    // `BulkRequest.validate`.
    let mut errs = Vec::new();
    if items.is_empty() {
        errs.push("no requests added".to_string());
    }
    for it in &items {
        if it.index.is_none() {
            errs.push("index is missing".into());
        }
        if it.action == Action::Delete && it.args.id.is_none() {
            errs.push("id is missing".into());
        }
        if it.action == Action::Update && it.args.id.is_none() {
            errs.push("id is missing".into());
        }
        if it.action == Action::Update && it.args.if_seq_no.is_some() && it.retry_on_conflict > 0 {
            errs.push("compare and write operations can not be retried".into());
        }
    }
    if !errs.is_empty() {
        return Err(EsError::action_request_validation(&errs));
    }
    let mut out_items = Vec::with_capacity(items.len());
    let mut errors = false;
    let mut written: Vec<(String, u32)> = Vec::new();
    for it in items {
        let name = it.action.name();
        let expr = it.index.clone().unwrap_or_default();
        let r = run_item(ctx, &it, &expr).await;
        let mut m = Map::new();
        match r {
            Ok((mut body, uuid, shard, ok)) => {
                if refresh != RefreshPolicy::None && ok {
                    written.push((uuid, shard));
                    if refresh == RefreshPolicy::Immediate
                        && let Value::Obj(b) = &mut body
                    {
                        insert_after(b, "result", "forced_refresh", Value::Bool(true));
                    }
                }
                m.insert(name.into(), body);
            }
            Err(b) => {
                let (e, index, id, fs) = *b;
                errors = true;
                let mut im = Map::new();
                im.insert("_index".into(), Value::from(index));
                im.insert("_id".into(), id.map(Value::from).unwrap_or(Value::Null));
                im.insert("status".into(), Value::from(e.status as i64));
                if let Some(f) = fs {
                    im.insert("failure_store".into(), Value::from(f));
                }
                im.insert("error".into(), e.to_value(false));
                m.insert(name.into(), Value::Obj(im));
            }
        }
        out_items.push(Value::Obj(m));
    }
    if !written.is_empty() {
        write::refresh_shards(ctx.cluster, &written)?;
    }
    let mut resp = Map::new();
    resp.insert("errors".into(), Value::Bool(errors));
    resp.insert("took".into(), Value::from(start.elapsed().as_millis() as i64));
    // `BulkResponse.ingestTookInMillis`, reported once a pipeline ran.
    let ingest = ctx.ingest_nanos.load(std::sync::atomic::Ordering::Relaxed);
    if ingest > 0 {
        resp.insert("ingest_took".into(), Value::from((ingest / 1_000_000) as i64));
    }
    resp.insert("items".into(), Value::Arr(out_items));
    Reply::ok(Value::Obj(resp))
}

fn insert_after(m: &mut Map, after: &str, key: &str, v: Value) {
    let pos = m.get_index_of(after).map(|i| i + 1).unwrap_or(m.len());
    m.shift_insert(pos, key.to_string(), v);
}

/// Whether an index item failed while its target was being resolved (before an auto id
/// exists): missing or forbidden targets, data stream write rules, timestamps.
fn before_routing(e: &EsError) -> bool {
    matches!(e.kind.as_ref(), "index_not_found_exception" | "resource_not_found_exception" | "timestamp_error" | "invalid_index_name_exception")
        || e.reason().starts_with("only write ops with an op_type of create are allowed in data streams")
}

/// An item's error: the exception, the index and id it names, and its `failure_store` status.
type ItemResult = Result<(Value, String, u32, bool), Box<(EsError, String, Option<String>, Option<&'static str>)>>;

async fn run_item(ctx: &Ctx<'_>, it: &Item, expr: &str) -> ItemResult {
    let fail = |e: EsError, index: &str, id: Option<String>| Err(Box::new((e, index.to_string(), id, None)));
    let target_name = |e: &EsError| -> String {
        // Errors on a resolved index name it; others the expression.
        e.meta().iter().find(|(k, _)| k == "index").and_then(|(_, v)| v.as_str().map(str::to_string)).unwrap_or_else(|| expr.to_string())
    };
    match it.action {
        Action::Index | Action::Create => {
            let mut args = it.args.clone();
            let auto_id = args.id.is_none();
            if auto_id {
                // Auto ids are unique, so `index` behaves as `create` without forcing it: time
                // series ids are generated from the document and an `index` overwrites.
                args.id = Some(crate::meta::names::time_based_id());
            }
            let id = args.id.clone();
            if id.as_deref() == Some("") {
                // Bulk items fail this check while being routed, once their target resolved.
                if let Err(e) = write::resolve_target(ctx.cluster, expr, &args, ctx.now_ms).await {
                    return fail(e, expr, id);
                }
                return fail(EsError::illegal_argument("if _id is specified it must not be empty"), expr, id);
            }
            let errs = write::validate_args(&args, it.action);
            if !errs.is_empty() {
                return fail(EsError::action_request_validation(&errs), expr, id);
            }
            match document::index_one(ctx, expr, args, it.data.clone(), auto_id).await {
                Ok(o) => {
                    let mut m = Map::new();
                    o.write_into(&mut m);
                    m.insert("status".into(), Value::from(o.status() as i64));
                    let ok = o.result != "noop";
                    Ok((Value::Obj(m), o.uuid, o.shard, ok))
                }
                Err(e) => {
                    let idx = target_name(&e);
                    // Time series ids are generated from the document: items name the id once
                    // it is known (a version conflict says it), `null` before.
                    let id = if auto_id && write::resolve_existing(&ctx.state(), expr).is_some_and(|i| crate::engine::tsdb::is_time_series(&i)) {
                        (e.kind == "version_conflict_engine_exception").then(|| e.reason().strip_prefix('[').and_then(|r| r.split(']').next()).map(str::to_string)).flatten()
                    } else if auto_id && before_routing(&e) {
                        // Ids are generated once the target index is known.
                        None
                    } else {
                        id
                    };
                    // A data stream without a failure store says so (on the item).
                    let mut e = e;
                    let fs = e.take_trailing("failure_store").map(|_| "not_enabled");
                    // Document failures name the concrete index they were written to.
                    let idx = match fs {
                        // Timestamp errors happen before a backing index is chosen.
                        Some(_) if e.kind == "timestamp_error" => expr.to_string(),
                        Some(_) => write::resolve_existing(&ctx.state(), expr).map(|i| i.name.clone()).unwrap_or(idx),
                        None => idx,
                    };
                    Err(Box::new((e, idx, id, fs)))
                }
            }
        }
        Action::Delete => {
            let args = it.args.clone();
            let id = args.id.clone().unwrap_or_default();
            let errs = write::validate_args(&args, Action::Delete);
            if !errs.is_empty() {
                return fail(EsError::action_request_validation(&errs), expr, Some(id));
            }
            // Data streams are append-only: deletes go to their backing indices.
            if ctx.state().metadata.data_streams.contains_key(expr) {
                return fail(EsError::illegal_argument("only write ops with an op_type of create are allowed in data streams"), expr, Some(id));
            }
            let r = async {
                let mut a = args.clone();
                a.create = true;
                let target = write::resolve_target(ctx.cluster, expr, &a, ctx.now_ms).await?;
                let routing = write::effective_routing(&target, args.routing.as_deref(), &id)?;
                write::check_routing(&target.index, &id, routing.as_deref())?;
                write::apply_delete(ctx.cluster, &target.index, &id, routing.as_deref(), &args)
            }
            .await;
            match r {
                Ok(o) => {
                    let mut m = Map::new();
                    o.write_into(&mut m);
                    m.insert("status".into(), Value::from(o.status() as i64));
                    Ok((Value::Obj(m), o.uuid, o.shard, true))
                }
                Err(e) => {
                    let idx = target_name(&e);
                    fail(e, &idx, Some(id))
                }
            }
        }
        Action::Update => {
            let id = it.args.id.clone().unwrap_or_default();
            let body = match crate::json::parse(&it.data) {
                Ok(Some(v)) => v,
                Ok(None) => Value::Obj(Map::new()),
                Err(e) => return fail(e.into(), expr, Some(id)),
            };
            let mut ub = match UpdateBody::parse(&body, Some(&it.data)) {
                Ok(b) => b,
                Err(e) => return fail(e, expr, Some(id)),
            };
            if ub.source.is_none() {
                ub.source = it.source.clone();
            }
            let errs = ub.validate(&it.args, it.retry_on_conflict);
            if !errs.is_empty() {
                return fail(EsError::action_request_validation(&errs), expr, Some(id));
            }
            match update::update_one(ctx, expr, &id, &it.args, &ub, it.retry_on_conflict).await {
                Ok(o) => {
                    let v = match update::render(&o, ub.source.as_ref()) {
                        Ok(v) => v,
                        Err(e) => return fail(e, expr, Some(id)),
                    };
                    let mut m = match v {
                        Value::Obj(m) => m,
                        _ => Map::new(),
                    };
                    let status = o.write.status();
                    let pos = m.get_index_of("get").unwrap_or(m.len());
                    m.shift_insert(pos, "status".into(), Value::from(status as i64));
                    let ok = o.write.result != "noop";
                    Ok((Value::Obj(m), o.write.uuid, o.write.shard, ok))
                }
                Err(e) => {
                    let idx = target_name(&e);
                    fail(e, &idx, Some(id))
                }
            }
        }
    }
}
