//! Shard manifests (commit points) and cluster state objects.
//!
//! Both are generational: each write goes to a new key (`<epoch>-<gen>-<token>.json` /
//! `<epoch>-<gen>-<token>.json.zst`, see [`crate::Version`]) one generation past the highest that exists, and
//! "latest" is resolved by listing the prefix and taking the highest `(epoch, generation)`, so a
//! write is atomic (a reader sees the old or the new generation, never a mix) and nothing is
//! overwritten. The epoch is the writer's ownership epoch: a write under an epoch lower than one
//! already present is refused ([`crate::Fenced`]), and is never the latest even if it races in.
//! The last few generations are retained so a reader holding an older one (another node) still
//! finds the objects it references, and with a point-in-time window (`manifest_retain_ms`) so is
//! every generation that was current at some moment inside the window (cluster states too);
//! garbage collection treats every retained manifest as live. Reading "the latest" reads exactly
//! the newest generation; if that one can't be read it is an error, never an older generation.
//! A deleted shard's history ends with a tombstone generation (`<version>.deleted.json`) while
//! the point-in-time window still needs it.
//!
//! A manifest is small JSON: the segment list with live-docs generations, the sequence-number
//! checkpoint, mapping version, free-form user data, and the WAL ranges that may still hold
//! operations past the checkpoint ([`WalSource`], maintained by the store).

use crate::keys::{LiveGen, SegmentId};
use crate::wal::WalSource;
use bytes::Bytes;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

/// Elasticsearch's `SequenceNumbers.NO_OPS_PERFORMED`.
pub const NO_OPS_PERFORMED: i64 = -1;

/// One segment of a commit point.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SegmentRef {
    pub id: SegmentId,
    /// Size of the segment object.
    pub bytes: u64,
    /// Documents in the segment, deleted ones included.
    #[serde(default)]
    pub max_doc: u32,
    /// Deleted documents according to `live`.
    #[serde(default)]
    pub del_count: u32,
    /// The live-docs object, once the segment has deletions.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub live: Option<LiveGen>,
}

/// Where a branch's shard came from.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct BranchBase {
    pub cluster: String,
    /// The parent manifest generation the branch started from.
    pub generation: u64,
    /// The requested point in time, if any.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub at_ms: Option<u64>,
    pub branched_ms: u64,
}

/// A shard's commit point.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ShardManifest {
    /// Assigned by the store when written.
    #[serde(default)]
    pub generation: u64,
    /// The ownership epoch it was written under (set by the store, see [`crate::Store::set_epoch`]).
    #[serde(default)]
    pub epoch: u64,
    /// Unix ms the store wrote it at (point-in-time branches pick by it).
    #[serde(default)]
    pub written_ms: u64,
    pub segments: Vec<SegmentRef>,
    /// Highest seq_no in the segments.
    pub max_seq_no: i64,
    /// Every operation up to here is in the segments; replay starts after it.
    pub local_checkpoint: i64,
    #[serde(default)]
    pub primary_term: u64,
    #[serde(default)]
    pub mapping_version: u64,
    /// Free-form commit data (history uuid, sync ids, …).
    #[serde(default)]
    pub user_data: BTreeMap<String, String>,
    /// WAL ranges that may hold operations past the checkpoint. Maintained by the store:
    /// whatever a caller puts here is replaced when the store tracks the shard.
    #[serde(default)]
    pub wal: Vec<WalSource>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub base: Option<BranchBase>,
    /// The WAL file the writing node was appending to when this was written (set by the store).
    /// Once a newer manifest exists, this one's open-ended range on that node never needs files
    /// past it, which lets garbage collection cap the ranges of retained older manifests.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub wal_head: Option<WalHead>,
}

/// The last WAL file of `node` that an older commit point's open-ended range on that node can
/// still need, given the commit point written after it (its WAL ranges and head): operations
/// appended before the successor are in files up to the successor's head when the same node
/// wrote it, or up to the range the successor froze when the shard had moved away. `None` when
/// the successor says nothing about `node` (keep the range open).
pub(crate) fn successor_cap(wal: &[WalSource], head: Option<&WalHead>, node: &str) -> Option<u64> {
    if let Some(to) = wal.iter().filter(|s| s.node == node).filter_map(|s| s.to).max() {
        return Some(to);
    }
    head.filter(|h| h.node == node).map(|h| h.file)
}

/// A node's current WAL file at some moment.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct WalHead {
    pub node: String,
    pub file: u64,
}

impl Default for ShardManifest {
    fn default() -> Self {
        Self {
            generation: 0,
            epoch: 0,
            written_ms: 0,
            segments: Vec::new(),
            max_seq_no: NO_OPS_PERFORMED,
            local_checkpoint: NO_OPS_PERFORMED,
            primary_term: 1,
            mapping_version: 0,
            user_data: BTreeMap::new(),
            wal: Vec::new(),
            base: None,
            wal_head: None,
        }
    }
}

impl ShardManifest {
    pub fn to_json(&self) -> Vec<u8> {
        serde_json::to_vec(self).expect("manifest serializes")
    }

    pub fn from_json(data: &[u8]) -> anyhow::Result<Self> {
        Ok(serde_json::from_slice(data)?)
    }

    /// Object keys this manifest keeps alive: its segments and live-docs objects.
    pub fn referenced_keys(&self) -> impl Iterator<Item = String> + '_ {
        self.segments.iter().flat_map(|s| std::iter::once(s.id.key()).chain(s.live.map(|g| s.id.live_key(g))))
    }

    /// Bytes of segments referenced.
    pub fn segment_bytes(&self) -> u64 {
        self.segments.iter().map(|s| s.bytes).sum()
    }
}

