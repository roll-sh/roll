//! The `s3` repository type (Elasticsearch's `repository-s3`): snapshots in an S3-compatible
//! bucket the tenant names, so they outlive the cluster and another cluster (on any roll
//! deployment) can restore them. The `gcs` and `azure` types ([`super::cloud`]) keep the same
//! layout in a GCS bucket or an Azure container through [`Bucket`]: only how the store is
//! reached ([`crate::blob::Store`]) and their settings differ.
//!
//! Settings: `bucket` (required), `base_path`, `endpoint` (a host, `host:port` or URL; AWS's
//! regional endpoint by default), `protocol`, `region` (`us-east-1` by default; the signing
//! region), `path_style_access`, `readonly`, `chunk_size`, `buffer_size`, `max_multipart_parts`,
//! `max_multipart_upload_cleanup_size`, the throttles `max_snapshot_bytes_per_sec` and
//! `max_restore_bytes_per_sec` ([`super::transfer`]), and the credentials `access_key`,
//! `secret_key` and `session_token`. Elasticsearch reads credentials from its keystore; a roll cluster has no
//! keystore its tenant can write, so they come with the repository's settings and are kept apart
//! from them ([`Repository::secrets`]), sealed under the deployment's key ([`crate::seal`]),
//! opened only to sign a request and never shown: `GET _snapshot` and the cluster state print
//! the settings without them. A credential may instead be a roll secrets reference
//! (`roll://ENV/KEY`), kept in the settings as written and resolved when the client is built
//! ([`super::refs`]). A repository without credentials reads and writes anonymously;
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
//!   version), one gzip stream per segment cut into objects of `chunk_size` (`n` from 0), each
//!   uploaded in `buffer_size` parts when larger ([`super::transfer::Upload`]). Snapshots written
//!   before `chunk_size` gzipped each ~8 MiB chunk on its own: a reader takes the chunks as one
//!   multi-member stream, so both read alike. A segment's live documents only ever shrink, so
//!   the key names one set of documents for good: a later snapshot of an unchanged segment
//!   reuses its chunks (incremental snapshots, as Elasticsearch reuses files) and only new
//!   segments are read and uploaded.
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
use std::collections::{HashMap, HashSet};
use std::io::{Read, Write};

/// Node setting that lets `s3`, `gcs` and `azure` repositories reach private addresses (local
/// development and tests: MinIO, fake-gcs-server, Azurite).
pub const ALLOW_PRIVATE: &str = "roll.repository.s3.allow_private_hosts";
/// Associated data of sealed repository credentials.
pub(super) const SECRET_KIND: &str = "s3 repository credential";
/// Settings that are credentials ([`Repository::secrets`]).
pub const SECRET_SETTINGS: [&str; 3] = ["access_key", "secret_key", "session_token"];

/// Bounds on what is read from a bucket. The repository data is bounded compressed and
/// inflated, and in what parsing it would build: a gzipped `[0,0,…]` is two bytes a value.
const DATA_MAX: usize = 256 << 20;
const DATA_VALUES_MAX: usize = 8 << 20;
const DATA_SNAPSHOTS_MAX: usize = 10_000;
/// Indices over every snapshot of the repository data.
const DATA_INDICES_MAX: usize = 100_000;
const SHARD_FILE_MAX: usize = 64 << 20;
const HEAD_MAX: usize = 64 << 10;
/// Compressed bytes gathered before they go to the object being written.
const DRAIN: usize = 1 << 20;
/// Compressed bytes inflated at a time while reading: what one step can inflate to is bounded
/// (deflate's ratio is at most ~1032:1), and records are taken out between steps.
const FEED: usize = 8 << 10;
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
    // `S3Repository`'s constructor: `buffer_size`, `chunk_size`, `max_multipart_parts`, then
    // the chunk no smaller than the buffer.
    let buffer = transfer::bounded_size(settings, "buffer_size", transfer::default_buffer(cluster), transfer::MIN_PART, transfer::MAX_PART).map_err(create)?;
    let chunk = transfer::bounded_size(settings, "chunk_size", transfer::MAX_FILE, transfer::MIN_PART, transfer::MAX_FILE).map_err(create)?;
    transfer::max_parts(settings).map_err(create)?;
    if buffer > chunk {
        let shown = |key: &str, v: i64| settings.get(key).map_or_else(|| crate::units::ByteSize(v).human(), str::to_string);
        let why = format!("chunk_size ({}) can't be lower than buffer_size ({}).", shown("chunk_size", chunk), shown("buffer_size", buffer));
        return Err(create(repository_exception(name, why)));
    }
    base_path(settings).map_err(create)?;
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

