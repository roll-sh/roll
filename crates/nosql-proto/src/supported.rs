//! SUPPORTED: the `[string multimap]` answer to OPTIONS, and the exact maps each reference
//! sends.
//!
//! Captured from the images (any protocol version, before and after STARTUP):
//!
//! - `cassandra:5.0.9`, in this order (Java `HashMap` iteration):
//!   `PROTOCOL_VERSIONS [3/v3, 4/v4, 5/v5, 6/v6-beta]`, `COMPRESSION [snappy, lz4]`,
//!   `CQL_VERSION [3.4.7]`.
//! - `scylladb/scylla:2026.3.3` (`--smp 1`), keys sorted: `CLIENT_OPTIONS ['']`,
//!   `COMPRESSION [lz4, snappy]`, `CQL_VERSION [3.3.1]`, `SCYLLA_HOST_ID [<uuid>]`,
//!   `SCYLLA_LWT_ADD_METADATA_MARK [LWT_OPTIMIZATION_META_BIT_MASK=2147483648]`,
//!   `SCYLLA_NR_SHARDS [1]`, `SCYLLA_PARTITIONER [org.apache.cassandra.dht.Murmur3Partitioner]`,
//!   `SCYLLA_RATE_LIMIT_ERROR [ERROR_CODE=61440]`, `SCYLLA_SHARD [0]`,
//!   `SCYLLA_SHARDING_ALGORITHM [biased-token-round-robin]`, `SCYLLA_SHARDING_IGNORE_MSB [12]`,
//!   `SCYLLA_SHARD_AWARE_PORT [19042]`, `SCYLLA_USE_METADATA_ID ['']`,
//!   `TABLETS_ROUTING_V1 ['']`. No `PROTOCOL_VERSIONS`.

use crate::version::{Features, Version};

/// A SUPPORTED body, in wire order.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Supported(pub Vec<(String, Vec<String>)>);

impl Supported {
    pub fn get(&self, key: &str) -> Option<&[String]> {
        self.0.iter().find(|(k, _)| k == key).map(|(_, v)| v.as_slice())
    }

    pub fn has(&self, key: &str) -> bool {
        self.get(key).is_some()
    }

    /// The `KEY=VALUE` parameter of an extension (`SCYLLA_RATE_LIMIT_ERROR` → `ERROR_CODE`).
    pub fn param(&self, key: &str, name: &str) -> Option<&str> {
        self.get(key)?.iter().find_map(|v| v.strip_prefix(name)?.strip_prefix('='))
    }

    /// Versions listed in PROTOCOL_VERSIONS (`4/v4` → 4).
    pub fn protocol_versions(&self) -> Vec<Version> {
        self.get("PROTOCOL_VERSIONS")
            .unwrap_or_default()
            .iter()
            .filter_map(|v| Version::from_u8(v.split('/').next()?.parse().ok()?))
            .collect()
    }

    /// The ScyllaDB extensions a client can turn on by echoing them in STARTUP.
    pub fn scylla_features(&self) -> Features {
        Features {
            metadata_id: self.has("SCYLLA_USE_METADATA_ID"),
            tablets_v2: false,
            rate_limit_code: self.param("SCYLLA_RATE_LIMIT_ERROR", "ERROR_CODE").and_then(|c| c.parse().ok()),
        }
    }
}

fn list(v: &[&str]) -> Vec<String> {
    v.iter().map(|s| s.to_string()).collect()
}

/// Cassandra 5.0.9's map.
pub fn cassandra() -> Supported {
    Supported(vec![
        ("PROTOCOL_VERSIONS".into(), list(&["3/v3", "4/v4", "5/v5", "6/v6-beta"])),
        ("COMPRESSION".into(), list(&["snappy", "lz4"])),
        ("CQL_VERSION".into(), list(&["3.4.7"])),
    ])
}

/// The node and shard facts in ScyllaDB's map.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ScyllaNode {
    pub host_id: String,
    pub shard: u32,
    pub nr_shards: u32,
    pub ignore_msb: u32,
    pub shard_aware_port: Option<u16>,
    pub shard_aware_port_ssl: Option<u16>,
    /// `TABLETS_ROUTING_V1` (keyspaces may use tablets).
    pub tablets: bool,
    /// `TABLETS_ROUTING_V2_EXPERIMENTAL` (only with strongly consistent tables enabled).
    pub tablets_v2: bool,
}

impl ScyllaNode {
    /// The reference image's values (one shard, ignore_msb 12, shard-aware port 19042).
    pub fn new(host_id: impl Into<String>) -> Self {
        ScyllaNode {
            host_id: host_id.into(),
            shard: 0,
            nr_shards: 1,
            ignore_msb: 12,
            shard_aware_port: Some(19042),
            shard_aware_port_ssl: None,
            tablets: true,
            tablets_v2: false,
        }
    }
}

