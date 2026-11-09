//! Manifests: where a database's state lives in object storage.
//!
//! ```text
//! nosql/log/<node>/<seq>.zst                    commit log segments (all databases on a node)
//! nosql/db/<db>/manifest.json                   the current manifest
//! nosql/db/<db>/manifests/<version>.json        older versions, kept for point-in-time restore
//! nosql/db/<db>/snapshots/<name>.json           named snapshots (manifest copies)
//! nosql/db/<db>/sst/<table-id>/<sst-id>.data    SSTable data, read by range
//! nosql/db/<db>/sst/<table-id>/<sst-id>.index   SSTable index, filter and statistics
//! ```
//!
//! A manifest lists, per table, its SSTables by full object key (so a branch references its
//! parent's objects) and how far the commit log is already in them (`flushed_lsn`), the
//! segments holding the database's frames, and the *base logs*: other databases' frames a
//! branch or an in-place restore replays before its own. The current manifest is rewritten
//! lazily as segments ship; every structural change (flush, compaction, truncate, drop,
//! branch, restore) also writes a numbered version under `manifests/`.

use anyhow::{Context, Result};
use bytes::Bytes;
use pg_storage::Objects;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};

/// A table id (Cassandra's table UUID), printed as a UUID.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(into = "String", try_from = "String")]
pub struct TableId(pub u128);

impl std::fmt::Display for TableId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let b = self.0.to_be_bytes();
        let h = |r: std::ops::Range<usize>| b[r].iter().map(|x| format!("{x:02x}")).collect::<String>();
        write!(f, "{}-{}-{}-{}-{}", h(0..4), h(4..6), h(6..8), h(8..10), h(10..16))
    }
}

impl std::str::FromStr for TableId {
    type Err = anyhow::Error;

    fn from_str(s: &str) -> Result<Self> {
        let hex: String = s.chars().filter(|c| *c != '-').collect();
        if hex.len() != 32 {
            anyhow::bail!("bad table id {s:?}");
        }
        Ok(TableId(u128::from_str_radix(&hex, 16).with_context(|| format!("bad table id {s:?}"))?))
    }
}

impl From<TableId> for String {
    fn from(t: TableId) -> String {
        t.to_string()
    }
}

impl TryFrom<String> for TableId {
    type Error = anyhow::Error;
    fn try_from(s: String) -> Result<Self> {
        s.parse()
    }
}

/// A new SSTable id: a version-1 UUID (timestamp, so ids sort by creation) whose clock sequence
/// and node fields are random. Never reused.
pub fn new_sstable_id() -> String {
    use rand::RngCore;
    let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap_or_default();
    // 100 ns intervals since 1582-10-15.
    let t = (now.as_nanos() / 100) as u64 + 0x01B2_1DD2_1381_4000;
    let mut r = [0u8; 8];
    rand::thread_rng().fill_bytes(&mut r);
    let time_low = t as u32;
    let time_mid = (t >> 32) as u16;
    let time_hi = ((t >> 48) as u16 & 0x0fff) | 0x1000;
    let clock = (u16::from_be_bytes([r[0], r[1]]) & 0x3fff) | 0x8000;
    format!("{time_low:08x}-{time_mid:04x}-{time_hi:04x}-{clock:04x}-{}", r[2..8].iter().map(|b| format!("{b:02x}")).collect::<String>())
}

pub fn manifest_key(db: &str) -> String {
    format!("nosql/db/{db}/manifest.json")
}

pub fn versions_prefix(db: &str) -> String {
    format!("nosql/db/{db}/manifests/")
}

pub fn version_key(db: &str, version: u64) -> String {
    format!("nosql/db/{db}/manifests/{version:020}.json")
}

pub fn snapshot_key(db: &str, name: &str) -> String {
    format!("nosql/db/{db}/snapshots/{name}.json")
}

/// A snapshot's description ([`crate::db::Database::put_snapshot_info`]); not a manifest.
pub fn snapshot_info_key(db: &str, name: &str) -> String {
    format!("nosql/db/{db}/snapshot-info/{name}.json")
}

pub fn segment_key(node: &str, seq: u64) -> String {
    format!("nosql/log/{node}/{seq:016}.zst")
}

pub fn segment_seq(key: &str) -> Option<u64> {
    key.rsplit('/').next()?.strip_suffix(".zst")?.parse().ok()
}