/// `base_path`: `/`-separated names, none empty, `.` or `..` (keys are built from it).
pub(super) fn base_path(settings: &Settings) -> EsResult<()> {
    let base = settings.get("base_path").unwrap_or_default();
    if !base.is_empty() && base.trim_end_matches('/').split('/').any(|seg| seg.is_empty() || seg == "." || seg == "..") {
        return Err(EsError::illegal_argument(format!("invalid base_path [{base}]")));
    }
    Ok(())
}

pub(super) fn allow_private(cluster: &crate::Cluster) -> bool {
    cluster.config.node_setting(ALLOW_PRIVATE).is_some_and(|v| v.eq_ignore_ascii_case("true"))
}

fn location(cluster: &crate::Cluster, s: &Settings) -> Result<Location, String> {
    let region = s.get("region").filter(|r| !r.is_empty()).unwrap_or("us-east-1").to_string();
    let endpoint = client::endpoint(s.get("endpoint"), s.get("protocol"), &region, allow_private(cluster))?;
    Ok(Location { endpoint, bucket: s.get("bucket").unwrap_or_default().to_string(), region, path_style: s.get("path_style_access").is_some_and(|v| v == "true") })
}

/// An `s3` repository's client, signing with its credentials when it has them.
fn s3_client(cluster: &crate::Cluster, name: &str, r: &Repository, creds: &super::refs::Creds) -> EsResult<client::Client> {
    let at = location(cluster, &r.settings).map_err(|e| failed(name, e))?;
    let creds = match (creds.get("access_key"), creds.get("secret_key")) {
        (Some(a), Some(s)) => Some(Credentials { access_key: a.to_string(), secret_key: s.to_string(), session_token: creds.get("session_token").map(str::to_string) }),
        _ => None,
    };
    client::Client::new(at, creds, allow_private(cluster)).map_err(|e| failed(name, e))
}

/// `<base_path>/roll/`: where a repository keeps everything.
fn prefix(r: &Repository) -> String {
    let base = r.settings.get("base_path").unwrap_or_default().trim_matches('/');
    if base.is_empty() { "roll/".to_string() } else { format!("{base}/roll/") }
}

fn unreachable(name: &str, prefix: &str, cause: EsError) -> EsError {
    let path = prefix.trim_end_matches('/');
    EsError::new(500, "repository_verification_exception", format!("[{name}] path [{path}] is not accessible on master node")).caused_by(cause)
}

