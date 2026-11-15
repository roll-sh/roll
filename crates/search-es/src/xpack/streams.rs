//! Streams: `POST _streams/<type>/_enable|_disable` and `GET _streams/status`, plus the write
//! restrictions an enabled stream imposes.
//!
//! Elasticsearch 9.5.5 knows three stream types (`logs`, `logs.ecs`, `logs.otel`) and keeps
//! whether each is enabled in the cluster state; enabling one doesn't create anything until
//! data is routed to it, and fails (409) while indices of its name or under its prefix exist.
//! roll keeps the same flags in the cluster metadata.
//!
//! Once a stream is enabled, writes are restricted like `TransportAbstractBulkAction` and
//! `RestIndexAction` do: a document may not target a child (`<stream>.<x>`) directly, a write
//! to the stream itself may name no pipeline and only a few request parameters, and an ingest
//! pipeline may not reroute a document into a child stream it didn't come from. These checks
//! sit on the write path, so they bail out on the first byte (`l`) for every other index.

use crate::error::{EsError, EsResult};
use crate::json::{Map, Value};
use crate::meta::ClusterState;
use crate::obj;
use crate::rest::{Ctx, Reply};
use crate::xpack::store;

const STREAMS: &str = "streams";
/// `StreamType.values()` order.
const TYPES: [&str; 3] = ["logs", "logs.ecs", "logs.otel"];

/// `TransportAbstractBulkAction.STREAMS_ALLOWED_PARAMS`, in the order its `HashSet` prints.
const ALLOWED_PARAMS: [&str; 13] = [
    "pretty",
    "include_source_on_error",
    "index",
    "refresh",
    "serverlessRequest",
    "operatorRequest",
    "error_trace",
    "require_data_stream",
    "timeout",
    "_slice",
    "id",
    "op_type",
    "filter_path",
];

pub async fn set_enabled(ctx: &Ctx<'_>, enabled: bool) -> EsResult<Reply> {
    ctx.validate_timeouts()?;
    let name = ctx.path_param("name").unwrap_or_default().to_string();
    if !TYPES.contains(&name.as_str()) {
        return Err(EsError::illegal_argument(format!("Unknown stream type [{name}]")));
    }
    if name == "logs" {
        ctx.warn("The 'logs' stream is deprecated. Use individual stream names such as 'logs.otel' and 'logs.ecs' instead");
    }
    let now = ctx.now_ms;
    ctx.cluster
        .update(move |st| {
            if enabled {
                if st.metadata.xpack.get(STREAMS, &name).is_some() {
                    return Ok(());
                }
                let prefix = format!("{name}.");
                let clash = |n: &String| *n == name || n.starts_with(&prefix);
                if st.metadata.indices.keys().any(clash) || st.metadata.data_streams.keys().any(clash) {
                    return Err(EsError::new(
                        409,
                        "status_exception",
                        format!("Cannot enable streams: indices named '{name}' or starting with '{name}.' already exist."),
                    ));
                }
                store::put(st, STREAMS, &name, obj! { "enabled" => true }, now).map(|_| ())
            } else {
                store::remove(st, STREAMS, &name);
                Ok(())
            }
        })
        .await?;
    Reply::acknowledged()
}

pub fn status(ctx: &Ctx) -> EsResult<Reply> {
    let st = ctx.state();
    let out: Map = TYPES.iter().map(|t| (t.to_string(), obj! { "enabled" => enabled(&st, t) })).collect();
    Reply::ok(Value::Obj(out))
}

fn enabled(st: &ClusterState, ty: &str) -> bool {
    st.metadata.xpack.get(STREAMS, ty).is_some()
}

/// Whether `index` could be a stream or one of its children (all of them start with `logs`).
#[inline]
fn candidate(index: &str) -> bool {
    index.starts_with("logs")
}

/// `StreamType.exactEnabledStreamMatch`: the enabled stream named `index`.
pub fn exact_enabled(st: &ClusterState, index: &str) -> Option<&'static str> {
    if !candidate(index) {
        return None;
    }
    TYPES.into_iter().find(|t| *t == index && enabled(st, t))
}

/// `StreamType.enabledParentStreamOf`: the enabled stream `index` is a child of (the most
/// specific one), unless `index` is itself an enabled stream.
pub fn enabled_parent(st: &ClusterState, index: &str) -> Option<&'static str> {
    if !candidate(index) || exact_enabled(st, index).is_some() {
        return None;
    }
    let mut parent = None;
    for t in TYPES {
        if index.len() > t.len() && index.starts_with(t) && index.as_bytes()[t.len()] == b'.' && enabled(st, t) {
            // `logs` only when no finer-grained stream matches.
            if parent.is_none() || t != "logs" {
                parent = Some(t);
            }
        }
    }
    parent
}

fn params_error(used: &[&str]) -> EsError {
    EsError::illegal_argument(format!(
        "When writing to a stream, only the following parameters are allowed: [{}] however the following were used: [{}]",
        ALLOWED_PARAMS.join(", "),
        java_hash_order(used).join(", ")
    ))
}

