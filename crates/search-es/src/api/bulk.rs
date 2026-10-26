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
    // The ids index/create items are written with: auto ids are generated here, in item
    // order, so documents can be prepared ahead (in parallel) before their turn.
    let auto: Vec<bool> = items.iter().map(|it| matches!(it.action, Action::Index | Action::Create) && it.args.id.is_none()).collect();
    let ids: Vec<Option<String>> = items
        .iter()
        .zip(&auto)
        .map(|(it, &a)| if a { Some(crate::meta::names::time_based_id()) } else { it.args.id.clone() })
        .collect();
    let mut ahead = super::bulk_ahead::Ahead::new(ctx.cluster, &items, &ids);
    // Items are written straight into one pre-rendered JSON array (`json::raw`): about 200
    // bytes each, instead of a tree of maps per item.
    let mut out_items = crate::json::raw::buffer(items.len() * 200 + 2);
    out_items.push(b'[');
    let mut errors = false;
    let mut written: Vec<(String, u32)> = Vec::new();
    for (i, it) in items.iter().enumerate() {
        let name = it.action.name();
        let expr = it.index.clone().unwrap_or_default();
        let mut take = |idx: &std::sync::Arc<crate::meta::IndexMeta>| ahead.take(ctx.cluster, &items, &ids, i, idx);
        let generated = auto[i].then(|| ids[i].clone()).flatten();
        let r = run_item(ctx, it, &expr, generated, &mut take).await;
        if i > 0 {
            out_items.push(b',');
        }
        let mut m = Map::new();
        match r {
            Ok((body, uuid, shard, ok)) => {
                let forced = refresh == RefreshPolicy::Immediate && ok;
                if refresh != RefreshPolicy::None && ok {
                    written.push((uuid, shard));
                }
                match body {
                    ItemBody::Write(o) => {
                        o.write_bulk_item(&mut out_items, name, forced);
                        continue;
                    }
                    ItemBody::Tree(mut body) => {
                        if forced && let Value::Obj(b) = &mut body {
                            insert_after(b, "result", "forced_refresh", Value::Bool(true));
                        }
                        m.insert(name.into(), body);
                    }
                }
            }
            Err(b) => {
                let (e, index, id, fs) = *b;
                errors = true;
                // An item that failed on its shard still sent that shard a request, which
                // `refresh` refreshes (`TransportWriteAction`), failed items or not.
                if refresh != RefreshPolicy::None
                    && matches!(
                        e.kind.as_ref(),
                        "version_conflict_engine_exception" | "document_missing_exception" | "document_parsing_exception" | "mapper_parsing_exception" | "strict_dynamic_mapping_exception"
                    )
                    && let Some(id) = &id
                    && let Some(idx) = write::resolve_existing(&ctx.state(), &index)
                {
                    written.push((idx.uuid.clone(), crate::meta::routing::shard_for(&idx, id, it.args.routing.as_deref())));
                }
                let mut im = Map::new();
                im.insert("_index".into(), Value::from(index));
                im.insert("_id".into(), id.map(Value::from).unwrap_or(Value::Null));
                im.insert("status".into(), Value::from(e.status as i64));
                if let Some(f) = fs {
                    im.insert("failure_store".into(), Value::from(f));
                }
                im.insert("error".into(), crate::opensearch::item_error(ctx, &e, false));
                m.insert(name.into(), Value::Obj(im));
            }
        }
        crate::json::write::write_json(&mut out_items, &Value::Obj(m), false);
    }
    out_items.push(b']');
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
    // OpenSearch's `BulkResponse.toXContent` writes `took` (and `ingest_took`) before `errors`.
    if ctx.cluster.dialect == crate::cluster::Dialect::OpenSearch
        && let Some(e) = resp.shift_remove("errors")
    {
        resp.insert("errors".into(), e);
    }
    resp.insert("items".into(), crate::json::raw::from_buffer(out_items));
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

/// A succeeded item's response: an index, create or delete outcome (rendered straight to
/// JSON, [`write::WriteOutcome::write_bulk_item`]) or an update's tree.
enum ItemBody {
    Write(write::WriteOutcome),
    Tree(Value),
}

