//! The `s3` repository type (Elasticsearch's `repository-s3`): snapshots in an S3-compatible
//! bucket the tenant names, so they outlive the cluster and another cluster (on any roll
//! deployment) can restore them.
//!
//! Settings: `bucket` (required), `base_path`, `endpoint` (a host, `host:port` or URL; AWS's
//! regional endpoint by default), `protocol`, `region` (`us-east-1` by default; the signing
//! region), `path_style_access`, `readonly`, and the credentials `access_key`, `secret_key` and
//! `session_token`. Elasticsearch reads credentials from its keystore; a roll cluster has no
//! keystore its tenant can write, so they come with the repository's settings and are kept apart
//! from them ([`Repository::secrets`]), sealed under the deployment's key ([`crate::seal`]),
//! opened only to sign a request and never shown: `GET _snapshot` and the cluster state print
//! the settings without them. A repository without credentials reads and writes anonymously;
//! roll's own credentials are never used. The endpoint is reached as every URL a tenant chooses
//! is ([`crate::s3`]: public addresses only unless the node sets [`ALLOW_PRIVATE`]).
//!
//! What the bucket holds, under `<base_path>/roll/` (a layout of roll's own, so an
//! Elasticsearch repository at the same path is left alone):
//!
//! - `index-<N>` (gzipped JSON): the repository data at generation `N`, every snapshot with its
//!   index metadata, written only if absent (`If-None-Match`), so two writers never both commit
//!   generation `N`; `index.latest` names the newest, and readers look past it for a newer one
//!   a crash left unnamed. The cluster keeps a copy in its state, read again before every
//!   snapshot operation and `GET _snapshot` on the repository.
//! - `indices/<index uuid>/<shard>/snap-<snapshot uuid>.json`: what a shard snapshot holds, the
//!   segments it read, each by `<segment id>-<live docs>` and its chunks.
//! - `indices/<index uuid>/<shard>/<segment id>-<live docs>.<n>.gz`: the documents a restore
//!   indexes again (id, routing, source with its vectors, sequence number, primary term and
//!   version), gzipped, about 8 MiB a chunk. A segment's live documents only ever shrink, so the
//!   key names one set of documents for good: a later snapshot of an unchanged segment reuses
//!   its chunks (incremental snapshots, as Elasticsearch reuses files) and only new segments are
//!   read and uploaded.
//!
//! These are documents, not roll's segment files: a restore parses JSON it reads with bounds,
//! never segment files from a bucket someone else may have written, and the format is the same
//! whichever roll version wrote it. Taking a snapshot reads the shards' current searchers (a
//! closed index's last commit, pinned in the cluster's own storage while it is read); the
//! cluster's own storage keeps nothing once the snapshot is in the bucket.

use super::*;
use crate::s3::{self as client, Credentials, Location};
use crate::write::{CopyTarget, DocCopy};
use bytes::Bytes;
use std::collections::{BTreeMap, HashMap, HashSet};
use std::io::{BufRead, Read, Write};

/// Node setting that lets `s3` repositories reach private addresses (local development).
pub const ALLOW_PRIVATE: &str = "roll.repository.s3.allow_private_hosts";
/// Associated data of sealed repository credentials.
const SECRET_KIND: &str = "s3 repository credential";
/// Settings that are credentials ([`Repository::secrets`]).
pub const SECRET_SETTINGS: [&str; 3] = ["access_key", "secret_key", "session_token"];

/// Bounds on what is read from a bucket.
const DATA_MAX: usize = 256 << 20;
const SHARD_FILE_MAX: usize = 64 << 20;
const CHUNK_MAX: usize = 128 << 20;
const HEAD_MAX: usize = 64 << 10;
const SOURCE_MAX: usize = 512 << 20;
/// A chunk is closed once it holds this much compressed.
const CHUNK_TARGET: usize = 8 << 20;
/// Generations past `index.latest` a reader looks for.
const PROBES: i64 = 16;
/// `index.number_of_shards`' bound: repository data from a bucket is someone's input.
const MAX_SHARDS: u32 = 1024;

fn failed(name: &str, e: impl std::fmt::Display) -> EsError {
    repository_exception(name, format!("{e}"))
}

/// Validates an `s3` repository's settings (`PUT _snapshot`).
pub(super) fn validate(cluster: &crate::Cluster, name: &str, settings: &Settings) -> EsResult<()> {
    let create = |e: EsError| repository_exception(name, "failed to create repository").caused_by(e);
    if settings.get("client") == Some(crate::meta::snapshots::ROLL_CLIENT) {
        return Err(create(EsError::illegal_argument(format!("the client [{}] is reserved for the cluster's default repository", crate::meta::snapshots::ROLL_CLIENT))));
    }
    let bucket = settings.get("bucket").unwrap_or_default();
    if bucket.is_empty() {
        return Err(create(EsError::illegal_argument("Invalid S3 bucket name, cannot be null or empty")));
    }
    if !client::valid_bucket(bucket) {
        return Err(create(EsError::illegal_argument(format!("invalid bucket name [{bucket}]"))));
    }
    let base = settings.get("base_path").unwrap_or_default();
    if !base.is_empty() && base.trim_end_matches('/').split('/').any(|seg| seg.is_empty() || seg == "." || seg == "..") {
        return Err(create(EsError::illegal_argument(format!("invalid base_path [{base}]"))));
    }
    if let Some(v) = settings.get("path_style_access") {
        crate::units::parse_bool(v).map_err(create)?;
    }
    location(cluster, settings).map_err(|e| create(EsError::illegal_argument(e)))?;
    let given: Vec<&str> = SECRET_SETTINGS[..2].iter().copied().filter(|k| settings.get(k).is_some_and(|v| !v.is_empty())).collect();
    if given.len() == 1 {
        let missing = if given[0] == "access_key" { "secret_key" } else { "access_key" };
        return Err(create(EsError::illegal_argument(format!("Missing [{missing}] for s3 repository [{name}]: give both [access_key] and [secret_key], or neither"))));
    }
    Ok(())
}

