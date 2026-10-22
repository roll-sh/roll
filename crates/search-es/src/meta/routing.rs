//! Document routing (`IndexRouting`): which shard a routing value (or `_id`) maps to.
//!
//! Elasticsearch hashes the routing string's UTF-16 code units (little-endian bytes) with
//! Murmur3 x86 32-bit, seed 0, then takes `floorMod(hash, number_of_shards)`. Indices created
//! before index version 9.0.64 (none created by roll) used `floorMod(hash, routing_num_shards)
//! / routing_factor` instead. With `index.routing_partition_size` > 1 a routing value selects a
//! range of shards and the `_id` picks one inside it.

use crate::meta::IndexMeta;

/// Lucene's `StringHelper.murmurhash3_x86_32`.
pub fn murmur3_x86_32(data: &[u8], seed: u32) -> i32 {
    const C1: u32 = 0xcc9e_2d51;
    const C2: u32 = 0x1b87_3593;
    let mut h = seed;
    let chunks = data.chunks_exact(4);
    let tail = chunks.remainder();
    for c in chunks {
        let mut k = u32::from_le_bytes([c[0], c[1], c[2], c[3]]);
        k = k.wrapping_mul(C1).rotate_left(15).wrapping_mul(C2);
        h ^= k;
        h = h.rotate_left(13).wrapping_mul(5).wrapping_add(0xe654_6b64);
    }
    let mut k = 0u32;
    if tail.len() >= 3 {
        k ^= (tail[2] as u32) << 16;
    }
    if tail.len() >= 2 {
        k ^= (tail[1] as u32) << 8;
    }
    if !tail.is_empty() {
        k ^= tail[0] as u32;
        k = k.wrapping_mul(C1).rotate_left(15).wrapping_mul(C2);
        h ^= k;
    }
    h ^= data.len() as u32;
    h ^= h >> 16;
    h = h.wrapping_mul(0x85eb_ca6b);
    h ^= h >> 13;
    h = h.wrapping_mul(0xc2b2_ae35);
    h ^= h >> 16;
    h as i32
}

/// `Murmur3HashFunction.hash(String)`: the UTF-16 code units, low byte first.
pub fn hash_routing(routing: &str) -> i32 {
    let mut bytes = Vec::with_capacity(routing.len() * 2);
    for u in routing.encode_utf16() {
        bytes.push(u as u8);
        bytes.push((u >> 8) as u8);
    }
    murmur3_x86_32(&bytes, 0)
}

/// The first index version routing hashes with `floorMod(hash, number_of_shards)`
/// (`IndexVersions.MOD_ROUTING_FUNCTION`); older indices go through `routing_num_shards`.
const MOD_ROUTING_FUNCTION: i64 = 9_064_000;

/// `RoutingFunction.shardNum`: a routing hash to a shard number.
fn shard_of(idx: &IndexMeta, hash: i32) -> u32 {
    let shards = idx.number_of_shards().max(1) as i64;
    let created = idx.settings.get("index.version.created").and_then(|v| v.parse::<i64>().ok()).unwrap_or(i64::MAX);
    if created >= MOD_ROUTING_FUNCTION {
        return (hash as i64).rem_euclid(shards) as u32;
    }
    let routing_num_shards = idx.routing_num_shards.max(1) as i64;
    let factor = (routing_num_shards / shards).max(1);
    ((hash as i64).rem_euclid(routing_num_shards) / factor) as u32
}

fn partition_size(idx: &IndexMeta) -> i32 {
    idx.settings.get("index.routing_partition_size").and_then(|p| p.parse::<i32>().ok()).unwrap_or(1).max(1)
}

/// The shard a document with this `_id` and optional routing goes to.
pub fn shard_for(idx: &IndexMeta, id: &str, routing: Option<&str>) -> u32 {
    let partition = partition_size(idx);
    match routing {
        // Java int arithmetic: the offset addition wraps.
        Some(r) if partition > 1 => shard_of(idx, hash_routing(r).wrapping_add(hash_routing(id).rem_euclid(partition))),
        Some(r) => shard_of(idx, hash_routing(r)),
        None => shard_of(idx, hash_routing(id)),
    }
}

/// The shards a search with this routing value reaches (one, or the routing partition).
pub fn search_shards_for(idx: &IndexMeta, routing: &str) -> Vec<u32> {
    let base = hash_routing(routing);
    let mut out: Vec<u32> = (0..partition_size(idx)).map(|o| shard_of(idx, base.wrapping_add(o))).collect();
    out.sort_unstable();
    out.dedup();
    out
}

