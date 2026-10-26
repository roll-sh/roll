//! OpenSearch's point-in-time API: `POST /{index}/_search/point_in_time`, `GET` and `DELETE
//! /_search/point_in_time/_all`, `DELETE /_search/point_in_time`.
//!
//! The contexts are the same ones Elasticsearch's `_pit` opens ([`crate::search::contexts`]):
//! the shard searchers of the moment the PIT was created, kept until their keep alive passes
//! (extended by every search that names the PIT). OpenSearch differs in the shape of the
//! responses (`pit_id`, `creation_time`, per-id `successful`), in its limit setting
//! (`point_in_time.max_keep_alive`, not `search.max_keep_alive`) and in how it reports ids it
//! can't read (as an unsupported transport version of the id's first bytes).

use crate::error::{EsError, EsResult};
use crate::json::Value;
use crate::obj;
use crate::rest::{Ctx, Reply};
use crate::search::contexts::SearchContext;
use crate::units::TimeValue;
use std::time::{Duration, Instant};

/// `PitService`/`SearchService` keep alive limit: `point_in_time.max_keep_alive` (24h).
fn check_keep_alive(ctx: &Ctx, ka: TimeValue) -> EsResult<Duration> {
    let st = ctx.state();
    let max = st
        .metadata
        .cluster_setting("point_in_time.max_keep_alive")
        .and_then(|v| TimeValue::parse(v, "point_in_time.max_keep_alive").ok())
        .unwrap_or(TimeValue::hours(24));
    if ka.as_millis() > max.as_millis() {
        return Err(EsError::illegal_argument(format!(
            "Keep alive for request ({}) is too large. It must be less than ({}). This limit can be set by changing the [point_in_time.max_keep_alive] cluster level setting.",
            ka.string_rep(),
            max.string_rep()
        )));
    }
    Ok(Duration::from_millis(ka.as_millis().max(0) as u64))
}

/// `POST /{index}/_search/point_in_time?keep_alive=..`.
pub fn create(ctx: &Ctx) -> EsResult<Reply> {
    ctx.bool_param("allow_partial_pit_creation", true)?;
    let Some(ka) = ctx.query("keep_alive") else {
        return Err(EsError::action_request_validation(&["keep alive not specified".into()]));
    };
    let ka_value = TimeValue::parse(ka, "keep_alive")?;
    let ka = check_keep_alive(ctx, ka_value)?;
    let mut req = crate::search::parse_request(ctx, None, None)?;
    req.source = Default::default();
    let st = ctx.state();
    let mut targets = crate::search::shard_targets(ctx.cluster, &st, &req)?;
    for t in targets.iter_mut() {
        t.searcher = Some(t.searcher()?);
    }
    let n = targets.len() as i64;
    let created = ctx.now_ms;
    let id = ctx.cluster.contexts.open(SearchContext {
        targets,
        request: None,
        expires: Instant::now() + ka,
        pit: true,
        offset: 0,
        indices: req.indices.clone(),
        created_ms: created,
        keep_alive_ms: ka_value.as_millis(),
    });
    Reply::ok(obj! {
        "pit_id" => id,
        "_shards" => obj! { "total" => n, "successful" => n, "skipped" => 0, "failed" => 0 },
        "creation_time" => created,
    })
}

/// `GET /_search/point_in_time/_all`.
pub fn list(ctx: &Ctx) -> EsResult<Reply> {
    let pits: Vec<Value> = ctx
        .cluster
        .contexts
        .pits()
        .into_iter()
        .map(|(id, created, keep_alive)| obj! { "pit_id" => id, "creation_time" => created, "keep_alive" => keep_alive })
        .collect();
    Reply::ok(obj! { "pits" => Value::Arr(pits) })
}

/// `DELETE /_search/point_in_time` with `{"pit_id": [..]}` (or one id as a string). Ids that
/// are well formed but no longer open are reported as deleted too, as OpenSearch reports them.
pub fn delete(ctx: &Ctx) -> EsResult<Reply> {
    let body = ctx.body_map()?.unwrap_or_default();
    let ids: Vec<String> = match body.get("pit_id") {
        Some(Value::Arr(a)) => a.iter().map(Value::to_es_string).collect(),
        Some(Value::Null) | None => Vec::new(),
        Some(v) => vec![v.to_es_string()],
    };
    if ids.is_empty() {
        return Err(EsError::action_request_validation(&["no pit ids specified".into()]));
    }
    for id in &ids {
        check_id(id)?;
    }
    let pits: Vec<Value> = ids
        .into_iter()
        .map(|id| {
            ctx.cluster.contexts.close(&id);
            obj! { "successful" => true, "pit_id" => id }
        })
        .collect();
    Reply::ok(obj! { "pits" => Value::Arr(pits) })
}

/// `DELETE /_search/point_in_time/_all`.
pub fn delete_all(ctx: &Ctx) -> EsResult<Reply> {
    let pits: Vec<Value> = ctx.cluster.contexts.close_pits().into_iter().map(|id| obj! { "successful" => true, "pit_id" => id }).collect();
    Reply::ok(obj! { "pits" => Value::Arr(pits) })
}

/// Rejects ids this node can't have minted the way `SearchContextId.decode` does in
/// OpenSearch: URL-safe base64 whose first bytes are a transport version (a vint); a version
/// no node speaks is a 500 `unsupported_version_exception` naming it.
pub fn check_id(id: &str) -> EsResult<()> {
    use base64::Engine as _;
    if let Some(msg) = crate::units::java_base64_url_error(id) {
        return Err(EsError::illegal_argument(msg));
    }
    let bytes = base64::engine::general_purpose::URL_SAFE_NO_PAD.decode(id.trim_end_matches('=')).map_err(|e| EsError::illegal_argument(e.to_string()))?;
    if bytes.starts_with(b"\x01pit") {
        return Ok(());
    }
    let mut version: u32 = 0;
    for (i, b) in bytes.iter().take(5).enumerate() {
        version |= u32::from(b & 0x7f) << (7 * i);
        if b & 0x80 == 0 {
            break;
        }
    }
    Err(EsError::new(500, "unsupported_version_exception", format!("Unsupported version [{}]", version_name(version as i32))))
}

/// `Version.toString` of a transport version id OpenSearch doesn't know: ids without
/// OpenSearch's mask bit are legacy Elasticsearch versions (`ES 0.0.1`).
fn version_name(id: i32) -> String {
    const MASK: i32 = 0x0800_0000;
    let (prefix, v) = if id & MASK != 0 { ("", id ^ MASK) } else { ("ES ", id) };
    format!("{prefix}{}.{}.{}", (v / 1_000_000) % 100, (v / 10_000) % 100, (v / 100) % 100)
}

#[cfg(test)]
mod tests {
    use super::*;

    // OpenSearch 3.9.0's answers for the same ids.
    #[test]
    fn foreign_ids_like_opensearch() {
        assert_eq!(check_id("abc").unwrap_err().reason(), "Unsupported version [ES 0.0.1]");
        assert_eq!(check_id("abc").unwrap_err().status, 500);
        assert!(check_id("AXBpdAAAAAAAAAAB").is_ok());
        assert_eq!(version_name(0x0800_0000 ^ 3_09_00_99), "3.9.0");
    }
}
