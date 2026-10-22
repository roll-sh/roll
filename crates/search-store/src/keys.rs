//! Object keys and the identifiers inside them.
//!
//! Every object the store writes has a key that is never reused: segments are named by a random
//! 128-bit id, live-docs objects by their segment and a unique generation token, WAL objects by a
//! per-node sequence that only grows, and manifests / cluster states by a [`Version`]: the
//! writer's ownership epoch, a per-path generation and a random token. (The token matters when a
//! cluster is deleted and a new one with the same name — a re-created branch — starts its
//! generations over: its keys still differ from the old ones, so nothing that cached an object
//! by key, such as garbage collection, can mistake one for the other.) Rewriting something therefore always means
//! writing a new key and switching references, which keeps readers (other nodes, branches,
//! garbage collection) free of torn or replaced objects.
//!
//! The epoch in manifest and state keys is what fences a deposed owner: two nodes that both
//! believe they own a cluster write different keys even for the same generation, and readers
//! order by `(epoch, generation)`, so once the new owner has written anything, nothing the old
//! one writes can become the latest (see [`crate::Fenced`]).
//!
//! ```text
//! search/seg/<segment id hex>.seg                                          segment (shared by branches)
//! search/seg/<segment id hex>.<live gen hex>.liv                           live docs of a segment
//! search/wal/<node>/<seq:016>.zst                                          sealed WAL file (all clusters on the node)
//! search/wal/<node>/<seq:016>.run                                          a run of the node started at file seq
//! search/c/<cluster>/state/<epoch:020>-<gen:020>-<token:016x>.json.zst              cluster metadata
//! search/c/<cluster>/i/<index uuid>/<shard>/<epoch:020>-<gen:020>-<token:016x>.json  shard manifest
//! search/c/<cluster>/i/<index uuid>/<shard>/<epoch:020>-<gen:020>-<token:016x>.deleted.json  shard tombstone
//! search/gc/pending.json                                                   garbage collector state
//! search/gc/marks.bin                                                      what every manifest references (collector cache)
//! ```

use serde::{Deserialize, Deserializer, Serialize, Serializer};
use std::fmt;

pub const SEG_PREFIX: &str = "search/seg/";
pub const WAL_PREFIX: &str = "search/wal/";
pub const CLUSTER_PREFIX: &str = "search/c/";
pub const GC_STATE: &str = "search/gc/pending.json";
/// What garbage collection knows about every manifest (see `gc::snapshot`), so a restarted
/// collector doesn't read every manifest in the bucket again.
pub const GC_MARKS: &str = "search/gc/marks.bin";

/// A shard of an index: the index's uuid (stable across renames and aliases) and the shard number.
#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub struct ShardId {
    pub index_uuid: String,
    pub shard: u32,
}

impl ShardId {
    pub fn new(index_uuid: impl Into<String>, shard: u32) -> Self {
        Self { index_uuid: index_uuid.into(), shard }
    }
}

impl fmt::Display for ShardId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "[{}][{}]", self.index_uuid, self.shard)
    }
}

fn hex_into(bytes: &[u8], out: &mut String) {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    for b in bytes {
        out.push(HEX[(b >> 4) as usize] as char);
        out.push(HEX[(b & 15) as usize] as char);
    }
}

fn unhex<const N: usize>(s: &str) -> Option<[u8; N]> {
    let s = s.as_bytes();
    if s.len() != N * 2 {
        return None;
    }
    let nib = |c: u8| match c {
        b'0'..=b'9' => Some(c - b'0'),
        b'a'..=b'f' => Some(c - b'a' + 10),
        b'A'..=b'F' => Some(c - b'A' + 10),
        _ => None,
    };
    let mut out = [0u8; N];
    for (i, o) in out.iter_mut().enumerate() {
        *o = (nib(s[2 * i])? << 4) | nib(s[2 * i + 1])?;
    }
    Some(out)
}

fn random_bytes<const N: usize>() -> [u8; N] {
    let mut b = [0u8; N];
    getrandom::fill(&mut b).expect("the OS random number generator failed");
    b
}

/// A segment's identity: 16 random bytes (the codec's `SegmentInfo::id`), written as 32 hex digits.
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct SegmentId(pub [u8; 16]);

/// What `Store::upload_segment` returns and `Store::open_segment` takes.
pub type SegmentKey = SegmentId;

impl SegmentId {
    pub fn random() -> Self {
        Self(random_bytes())
    }

    pub fn to_hex(&self) -> String {
        let mut s = String::with_capacity(32);
        hex_into(&self.0, &mut s);
        s
    }