/// Moves the credentials out of `settings`, as the cluster stores them (sealed when it can).
pub(super) fn take_secrets(cluster: &crate::Cluster, settings: &mut Settings) -> BTreeMap<String, String> {
    let mut out = BTreeMap::new();
    for k in SECRET_SETTINGS {
        if let Some(v) = settings.remove(k)
            && let Some(v) = v.as_str().filter(|v| !v.is_empty())
        {
            out.insert(k.to_string(), crate::seal::seal_given(cluster, SECRET_KIND, v));
        }
    }
    out
}

fn allow_private(cluster: &crate::Cluster) -> bool {
    cluster.config.node_setting(ALLOW_PRIVATE).is_some_and(|v| v.eq_ignore_ascii_case("true"))
}

fn location(cluster: &crate::Cluster, s: &Settings) -> Result<Location, String> {
    let region = s.get("region").filter(|r| !r.is_empty()).unwrap_or("us-east-1").to_string();
    let endpoint = client::endpoint(s.get("endpoint"), s.get("protocol"), &region, allow_private(cluster))?;
    Ok(Location { endpoint, bucket: s.get("bucket").unwrap_or_default().to_string(), region, path_style: s.get("path_style_access").is_some_and(|v| v == "true") })
}

/// A shard snapshot's contents in the bucket.
#[derive(Clone, Debug, Default, PartialEq)]
pub(super) struct ShardBlobs {
    segments: Vec<SegmentBlob>,
}

#[derive(Clone, Debug, PartialEq)]
struct SegmentBlob {
    /// `<segment id>-<live docs>`.
    key: String,
    docs: u64,
    /// Each chunk's size.
    chunks: Vec<u64>,
}

/// `<32 hex digits>-<digits>`: anything else in a shard file isn't roll's.
fn valid_segment_key(k: &str) -> bool {
    k.split_once('-').is_some_and(|(id, n)| id.len() == 32 && id.bytes().all(|b| b.is_ascii_hexdigit()) && !n.is_empty() && n.bytes().all(|b| b.is_ascii_digit()))
}

/// Index uuids name object keys: `[A-Za-z0-9_-]+` only.
fn valid_uuid(u: &str) -> bool {
    !u.is_empty() && u.len() <= 64 && u.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
}

impl ShardBlobs {
    fn to_json(&self) -> Value {
        obj! {
            "segments" => Value::Arr(self.segments.iter().map(|s| obj! {
                "key" => s.key.clone(), "docs" => s.docs as i64,
                "chunks" => Value::Arr(s.chunks.iter().map(|c| Value::from(*c as i64)).collect()),
            }).collect()),
        }
    }

    fn parse(raw: &[u8]) -> Option<ShardBlobs> {
        let v = crate::json::parse(raw).ok()??;
        let mut out = ShardBlobs::default();
        for s in v.get("segments")?.as_array()? {
            let key = s.get("key")?.as_str()?.to_string();
            if !valid_segment_key(&key) {
                return None;
            }
            let chunks = s.get("chunks")?.as_array()?.iter().map(|c| c.as_i64().filter(|n| *n >= 0).map(|n| n as u64)).collect::<Option<Vec<_>>>()?;
            out.segments.push(SegmentBlob { key, docs: s.get("docs").and_then(Value::as_i64).unwrap_or(0).max(0) as u64, chunks });
        }
        Some(out)
    }
}

/// A registered `s3` repository's bucket.
pub(super) struct Bucket {
    name: String,
    s3: client::Client,
    /// `<base_path>/roll/`.
    prefix: String,
}

impl Bucket {
    pub(super) fn open(cluster: &crate::Cluster, name: &str, r: &Repository) -> EsResult<Bucket> {
        let at = location(cluster, &r.settings).map_err(|e| failed(name, e))?;
        let creds = match (r.secrets.get("access_key"), r.secrets.get("secret_key")) {
            (Some(a), Some(s)) => Some(Credentials {
                access_key: crate::seal::open(cluster, SECRET_KIND, a)?,
                secret_key: crate::seal::open(cluster, SECRET_KIND, s)?,
                session_token: r.secrets.get("session_token").map(|t| crate::seal::open(cluster, SECRET_KIND, t)).transpose()?,
            }),
            _ => None,
        };
        let s3 = client::Client::new(at, creds, allow_private(cluster)).map_err(|e| failed(name, e))?;
        let base = r.settings.get("base_path").unwrap_or_default().trim_matches('/');
        let prefix = if base.is_empty() { "roll/".to_string() } else { format!("{base}/roll/") };
        Ok(Bucket { name: name.to_string(), s3, prefix })
    }

    fn fail(&self, what: &str, e: client::Error) -> EsError {
        repository_exception(&self.name, format!("{what}: {e}"))
    }

    fn shard_dir(&self, index_uuid: &str, shard: u32) -> String {
        format!("{}indices/{index_uuid}/{shard}/", self.prefix)
    }