/// ScyllaDB 2026.3's map for a connection on `node`'s shard.
pub fn scylla(node: &ScyllaNode) -> Supported {
    let mut m: Vec<(String, Vec<String>)> = vec![
        ("CLIENT_OPTIONS".into(), list(&[""])),
        ("COMPRESSION".into(), list(&["lz4", "snappy"])),
        ("CQL_VERSION".into(), list(&["3.3.1"])),
        ("SCYLLA_HOST_ID".into(), vec![node.host_id.clone()]),
        ("SCYLLA_LWT_ADD_METADATA_MARK".into(), list(&["LWT_OPTIMIZATION_META_BIT_MASK=2147483648"])),
        ("SCYLLA_NR_SHARDS".into(), vec![node.nr_shards.to_string()]),
        ("SCYLLA_PARTITIONER".into(), list(&["org.apache.cassandra.dht.Murmur3Partitioner"])),
        ("SCYLLA_RATE_LIMIT_ERROR".into(), list(&["ERROR_CODE=61440"])),
        ("SCYLLA_SHARD".into(), vec![node.shard.to_string()]),
        ("SCYLLA_SHARDING_ALGORITHM".into(), list(&["biased-token-round-robin"])),
        ("SCYLLA_SHARDING_IGNORE_MSB".into(), vec![node.ignore_msb.to_string()]),
        ("SCYLLA_USE_METADATA_ID".into(), list(&[""])),
    ];
    if let Some(p) = node.shard_aware_port {
        m.push(("SCYLLA_SHARD_AWARE_PORT".into(), vec![p.to_string()]));
    }
    if let Some(p) = node.shard_aware_port_ssl {
        m.push(("SCYLLA_SHARD_AWARE_PORT_SSL".into(), vec![p.to_string()]));
    }
    if node.tablets {
        m.push(("TABLETS_ROUTING_V1".into(), list(&[""])));
    }
    if node.tablets_v2 {
        m.push(("TABLETS_ROUTING_V2_EXPERIMENTAL".into(), list(&[""])));
    }
    m.sort_by(|a, b| a.0.cmp(&b.0));
    Supported(m)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::error_body::code;
    use crate::frame::Flags;
    use crate::response::{Response, ResponseFrame};
    use crate::wire::PutExt;
    use bytes::BytesMut;

    #[test]
    fn scylla_map_matches_the_image_bytes() {
        // OPTIONS at v4 against scylladb/scylla:2026.3.3 (stream 0x0102), the full body.
        let host = "c582a68c-31eb-4a66-a6cc-5136156e23fe";
        let mut expected = BytesMut::new();
        expected.put_string_multimap(&[
            ("CLIENT_OPTIONS", vec![""]),
            ("COMPRESSION", vec!["lz4", "snappy"]),
            ("CQL_VERSION", vec!["3.3.1"]),
            ("SCYLLA_HOST_ID", vec![host]),
            ("SCYLLA_LWT_ADD_METADATA_MARK", vec!["LWT_OPTIMIZATION_META_BIT_MASK=2147483648"]),
            ("SCYLLA_NR_SHARDS", vec!["1"]),
            ("SCYLLA_PARTITIONER", vec!["org.apache.cassandra.dht.Murmur3Partitioner"]),
            ("SCYLLA_RATE_LIMIT_ERROR", vec!["ERROR_CODE=61440"]),
            ("SCYLLA_SHARD", vec!["0"]),
            ("SCYLLA_SHARDING_ALGORITHM", vec!["biased-token-round-robin"]),
            ("SCYLLA_SHARDING_IGNORE_MSB", vec!["12"]),
            ("SCYLLA_SHARD_AWARE_PORT", vec!["19042"]),
            ("SCYLLA_USE_METADATA_ID", vec![""]),
            ("TABLETS_ROUTING_V1", vec![""]),
        ]);
        assert_eq!(expected.len(), 532, "the captured body length");
        let f = ResponseFrame::new(0x0102, Response::Supported(scylla(&ScyllaNode::new(host)))).to_frame(Version::V4);
        assert_eq!(f.body, expected.freeze());
        assert_eq!(f.flags, Flags(0));
    }

    #[test]
    fn helpers() {
        let c = cassandra();
        assert_eq!(c.protocol_versions(), vec![Version::V3, Version::V4, Version::V5, Version::V6]);
        assert_eq!(c.scylla_features(), Features::default());
        let s = scylla(&ScyllaNode { shard_aware_port_ssl: Some(19142), tablets_v2: true, ..ScyllaNode::new("h") });
        assert!(s.protocol_versions().is_empty());
        assert_eq!(s.param("SCYLLA_RATE_LIMIT_ERROR", "ERROR_CODE"), Some("61440"));
        let f = s.scylla_features();
        assert!(f.metadata_id);
        assert_eq!(f.rate_limit_code, Some(code::SCYLLA_RATE_LIMIT));
        assert!(s.has("SCYLLA_SHARD_AWARE_PORT_SSL") && s.has("TABLETS_ROUTING_V2_EXPERIMENTAL"));
        let keys: Vec<_> = s.0.iter().map(|(k, _)| k.clone()).collect();
        let mut sorted = keys.clone();
        sorted.sort();
        assert_eq!(keys, sorted);
    }
}