/// `_verify`, and `PUT _snapshot` with `verify`: a repository whose client can't be made isn't
/// accessible, as on Elasticsearch.
pub(super) async fn verify(cluster: &crate::Cluster, name: &str, r: &Repository) -> EsResult<()> {
    let bucket = Bucket::open(cluster, name, r).await.map_err(|e| unreachable(name, &prefix(r), e))?;
    bucket.verify().await
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

/// A registered `s3`, `gcs` or `azure` repository's bucket (or container).
pub(super) struct Bucket {
    pub(super) name: String,
    pub(super) store: crate::blob::Store,
    /// `<base_path>/roll/`.
    pub(super) prefix: String,
    /// The `request` breaker, charged while repository data is parsed and parts are held.
    pub(super) budget: Arc<search_aggs::memory::MemoryBudget>,
    /// The repository's traffic on this node: throttles and counters.
    pub(super) io: Arc<transfer::RepoIo>,
    pub(super) sizes: transfer::Sizes,
    /// How many dangling multipart uploads one cleanup aborts.
    upload_cleanup: usize,
    read_only: bool,
}

impl Bucket {
    /// The repository's client, built now: its credentials opened, references resolved again
    /// when their digests moved ([`super::refs`]).
    pub(super) async fn open(cluster: &crate::Cluster, name: &str, r: &Repository) -> EsResult<Bucket> {
        let creds = super::refs::credentials(cluster, name, r).await?;
        let (store, sizes) = match r.ty.as_str() {
            "gcs" => super::cloud::gcs_store(cluster, name, r, &creds)?,
            "azure" => super::cloud::azure_store(cluster, name, r, &creds)?,
            _ => (crate::blob::Store::S3(s3_client(cluster, name, r, &creds)?), transfer::sizes(cluster, &r.settings)),
        };
        let prefix = prefix(r);
        let st = cluster.state();
        let budget = cluster.breakers.request(cluster, &st);
        let io = cluster.snapshot_data.repo_io(name, r, &st);
        Ok(Bucket { name: name.to_string(), store, prefix, budget, io, sizes, upload_cleanup: transfer::upload_cleanup_limit(&r.settings), read_only: r.read_only() })
    }

    pub(super) fn fail(&self, what: &str, e: client::Error) -> EsError {
        repository_exception(&self.name, format!("{what}: {e}"))
    }

    fn failed_upload(&self, f: transfer::Failed) -> EsError {
        match f {
            transfer::Failed::Store(e) => self.fail("writing snapshot contents", e),
            transfer::Failed::Breaker(e) => e,
        }
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
        let Some(raw) = self.store.get(&key, DATA_MAX).await.map_err(|e| self.fail("reading the repository data", e))? else { return Ok(None) };
        let damaged = |why: String| {
            let cause = EsError::new(500, "corrupt_state_exception", why);
            failed(&self.name, format!("the repository data [{key}] is damaged")).caused_by(cause)
        };
        let text = inflate(&raw, DATA_MAX).map_err(damaged)?;
        drop(raw);
        let values = json_values(&text);
        if values > DATA_VALUES_MAX {
            return Err(damaged(format!("it holds more than [{DATA_VALUES_MAX}] values")));
        }
        // What the parsed tree takes, as a request holding it would be charged.
        let mut held = crate::breaker::Reservation::new(self.budget.clone());
        held.add((values * std::mem::size_of::<Value>() * 2 + text.len()) as u64, "<repository_data>")?;
        let v = crate::json::parse(&text).ok().flatten();
        drop(text);
        let data = v.as_ref().and_then(RepositoryData::from_stored).ok_or_else(|| damaged("it isn't repository data".into()))?;
        drop(v);
        if data.snapshots.len() > DATA_SNAPSHOTS_MAX {
            return Err(damaged(format!("it holds more than [{DATA_SNAPSHOTS_MAX}] snapshots")));
        }
        if data.snapshots.iter().map(|s| s.indices.len().max(s.index_meta.len()) + s.data_stream_meta.len()).sum::<usize>() > DATA_INDICES_MAX {
            return Err(damaged(format!("its snapshots hold more than [{DATA_INDICES_MAX}] indices")));
        }
        if data.generation != generation {
            return Err(failed(&self.name, format!("the repository data [{key}] claims generation [{}]", data.generation)));
        }
        Ok(Some(data))
    }

    /// The newest repository data (`None`: no snapshot was ever written here).
    pub(super) async fn read_data(&self) -> EsResult<Option<RepositoryData>> {
        let latest = self.store.get(&format!("{}index.latest", self.prefix), 64).await.map_err(|e| self.fail("reading index.latest", e))?;
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
        if !self.store.put_new(&key, body).await.map_err(|e| self.fail("writing the repository data", e))? {
            return Err(repository_exception(
                &self.name,
                format!("concurrent modification of the index-N file, expected current generation [{previous}] but it was not found in the repository: another cluster writes to this repository"),
            ));
        }
        self.store.put(&format!("{}index.latest", self.prefix), Bytes::from(data.generation.to_string())).await.map_err(|e| self.fail("writing index.latest", e))?;
        // Readers racing this one may still read the previous generation.
        if data.generation > 2 {
            let _ = self.store.delete(&format!("{}index-{}", self.prefix, data.generation - 2)).await;
        }
        Ok(())
    }

    /// Writes, reads back and deletes a test object (`_verify`, and `PUT` with `verify`); a
    /// read-only repository's is listed instead.
    async fn verify(&self) -> EsResult<()> {
        let key = format!("{}tests-{}", self.prefix, random_uuid());
        let seed = Bytes::from(random_uuid());
        let check = async {
            self.store.put(&key, seed.clone()).await?;
            let back = self.store.get(&key, 1024).await?;
            self.store.delete(&key).await?;
            Ok::<_, client::Error>(back)
        };
        let unreachable = |cause: EsError| unreachable(&self.name, &self.prefix, cause);
        if self.read_only {
            let path = self.prefix.trim_end_matches('/');
            return match self.store.list(&format!("{}index-", self.prefix)).await {
                Ok(_) => Ok(()),
                Err(e) => Err(unreachable(EsError::new(500, "i_o_exception", format!("Unable to list blobs by prefix [index-] for path [{path}]")).caused_by(self.fail("store location", e)))),
            };
        }
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
        let Some(raw) = self.store.get(&key, SHARD_FILE_MAX).await.map_err(|e| self.fail("reading a shard snapshot", e))? else { return Ok(None) };
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
            let mut reading = std::time::Duration::ZERO;
            let started = std::time::Instant::now();
            let mut docs = crate::write::live_root_docs(searcher, &|seg| wanted.contains(&seg.ord))?.into_iter().peekable();
            reading += started.elapsed();
            for (ord, key) in fresh {
                let mut up = SegmentUpload::new(self, index_uuid, shard, key);
                let sent = async {
                    while let Some(&doc) = docs.peek() {
                        if searcher.segment_of(doc).map(|(s, _)| s.ord) != Some(ord) {
                            break;
                        }
                        docs.next();
                        let started = std::time::Instant::now();
                        up.push(&crate::write::read_copy(searcher, vectors, doc)?);
                        reading += started.elapsed();
                        up.drain(DRAIN, &mut files, uploaded).await?;
                    }
                    Ok::<(), EsError>(())
                }
                .await;
                match sent {
                    Ok(()) => blobs.segments.push(up.finish(&mut files, uploaded).await?),
                    Err(e) => {
                        up.abort().await;
                        return Err(e);
                    }
                }
            }
            self.io.read_for_upload(reading);
        }
        let body = Bytes::from(blobs.to_json().to_json());
        let key = self.shard_file(snapshot, index_uuid, shard);
        let n = body.len() as u64;
        self.store.put(&key, body).await.map_err(|e| self.fail("writing a shard snapshot", e))?;
        uploaded.push(key);
        files.file_count += 1;
        files.size_in_bytes += n;
        files.incremental_file_count += 1;
        files.incremental_size_in_bytes += n;
        files.segments = blobs.segments.len() as u32;
        Ok(files)
    }

    /// Indexes every document shard `shard` of index `index_uuid` holds in `blobs` into `idx`
    /// (unrefreshed). Each segment's chunks are read as they arrive (paying the restore
    /// throttles) and their records indexed as they complete: no more than one record and what
    /// one step inflates is held.
    async fn import(&self, cluster: &crate::Cluster, idx: &Arc<IndexMeta>, blobs: &ShardBlobs, index_uuid: &str, shard: u32) -> EsResult<()> {
        let source_max = source_max(cluster);
        // Charged as the body of the write that indexed it would be. A target is made for each
        // piece read (it isn't held across a read).
        let index = |target: &CopyTarget, copy: DocCopy| -> EsResult<()> {
            let _in_flight = cluster.breakers.charge_in_flight(cluster, &cluster.state(), copy.source.len() as u64)?;
            target.index(&copy)
        };
        for seg in &blobs.segments {
            let mut records = Records::new(source_max);
            let first = self.chunk(index_uuid, shard, &seg.key, 0);
            let damaged = |key: &str, why: &str| EsError::new(500, "snapshot_restore_exception", format!("[{}] the snapshot contents [{key}] are damaged: {why}", self.name));
            for (n, &size) in seg.chunks.iter().enumerate() {
                let key = self.chunk(index_uuid, shard, &seg.key, n);
                let Some(mut body) = self.store.open(&key, None).await.map_err(|e| self.fail("reading snapshot contents", e))? else {
                    return Err(EsError::new(500, "snapshot_restore_exception", format!("[{}] the snapshot contents [{key}] are missing from the repository", self.name)));
                };
                let mut read = 0u64;
                while let Some(piece) = body.next().await.map_err(|e| self.fail("reading snapshot contents", e))? {
                    read += piece.len() as u64;
                    self.io.read(&cluster.snapshot_data.recovery, piece.len() as u64).await;
                    let target = CopyTarget::new(cluster, idx)?;
                    records.push(&piece, &mut |c| index(&target, c), &|why| damaged(&key, why))?;
                }
                if read != size {
                    return Err(damaged(&key, &format!("it holds [{read}] bytes, not the [{size}] recorded")));
                }
            }
            let target = CopyTarget::new(cluster, idx)?;
            records.finish(&mut |c| index(&target, c), &|why| damaged(&first, why))?;
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
                    let at = (uuid.clone(), shard);
                    if let std::collections::hash_map::Entry::Vacant(slot) = held.entry(at.clone()) {
                        let mut keys = HashSet::new();
                        for k in kept.snapshots.iter().filter(|k| recorded_indices(k).iter().any(|(u, _)| *u == uuid)) {
                            match self.read_shard(&k.uuid, &uuid, shard).await {
                                Ok(Some(b)) => keys.extend(b.segments.into_iter().map(|s| s.key)),
                                Ok(None) => {}
                                // Unsure what's still held: delete none of this shard's chunks.
                                Err(_) => keys.extend(blobs.segments.iter().map(|s| s.key.clone())),
                            }
                        }
                        slot.insert(keys);
                    }
                    let keep = &held[&at];
                    for seg in blobs.segments.iter().filter(|seg| !keep.contains(&seg.key)) {
                        for n in 0..seg.chunks.len() {
                            let _ = self.store.delete(&self.chunk(&uuid, shard, &seg.key, n)).await;
                        }
                    }
                    let _ = self.store.delete(&self.shard_file(&s.uuid, &uuid, shard)).await;
                }
            }
        }
        // As Elasticsearch does after snapshot deletions: uploads a crash left are aborted.
        let _ = self.abort_dangling().await;
    }

    /// Aborts the multipart uploads under the repository's prefix that aren't running here (what
    /// a crash or a lost connection left), at most `max_multipart_upload_cleanup_size` of them:
    /// how many. A server matching listed keys only exactly (MinIO) lists none here; its own
    /// stale-upload expiry removes them.
    async fn abort_dangling(&self) -> Result<u64, client::Error> {
        if self.upload_cleanup == 0 {
            return Ok(0);
        }
        let running = self.io.running_uploads();
        let uploads = self.store.list_uploads(&self.prefix, self.upload_cleanup + running.len()).await?;
        let mut aborted = 0;
        for (key, id) in uploads.into_iter().filter(|(_, id)| !running.contains(id)).take(self.upload_cleanup) {
            if self.store.abort(&key, &id).await.is_ok() {
                aborted += 1;
            }
        }
        Ok(aborted)
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
        let listed = self.store.list(&self.prefix).await.map_err(|e| self.fail("listing the repository", e))?;
        let (mut bytes, mut count) = (0, 0);
        for (key, size) in listed {
            let Some(rel) = key.strip_prefix(&self.prefix) else { continue };
            let stale = match rel.strip_prefix("index-").and_then(|g| g.parse::<i64>().ok()) {
                Some(g) => g + 1 < data.generation,
                None => rel.starts_with("tests-") || (rel.starts_with("indices/") && !referenced.contains(&key)),
            };
            if stale && self.store.delete(&key).await.is_ok() {
                bytes += size;
                count += 1;
            }
        }
        // Best effort: a server without `ListMultipartUploads` still cleans up its objects.
        let _ = self.abort_dangling().await;
        Ok((bytes, count))
    }

    /// `(snapshot uuid, index uuid, shard)` of every shard snapshot in the bucket.
    async fn shard_snapshots(&self) -> EsResult<HashSet<(String, String, u32)>> {
        let listed = self.store.list(&format!("{}indices/", self.prefix)).await.map_err(|e| self.fail("listing the repository", e))?;
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
            let _ = self.store.delete(k).await;
        }
    }
}