    fn shard_file(&self, snapshot: &str, index_uuid: &str, shard: u32) -> String {
        format!("{}snap-{snapshot}.json", self.shard_dir(index_uuid, shard))
    }

    fn chunk(&self, index_uuid: &str, shard: u32, segment: &str, n: usize) -> String {
        format!("{}{segment}.{n}.gz", self.shard_dir(index_uuid, shard))
    }

    async fn data_at(&self, generation: i64) -> EsResult<Option<RepositoryData>> {
        let key = format!("{}index-{generation}", self.prefix);
        let Some(raw) = self.s3.get(&key, DATA_MAX).await.map_err(|e| self.fail("reading the repository data", e))? else { return Ok(None) };
        let mut text = Vec::new();
        flate2::read::GzDecoder::new(&raw[..]).take(DATA_MAX as u64 * 8).read_to_end(&mut text).map_err(|e| failed(&self.name, format!("the repository data [{key}] is damaged: {e}")))?;
        let v = crate::json::parse(&text).ok().flatten();
        let data = v.as_ref().and_then(RepositoryData::from_stored).ok_or_else(|| failed(&self.name, format!("the repository data [{key}] is damaged")))?;
        if data.generation != generation {
            return Err(failed(&self.name, format!("the repository data [{key}] claims generation [{}]", data.generation)));
        }
        Ok(Some(data))
    }

    /// The newest repository data (`None`: no snapshot was ever written here).
    pub(super) async fn read_data(&self) -> EsResult<Option<RepositoryData>> {
        let latest = self.s3.get(&format!("{}index.latest", self.prefix), 64).await.map_err(|e| self.fail("reading index.latest", e))?;
        let mut generation = latest.and_then(|b| std::str::from_utf8(&b).ok().and_then(|t| t.trim().parse::<i64>().ok())).unwrap_or(0).max(0);
        let mut found = match generation {
            0 => None,
            g => self.data_at(g).await?,
        };
        for _ in 0..PROBES {
            match self.data_at(generation + 1).await? {
                Some(d) => {
                    generation += 1;
                    found = Some(d);
                }
                None => break,
            }
        }
        Ok(found)
    }

    /// Commits `data` (generation `previous + 1`): fails when another writer committed that
    /// generation first.
    async fn write_data(&self, data: &RepositoryData, previous: i64) -> EsResult<()> {
        if data.generation != previous + 1 {
            return Err(EsError::internal(format!("[{}] repository generation [{}] doesn't follow [{previous}]", self.name, data.generation)));
        }
        let mut enc = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
        let _ = enc.write_all(data.to_stored().to_json().as_bytes());
        let body = Bytes::from(enc.finish().unwrap_or_default());
        let key = format!("{}index-{}", self.prefix, data.generation);
        if !self.s3.put_new(&key, body).await.map_err(|e| self.fail("writing the repository data", e))? {
            return Err(repository_exception(
                &self.name,
                format!("concurrent modification of the index-N file, expected current generation [{previous}] but it was not found in the repository: another cluster writes to this repository"),
            ));
        }
        self.s3.put(&format!("{}index.latest", self.prefix), Bytes::from(data.generation.to_string())).await.map_err(|e| self.fail("writing index.latest", e))?;
        // Readers racing this one may still read the previous generation.
        if data.generation > 2 {
            let _ = self.s3.delete(&format!("{}index-{}", self.prefix, data.generation - 2)).await;
        }
        Ok(())
    }

    /// Writes, reads back and deletes a test object (`_verify`, and `PUT` with `verify`).
    pub(super) async fn verify(&self) -> EsResult<()> {
        let key = format!("{}tests-{}", self.prefix, random_uuid());
        let seed = Bytes::from(random_uuid());
        let check = async {
            self.s3.put(&key, seed.clone()).await?;
            let back = self.s3.get(&key, 1024).await?;
            self.s3.delete(&key).await?;
            Ok::<_, client::Error>(back)
        };
        let path = self.prefix.trim_end_matches('/').to_string();
        let unreachable = |cause: EsError| EsError::new(500, "repository_verification_exception", format!("[{}] path [{path}] is not accessible on master node", self.name)).caused_by(cause);
        match check.await {
            Ok(Some(b)) if b == seed => Ok(()),
            Ok(_) => Err(unreachable(repository_exception(&self.name, "a test object read back differs from what was written"))),
            Err(e) => Err(unreachable(self.fail("store location", e))),
        }
    }

    pub(super) async fn read_shard(&self, snapshot: &str, index_uuid: &str, shard: u32) -> EsResult<Option<ShardBlobs>> {
        if !valid_uuid(index_uuid) {
            return Err(failed(&self.name, format!("invalid index uuid [{index_uuid}] in the repository data")));
        }
        let key = self.shard_file(snapshot, index_uuid, shard);
        let Some(raw) = self.s3.get(&key, SHARD_FILE_MAX).await.map_err(|e| self.fail("reading a shard snapshot", e))? else { return Ok(None) };
        ShardBlobs::parse(&raw).map(Some).ok_or_else(|| failed(&self.name, format!("the shard snapshot [{key}] is damaged")))
    }