/// An item's response, the uuid and shard it was written to and whether it changed anything;
/// or its error: the exception, the index and id it names, and its `failure_store` status.
type ItemResult = Result<(ItemBody, String, u32, bool), Box<(EsError, String, Option<String>, Option<&'static str>)>>;

/// Runs one item; `generated`: the auto id of an index/create item without one, `ahead`: its
/// document prepared ahead, if any.
async fn run_item(ctx: &Ctx<'_>, it: &Item, expr: &str, generated: Option<String>, ahead: document::PreparedAhead<'_>) -> ItemResult {
    let fail = |e: EsError, index: &str, id: Option<String>| Err(Box::new((e, index.to_string(), id, None)));
    let target_name = |e: &EsError| -> String {
        // Errors on a resolved index name it (an ingest failure after a pipeline re-targeted
        // the document names the new target); others the expression.
        if let Some(i) = e.hidden("item_index").and_then(Value::as_str) {
            return i.to_string();
        }
        e.meta().iter().find(|(k, _)| k == "index").and_then(|(_, v)| v.as_str().map(str::to_string)).unwrap_or_else(|| expr.to_string())
    };
    match it.action {
        Action::Index | Action::Create => {
            let mut args = it.args.clone();
            let auto_id = args.id.is_none();
            if let Some(e) = crate::xpack::streams::check_item(ctx, expr, args.pipeline.as_deref()) {
                return fail(e, expr, args.id.clone());
            }
            if it.action == Action::Index
                && !args.create
                && let Some(e) = crate::opensearch::append_only_violation(&ctx.state(), expr, "INDEX", args.id.as_deref())
            {
                return fail(e, expr, args.id.clone());
            }
            if auto_id {
                // Auto ids are unique, so `index` behaves as `create` without forcing it: time
                // series ids are generated from the document and an `index` overwrites.
                args.id = Some(generated.unwrap_or_else(crate::meta::names::time_based_id));
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
            match document::index_one_ahead(ctx, expr, args, it.data.clone(), auto_id, Some(ahead)).await {
                Ok(mut o) => {
                    let ok = o.result != "noop";
                    let (uuid, shard) = (std::mem::take(&mut o.uuid), o.shard);
                    Ok((ItemBody::Write(o), uuid, shard, ok))
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
            if let Some(e) = crate::opensearch::append_only_violation(&ctx.state(), expr, "DELETE", Some(&id)) {
                return fail(e, expr, Some(id));
            }
            let errs = write::validate_args(&args, Action::Delete);
            if !errs.is_empty() {
                return fail(EsError::action_request_validation(&errs), expr, Some(id));
            }
            // Data streams are append-only: deletes go to their backing indices.
            if ctx.state().metadata.data_streams.contains_key(expr) {
                return fail(EsError::illegal_argument("only write ops with an op_type of create are allowed in data streams"), expr, Some(id));
            }
            let r = async {
                let target = write::resolve_delete_target(ctx.cluster, expr, &args, ctx.now_ms).await?;
                let routing = write::effective_routing(&target, args.routing.as_deref(), &id)?;
                write::check_routing(&target.index, &id, routing.as_deref())?;
                write::apply_delete(ctx.cluster, &target.index, &id, routing.as_deref(), &args)
            }
            .await;
            match r {
                Ok(mut o) => {
                    let (uuid, shard) = (std::mem::take(&mut o.uuid), o.shard);
                    Ok((ItemBody::Write(o), uuid, shard, true))
                }
                Err(e) => {
                    let idx = target_name(&e);
                    fail(e, &idx, Some(id))
                }
            }
        }
        Action::Update => {
            let id = it.args.id.clone().unwrap_or_default();
            if let Some(e) = crate::opensearch::append_only_violation(&ctx.state(), expr, "UPDATE", Some(&id)) {
                return fail(e, expr, Some(id));
            }
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
                    Ok((ItemBody::Tree(Value::Obj(m)), o.write.uuid, o.write.shard, ok))
                }
                Err(e) => {
                    let idx = target_name(&e);
                    fail(e, &idx, Some(id))
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::json::write::write_json;

    /// The item as the handler built it before: `write_into`, `status`, `forced_refresh` after
    /// `result`, under the action name.
    fn tree_item(o: &write::WriteOutcome, action: &str, forced: bool) -> Vec<u8> {
        let mut b = Map::new();
        o.write_into(&mut b);
        b.insert("status".into(), Value::from(o.status() as i64));
        if forced {
            insert_after(&mut b, "result", "forced_refresh", Value::Bool(true));
        }
        let mut m = Map::new();
        m.insert(action.into(), Value::Obj(b));
        let mut out = Vec::new();
        write_json(&mut out, &Value::Obj(m), false);
        out
    }

    #[test]
    fn direct_bulk_items_match_the_tree_rendering() {
        let base = write::WriteOutcome {
            index: "idx".into(),
            uuid: "u".into(),
            id: "1".into(),
            shard: 0,
            version: 1,
            result: "created",
            seq_no: 0,
            primary_term: 1,
            total: 2,
            forced_refresh: false,
            failure_store: None,
        };
        let mut cases = vec![base.clone()];
        for result in ["updated", "deleted", "not_found", "noop"] {
            cases.push(write::WriteOutcome { result, version: 7, seq_no: 41, primary_term: 3, ..base.clone() });
        }
        // Unassigned and dropped sequence numbers.
        cases.push(write::WriteOutcome { result: "noop", seq_no: -2, ..base.clone() });
        cases.push(write::WriteOutcome { result: "created", seq_no: -2, primary_term: 0, ..base.clone() });
        cases.push(write::WriteOutcome { failure_store: Some("used"), ..base.clone() });
        cases.push(write::WriteOutcome { forced_refresh: true, ..base.clone() });
        cases.push(write::WriteOutcome { index: "ü\"\\\n\u{1}x".into(), id: "\u{7f}\u{2028}é/\t".into(), version: i64::MAX, seq_no: i64::MAX, primary_term: i64::MAX, total: u32::MAX, ..base.clone() });
        cases.push(write::WriteOutcome { id: String::new(), version: -1, ..base.clone() });
        for o in &cases {
            for action in ["index", "create", "delete"] {
                for forced in [false, true] {
                    let mut direct = Vec::new();
                    o.write_bulk_item(&mut direct, action, forced);
                    assert_eq!(String::from_utf8(direct).unwrap(), String::from_utf8(tree_item(o, action, forced)).unwrap(), "{o:?} {action} {forced}");
                }
            }
        }
    }

    /// A raw items array reads back (pretty output, other formats, `filter_path`) as the tree
    /// it replaces.
    #[test]
    fn raw_items_materialize_to_the_tree() {
        let o = write::WriteOutcome {
            index: "i".into(),
            uuid: "u".into(),
            id: "a\"b".into(),
            shard: 0,
            version: 2,
            result: "updated",
            seq_no: 5,
            primary_term: 1,
            total: 1,
            forced_refresh: false,
            failure_store: None,
        };
        let mut b = crate::json::raw::buffer(0);
        b.push(b'[');
        o.write_bulk_item(&mut b, "index", false);
        b.push(b',');
        o.write_bulk_item(&mut b, "delete", true);
        b.push(b']');
        let mut v = crate::json::raw::from_buffer(b);
        let mut compact = Vec::new();
        write_json(&mut compact, &v, false);
        crate::json::raw::materialize(&mut v);
        let tree = crate::json::parse(&compact).unwrap().unwrap();
        assert_eq!(v, tree);
        let Value::Arr(a) = &v else { panic!() };
        assert_eq!(a.len(), 2);
        let mut t = Vec::new();
        write_json(&mut t, &v, false);
        assert_eq!(t, compact);
        assert_eq!(compact, [b"[".to_vec(), tree_item(&o, "index", false), b",".to_vec(), tree_item(&o, "delete", true), b"]".to_vec()].concat());
    }
}