    pub fn parse(hex: &str) -> Option<Self> {
        unhex::<16>(hex).map(Self)
    }

    /// The segment object's key.
    pub fn key(&self) -> String {
        format!("{SEG_PREFIX}{}.seg", self.to_hex())
    }

    /// Its file name in the local cache.
    pub fn file_name(&self) -> String {
        format!("{}.seg", self.to_hex())
    }

    /// A live-docs object's key.
    pub fn live_key(&self, live: LiveGen) -> String {
        format!("{SEG_PREFIX}{}.{}.liv", self.to_hex(), live.to_hex())
    }

    pub fn live_file_name(&self, live: LiveGen) -> String {
        format!("{}.{}.liv", self.to_hex(), live.to_hex())
    }
}

impl fmt::Display for SegmentId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.to_hex())
    }
}

impl fmt::Debug for SegmentId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "SegmentId({})", self.to_hex())
    }
}

impl Serialize for SegmentId {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(&self.to_hex())
    }
}

impl<'de> Deserialize<'de> for SegmentId {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let s = <std::borrow::Cow<'de, str>>::deserialize(d)?;
        Self::parse(&s).ok_or_else(|| serde::de::Error::custom(format!("invalid segment id [{s}]")))
    }
}

/// A live-docs generation. The high 64 bits are the Unix time in milliseconds it was written at
/// (so generations sort by age), the low 64 bits are random. A branch and its parent may each
/// write a new generation of the same shared segment at the same moment: the random half keeps
/// their keys apart, so a live-docs object is never overwritten and can be shared, not copied.
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct LiveGen(pub u128);

impl LiveGen {
    pub fn new(now_ms: u64) -> Self {
        let r = u64::from_le_bytes(random_bytes());
        Self(((now_ms as u128) << 64) | r as u128)
    }

    pub fn written_ms(&self) -> u64 {
        (self.0 >> 64) as u64
    }

    pub fn to_hex(&self) -> String {
        let mut s = String::with_capacity(32);
        hex_into(&self.0.to_be_bytes(), &mut s);
        s
    }

    pub fn parse(hex: &str) -> Option<Self> {
        unhex::<16>(hex).map(|b| Self(u128::from_be_bytes(b)))
    }
}

impl fmt::Display for LiveGen {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.to_hex())
    }
}

impl fmt::Debug for LiveGen {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "LiveGen({})", self.to_hex())
    }
}

impl Serialize for LiveGen {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(&self.to_hex())
    }
}

impl<'de> Deserialize<'de> for LiveGen {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let s = <std::borrow::Cow<'de, str>>::deserialize(d)?;
        Self::parse(&s).ok_or_else(|| serde::de::Error::custom(format!("invalid live docs generation [{s}]")))
    }
}

/// What a key under `search/seg/` names.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SegObject {
    Segment(SegmentId),
    LiveDocs(SegmentId, LiveGen),
}

pub fn parse_seg_key(key: &str) -> Option<SegObject> {
    parse_seg_file(key.strip_prefix(SEG_PREFIX)?)
}

/// Parse a file name in the segment namespace (`<id>.seg` or `<id>.<gen>.liv`).
pub fn parse_seg_file(name: &str) -> Option<SegObject> {
    if let Some(id) = name.strip_suffix(".seg") {
        return SegmentId::parse(id).map(SegObject::Segment);
    }
    let rest = name.strip_suffix(".liv")?;
    let (id, live) = rest.split_once('.')?;
    Some(SegObject::LiveDocs(SegmentId::parse(id)?, LiveGen::parse(live)?))
}

pub fn wal_prefix(node: &str) -> String {
    format!("{WAL_PREFIX}{node}/")
}

pub fn wal_key(node: &str, seq: u64) -> String {
    format!("{WAL_PREFIX}{node}/{seq:016}.zst")
}

/// The marker a node writes when a run (a process) starts appending at file `first_file`: every
/// file of the node's earlier runs is below it. Lets garbage collection and replay end an
/// open-ended range of a run that crashed without closing it (see `Wal::open`).
pub fn run_key(node: &str, first_file: u64) -> String {
    format!("{WAL_PREFIX}{node}/{first_file:016}.run")
}

/// `(node, first file)` of a run marker key.
pub fn parse_run_key(key: &str) -> Option<(&str, u64)> {
    let rest = key.strip_prefix(WAL_PREFIX)?;
    let (node, file) = rest.rsplit_once('/')?;
    let seq = file.strip_suffix(".run")?;
    if seq.len() != 16 || !seq.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    Some((node, seq.parse().ok()?))
}