    /// Uploads shard `shard` of index `index_uuid` (its live documents in `searcher`; `None`:
    /// empty) as snapshot `snapshot`'s, reusing what `previous` (the shard in the latest earlier
    /// snapshot here) holds. Keys written are added to `uploaded`.
    #[allow(clippy::too_many_arguments)]
    async fn export_shard(
        &self,
        searcher: Option<&search_index::Searcher>,
        vectors: &[Arc<crate::engine::fields::FieldInfo>],
        snapshot: &str,
        index_uuid: &str,
        shard: u32,
        previous: Option<&ShardBlobs>,
        uploaded: &mut Vec<String>,
    ) -> EsResult<ShardFiles> {
        let mut files = ShardFiles::default();
        let mut blobs = ShardBlobs::default();
        if let Some(searcher) = searcher {
            let reuse: HashMap<&str, &SegmentBlob> = previous.map(|p| p.segments.iter().map(|s| (s.key.as_str(), s)).collect()).unwrap_or_default();
            let mut fresh: Vec<(u32, String)> = Vec::new();
            for seg in searcher.segments().iter().filter(|s| s.num_docs > 0) {
                let key = format!("{}-{}", client::hex(&seg.reader.id()), seg.num_docs);
                match reuse.get(key.as_str()) {
                    Some(old) => {
                        files.file_count += old.chunks.len() as u64;
                        files.size_in_bytes += old.chunks.iter().sum::<u64>();
                        blobs.segments.push((*old).clone());
                    }
                    None => fresh.push((seg.ord, key)),
                }
            }
            let wanted: HashSet<u32> = fresh.iter().map(|(o, _)| *o).collect();
            let mut docs = crate::write::live_root_docs(searcher, &|seg| wanted.contains(&seg.ord))?.into_iter().peekable();
            for (ord, key) in fresh {
                let mut seg = SegmentBlob { key, docs: 0, chunks: Vec::new() };
                let mut chunk = Chunk::new();
                while let Some(&doc) = docs.peek() {
                    if searcher.segment_of(doc).map(|(s, _)| s.ord) != Some(ord) {
                        break;
                    }
                    docs.next();
                    chunk.push(&crate::write::read_copy(searcher, vectors, doc)?);
                    seg.docs += 1;
                    if chunk.len() >= CHUNK_TARGET {
                        let body = std::mem::replace(&mut chunk, Chunk::new()).finish();
                        self.put_chunk(index_uuid, shard, &mut seg, body, &mut files, uploaded).await?;
                    }
                }
                if chunk.records > 0 {
                    self.put_chunk(index_uuid, shard, &mut seg, chunk.finish(), &mut files, uploaded).await?;
                }
                blobs.segments.push(seg);
            }
        }
        let body = Bytes::from(blobs.to_json().to_json());
        let key = self.shard_file(snapshot, index_uuid, shard);
        let n = body.len() as u64;
        self.s3.put(&key, body).await.map_err(|e| self.fail("writing a shard snapshot", e))?;
        uploaded.push(key);
        files.file_count += 1;
        files.size_in_bytes += n;
        files.incremental_file_count += 1;
        files.incremental_size_in_bytes += n;
        files.segments = blobs.segments.len() as u32;
        Ok(files)
    }

    async fn put_chunk(&self, index_uuid: &str, shard: u32, seg: &mut SegmentBlob, body: Vec<u8>, files: &mut ShardFiles, uploaded: &mut Vec<String>) -> EsResult<()> {
        let key = self.chunk(index_uuid, shard, &seg.key, seg.chunks.len());
        let n = body.len() as u64;
        self.s3.put(&key, Bytes::from(body)).await.map_err(|e| self.fail("writing snapshot contents", e))?;
        uploaded.push(key);
        seg.chunks.push(n);
        files.file_count += 1;
        files.size_in_bytes += n;
        files.incremental_file_count += 1;
        files.incremental_size_in_bytes += n;
        Ok(())
    }

    /// Indexes every document shard `shard` of index `index_uuid` holds in `blobs` into `idx`
    /// (unrefreshed). A chunk is read, then indexed: nothing is held across the reads.
    async fn import(&self, cluster: &crate::Cluster, idx: &Arc<IndexMeta>, blobs: &ShardBlobs, index_uuid: &str, shard: u32) -> EsResult<()> {
        for seg in &blobs.segments {
            for n in 0..seg.chunks.len() {
                let key = self.chunk(index_uuid, shard, &seg.key, n);
                let Some(raw) = self.s3.get(&key, CHUNK_MAX).await.map_err(|e| self.fail("reading snapshot contents", e))? else {
                    return Err(EsError::new(500, "snapshot_restore_exception", format!("[{}] the snapshot contents [{key}] are missing from the repository", self.name)));
                };
                let damaged = |why: &str| EsError::new(500, "snapshot_restore_exception", format!("[{}] the snapshot contents [{key}] are damaged: {why}", self.name));
                let target = CopyTarget::new(cluster, idx)?;
                let mut r = std::io::BufReader::new(flate2::read::GzDecoder::new(&raw[..]));
                while let Some(copy) = read_record(&mut r).map_err(|e| damaged(&e))? {
                    target.index(&copy)?;
                }
            }
        }
        Ok(())
    }