/// The object key prefix of a new SSTable (`.data` / `.index` are appended).
pub fn sstable_key(db: &str, table: TableId, id: &str) -> String {
    format!("nosql/db/{db}/sst/{table}/{id}")
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct SstRef {
    /// Object key prefix (`nosql/db/<owner>/sst/<table>/<id>`).
    pub key: String,
    #[serde(default)]
    pub level: u32,
    #[serde(default)]
    pub run: u64,
    pub data_size: u64,
    pub index_size: u64,
    pub created_ms: i64,
    #[serde(default)]
    pub partitions: u64,
    #[serde(default)]
    pub min_timestamp: i64,
    #[serde(default)]
    pub max_timestamp: i64,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct TableEntry {
    pub sstables: Vec<SstRef>,
    /// Own-stream frames up to this position are in the SSTables.
    #[serde(default)]
    pub flushed_lsn: u64,
    /// The base logs' frames for this table are in the SSTables.
    #[serde(default)]
    pub base_done: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub truncated_ms: Option<i64>,
}

/// Where frames for a database are: one segment, and the database's positions in it.
#[derive(Clone, Debug, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct SegRef {
    pub node: String,
    pub seq: u64,
    pub first_lsn: u64,
    pub last_lsn: u64,
    #[serde(default)]
    pub last_ms: i64,
}

/// Frames of another stream to replay first (a branch's parent, or the history an in-place
/// restore rewound).
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct BaseLog {
    pub stream: String,
    pub segments: Vec<SegRef>,
    /// Frames from this position on (the stream's `replay_from` at the time).
    #[serde(default)]
    pub min_lsn: u64,
    pub until_lsn: u64,
    pub until_ms: i64,
    /// Per table: frames up to here are already in the referenced SSTables.
    #[serde(default)]
    pub flushed: BTreeMap<TableId, u64>,
    /// Tables whose frames of this stream are all in the referenced SSTables.
    #[serde(default)]
    pub skip: BTreeSet<TableId>,
}

/// Where a database came from (informational).
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Origin {
    pub parent: String,
    pub at_ms: i64,
    #[serde(default)]
    pub restore: bool,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Manifest {
    pub version: u64,
    pub written_ms: i64,
    /// The id this database's own frames are written under.
    pub stream: String,
    /// Own frames before this were written by a timeline an in-place restore replaced.
    #[serde(default)]
    pub replay_from: u64,
    pub next_lsn: u64,
    #[serde(default)]
    pub tables: BTreeMap<TableId, TableEntry>,
    #[serde(default)]
    pub dropped: BTreeSet<TableId>,
    #[serde(default)]
    pub bases: Vec<BaseLog>,
    #[serde(default)]
    pub segments: Vec<SegRef>,
    /// The node that owned the database when this was written: its segments after
    /// `after_seq` may hold newer frames (crash recovery).
    #[serde(default)]
    pub node: Option<String>,
    #[serde(default)]
    pub after_seq: u64,
    /// Own frames up to here may be needed by retained history; segments beyond it are kept.
    #[serde(default)]
    pub history_floor: u64,
    #[serde(default)]
    pub origin: Option<Origin>,
}

impl Manifest {
    pub fn new(db: &str) -> Self {
        Self { stream: db.to_string(), next_lsn: 1, replay_from: 1, ..Default::default() }
    }

    /// Smallest `flushed_lsn` over tables (own frames above it may still be needed).
    pub fn min_flushed(&self) -> u64 {
        self.tables.values().map(|t| t.flushed_lsn).min().unwrap_or(self.next_lsn.saturating_sub(1))
    }

    /// Drop segments no replay or retained history needs.
    pub fn prune_segments(&mut self, retain_from_ms: i64) {
        let floor = self.min_flushed().min(self.history_floor);
        self.segments.retain(|s| s.last_lsn > floor || s.last_ms >= retain_from_ms);
    }

    /// Every object key this manifest references (SSTables' data and index, segments).
    pub fn referenced(&self, keep: &mut BTreeSet<String>) {
        for t in self.tables.values() {
            for s in &t.sstables {
                keep.insert(format!("{}.data", s.key));
                keep.insert(format!("{}.index", s.key));
            }
        }
        for s in &self.segments {
            keep.insert(segment_key(&s.node, s.seq));
        }
        for b in &self.bases {
            for s in &b.segments {
                keep.insert(segment_key(&s.node, s.seq));
            }
        }
    }
}

pub async fn read_json(objects: &Objects, key: &str) -> Result<Option<Manifest>> {
    match objects.get(key).await? {
        Some(raw) => Ok(Some(serde_json::from_slice(&raw).with_context(|| format!("corrupt manifest {key}"))?)),
        None => Ok(None),
    }
}

pub async fn read_manifest(objects: &Objects, db: &str) -> Result<Option<Manifest>> {
    read_json(objects, &manifest_key(db)).await
}

/// Write the current manifest, and with `history` also its numbered version.
pub async fn write_manifest(objects: &Objects, db: &str, m: &Manifest, history: bool) -> Result<()> {
    let raw = Bytes::from(serde_json::to_vec(m)?);
    #[cfg(test)]
    crate::failpoint::check(&manifest_key(db))?;
    if history {
        objects.put(&version_key(db, m.version), raw.clone()).await?;
    }
    objects.put(&manifest_key(db), raw).await
}

/// Numbered versions of a database's manifest, ascending.
pub async fn list_versions(objects: &Objects, db: &str) -> Result<Vec<(u64, String)>> {
    let mut v: Vec<(u64, String)> = objects
        .list(&versions_prefix(db))
        .await?
        .into_iter()
        .filter_map(|(k, _)| Some((k.rsplit('/').next()?.strip_suffix(".json")?.parse().ok()?, k)))
        .collect();
    v.sort();
    Ok(v)
}

/// The newest state of `db` written at or before `at_ms`: the current manifest if it is that
/// old, else the newest numbered version that is (binary search, `O(log n)` reads).
pub async fn manifest_at(objects: &Objects, db: &str, current: &Manifest, at_ms: i64) -> Result<Option<Manifest>> {
    if current.written_ms <= at_ms {
        return Ok(Some(current.clone()));
    }
    let versions = list_versions(objects, db).await?;
    let (mut lo, mut hi) = (0usize, versions.len());
    let mut best: Option<Manifest> = None;
    while lo < hi {
        let mid = (lo + hi) / 2;
        match read_json(objects, &versions[mid].1).await? {
            Some(m) if m.written_ms <= at_ms => {
                best = Some(m);
                lo = mid + 1;
            }
            Some(_) => hi = mid,
            None => {
                // Expired between list and read: treat as too old.
                lo = mid + 1;
            }
        }
    }
    Ok(best)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ids() {
        let t = TableId(0x0123_4567_89ab_cdef_0011_2233_4455_6677);
        assert_eq!(t.to_string(), "01234567-89ab-cdef-0011-223344556677");
        assert_eq!(t.to_string().parse::<TableId>().unwrap(), t);
        assert!("xyz".parse::<TableId>().is_err());
        let a = new_sstable_id();
        let b = new_sstable_id();
        assert_ne!(a, b);
        assert_eq!(a.len(), 36);
        assert_eq!(&a[14..15], "1", "version 1");
        let j = serde_json::to_string(&t).unwrap();
        assert_eq!(serde_json::from_str::<TableId>(&j).unwrap(), t);
    }

    #[test]
    fn segment_pruning_respects_flushed_and_history() {
        let mut m = Manifest::new("d");
        m.next_lsn = 100;
        m.tables.insert(TableId(1), TableEntry { flushed_lsn: 50, ..Default::default() });
        m.history_floor = 30;
        m.segments = vec![
            SegRef { node: "n".into(), seq: 1, first_lsn: 1, last_lsn: 20, last_ms: 10 },
            SegRef { node: "n".into(), seq: 2, first_lsn: 21, last_lsn: 40, last_ms: 20 },
            SegRef { node: "n".into(), seq: 3, first_lsn: 41, last_lsn: 99, last_ms: 30 },
        ];
        m.prune_segments(1000);
        assert_eq!(m.segments.iter().map(|s| s.seq).collect::<Vec<_>>(), vec![2, 3]);
        let mut keep = BTreeSet::new();
        m.referenced(&mut keep);
        assert!(keep.contains("nosql/log/n/0000000000000002.zst"));
        assert_eq!(segment_seq("nosql/log/n/0000000000000002.zst"), Some(2));
    }
}