/// `(node, seq)` of a WAL object key.
pub fn parse_wal_key(key: &str) -> Option<(&str, u64)> {
    let rest = key.strip_prefix(WAL_PREFIX)?;
    let (node, file) = rest.rsplit_once('/')?;
    let seq = file.strip_suffix(".zst")?;
    if seq.len() != 16 || !seq.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    Some((node, seq.parse().ok()?))
}

pub fn cluster_prefix(cluster: &str) -> String {
    format!("{CLUSTER_PREFIX}{cluster}/")
}

pub fn state_prefix(cluster: &str) -> String {
    format!("{CLUSTER_PREFIX}{cluster}/state/")
}

pub fn state_key(cluster: &str, v: Version) -> String {
    format!("{CLUSTER_PREFIX}{cluster}/state/{v}.json.zst")
}

pub fn shards_prefix(cluster: &str) -> String {
    format!("{CLUSTER_PREFIX}{cluster}/i/")
}

pub fn manifest_prefix(cluster: &str, shard: &ShardId) -> String {
    format!("{CLUSTER_PREFIX}{cluster}/i/{}/{}/", shard.index_uuid, shard.shard)
}

pub fn manifest_key(cluster: &str, shard: &ShardId, v: Version) -> String {
    format!("{CLUSTER_PREFIX}{cluster}/i/{}/{}/{v}.json", shard.index_uuid, shard.shard)
}

/// A shard's tombstone: the generation that records its deletion while the point-in-time window
/// still needs its earlier commit points (see `Store::delete_shard`).
pub fn tombstone_key(cluster: &str, shard: &ShardId, v: Version) -> String {
    format!("{CLUSTER_PREFIX}{cluster}/i/{}/{}/{v}{TOMBSTONE_SUFFIX}", shard.index_uuid, shard.shard)
}

/// [`manifest_key`] or [`tombstone_key`].
pub fn shard_version_key(cluster: &str, shard: &ShardId, v: ShardVersion) -> String {
    if v.deleted { tombstone_key(cluster, shard, v.version) } else { manifest_key(cluster, shard, v.version) }
}

pub const TOMBSTONE_SUFFIX: &str = ".deleted.json";

/// A generation in a shard's history: a commit point, or the tombstone that ends it. Orders by
/// version alone.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct ShardVersion {
    pub version: Version,
    pub deleted: bool,
}

impl Ord for ShardVersion {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        self.version.cmp(&other.version).then(self.deleted.cmp(&other.deleted))
    }
}

impl PartialOrd for ShardVersion {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

/// The generation in a shard history key's file name (`<version>.json` or
/// `<version>.deleted.json`).
pub fn parse_shard_version(file: &str) -> Option<ShardVersion> {
    if let Some(v) = file.strip_suffix(TOMBSTONE_SUFFIX) {
        return Some(ShardVersion { version: parse_version(v, "")?, deleted: true });
    }
    Some(ShardVersion { version: parse_version(file, ".json")?, deleted: false })
}

/// Where a manifest or cluster state sits in its path's history: ordered by the writer's
/// ownership epoch first, then by generation (the token only makes the key unique). Written as
/// `<epoch:020>-<gen:020>-<token:016x>` (20 digits hold any `u64`, so a lease epoch may be a
/// nanosecond timestamp), so listing order is history order.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub struct Version {
    pub epoch: u64,
    pub generation: u64,
    pub token: u64,
}

impl Version {
    pub fn new(epoch: u64, generation: u64, token: u64) -> Self {
        Self { epoch, generation, token }
    }

    /// A version for a new write: a random token.
    pub fn fresh(epoch: u64, generation: u64) -> Self {
        Self { epoch, generation, token: u64::from_le_bytes(random_bytes()) }
    }
}

impl fmt::Display for Version {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{:020}-{:020}-{:016x}", self.epoch, self.generation, self.token)
    }
}

/// Exactly 20 decimal digits that fit a `u64`.
fn parse_digits20(s: &str) -> Option<u64> {
    if s.len() != 20 || !s.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    s.parse().ok()
}

/// The version in a manifest or cluster-state key's file name.
pub fn parse_version(key: &str, suffix: &str) -> Option<Version> {
    let file = key.rsplit('/').next()?.strip_suffix(suffix)?;
    let mut parts = file.split('-');
    let (epoch, generation, token) = (parts.next()?, parts.next()?, parts.next()?);
    if parts.next().is_some() || token.len() != 16 || !token.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f')) {
        return None;
    }
    Some(Version { epoch: parse_digits20(epoch)?, generation: parse_digits20(generation)?, token: u64::from_str_radix(token, 16).ok()? })
}

