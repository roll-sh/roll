//! Fleet (`_fleet`): shard global checkpoints, searches that wait for checkpoints, and Fleet
//! secrets.
//!
//! A shard's global checkpoint is its local checkpoint (one copy per shard). `_fleet/
//! global_checkpoints` can wait for every shard to pass a given checkpoint (and for the index
//! to exist), polling the shards until the timeout. `_fleet/_fleet_search` and
//! `_fleet/_fleet_msearch` are searches whose `wait_for_checkpoints` (one per shard) must
//! already be issued; the search then sees them, which Elasticsearch gets by waiting for the
//! next refresh and roll by refreshing the index. Secrets live in `.fleet-secrets-7` on
//! Elasticsearch, here the `fleet_secret` family.

use crate::error::{EsError, EsResult};
use crate::json::{Map, Value};
use crate::meta::IndexMeta;
use crate::obj;
use crate::rest::{Ctx, Reply};
use crate::xpack::{internal, store};
use std::sync::Arc;

pub const SECRETS: &str = "fleet_secret";

/// The one index `expr` names (an index, alias or data stream write index).
fn single_index(ctx: &Ctx, expr: &str) -> EsResult<Option<Arc<IndexMeta>>> {
    let st = ctx.state();
    let opts = crate::meta::resolve::IndicesOptions::STRICT_EXPAND_OPEN;
    match crate::meta::resolve::resolve(&st, &[expr.to_string()], &opts, ctx.now_ms) {
        Ok(names) => Ok(names.first().and_then(|n| st.metadata.indices.get(n).cloned())),
        Err(e) if e.kind == "index_not_found_exception" => Ok(None),
        Err(e) => Err(e),
    }
}

/// Every shard's checkpoint (`-1` before any write).
fn checkpoints(ctx: &Ctx, idx: &IndexMeta) -> Vec<i64> {
    let n = idx.number_of_shards();
    match ctx.cluster.engine.loaded(&idx.uuid) {
        Some(e) => (0..n).map(|s| e.shard(s).shard.local_checkpoint()).collect(),
        None => vec![-1; n as usize],
    }
}

fn longs(s: &str, name: &str) -> EsResult<Vec<i64>> {
    s.split(',').filter(|x| !x.trim().is_empty()).map(|x| x.trim().parse::<i64>().map_err(|_| EsError::illegal_argument(format!("Failed to parse long parameter [{name}] with value [{x}]")))).collect()
}

/// `GET {index}/_fleet/global_checkpoints`.
pub async fn global_checkpoints(ctx: &Ctx<'_>) -> EsResult<Reply> {
    let index = ctx.path_param("index").unwrap_or_default().to_string();
    let wait_for_advance = ctx.bool_param("wait_for_advance", false)?;
    let wait_for_index = ctx.bool_param("wait_for_index", false)?;
    let wanted = ctx.query("checkpoints").map(|c| longs(c, "checkpoints")).transpose()?.unwrap_or_default();
    let timeout = ctx.time_param("timeout", crate::units::TimeValue::seconds(30))?.as_millis().max(0) as u64;
    if wait_for_index && !wait_for_advance {
        return Err(EsError::action_request_validation(&["If wait_for_index is set to true, wait_for_advance must also be set to true.".into()]));
    }
    let deadline = std::time::Instant::now() + std::time::Duration::from_millis(timeout);
    let pause = || tokio::time::sleep(std::time::Duration::from_millis(5));
    let idx = loop {
        match single_index(ctx, &index)? {
            Some(i) => break i,
            None if wait_for_index && std::time::Instant::now() < deadline => pause().await,
            None => return Err(EsError::index_not_found(&index)),
        }
    };
    let mut current = checkpoints(ctx, &idx);
    if !wait_for_advance || wanted.is_empty() {
        return Reply::ok(obj! { "timed_out" => false, "global_checkpoints" => Value::Arr(current.into_iter().map(Value::from).collect()) });
    }
    if wanted.len() != current.len() {
        return Err(EsError::status(
            400,
            format!("number of checkpoints must equal number of shards. [shard count: {}, checkpoint count: {}]", current.len(), wanted.len()),
        ));
    }
    let mut timed_out = false;
    while current.iter().zip(&wanted).any(|(c, w)| c <= w) {
        if std::time::Instant::now() >= deadline {
            timed_out = true;
            break;
        }
        pause().await;
        current = checkpoints(ctx, &idx);
    }
    Reply::ok(obj! { "timed_out" => timed_out, "global_checkpoints" => Value::Arr(current.into_iter().map(Value::from).collect()) })
}