/// A segment's records going up: one gzip stream, cut into objects of the repository's chunk
/// size, each an [`transfer::Upload`].
struct SegmentUpload<'a> {
    bucket: &'a Bucket,
    index_uuid: &'a str,
    shard: u32,
    seg: SegmentBlob,
    enc: flate2::write::GzEncoder<Vec<u8>>,
    current: Option<transfer::Upload<'a>>,
}

impl<'a> SegmentUpload<'a> {
    fn new(bucket: &'a Bucket, index_uuid: &'a str, shard: u32, key: String) -> SegmentUpload<'a> {
        let enc = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::fast());
        SegmentUpload { bucket, index_uuid, shard, seg: SegmentBlob { key, docs: 0, chunks: Vec::new() }, enc, current: None }
    }

    fn push(&mut self, c: &DocCopy) {
        write_record(&mut self.enc, c);
        self.seg.docs += 1;
    }

    /// Sends what was compressed once it is at least `min` bytes.
    async fn drain(&mut self, min: usize, files: &mut ShardFiles, uploaded: &mut Vec<String>) -> EsResult<()> {
        if self.enc.get_ref().len() < min.max(1) {
            return Ok(());
        }
        let out = std::mem::take(self.enc.get_mut());
        self.feed(&out, files, uploaded).await
    }

    async fn feed(&mut self, mut data: &[u8], files: &mut ShardFiles, uploaded: &mut Vec<String>) -> EsResult<()> {
        let bucket = self.bucket;
        while !data.is_empty() {
            let (index_uuid, shard, seg) = (self.index_uuid, self.shard, &self.seg);
            let up = self.current.get_or_insert_with(|| {
                let key = bucket.chunk(index_uuid, shard, &seg.key, seg.chunks.len());
                transfer::Upload::new(&bucket.store, &bucket.io, bucket.budget.clone(), key, bucket.sizes.buffer)
            });
            let n = (bucket.sizes.chunk - up.size()).min(data.len() as u64) as usize;
            up.write(&data[..n]).await.map_err(|e| bucket.failed_upload(e))?;
            data = &data[n..];
            if up.size() == bucket.sizes.chunk {
                self.close(files, uploaded).await?;
            }
        }
        Ok(())
    }

    /// Completes the object being written.
    async fn close(&mut self, files: &mut ShardFiles, uploaded: &mut Vec<String>) -> EsResult<()> {
        let Some(mut up) = self.current.take() else { return Ok(()) };
        let n = up.finish().await.map_err(|e| self.bucket.failed_upload(e))?;
        uploaded.push(up.key().to_string());
        self.seg.chunks.push(n);
        files.file_count += 1;
        files.size_in_bytes += n;
        files.incremental_file_count += 1;
        files.incremental_size_in_bytes += n;
        Ok(())
    }

    /// Ends the stream and completes its last object.
    async fn finish(mut self, files: &mut ShardFiles, uploaded: &mut Vec<String>) -> EsResult<SegmentBlob> {
        let enc = std::mem::replace(&mut self.enc, flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::fast()));
        let rest = enc.finish().unwrap_or_default();
        if let Err(e) = self.feed(&rest, files, uploaded).await {
            self.abort().await;
            return Err(e);
        }
        self.close(files, uploaded).await?;
        Ok(self.seg)
    }

    /// Abandons the object being written (its multipart upload is aborted).
    async fn abort(&mut self) {
        if let Some(mut up) = self.current.take() {
            up.abort().await;
        }
    }
}