    /// Deletes what the snapshots `gone` held that no snapshot of `kept` holds. Failures leave
    /// objects behind for `_cleanup`.
    async fn delete_snapshots(&self, gone: &[Arc<Snapshot>], kept: &RepositoryData) {
        let mut held: HashMap<(String, u32), HashSet<String>> = HashMap::new();
        for s in gone {
            for (uuid, shards) in recorded_indices(s) {
                for shard in 0..shards {
                    let Ok(Some(blobs)) = self.read_shard(&s.uuid, &uuid, shard).await else { continue };
                    if !held.contains_key(&(uuid.clone(), shard)) {
                        let mut keys = HashSet::new();
                        for k in kept.snapshots.iter().filter(|k| recorded_indices(k).iter().any(|(u, _)| *u == uuid)) {
                            match self.read_shard(&k.uuid, &uuid, shard).await {
                                Ok(Some(b)) => keys.extend(b.segments.into_iter().map(|s| s.key)),
                                Ok(None) => {}
                                // Unsure what's still held: delete none of this shard's chunks.
                                Err(_) => keys.extend(blobs.segments.iter().map(|s| s.key.clone())),
                            }
                        }
                        held.insert((uuid.clone(), shard), keys);
                    }
                    let keep = &held[&(uuid.clone(), shard)];
                    for seg in blobs.segments.iter().filter(|seg| !keep.contains(&seg.key)) {
                        for n in 0..seg.chunks.len() {
                            let _ = self.s3.delete(&self.chunk(&uuid, shard, &seg.key, n)).await;
                        }
                    }
                    let _ = self.s3.delete(&self.shard_file(&s.uuid, &uuid, shard)).await;
                }
            }
        }
    }

    /// Every object under `indices/` that no snapshot of `data` references, older repository
    /// data and test objects (`_cleanup`): `(bytes, objects)` deleted.
    async fn cleanup(&self, data: &RepositoryData) -> EsResult<(u64, u64)> {
        let mut referenced: HashSet<String> = HashSet::new();
        for s in &data.snapshots {
            for (uuid, shards) in recorded_indices(s) {
                for shard in 0..shards {
                    referenced.insert(self.shard_file(&s.uuid, &uuid, shard));
                    if let Some(b) = self.read_shard(&s.uuid, &uuid, shard).await? {
                        for seg in b.segments {
                            referenced.extend((0..seg.chunks.len()).map(|n| self.chunk(&uuid, shard, &seg.key, n)));
                        }
                    }
                }
            }
        }
        let listed = self.s3.list(&self.prefix).await.map_err(|e| self.fail("listing the repository", e))?;
        let (mut bytes, mut count) = (0, 0);
        for (key, size) in listed {
            let Some(rel) = key.strip_prefix(&self.prefix) else { continue };
            let stale = match rel.strip_prefix("index-").and_then(|g| g.parse::<i64>().ok()) {
                Some(g) => g + 1 < data.generation,
                None => rel.starts_with("tests-") || (rel.starts_with("indices/") && !referenced.contains(&key)),
            };
            if stale && self.s3.delete(&key).await.is_ok() {
                bytes += size;
                count += 1;
            }
        }
        Ok((bytes, count))
    }

    /// `(snapshot uuid, index uuid, shard)` of every shard snapshot in the bucket.
    async fn shard_snapshots(&self) -> EsResult<HashSet<(String, String, u32)>> {
        let listed = self.s3.list(&format!("{}indices/", self.prefix)).await.map_err(|e| self.fail("listing the repository", e))?;
        Ok(listed
            .iter()
            .filter_map(|(k, _)| {
                let rel = k.strip_prefix(&self.prefix)?.strip_prefix("indices/")?;
                let mut parts = rel.split('/');
                let (uuid, shard, file) = (parts.next()?, parts.next()?.parse().ok()?, parts.next()?);
                let snap = file.strip_prefix("snap-")?.strip_suffix(".json")?;
                Some((snap.to_string(), uuid.to_string(), shard))
            })
            .collect())
    }

    async fn delete_keys(&self, keys: &[String]) {
        for k in keys {
            let _ = self.s3.delete(k).await;
        }
    }
}

/// A chunk being written: records, gzipped.
struct Chunk {
    enc: flate2::write::GzEncoder<Vec<u8>>,
    records: usize,
}

impl Chunk {
    fn new() -> Chunk {
        Chunk { enc: flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::fast()), records: 0 }
    }

    /// One record: a JSON line (id, routing, sequence number, primary term, version and the
    /// source's length), the source's bytes as stored, a line feed.
    fn push(&mut self, c: &DocCopy) {
        let head = obj! {
            "id" => c.id.clone(),
            "routing" => c.routing.clone().map(Value::from).unwrap_or(Value::Null),
            "seq_no" => c.seq_no, "primary_term" => c.primary_term, "version" => c.version,
            "len" => c.source.len() as i64,
        };
        let w = &mut self.enc;
        // Writing into a Vec doesn't fail.
        let _ = w.write_all(head.to_json().as_bytes()).and_then(|_| w.write_all(b"\n")).and_then(|_| w.write_all(&c.source)).and_then(|_| w.write_all(b"\n"));
        self.records += 1;
    }

    fn len(&self) -> usize {
        self.enc.get_ref().len()
    }

    fn finish(self) -> Vec<u8> {
        self.enc.finish().unwrap_or_default()
    }
}

/// The next record of a chunk ([`Chunk::push`]), `None` at its end.
fn read_record(r: &mut impl BufRead) -> Result<Option<DocCopy>, String> {
    let mut head = Vec::new();
    let n = r.by_ref().take(HEAD_MAX as u64 + 1).read_until(b'\n', &mut head).map_err(|e| e.to_string())?;
    if n == 0 {
        return Ok(None);
    }
    if head.last() != Some(&b'\n') {
        return Err("a record's header is cut short or too long".into());
    }
    let v = crate::json::parse(&head).ok().flatten().ok_or("a record's header isn't JSON")?;
    let int = |k: &str| v.get(k).and_then(Value::as_i64).ok_or_else(|| format!("a record has no [{k}]"));
    let len = int("len")?;
    if !(0..=SOURCE_MAX as i64).contains(&len) {
        return Err(format!("a record's source length [{len}] is out of bounds"));
    }
    let mut source = Vec::new();
    r.by_ref().take(len as u64).read_to_end(&mut source).map_err(|e| e.to_string())?;
    let mut lf = [0u8; 1];
    if source.len() != len as usize || r.read_exact(&mut lf).is_err() || lf[0] != b'\n' {
        return Err("a record's source is cut short".into());
    }
    Ok(Some(DocCopy {
        id: v.get("id").and_then(Value::as_str).ok_or("a record has no [id]")?.to_string(),
        routing: v.get("routing").and_then(Value::as_str).map(str::to_string),
        source: Bytes::from(source),
        seq_no: int("seq_no")?,
        primary_term: int("primary_term")?,
        version: int("version")?,
    }))
}