/// A shard-level failure of a fleet search, as `all shards failed` renders it.
fn shard_failure(ctx: &Ctx, idx: &IndexMeta, shard: usize, msg: &str) -> EsError {
    let reason = obj! { "type" => "illegal_argument_exception", "reason" => msg };
    let failed = crate::arr![obj! { "shard" => shard, "index" => idx.name.clone(), "node" => ctx.cluster.node.id.clone(), "reason" => reason }];
    EsError::new(400, "search_phase_execution_exception", "all shards failed")
        .with_meta("phase", "query")
        .with_meta("grouped", true)
        .with_meta("failed_shards", failed)
        .caused_by(EsError::java(400, "illegal_argument_exception", msg).caused_by(EsError::java(400, "illegal_argument_exception", msg)))
        .with_root_causes(vec![EsError::java(400, "illegal_argument_exception", msg)])
}

/// Checks a search's `wait_for_checkpoints` on `index` and makes them visible.
fn wait_for_checkpoints(ctx: &Ctx, index: &str, wanted: &[i64]) -> EsResult<()> {
    if wanted.is_empty() {
        return Ok(());
    }
    let Some(idx) = single_index(ctx, index)? else { return Err(EsError::index_not_found(index)) };
    let current = checkpoints(ctx, &idx);
    if wanted.len() != current.len() {
        return Err(EsError::illegal_argument(format!(
            "Target configured with wait_for_checkpoints must search the same number of shards as checkpoints provided. [{}] checkpoints provided. Target [{index}] which resolved to index [{}] has [{}] shards.",
            wanted.len(),
            idx.name,
            current.len()
        )));
    }
    if crate::engine::IndexEngine::refresh_interval(&idx) < 0 {
        return Err(shard_failure(ctx, &idx, 0, "Cannot use wait_for_checkpoints with [index.refresh_interval=-1]"));
    }
    let max_seq: Vec<i64> = match ctx.cluster.engine.loaded(&idx.uuid) {
        Some(e) => (0..idx.number_of_shards()).map(|s| e.shard(s).shard.max_seq_no()).collect(),
        None => vec![-1; current.len()],
    };
    for (shard, (w, m)) in wanted.iter().zip(&max_seq).enumerate() {
        if w > m {
            return Err(shard_failure(ctx, &idx, shard, &format!("Cannot wait for unissued seqNo checkpoint [wait_for_checkpoint={w}, max_issued_seqNo={m}]")));
        }
    }
    if let Some(e) = ctx.cluster.engine.loaded(&idx.uuid) {
        e.refresh_all()?;
    }
    Ok(())
}

/// `{index}/_fleet/_fleet_search`.
pub async fn search(ctx: &Ctx<'_>) -> EsResult<Reply> {
    let index = ctx.path_param("index").unwrap_or_default().to_string();
    let wanted = ctx.query("wait_for_checkpoints").map(|c| longs(c, "wait_for_checkpoints")).transpose()?.unwrap_or_default();
    wait_for_checkpoints(ctx, &index, &wanted)?;
    crate::search::search(ctx).await
}