/// One record: a JSON line (id, routing, sequence number, primary term, version and the
/// source's length), the source's bytes as stored, a line feed.
fn write_record(w: &mut impl Write, c: &DocCopy) {
    let head = obj! {
        "id" => c.id.clone(),
        "routing" => c.routing.clone().map(Value::from).unwrap_or(Value::Null),
        "seq_no" => c.seq_no, "primary_term" => c.primary_term, "version" => c.version,
        "len" => c.source.len() as i64,
    };
    // Writing into a Vec (through an encoder) doesn't fail.
    let _ = w.write_all(head.to_json().as_bytes()).and_then(|_| w.write_all(b"\n")).and_then(|_| w.write_all(&c.source)).and_then(|_| w.write_all(b"\n"));
}

/// Records taken out of a segment's gzipped chunks as their bytes arrive ([`SegmentUpload`]'s
/// format; older snapshots' chunks are gzip members of their own, read the same way).
struct Records {
    dec: flate2::write::MultiGzDecoder<Vec<u8>>,
    /// How much of the inflated bytes was taken out already.
    at: usize,
    source_max: usize,
}

impl Records {
    fn new(source_max: usize) -> Records {
        Records { dec: flate2::write::MultiGzDecoder::new(Vec::new()), at: 0, source_max }
    }

