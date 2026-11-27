//! `cluster.max_shards_per_node` (Elasticsearch's and OpenSearch's `ShardLimitValidator`): an
//! action that would open more shards than the cluster allows is refused before it changes
//! anything. Creating an index (explicitly, by auto-create, a data stream's backing index, a
//! rollover) and opening closed ones are checked.
//!
//! A cluster is one node here, so its limit is the setting itself (default 1000). Shards are
//! counted as both do: an open index's primaries times one plus its replicas, whether or not the
//! replicas are assigned; closed indices don't count. The setting is dynamic and needs the
//! cluster privilege to change, so a key that may only create documents (an `otel` or `append`
//! key) can't raise it, and can't make a cluster hold more than it allows.

use super::{ClusterState, IndexMeta, settings::Settings};
use crate::error::{EsError, EsResult};

/// The setting's default in both products.
pub const DEFAULT_MAX_SHARDS_PER_NODE: u64 = 1000;

/// Shards an index of these settings opens: its primaries times one plus its replicas.
pub fn shards_of(settings: &Settings) -> u64 {
    let n = |k: &str, d: u64| settings.get(k).and_then(|v| v.parse::<u64>().ok()).unwrap_or(d);
    n("index.number_of_shards", 1).saturating_mul(n("index.number_of_replicas", 1).saturating_add(1))
}

fn index_shards(i: &IndexMeta) -> u64 {
    if i.is_closed() { 0 } else { shards_of(&i.settings) }
}

/// Shards open in the cluster now.
pub fn open_shards(st: &ClusterState) -> u64 {
    st.metadata.indices.values().map(|i| index_shards(i)).fold(0u64, u64::saturating_add)
}

/// The cluster's limit: `cluster.max_shards_per_node` times its one node. A value the setting's
/// own validation lets through but that isn't positive (OpenSearch allows -1) means the default.
pub fn limit(st: &ClusterState) -> u64 {
    st.metadata
        .cluster_setting("cluster.max_shards_per_node")
        .and_then(|v| v.parse::<i64>().ok())
        .filter(|n| *n > 0)
        .map_or(DEFAULT_MAX_SHARDS_PER_NODE, |n| n as u64)
}

/// Refuses an action that would add `new` shards past the limit, with each product's message.
pub fn check_adding(st: &ClusterState, new: u64) -> EsResult<()> {
    if new == 0 {
        return Ok(());
    }
    let (current, max) = (open_shards(st), limit(st));
    if current.saturating_add(new) <= max {
        return Ok(());
    }
    let msg = match st.dialect {
        crate::cluster::Dialect::OpenSearch => {
            format!("this action would add [{new}] total shards, but this cluster currently has [{current}]/[{max}] maximum shards open")
        }
        crate::cluster::Dialect::Elasticsearch => {
            format!("this action would add [{new}] shards, but this cluster currently has [{current}]/[{max}] maximum normal shards open")
        }
    };
    Err(EsError::new(400, "validation_exception", format!("Validation Failed: 1: {msg};")))
}

/// Refuses putting an index of `settings` at `name` (a restore, a resize or downsample target)
/// when the shards it adds, net of an open index it replaces, pass the limit.
pub fn check_putting(st: &ClusterState, name: &str, settings: &Settings) -> EsResult<()> {
    let replaced = st.metadata.indices.get(name).map_or(0, |i| index_shards(i));
    check_adding(st, shards_of(settings).saturating_sub(replaced))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shards_count_primaries_and_replicas() {
        let mut s = Settings::default();
        assert_eq!(shards_of(&s), 2, "1 primary and 1 replica by default");
        s.set("index.number_of_shards", "3");
        s.set("index.number_of_replicas", "0");
        assert_eq!(shards_of(&s), 3);
        s.set("index.number_of_replicas", "2");
        assert_eq!(shards_of(&s), 9);
    }
}