/// The request's parameter names (path and query, without repeats) and whether any of them is
/// outside [`ALLOWED_PARAMS`].
fn used_params<'a>(ctx: &'a Ctx<'_>) -> (Vec<&'a str>, bool) {
    let mut used: Vec<&str> = Vec::new();
    for k in ctx.route.params.iter().map(|(k, _)| *k).chain(ctx.query_names()) {
        if !used.contains(&k) {
            used.push(k);
        }
    }
    let restricted = used.iter().any(|k| !ALLOWED_PARAMS.contains(k));
    (used, restricted)
}

/// `RestIndexAction.validateStreamsParamRestrictions`: a single-document write to an enabled
/// stream with a parameter outside the allowed ones.
pub fn check_index_request(ctx: &Ctx, index: &str, op_type_added: bool) -> EsResult<()> {
    if exact_enabled(&ctx.state(), index).is_none() {
        return Ok(());
    }
    let (mut used, restricted) = used_params(ctx);
    if restricted {
        // The auto-id handler adds `op_type=create` to the parameters first.
        if op_type_added && !used.contains(&"op_type") {
            used.push("op_type");
        }
        return Err(params_error(&used));
    }
    Ok(())
}

/// `TransportAbstractBulkAction`'s per-item checks for an index/create item targeting `index`
/// with `pipeline` (the item's or the request's).
pub fn check_item(ctx: &Ctx, index: &str, pipeline: Option<&str>) -> Option<EsError> {
    if !candidate(index) {
        return None;
    }
    let st = ctx.state();
    if let Some(parent) = enabled_parent(&st, index) {
        return Some(EsError::illegal_argument(format!("Direct writes to child streams are prohibited. Index directly into the [{parent}] stream instead")));
    }
    let stream = exact_enabled(&st, index)?;
    if let Some(p) = pipeline {
        return Some(EsError::illegal_argument(format!(
            "Cannot provide a pipeline when writing to a stream however the [{p}] pipeline was provided when writing to the [{stream}] stream"
        )));
    }
    let (used, restricted) = used_params(ctx);
    restricted.then(|| params_error(&used))
}

/// `IngestService`'s check when a pipeline changed a document's `_index`: moving it into a
/// child stream is only allowed when the document already went through that stream.
pub fn check_reroute(st: &ClusterState, pipeline: &str, original: &str, new_index: &str, history: &[String]) -> Option<EsError> {
    let parent = enabled_parent(st, new_index)?;
    if history.iter().any(|h| h == parent) {
        return None;
    }
    Some(EsError::illegal_argument(format!(
        "Pipeline [{pipeline}] can't change the target index (from [{original}] to [{parent}] child stream [{new_index}]) History: [{}]",
        history.join(", ")
    )))
}

/// `String.hashCode` over UTF-16 code units.
fn java_string_hash(s: &str) -> i32 {
    let mut h: i32 = 0;
    for u in s.encode_utf16() {
        h = h.wrapping_mul(31).wrapping_add(u as i32);
    }
    h
}

/// The order a `java.util.HashMap` / `HashSet` with default capacity iterates `keys` inserted
/// in this order: by bucket (`(h ^ h >>> 16) & (n - 1)`, the table doubling from 16 once it
/// holds more than three quarters), insertion order within a bucket. Elasticsearch prints
/// request parameter sets this way.
pub fn java_hash_order<'a>(keys: &[&'a str]) -> Vec<&'a str> {
    let mut n = 16usize;
    while keys.len() > n * 3 / 4 {
        n *= 2;
    }
    let bucket = |k: &str| {
        let h = java_string_hash(k);
        ((h ^ ((h as u32) >> 16) as i32) as u32 as usize) & (n - 1)
    };
    let mut out: Vec<(usize, usize, &str)> = keys.iter().enumerate().map(|(i, k)| (bucket(k), i, *k)).collect();
    out.sort_unstable();
    out.into_iter().map(|(_, _, k)| k).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hash_order_matches_java() {
        // The allowed-parameter set as Elasticsearch prints it, from its insertion order.
        let inserted = [
            "error_trace",
            "filter_path",
            "id",
            "index",
            "op_type",
            "pretty",
            "refresh",
            "require_data_stream",
            "_slice",
            "timeout",
            "include_source_on_error",
            "serverlessRequest",
            "operatorRequest",
        ];
        assert_eq!(java_hash_order(&inserted), ALLOWED_PARAMS.to_vec());
        // `[routing, index, op_type]`, as a real 9.5.5 printed the used parameters.
        assert_eq!(java_hash_order(&["index", "routing", "op_type"]), vec!["routing", "index", "op_type"]);
    }

    #[test]
    fn java_hash() {
        assert_eq!(java_string_hash(""), 0);
        assert_eq!(java_string_hash("index"), 100_346_066);
        assert_eq!(java_string_hash("routing"), 1_385_652_422);
    }
}