    /// Inflates `raw` [`FEED`] bytes at a time, handing each record completed to `each`.
    fn push(&mut self, raw: &[u8], each: &mut impl FnMut(DocCopy) -> EsResult<()>, damaged: &dyn Fn(&str) -> EsError) -> EsResult<()> {
        for piece in raw.chunks(FEED) {
            // Flushed so what was inflated is in the buffer, not held back inside the decoder.
            self.dec.write_all(piece).and_then(|_| self.dec.flush()).map_err(|e| damaged(&e.to_string()))?;
            self.take(each, damaged)?;
        }
        Ok(())
    }

    fn take(&mut self, each: &mut impl FnMut(DocCopy) -> EsResult<()>, damaged: &dyn Fn(&str) -> EsError) -> EsResult<()> {
        loop {
            let next = parse_record(&self.dec.get_ref()[self.at..], self.source_max).map_err(|e| damaged(&e))?;
            let Some((copy, used)) = next else { break };
            self.at += used;
            each(copy)?;
        }
        let v = self.dec.get_mut();
        if self.at == v.len() {
            v.clear();
            self.at = 0;
        } else if self.at >= 1 << 20 {
            v.drain(..self.at);
            self.at = 0;
        }
        Ok(())
    }

    /// The end of the segment: nothing may be left over.
    fn finish(mut self, each: &mut impl FnMut(DocCopy) -> EsResult<()>, damaged: &dyn Fn(&str) -> EsError) -> EsResult<()> {
        self.dec.try_finish().map_err(|e| damaged(&e.to_string()))?;
        self.take(each, damaged)?;
        match self.at == self.dec.get_ref().len() {
            true => Ok(()),
            false => Err(damaged("a record is cut short")),
        }
    }
}