/// Repository `name` when it keeps its contents in a tenant's bucket.
pub(super) fn bucket_repository(st: &ClusterState, name: &str) -> Option<Arc<Repository>> {
    st.metadata.repositories.get(name).filter(|r| r.in_bucket()).cloned()
}

/// Makes the cluster's copy of the bucket's repository data the bucket's (one state write when
/// it changed).
async fn store_copy(cluster: &crate::Cluster, r: &Repository, latest: Option<RepositoryData>) -> EsResult<()> {
    let key = r.location_key();
    let same = match (cluster.state().metadata.repository_data.get(&key), &latest) {
        (Some(c), Some(l)) => c.uuid == l.uuid && c.generation == l.generation,
        (None, None) => true,
        _ => false,
    };
    if same {
        return Ok(());
    }
    cluster
        .update(move |st| {
            match latest {
                Some(l) => st.metadata.repository_data.insert(key, Arc::new(l)),
                None => st.metadata.repository_data.remove(&key),
            };
            Ok(())
        })
        .await
}

/// Reads the bucket's repository data into the cluster's copy.
pub(super) async fn refresh(cluster: &crate::Cluster, name: &str, r: &Repository) -> EsResult<Bucket> {
    let bucket = Bucket::open(cluster, name, r)?;
    let latest = bucket.read_data().await?;
    store_copy(cluster, r, latest).await?;
    Ok(bucket)
}

/// Applies `f` (a change of the repository's data, as the in-cluster operations make it) to the
/// bucket's newest data, commits the result there, then to the cluster's copy.
pub(super) async fn commit_with<R>(ctx: &Ctx<'_>, name: &str, r: &Repository, f: impl FnOnce(&mut ClusterState) -> EsResult<R>) -> EsResult<R> {
    let bucket = refresh(ctx.cluster, name, r).await?;
    let key = r.location_key();
    let mut st = (*ctx.state()).clone();
    let before = st.metadata.repository_data.get(&key).cloned();
    let out = f(&mut st)?;
    let after = st.metadata.repository_data.get(&key).cloned();
    if let Some(after) = after
        && before.as_ref().is_none_or(|b| b.generation != after.generation)
    {
        bucket.write_data(&after, before.as_ref().map_or(0, |b| b.generation)).await?;
        store_copy(ctx.cluster, r, Some((*after).clone())).await?;
    }
    Ok(out)
}