/// `{index}/_fleet/_fleet_msearch`: each header may carry `wait_for_checkpoints`; the searches
/// that pass run as one `_msearch`, the others answer with their error in place.
pub async fn msearch(ctx: &Ctx<'_>) -> EsResult<Reply> {
    let default_index = ctx.path_param("index").map(str::to_string);
    let text = std::str::from_utf8(&ctx.body).map_err(|_| EsError::illegal_argument("msearch body must be UTF-8"))?;
    let lines: Vec<&str> = text.split('\n').filter(|l| !l.trim().is_empty()).collect();
    let mut forward = String::new();
    let mut slots: Vec<Result<(), EsError>> = Vec::new();
    for pair in lines.chunks(2) {
        let mut header = match crate::json::parse(pair[0].as_bytes())? {
            Some(Value::Obj(m)) => m,
            _ => Map::new(),
        };
        let wanted: Vec<i64> = match header.shift_remove("wait_for_checkpoints") {
            Some(Value::Arr(a)) => a.iter().filter_map(Value::as_i64).collect(),
            Some(Value::Num(n)) => n.as_i64().into_iter().collect(),
            _ => Vec::new(),
        };
        header.shift_remove("wait_for_checkpoints_timeout");
        let index = header.get("index").map(Value::to_es_string).or_else(|| default_index.clone()).unwrap_or_default();
        match wait_for_checkpoints(ctx, &index, &wanted) {
            Ok(()) => {
                if !header.contains_key("index") && !index.is_empty() {
                    header.insert("index".into(), index.into());
                }
                forward.push_str(&Value::Obj(header).to_json());
                forward.push('\n');
                forward.push_str(pair.get(1).copied().unwrap_or("{}"));
                forward.push('\n');
                slots.push(Ok(()));
            }
            Err(e) => slots.push(Err(e)),
        }
    }
    let mut answered: std::vec::IntoIter<Value> = if forward.is_empty() {
        Vec::new().into_iter()
    } else {
        let r = internal::call_ndjson(ctx.cluster, "POST", "/_msearch", forward).await?;
        r.get("responses").and_then(Value::as_array).cloned().unwrap_or_default().into_iter()
    };
    let mut responses = Vec::with_capacity(slots.len());
    for s in slots {
        responses.push(match s {
            Ok(()) => answered.next().unwrap_or(Value::Null),
            Err(e) => obj! { "error" => e.to_value_with_root(false), "status" => i64::from(e.status) },
        });
    }
    Reply::ok(obj! { "took" => 0, "responses" => Value::Arr(responses) })
}

/// `POST _fleet/secret`.
pub async fn post_secret(ctx: &Ctx<'_>) -> EsResult<Reply> {
    let body = ctx.body_map()?.unwrap_or_default();
    if let Some(k) = body.keys().find(|k| *k != "value") {
        return Err(crate::xpack::unknown_field(ctx, "post_secret_request", k));
    }
    let value = match body.get("value") {
        None | Some(Value::Null) => return Err(EsError::action_request_validation(&["value is missing".into()])),
        Some(v @ Value::Str(_)) => v.clone(),
        Some(v) => {
            let (l, c) = crate::xpack::value_loc(ctx, "value");
            let token = match v {
                Value::Arr(_) => "START_ARRAY",
                Value::Obj(_) => "START_OBJECT",
                Value::Num(_) => "VALUE_NUMBER",
                _ => "VALUE_BOOLEAN",
            };
            return Err(EsError::x_content_parse(l, c, format!("[post_secret_request] value doesn't support values of type: {token}")));
        }
    };
    let id = crate::meta::names::time_based_id();
    let out = id.clone();
    let now = ctx.now_ms;
    ctx.cluster.update(move |st| store::put(st, SECRETS, &id, obj! { "value" => value }, now).map(|_| ())).await?;
    Reply::ok(obj! { "id" => out })
}

fn no_secret(id: &str) -> EsError {
    EsError::resource_not_found(format!("No secret with id [{id}]"))
}

/// `GET _fleet/secret/{id}`.
pub fn get_secret(ctx: &Ctx) -> EsResult<Reply> {
    let id = ctx.path_param("id").unwrap_or_default();
    match ctx.state().metadata.xpack.get(SECRETS, id) {
        Some(s) => Reply::ok(obj! { "id" => id, "value" => s.get("value").cloned().unwrap_or(Value::Null) }),
        None => Err(no_secret(id)),
    }
}

/// `DELETE _fleet/secret/{id}`.
pub async fn delete_secret(ctx: &Ctx<'_>) -> EsResult<Reply> {
    let id = ctx.path_param("id").unwrap_or_default().to_string();
    ctx.cluster.update(move |st| store::remove(st, SECRETS, &id).map(|_| ()).ok_or_else(|| no_secret(&id))).await?;
    Reply::ok(obj! { "deleted" => true })
}