/// Inflates gzipped `raw`, reading no more than `max` bytes of it inflated.
fn inflate(raw: &[u8], max: usize) -> Result<Vec<u8>, String> {
    let mut out = Vec::new();
    flate2::read::GzDecoder::new(raw).take(max as u64 + 1).read_to_end(&mut out).map_err(|e| e.to_string())?;
    if out.len() > max {
        return Err(format!("it inflates past [{max}] bytes"));
    }
    Ok(out)
}

/// At least as many values (keys included) as parsing JSON `text` would build: one more
/// than its separators and opening brackets outside strings.
pub(super) fn json_values(text: &[u8]) -> usize {
    let (mut n, mut in_str, mut escaped) = (1, false, false);
    for &b in text {
        match (in_str, b) {
            (true, _) if escaped => escaped = false,
            (true, b'\\') => escaped = true,
            (true, b'"') => in_str = false,
            (true, _) => {}
            (false, b'"') => in_str = true,
            (false, b',' | b':' | b'[' | b'{') => n += 1,
            (false, _) => {}
        }
    }
    n
}

/// The largest document source a restore indexes: the node's `http.max_content_length`, as
/// no write could have sent a larger one.
pub(super) fn source_max(cluster: &crate::Cluster) -> usize {
    let node = cluster.config.node_setting("http.max_content_length").and_then(|v| crate::units::ByteSize::parse(v, "http.max_content_length").ok());
    node.map_or(crate::api::nodes::MAX_CONTENT_LENGTH, |b| b.0).clamp(0, i32::MAX as i64) as usize
}

/// The record at the start of `buf` ([`SegmentUpload::push`]) and the bytes it takes, or `None`
/// when `buf` doesn't hold all of it yet. A header past [`HEAD_MAX`] or a source longer than
/// `source_max` is refused.
fn parse_record(buf: &[u8], source_max: usize) -> Result<Option<(DocCopy, usize)>, String> {
    let Some(nl) = buf.iter().take(HEAD_MAX + 1).position(|b| *b == b'\n') else {
        return match buf.len() > HEAD_MAX {
            true => Err("a record's header is too long".into()),
            false => Ok(None),
        };
    };
    let v = crate::json::parse(&buf[..=nl]).ok().flatten().ok_or("a record's header isn't JSON")?;
    let int = |k: &str| v.get(k).and_then(Value::as_i64).ok_or_else(|| format!("a record has no [{k}]"));
    let len = int("len")?;
    if !(0..=source_max as i64).contains(&len) {
        return Err(format!("a record's source length [{len}] is out of bounds"));
    }
    let end = nl + 1 + len as usize + 1;
    if buf.len() < end {
        return Ok(None);
    }
    if buf[end - 1] != b'\n' {
        return Err("a record's source is cut short".into());
    }
    let copy = DocCopy {
        id: v.get("id").and_then(Value::as_str).ok_or("a record has no [id]")?.to_string(),
        routing: v.get("routing").and_then(Value::as_str).map(str::to_string),
        source: Bytes::copy_from_slice(&buf[nl + 1..end - 1]),
        seq_no: int("seq_no")?,
        primary_term: int("primary_term")?,
        version: int("version")?,
        ignored_source: Vec::new(),
    };
    Ok(Some((copy, end)))
}