/// What a key under `search/c/` names.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ClusterObject<'a> {
    State { cluster: &'a str, version: Version },
    /// A shard's commit point, or (`deleted`) its tombstone.
    Manifest { cluster: &'a str, index_uuid: &'a str, shard: u32, version: Version, deleted: bool },
}

pub fn parse_cluster_key(key: &str) -> Option<ClusterObject<'_>> {
    let rest = key.strip_prefix(CLUSTER_PREFIX)?;
    let (cluster, rest) = rest.split_once('/')?;
    if let Some(file) = rest.strip_prefix("state/") {
        if file.contains('/') {
            return None;
        }
        return Some(ClusterObject::State { cluster, version: parse_version(file, ".json.zst")? });
    }
    let rest = rest.strip_prefix("i/")?;
    let mut parts = rest.split('/');
    let (index_uuid, shard, file) = (parts.next()?, parts.next()?, parts.next()?);
    if parts.next().is_some() {
        return None;
    }
    let v = parse_shard_version(file)?;
    Some(ClusterObject::Manifest { cluster, index_uuid, shard: shard.parse().ok()?, version: v.version, deleted: v.deleted })
}

/// Cluster names end up in object keys and file names: keep them to a safe alphabet.
pub fn validate_cluster(name: &str) -> anyhow::Result<()> {
    let ok = !name.is_empty()
        && name.len() <= 255
        && name != "."
        && name != ".."
        && name.bytes().all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.'));
    anyhow::ensure!(ok, "invalid cluster name [{name}]");
    Ok(())
}