/// `PUT _snapshot/{repository}/{snapshot}` into a bucket: plans the snapshot against the
/// bucket's newest data, uploads each included shard, then commits the snapshot to the bucket
/// (and the cluster's copy). An index created since the plan that the snapshot would now
/// include is uploaded before trying again. What a failed snapshot uploaded is deleted.
pub(super) async fn create<B>(ctx: &Ctx<'_>, repo: &str, r: &Repository, uuid: &str, files: &CapturedFiles, build: B) -> EsResult<Arc<Snapshot>>
where
    B: FnOnce(&mut ClusterState) -> EsResult<Arc<Snapshot>> + Clone + Send,
{
    let bucket = refresh(ctx.cluster, repo, r).await?;
    let key = r.location_key();
    let (planned, running) = {
        let st = ctx.state();
        let planned = build.clone()(&mut (*st).clone())?;
        let running = ctx.cluster.snapshot_data.start_running(repo, &planned, &st)?;
        (planned, running)
    };
    let _taking = ctx.cluster.snapshot_data.taking(uuid);
    let mut uploaded = Vec::new();
    let mut committed = false;
    let result = async {
        let mut exported: HashSet<String> = HashSet::new();
        let mut plan = (*planned).clone();
        for _ in 0..8 {
            let missing: Vec<(String, u32)> = recorded_indices(&plan).into_iter().filter(|(u, _)| !exported.contains(u)).collect();
            for (index_uuid, shards) in &missing {
                let f = export_index(ctx, &bucket, uuid, &key, index_uuid, *shards, &running.entry, &mut uploaded).await?;
                files.lock().insert(index_uuid.clone(), f);
                exported.insert(index_uuid.clone());
            }
            // Recorded against the bucket's newest data, unless an index appeared meanwhile.
            store_copy(ctx.cluster, r, bucket.read_data().await?).await?;
            let mut st = (*ctx.state()).clone();
            let before = st.metadata.repository_data.get(&key).map(|d| d.generation).unwrap_or(0);
            let snapshot = build.clone()(&mut st)?;
            if recorded_indices(&snapshot).iter().any(|(u, _)| !exported.contains(u)) {
                plan = (*snapshot).clone();
                continue;
            }
            let after = st.metadata.repository_data.get(&key).cloned().ok_or_else(|| EsError::internal("the snapshot recorded no repository data"))?;
            bucket.write_data(&after, before).await?;
            committed = true;
            store_copy(ctx.cluster, r, Some((*after).clone())).await?;
            return Ok(snapshot);
        }
        let snap = &plan.name;
        Err(EsError::new(500, "snapshot_exception", format!("[{repo}:{snap}] indices kept being created while the snapshot was being taken")))
    }
    .await;
    drop(running);
    // Closed indices were read from pins of their last commit: they go now.
    if let Some(store) = ctx.cluster.engine.snapshot_storage() {
        let _ = drop_unused_copies(ctx.cluster, &*store, |c| c.snapshot == uuid, LEFTOVER_GRACE).await;
    }
    if result.is_err() && !committed {
        bucket.delete_keys(&uploaded).await;
    }
    result
}

/// Uploads every shard of index `index_uuid` (`shards` of them) as snapshot `uuid`'s.
#[allow(clippy::too_many_arguments)]
async fn export_index(ctx: &Ctx<'_>, bucket: &Bucket, uuid: &str, location: &str, index_uuid: &str, shards: u32, running: &Running, uploaded: &mut Vec<String>) -> EsResult<Vec<ShardFiles>> {
    let st = ctx.state();
    let idx = st.metadata.indices.values().find(|i| i.uuid == index_uuid).cloned();
    let previous = previous_snapshot(&st, location, index_uuid).map(|(u, _)| u);
    let loaded = ctx.cluster.engine.loaded(index_uuid);
    let vectors = match (&loaded, &idx) {
        (Some(ie), _) => crate::engine::source_vectors::copied(&ie.table()),
        (None, Some(i)) => ctx.cluster.engine.copied_vectors(ctx.cluster, &st, i)?,
        (None, None) => Vec::new(),
    };
    // A shard not open here (a closed index) is read from a pin of its last commit.
    let mut pinned: Vec<Option<search_index::Searcher>> = Vec::new();
    if loaded.is_none()
        && let (Some(store), Some(i)) = (ctx.cluster.engine.snapshot_storage(), &idx)
    {
        for n in 0..shards {
            store.capture(uuid, index_uuid, n, None, None).await?;
        }
        for shard in store.load(uuid, index_uuid, shards).await? {
            pinned.push(ctx.cluster.engine.snapshot_searchers(ctx.cluster, &st, i, vec![shard])?.pop());
        }
    }
    let mut out = Vec::with_capacity(shards as usize);
    for n in 0..shards {
        let start_ms = crate::time::now_millis();
        running.set(index_uuid, n, ShardRun::Started { start_ms });
        let searcher = match &loaded {
            Some(ie) => match ie.shards.get(n as usize) {
                Some(s) => {
                    s.shard.refresh().map_err(crate::engine::convert::index_error)?;
                    Some(s.searcher())
                }
                None => None,
            },
            None => pinned.get_mut(n as usize).and_then(Option::take),
        };
        let prev = match &previous {
            Some(p) => bucket.read_shard(p, index_uuid, n).await?,
            None => None,
        };
        let mut f = bucket.export_shard(searcher.as_ref(), &vectors, uuid, index_uuid, n, prev.as_ref(), uploaded).await?;
        f.start_ms = start_ms;
        f.time_ms = (crate::time::now_millis() - start_ms).max(0);
        running.set(index_uuid, n, ShardRun::Done(f));
        out.push(f);
    }
    Ok(out)
}

/// `DELETE _snapshot/{repository}/{snapshot}` in a bucket: commits the repository data without
/// them (`apply` is the in-cluster deletion, returning the deleted snapshots' uuids), then
/// deletes what only they held.
pub(super) async fn delete(ctx: &Ctx<'_>, repo: &str, r: &Repository, apply: impl FnOnce(&mut ClusterState) -> EsResult<Vec<String>>) -> EsResult<Vec<String>> {
    let key = r.location_key();
    let mut before: Option<Arc<RepositoryData>> = None;
    let gone = commit_with(ctx, repo, r, |st| {
        before = st.metadata.repository_data.get(&key).cloned();
        apply(st)
    })
    .await?;
    if let (false, Some(before)) = (gone.is_empty(), before) {
        let kept = ctx.state().metadata.repository_data.get(&key).map(|d| (**d).clone()).unwrap_or_default();
        let doomed: Vec<Arc<Snapshot>> = before.snapshots.iter().filter(|s| gone.contains(&s.uuid)).cloned().collect();
        Bucket::open(ctx.cluster, repo, r)?.delete_snapshots(&doomed, &kept).await;
    }
    Ok(gone)
}

/// `_clone` in a bucket: the target's shard snapshots are the source's, for the indices it
/// keeps. Returns the keys written.
pub(super) async fn clone_shards(ctx: &Ctx<'_>, repo: &str, r: &Repository, source: &str, target_uuid: &str, patterns: &[String]) -> EsResult<Vec<String>> {
    let bucket = refresh(ctx.cluster, repo, r).await?;
    let st = ctx.state();
    let Some(src) = st.metadata.repository_data.get(&r.location_key()).and_then(|d| d.find(source).cloned()) else { return Ok(Vec::new()) };
    let mut written = Vec::new();
    for (index, meta) in &src.index_meta {
        if !patterns.iter().any(|p| glob_match(p, index)) {
            continue;
        }
        let Some((uuid, shards)) = stored_index(meta) else { continue };
        for shard in 0..shards {
            let blobs = bucket.read_shard(&src.uuid, &uuid, shard).await?.unwrap_or_default();
            let key = bucket.shard_file(target_uuid, &uuid, shard);
            if let Err(e) = bucket.s3.put(&key, Bytes::from(blobs.to_json().to_json())).await {
                bucket.delete_keys(&written).await;
                return Err(bucket.fail("writing a shard snapshot", e));
            }
            written.push(key);
        }
    }
    Ok(written)
}

/// Deletes shard snapshots [`clone_shards`] wrote for a clone that wasn't recorded.
pub(super) async fn forget_keys(ctx: &Ctx<'_>, repo: &str, r: &Repository, keys: &[String]) {
    if let Ok(b) = Bucket::open(ctx.cluster, repo, r) {
        b.delete_keys(keys).await;
    }
}

/// Whether the bucket's newest data records snapshot `uuid` (after a failed commit: it may
/// have been written anyway).
pub(super) async fn recorded(ctx: &Ctx<'_>, repo: &str, r: &Repository, uuid: &str) -> bool {
    match Bucket::open(ctx.cluster, repo, r) {
        Ok(b) => match b.read_data().await {
            Ok(d) => d.is_some_and(|d| d.snapshots.iter().any(|s| s.uuid == uuid)),
            Err(_) => true,
        },
        Err(_) => true,
    }
}

/// Every shard snapshot a restore of `copies` (`(target, source name, source index uuid)`)
/// reads, by source index uuid, read before any index is created: a snapshot whose contents
/// are missing fails without leaving empty indices behind.
pub(super) async fn read_restore(ctx: &Ctx<'_>, bucket: &Bucket, snapshot: &str, copies: &[(String, String, String)]) -> EsResult<HashMap<String, Vec<ShardBlobs>>> {
    let mut out = HashMap::new();
    for (_, source, source_uuid) in copies {
        if out.contains_key(source_uuid) {
            continue;
        }
        let Some(shards) = shards_in_snapshot(ctx, snapshot, source) else { continue };
        if shards > MAX_SHARDS {
            return Err(EsError::new(500, "snapshot_restore_exception", format!("[{}] index [{source}] has [{shards}] shards in the repository data, more than [{MAX_SHARDS}]", bucket.name)));
        }
        let mut all = Vec::with_capacity(shards as usize);
        for n in 0..shards {
            let Some(b) = bucket.read_shard(snapshot, source_uuid, n).await? else {
                return Err(EsError::new(500, "snapshot_restore_exception", format!("[{snapshot}] the contents of shard [{source_uuid}][{n}] are missing from the repository")));
            };
            all.push(b);
        }
        out.insert(source_uuid.clone(), all);
    }
    Ok(out)
}

/// Indexes a restored index's documents from the bucket.
pub(super) async fn restore_index(ctx: &Ctx<'_>, bucket: &Bucket, idx: &Arc<IndexMeta>, source_uuid: &str, shards: &[ShardBlobs]) -> EsResult<()> {
    for (n, blobs) in shards.iter().enumerate() {
        bucket.import(ctx.cluster, idx, blobs, source_uuid, n as u32).await?;
    }
    CopyTarget::new(ctx.cluster, idx)?.finish()
}

/// `_verify_integrity`: the shard snapshots present in the bucket.
pub(super) async fn present_shards(ctx: &Ctx<'_>, repo: &str, r: &Repository) -> EsResult<HashSet<(String, String, u32)>> {
    refresh(ctx.cluster, repo, r).await?.shard_snapshots().await
}

/// `_cleanup`: `(bytes, objects)` deleted.
pub(super) async fn cleanup(ctx: &Ctx<'_>, repo: &str, r: &Repository) -> EsResult<(u64, u64)> {
    let bucket = refresh(ctx.cluster, repo, r).await?;
    if !ctx.cluster.snapshot_data.running_in(repo, None).is_empty() {
        return Err(EsError::illegal_state(format!("Cannot cleanup [{repo}] - a snapshot is currently running")));
    }
    let data = ctx.state().metadata.repository_data.get(&r.location_key()).map(|d| (**d).clone()).unwrap_or_default();
    bucket.cleanup(&data).await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn records_round_trip_and_bounds_hold() {
        let copies = [
            DocCopy { id: "1".into(), routing: None, source: Bytes::from_static(b"{\"a\":1}"), seq_no: 0, primary_term: 1, version: 1 },
            DocCopy { id: "x\ny".into(), routing: Some("r".into()), source: Bytes::from_static(b"{\n \"b\" : \"\\n\"}"), seq_no: 7, primary_term: 2, version: 3 },
        ];
        let mut c = Chunk::new();
        for d in &copies {
            c.push(d);
        }
        let raw = c.finish();
        let mut r = std::io::BufReader::new(flate2::read::GzDecoder::new(&raw[..]));
        let back: Vec<DocCopy> = std::iter::from_fn(|| read_record(&mut r).unwrap()).collect();
        assert_eq!(back, copies);
        // A length past the bound, or past what's there, is refused rather than allocated.
        let huge = format!("{{\"id\":\"1\",\"seq_no\":0,\"primary_term\":1,\"version\":1,\"len\":{}}}\n", u64::MAX / 2);
        assert!(read_record(&mut std::io::Cursor::new(huge.into_bytes())).is_err());
        let short = b"{\"id\":\"1\",\"seq_no\":0,\"primary_term\":1,\"version\":1,\"len\":10}\n{}\n".to_vec();
        assert!(read_record(&mut std::io::Cursor::new(short)).is_err());
        assert!(ShardBlobs::parse(br#"{"segments":[{"key":"../../x","docs":1,"chunks":[1]}]}"#).is_none());
    }
}