/// Largest cluster state accepted when reading one back (decompressed). Elasticsearch's own
/// cluster states for tens of thousands of fields stay in the tens of MB.
const MAX_STATE: u64 = 512 << 20;

/// A cluster's metadata (indices, mappings, settings, aliases, templates, pipelines) as opaque
/// JSON owned by search-es.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ClusterState {
    /// Assigned by the store when written.
    pub generation: u64,
    /// The ownership epoch it was written under (set by the store).
    pub epoch: u64,
    pub written_ms: u64,
    pub json: Bytes,
}

impl ClusterState {
    pub fn new(json: impl Into<Bytes>) -> Self {
        Self { generation: 0, epoch: 0, written_ms: 0, json: json.into() }
    }

    pub fn from_value<T: Serialize>(v: &T) -> anyhow::Result<Self> {
        Ok(Self::new(serde_json::to_vec(v)?))
    }

    pub fn parse<T: serde::de::DeserializeOwned>(&self) -> anyhow::Result<T> {
        Ok(serde_json::from_slice(&self.json)?)
    }

    /// The stored form: zstd of `{"generation":…,"epoch":…,"written_ms":…,"state":<json>}`.
    pub(crate) fn encode(&self, level: i32) -> anyhow::Result<Vec<u8>> {
        // Never write what `decode` would refuse.
        anyhow::ensure!((self.json.len() as u64) < MAX_STATE - 256, "cluster state of {} bytes is over the {MAX_STATE} byte limit", self.json.len());
        // Fails on anything that isn't one JSON value, so the wrapper is always valid JSON.
        let raw: &serde_json::value::RawValue = serde_json::from_slice(&self.json)?;
        let mut out = Vec::with_capacity(self.json.len() + 64);
        out.extend_from_slice(format!("{{\"generation\":{},\"epoch\":{},\"written_ms\":{},\"state\":", self.generation, self.epoch, self.written_ms).as_bytes());
        out.extend_from_slice(raw.get().as_bytes());
        out.push(b'}');
        Ok(zstd::bulk::compress(&out, level)?)
    }

    pub(crate) fn decode(data: &[u8]) -> anyhow::Result<Self> {
        #[derive(Deserialize)]
        struct Wrapper<'a> {
            generation: u64,
            #[serde(default)]
            epoch: u64,
            written_ms: u64,
            #[serde(borrow)]
            state: &'a serde_json::value::RawValue,
        }
        let raw = inflate_bounded(data, MAX_STATE)?;
        let w: Wrapper = serde_json::from_slice(&raw)?;
        Ok(Self { generation: w.generation, epoch: w.epoch, written_ms: w.written_ms, json: Bytes::copy_from_slice(w.state.get().as_bytes()) })
    }
}

/// Decompress a zstd frame, growing the output with what actually decompresses (never to a
/// size the frame claims) and refusing more than `limit` bytes.
fn inflate_bounded(data: &[u8], limit: u64) -> anyhow::Result<Vec<u8>> {
    use std::io::Read;
    let mut raw = Vec::new();
    let n = zstd::stream::read::Decoder::new(data)?.take(limit + 1).read_to_end(&mut raw)?;
    anyhow::ensure!(n as u64 <= limit, "cluster state decompresses past {limit} bytes");
    Ok(raw)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn manifest_json_round_trip_and_defaults() {
        let m = ShardManifest {
            generation: 3,
            epoch: 2,
            written_ms: 99,
            segments: vec![
                SegmentRef { id: SegmentId([1; 16]), bytes: 10, max_doc: 5, del_count: 0, live: None },
                SegmentRef { id: SegmentId([2; 16]), bytes: 20, max_doc: 7, del_count: 2, live: Some(LiveGen(5)) },
            ],
            max_seq_no: 11,
            local_checkpoint: 11,
            primary_term: 1,
            mapping_version: 4,
            user_data: [("history_uuid".to_string(), "h".to_string())].into(),
            wal: vec![WalSource { skip: vec![[4, 5]], ..WalSource::open("n", "c", 2) }],
            base: None,
            wal_head: Some(WalHead { node: "n".into(), file: 9 }),
        };
        let back = ShardManifest::from_json(&m.to_json()).unwrap();
        assert_eq!(back, m);
        let keys: Vec<String> = m.referenced_keys().collect();
        assert_eq!(keys.len(), 3);
        assert!(keys[2].ends_with(".liv"));
        assert_eq!(m.segment_bytes(), 30);
        let minimal = ShardManifest::from_json(br#"{"segments":[],"max_seq_no":-1,"local_checkpoint":-1}"#).unwrap();
        assert_eq!(minimal.wal, vec![]);
        assert_eq!(minimal.generation, 0);
    }

    #[test]
    fn cluster_state_round_trip() {
        let s = ClusterState { generation: 7, epoch: 3, written_ms: 123, json: Bytes::from_static(b"{\"indices\":{\"a\":{\"x\":[1,2,\"\\u00e9\"]}}}") };
        let back = ClusterState::decode(&s.encode(3).unwrap()).unwrap();
        assert_eq!(back, s);
        assert!(ClusterState::new("not json").encode(1).is_err());
        assert!(ClusterState::new("{} {}").encode(1).is_err());
        // A frame that inflates past the limit is refused without inflating it all.
        let bomb = zstd::bulk::compress(&[b' '; 1 << 20], 1).unwrap();
        assert!(bomb.len() < 1 << 10);
        let err = inflate_bounded(&bomb, 1000).unwrap_err();
        assert!(err.to_string().contains("decompresses past 1000 bytes"), "{err}");
        assert_eq!(inflate_bounded(&bomb, 1 << 20).unwrap().len(), 1 << 20);
        let v: serde_json::Value = back.parse().unwrap();
        assert_eq!(v["indices"]["a"]["x"][2], "é");
    }
}