/// Repository `name` when it keeps its contents in a tenant's bucket.
pub(super) fn bucket_repository(st: &ClusterState, name: &str) -> Option<Arc<Repository>> {
    st.metadata.repositories.get(name).filter(|r| r.in_bucket()).cloned()
}

/// Makes the cluster's copy of the bucket's repository data the bucket's (one state write when
/// it changed).
pub(super) async fn store_copy(cluster: &crate::Cluster, r: &Repository, latest: Option<RepositoryData>) -> EsResult<()> {
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
    let bucket = Bucket::open(cluster, name, r).await?;
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
        bucket.io.shard_started();
        let exported = bucket.export_shard(searcher.as_ref(), &vectors, uuid, index_uuid, n, prev.as_ref(), uploaded).await;
        bucket.io.shard_done();
        let mut f = exported?;
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
        Bucket::open(ctx.cluster, repo, r).await?.delete_snapshots(&doomed, &kept).await;
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
            if let Err(e) = bucket.store.put(&key, Bytes::from(blobs.to_json().to_json())).await {
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
    if let Ok(b) = Bucket::open(ctx.cluster, repo, r).await {
        b.delete_keys(keys).await;
    }
}

/// Whether the bucket's newest data records snapshot `uuid` (after a failed commit: it may
/// have been written anyway).
pub(super) async fn recorded(ctx: &Ctx<'_>, repo: &str, r: &Repository, uuid: &str) -> bool {
    match Bucket::open(ctx.cluster, repo, r).await {
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

    fn gzip_member(raw: &[u8]) -> Vec<u8> {
        let mut enc = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::fast());
        enc.write_all(raw).unwrap();
        enc.finish().unwrap()
    }

    #[test]
    fn records_round_trip_and_bounds_hold() {
        let copies = [
            DocCopy { id: "1".into(), routing: None, source: Bytes::from_static(b"{\"a\":1}"), seq_no: 0, primary_term: 1, version: 1, ignored_source: Vec::new() },
            DocCopy { id: "x\ny".into(), routing: Some("r".into()), source: Bytes::from_static(b"{\n \"b\" : \"\\n\"}"), seq_no: 7, primary_term: 2, version: 3, ignored_source: Vec::new() },
        ];
        // Two gzip members (as older snapshots' chunks are), fed in awkward pieces.
        let mut raw = Vec::new();
        for d in &copies {
            let mut one = Vec::new();
            write_record(&mut one, d);
            raw.extend(gzip_member(&one));
        }
        let damaged = |why: &str| EsError::internal(why.to_string());
        let mut back = Vec::new();
        let mut keep = |c| {
            back.push(c);
            Ok(())
        };
        let mut records = Records::new(1 << 20);
        for piece in raw.chunks(7) {
            records.push(piece, &mut keep, &damaged).unwrap();
        }
        records.finish(&mut keep, &damaged).unwrap();
        assert_eq!(back, copies);
        // A length past the bound is refused rather than waited for; a cut record fails at the end.
        let huge = format!("{{\"id\":\"1\",\"seq_no\":0,\"primary_term\":1,\"version\":1,\"len\":{}}}\n", u64::MAX / 2);
        assert!(parse_record(huge.as_bytes(), 1 << 20).is_err());
        assert!(parse_record(&vec![b'x'; HEAD_MAX + 2], 1 << 20).is_err());
        let short = b"{\"id\":\"1\",\"seq_no\":0,\"primary_term\":1,\"version\":1,\"len\":10}\n{}\n";
        assert!(matches!(parse_record(short, 1 << 20), Ok(None)));
        let mut cut = Records::new(1 << 20);
        cut.push(&gzip_member(short), &mut |_| Ok(()), &damaged).unwrap();
        assert!(cut.finish(&mut |_| Ok(()), &damaged).is_err());
        assert!(ShardBlobs::parse(br#"{"segments":[{"key":"../../x","docs":1,"chunks":[1]}]}"#).is_none());
        // Repository data is bounded inflated, and by the values parsing it would build.
        let mut enc = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::fast());
        enc.write_all(&vec![b' '; 4 << 20]).unwrap();
        let bomb = enc.finish().unwrap();
        assert!(inflate(&bomb, 1 << 20).is_err() && inflate(&bomb, 4 << 20).is_ok());
        assert_eq!(json_values(br#"{"a":[1,2,{"b":"x,[\"{:"}]}"#), 8);
    }
}
