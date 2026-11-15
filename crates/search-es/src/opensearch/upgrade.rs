//! `POST /{index}/_upgrade` and `GET /{index}/_upgrade`: OpenSearch's (deprecated) index
//! upgrade API.
//!
//! Upgrading rewrites segments written by an older Lucene. Every segment roll writes is in the
//! current format, so there is never anything to rewrite: the upgrade answers what OpenSearch
//! answers for already-current indices (`upgrade_version` = this version,
//! `oldest_lucene_segment_version` = the Lucene version roll reports), and the status reports
//! each index's size with nothing left to upgrade. Like OpenSearch's broadcast-by-node
//! actions, `_shards` counts every shard copy of the targeted open indices.

use crate::api::resolve_indices;
use crate::error::EsResult;
use crate::json::{Map, Value};
use crate::meta::IndexMeta;
use crate::meta::resolve::IndicesOptions;
use crate::obj;
use crate::rest::{Ctx, Reply};
use std::sync::Arc;

fn targets(ctx: &Ctx) -> EsResult<Vec<Arc<IndexMeta>>> {
    let st = ctx.state();
    let names = resolve_indices(ctx, IndicesOptions::STRICT_EXPAND_OPEN_FORBID_CLOSED)?;
    Ok(names.iter().filter_map(|n| st.metadata.indices.get(n)).filter(|i| !i.is_closed()).cloned().collect())
}

/// Bytes of an index's primaries and replicas (replicas live on the same storage, so the
/// primaries' segment bytes).
fn size_in_bytes(ctx: &Ctx, idx: &IndexMeta) -> i64 {
    ctx.cluster.engine.loaded(&idx.uuid).map(|e| e.shards.iter().map(|s| s.shard.stats().segment_bytes as i64).sum()).unwrap_or(0)
}

/// `POST _upgrade`.
pub fn upgrade(ctx: &Ctx) -> EsResult<Reply> {
    // Accepted and validated; with nothing to rewrite they change nothing.
    ctx.bool_param("wait_for_completion", false)?;
    ctx.bool_param("only_ancient_segments", false)?;
    let indices = targets(ctx)?;
    let mut copies = 0u64;
    let mut upgraded = Map::new();
    for idx in &indices {
        copies += u64::from(idx.number_of_shards()) * (1 + u64::from(idx.number_of_replicas()));
        upgraded.insert(
            idx.name.clone(),
            obj! { "upgrade_version" => crate::version::OPENSEARCH_VERSION, "oldest_lucene_segment_version" => crate::version::OPENSEARCH_LUCENE_VERSION },
        );
    }
    Reply::ok(obj! {
        "_shards" => obj! { "total" => copies, "successful" => copies, "failed" => 0 },
        "upgraded_indices" => Value::Obj(upgraded),
    })
}

/// `GET _upgrade`: the bytes left to upgrade (always none).
pub fn status(ctx: &Ctx) -> EsResult<Reply> {
    let indices = targets(ctx)?;
    let mut total = 0i64;
    let mut per = Map::new();
    for idx in &indices {
        let size = size_in_bytes(ctx, idx);
        total += size;
        per.insert(idx.name.clone(), obj! { "size_in_bytes" => size, "size_to_upgrade_in_bytes" => 0, "size_to_upgrade_ancient_in_bytes" => 0 });
    }
    Reply::ok(obj! {
        "size_in_bytes" => total,
        "size_to_upgrade_in_bytes" => 0,
        "size_to_upgrade_ancient_in_bytes" => 0,
        "indices" => Value::Obj(per),
    })
}