/// Index uuids (Elasticsearch's are 22 url-safe base64 characters) end up in keys too.
pub fn validate_shard(shard: &ShardId) -> anyhow::Result<()> {
    let u = &shard.index_uuid;
    let ok = !u.is_empty()
        && u.len() <= 255
        && u != "."
        && u != ".."
        && u.bytes().all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.'));
    anyhow::ensure!(ok, "invalid index uuid [{u}]");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn segment_ids_round_trip() {
        let id = SegmentId([0xab; 16]);
        assert_eq!(id.to_hex(), "abababababababababababababababab");
        assert_eq!(SegmentId::parse(&id.to_hex()), Some(id));
        assert_eq!(SegmentId::parse("ABABABABABABABABABABABABABABABAB"), Some(id));
        assert_eq!(SegmentId::parse("abab"), None);
        assert_eq!(SegmentId::parse("zzababababababababababababababab"), None);
        assert_ne!(SegmentId::random(), SegmentId::random());
        let json = serde_json::to_string(&id).unwrap();
        assert_eq!(json, "\"abababababababababababababababab\"");
        assert_eq!(serde_json::from_str::<SegmentId>(&json).unwrap(), id);
        assert!(serde_json::from_str::<SegmentId>("\"xx\"").is_err());
    }

    #[test]
    fn live_gens_sort_by_time_and_differ() {
        let a = LiveGen::new(1000);
        let b = LiveGen::new(1000);
        let c = LiveGen::new(1001);
        assert_ne!(a, b);
        assert!(a < c && b < c);
        assert_eq!(a.written_ms(), 1000);
        assert_eq!(LiveGen::parse(&a.to_hex()), Some(a));
        let json = serde_json::to_string(&a).unwrap();
        assert_eq!(serde_json::from_str::<LiveGen>(&json).unwrap(), a);
    }

    #[test]
    fn keys_parse_back() {
        let id = SegmentId([1; 16]);
        let live = LiveGen(0x0102);
        assert_eq!(parse_seg_key(&id.key()), Some(SegObject::Segment(id)));
        assert_eq!(parse_seg_key(&id.live_key(live)), Some(SegObject::LiveDocs(id, live)));
        assert_eq!(parse_seg_key("search/seg/garbage"), None);
        assert_eq!(parse_seg_key("search/seg/0101.seg"), None);
        assert_eq!(wal_key("n1", 7), "search/wal/n1/0000000000000007.zst");
        assert_eq!(parse_wal_key(&wal_key("n1", 7)), Some(("n1", 7)));
        assert_eq!(parse_wal_key("search/wal/n1/7.zst"), None);
        let s = ShardId::new("uuidAAA", 3);
        let v = Version::new(5, 2, 0xab);
        assert_eq!(manifest_key("c1", &s, v), "search/c/c1/i/uuidAAA/3/00000000000000000005-00000000000000000002-00000000000000ab.json");
        assert_eq!(parse_cluster_key(&manifest_key("c1", &s, v)), Some(ClusterObject::Manifest { cluster: "c1", index_uuid: "uuidAAA", shard: 3, version: v, deleted: false }));
        assert_eq!(tombstone_key("c1", &s, v), "search/c/c1/i/uuidAAA/3/00000000000000000005-00000000000000000002-00000000000000ab.deleted.json");
        assert_eq!(parse_cluster_key(&tombstone_key("c1", &s, v)), Some(ClusterObject::Manifest { cluster: "c1", index_uuid: "uuidAAA", shard: 3, version: v, deleted: true }));
        assert_eq!(parse_version(&tombstone_key("c1", &s, v), ".json"), None, "a tombstone is not a commit point");
        let tomb = ShardVersion { version: v, deleted: true };
        assert_eq!(shard_version_key("c1", &s, tomb), tombstone_key("c1", &s, v));
        assert!(ShardVersion { version: Version::new(5, 1, u64::MAX), deleted: false } < tomb);
        let v = Version::fresh(0, 9);
        assert_ne!(v.token, Version::fresh(0, 9).token);
        assert_eq!(&state_key("c1", v)[..60], "search/c/c1/state/00000000000000000000-00000000000000000009-");
        assert_eq!(parse_cluster_key(&state_key("c1", v)), Some(ClusterObject::State { cluster: "c1", version: v }));
        for bad in [
            "search/c/c1/i/u/x/00000000000000000000-00000000000000000001-0000000000000000.json",
            "search/c/c1/i/u/1/00000000000000000001.json",
            "search/c/c1/i/u/1/00000000000000000000-00000000000000000001.json",
            "search/c/c1/i/u/1/0000000000000000000+-00000000000000000001-0000000000000000.json",
            "search/c/c1/i/u/1/00000000000000000000-00000000000000000001-000000000000000G.json",
            "search/c/c1/i/u/1/00000000000000000000-00000000000000000001-00000000000000AB.json",
            "search/c/c1/i/u/1/00000000000000000000-00000000000000000001-0000000000000000-1.json",
            "search/c/c1/i/u/1/00000000000000000000-00000000000000000001-0000000000000000.gone.json",
            // The 16-digit form of earlier builds, and a value past u64::MAX.
            "search/c/c1/i/u/1/0000000000000000-0000000000000001-0000000000000000.json",
            "search/c/c1/i/u/1/18446744073709551616-00000000000000000001-0000000000000000.json",
        ] {
            assert_eq!(parse_cluster_key(bad), None, "{bad}");
        }
        assert_eq!(parse_cluster_key("search/c/c1/other"), None);
        // Listing order (lexicographic) is history order: epoch first, then generation, at any
        // magnitude (a lease epoch may be a nanosecond timestamp).
        let mut names = [Version::new(2, 1, 0), Version::new(1, 30, 7), Version::new(1, 4, u64::MAX), Version::new(10_000_000_000_000_000, 1, 0), Version::new(u64::MAX, u64::MAX, 1)]
            .map(|v| state_key("c", v));
        names.sort();
        let order: Vec<Version> = names.iter().map(|k| parse_version(k, ".json.zst").unwrap()).collect();
        assert_eq!(order, [Version::new(1, 4, u64::MAX), Version::new(1, 30, 7), Version::new(2, 1, 0), Version::new(10_000_000_000_000_000, 1, 0), Version::new(u64::MAX, u64::MAX, 1)]);
        // Run markers live among the node's WAL objects but are never taken for one.
        assert_eq!(run_key("n1", 42), "search/wal/n1/0000000000000042.run");
        assert_eq!(parse_run_key(&run_key("n1", 42)), Some(("n1", 42)));
        assert_eq!(parse_wal_key(&run_key("n1", 42)), None);
        assert_eq!(parse_run_key(&wal_key("n1", 42)), None);
        assert_eq!(parse_run_key("search/wal/n1/+000000000000042.run"), None);
        assert_eq!(parse_wal_key("search/wal/n1/+000000000000042.zst"), None);
    }

    #[test]
    fn names_are_validated() {
        assert!(validate_cluster("my-app_1.preview").is_ok());
        for bad in ["", ".", "..", "a/b", "a b", &"x".repeat(256)] {
            assert!(validate_cluster(bad).is_err(), "{bad}");
        }
        assert!(validate_shard(&ShardId::new("q2DfV1x-TfOJ0yH9Z0bJ_w", 0)).is_ok());
        assert!(validate_shard(&ShardId::new("../x", 0)).is_err());
    }
}