/// A search `preference` (`OperationRouting.preferenceActiveShardIterator`): an optional
/// `_shards:<ids>` restriction, then a node preference or a custom string. Every primary lives on
/// this node, so node preferences only matter for `_only_nodes`, which must match it.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Preference {
    /// The shards `_shards:` names (`None`: every shard).
    pub shards: Option<Vec<u32>>,
    /// The `_only_nodes:` selectors that must pick the node.
    pub only_nodes: Option<String>,
}

impl Preference {
    /// Parses a preference. `opensearch` accepts OpenSearch's extra copy preferences
    /// (`_primary`, `_replica`, …).
    pub fn parse(pref: &str, opensearch: bool) -> Result<Preference, crate::error::EsError> {
        let mut out = Preference::default();
        if pref.is_empty() || !pref.starts_with('_') {
            return Ok(out);
        }
        let mut rest = pref;
        if preference_type(rest, opensearch)? == "_shards" {
            let body = &rest["_shards:".len().min(rest.len())..];
            let (ids, tail) = match body.find('|') {
                Some(i) => (&body[..i], &body[i + 1..]),
                None => (body, ""),
            };
            let mut shards = Vec::new();
            for id in ids.split(',').filter(|s| !s.is_empty()) {
                // Integer.parseInt: no whitespace or `+` tolerance beyond Java's.
                let n = crate::units::parse_java_int(id).ok_or_else(|| crate::error::EsError::number_format(id))?;
                if n >= 0 {
                    shards.push(n as u32);
                }
            }
            shards.sort_unstable();
            shards.dedup();
            out.shards = Some(shards);
            if tail.is_empty() {
                return Ok(out);
            }
            rest = tail;
            if !rest.starts_with('_') {
                return Ok(out);
            }
        }
        // Other node preferences (and `_shards` again) mean nothing more on one node.
        if preference_type(rest, opensearch)? == "_only_nodes" {
            out.only_nodes = Some(rest.split_once(':').map(|(_, v)| v.to_string()).unwrap_or_default());
        }
        Ok(out)
    }

    /// Whether the preference keeps `shard`.
    pub fn keeps(&self, shard: u32) -> bool {
        self.shards.as_ref().is_none_or(|s| s.binary_search(&shard).is_ok())
    }
}

/// `Preference.parse`: the part before the first `:`, which must name a known preference.
fn preference_type(pref: &str, opensearch: bool) -> Result<&str, crate::error::EsError> {
    let ty = pref.split_once(':').map_or(pref, |(t, _)| t);
    let known = match ty {
        "_shards" | "_prefer_nodes" | "_local" | "_only_local" | "_onlyLocal" | "_only_nodes" => true,
        "_primary" | "_primary_first" | "_primaryFirst" | "_replica" | "_replica_first" | "_replicaFirst" | "_search_replica" => opensearch,
        _ => false,
    };
    if known { Ok(ty) } else { Err(crate::error::EsError::illegal_argument(format!("no Preference for [{ty}]"))) }
}

#[cfg(test)]
mod tests {
    #[test]
    fn preferences() {
        let p = |s: &str| super::Preference::parse(s, false);
        assert_eq!(p("abc").unwrap(), super::Preference::default());
        assert_eq!(p("_local").unwrap(), super::Preference::default());
        assert_eq!(p("_shards:1,9,1").unwrap().shards, Some(vec![1, 9]));
        assert_eq!(p("_shards:").unwrap().shards, Some(vec![]));
        assert_eq!(p("_shards:2|").unwrap().shards, Some(vec![2]));
        assert_eq!(p("_shards:2|_only_nodes:zzz").unwrap().only_nodes.as_deref(), Some("zzz"));
        assert_eq!(p("_shards:x").unwrap_err().reason(), "For input string: \"x\"");
        assert_eq!(p("_shards:0, 1").unwrap_err().reason(), "For input string: \" 1\"");
        assert_eq!(p("_bogus").unwrap_err().reason(), "no Preference for [_bogus]");
        assert_eq!(p("_shards:1|_bogus").unwrap_err().reason(), "no Preference for [_bogus]");
        assert_eq!(p("_primary").unwrap_err().reason(), "no Preference for [_primary]");
        assert!(super::Preference::parse("_primary", true).is_ok());
        assert!(!p("_shards:1").unwrap().keeps(0));
    }

    use super::*;

    #[test]
    fn murmur3_matches_lucene() {
        // Reference values from Lucene's StringHelper.murmurhash3_x86_32.
        assert_eq!(murmur3_x86_32(b"", 0), 0);
        assert_eq!(murmur3_x86_32(b"hello", 0), 613153351);
        assert_eq!(murmur3_x86_32(b"The quick brown fox jumps over the lazy dog", 0), 776992547);
    }
}
