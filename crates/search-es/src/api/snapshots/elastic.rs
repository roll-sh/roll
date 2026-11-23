//! Repositories Elasticsearch wrote: an `fs` location, or an `s3`, `gcs` or `azure` bucket
//! path, holding `index.latest` and `index-N` at its base path (roll's own repositories keep
//! theirs under `<base_path>/roll/`). Such a repository is read only, whatever its
//! registration says: roll never writes a blob where Elasticsearch keeps its snapshots.
//!
//! What is read, as observed in what Elasticsearch 8.x and 9.x write (its source isn't read):
//!
//! - `index.latest`: the current generation, a big-endian long; `index-<N>`: the repository
//!   data as JSON (snapshots with their uuid, state, times and versions; indices with their
//!   ids and the snapshots holding them; where each index's metadata blob is).
//! - `snap-<uuid>.dat`, `meta-<uuid>.dat`, `indices/<id>/meta-<blob>.dat`, and per shard
//!   `indices/<id>/<shard>/snap-<uuid>.dat`: Lucene CodecUtil header (codec `snapshot`,
//!   `metadata`, `index-metadata`) and footer (CRC32, always checked) around SMILE, raw
//!   DEFLATE compressed after a `DFL\0` marker when the repository compresses metadata.
//! - A shard snapshot lists the shard's Lucene files: each is a blob `__<uuid>`, in parts
//!   `__<uuid>.part<n>` of `part_size` bytes when larger, or `v__<uuid>` whose bytes are the
//!   `meta_hash` itself (small files: segment infos, segment infos of each segment).
//!
//! Listing (`GET _snapshot`) answers from these as Elasticsearch does from the same data.
//! Restoring is [`super::elastic_restore`]. Everything here is a tenant's bytes: names that
//! become keys or paths are checked, blobs are bounded before and after inflating, SMILE is
//! parsed with a depth bound and charged to the `request` breaker as it is built.

use super::*;
use bytes::Bytes;
use std::collections::HashMap;

/// A metadata blob as stored (compressed), and inflated.
const BLOB_MAX: usize = 64 << 20;
const INFLATED_MAX: usize = 256 << 20;
/// `index-N`.
const DATA_MAX: usize = 256 << 20;
const DATA_SNAPSHOTS_MAX: usize = 10_000;
const DATA_INDICES_MAX: usize = 100_000;
const SMILE_DEPTH: usize = 100;
/// Files one shard snapshot may list.
pub(super) const MAX_FILES: usize = 100_000;
pub(super) const MAX_SHARDS: u32 = 1024;
/// Generations past `index.latest` looked for.
const PROBES: i64 = 16;

/// Where an Elasticsearch repository's blobs are.
pub(super) enum Place {
    /// The `fs` location (under `path.repo`).
    Fs(std::path::PathBuf),
    /// A bucket and the prefix of the repository's base path (`""` or `<base_path>/`).
    Bucket(Box<s3::Bucket>, String),
}

/// An Elasticsearch repository's blobs.
pub(super) struct Source {
    pub(super) repo: String,
    /// The repository's location ([`Repository::location_key`]).
    pub(super) location: String,
    /// What the repository data cache is keyed by: the location and how it's reached (type,
    /// endpoint, client), so two places with one bucket path don't share an entry.
    pub(super) cache_key: String,
    pub(super) place: Place,
    /// `indices.recovery.max_bytes_per_sec`, which data reads pay with the repository's own
    /// `max_restore_bytes_per_sec`.
    pub(super) recovery: Arc<transfer::RateLimiter>,
}

/// Blob names and ids that become keys or paths: `[A-Za-z0-9_-]` (and a `.partN` suffix).
pub(super) fn valid_name(s: &str) -> bool {
    !s.is_empty() && s.len() <= 128 && s.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
}

fn damaged(repo: &str, what: &str, why: impl std::fmt::Display) -> EsError {
    repository_exception(repo, format!("the Elasticsearch repository's [{what}] is damaged: {why}"))
}

impl Source {
    fn key(&self, rel: &str) -> String {
        match &self.place {
            Place::Bucket(_, prefix) => format!("{prefix}{rel}"),
            Place::Fs(_) => rel.to_string(),
        }
    }

    fn fs_path(root: &std::path::Path, rel: &str) -> std::path::PathBuf {
        let mut p = root.to_path_buf();
        for part in rel.split('/') {
            p.push(part);
        }
        p
    }

    /// The blob at `rel`, `None` when there's none; more than `max` bytes is an error.
    pub(super) async fn get(&self, rel: &str, max: usize) -> EsResult<Option<Bytes>> {
        match &self.place {
            Place::Bucket(b, _) => b.store.get(&self.key(rel), max).await.map_err(|e| b.fail("reading the Elasticsearch repository", e)),
            Place::Fs(root) => {
                let path = Self::fs_path(root, rel);
                let repo = self.repo.clone();
                tokio::task::spawn_blocking(move || -> EsResult<Option<Bytes>> {
                    let meta = match std::fs::symlink_metadata(&path) {
                        Ok(m) => m,
                        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
                        Err(e) => return Err(repository_exception(&repo, format!("reading [{}]: {e}", path.display()))),
                    };
                    if !meta.is_file() {
                        return Ok(None);
                    }
                    if meta.len() > max as u64 {
                        return Err(repository_exception(&repo, format!("[{}] is larger than [{max}] bytes", path.display())));
                    }
                    std::fs::read(&path).map(|b| Some(Bytes::from(b))).map_err(|e| repository_exception(&repo, format!("reading [{}]: {e}", path.display())))
                })
                .await
                .map_err(|e| EsError::internal(e.to_string()))?
            }
        }
    }

    /// [`Source::get`], charging the bytes to `held` as they arrive (before they're held).
    pub(super) async fn get_charged(&self, rel: &str, max: usize, held: &mut crate::breaker::Reservation) -> EsResult<Option<Bytes>> {
        match &self.place {
            Place::Bucket(b, _) => {
                let Some(mut body) = b.store.open(&self.key(rel), None).await.map_err(|e| b.fail("reading the Elasticsearch repository", e))? else { return Ok(None) };
                let mut out = Vec::new();
                while let Some(piece) = body.next().await.map_err(|e| b.fail("reading the Elasticsearch repository", e))? {
                    if out.len() + piece.len() > max {
                        return Err(repository_exception(&self.repo, format!("[{rel}] is larger than [{max}] bytes")));
                    }
                    held.add(piece.len() as u64, "<snapshot_metadata>")?;
                    out.extend_from_slice(&piece);
                }
                Ok(Some(Bytes::from(out)))
            }
            Place::Fs(_) => {
                let Some(len) = self.size(rel).await? else { return Ok(None) };
                if len > max as u64 {
                    return Err(repository_exception(&self.repo, format!("[{rel}] is larger than [{max}] bytes")));
                }
                held.add(len, "<snapshot_metadata>")?;
                let got = self.get(rel, max).await?;
                if let Some(b) = &got
                    && b.len() as u64 != len
                {
                    held.add((b.len() as u64).saturating_sub(len), "<snapshot_metadata>")?;
                }
                Ok(got)
            }
        }
    }

    /// The size of blob `rel`, `None` when there's none.
    pub(super) async fn size(&self, rel: &str) -> EsResult<Option<u64>> {
        match &self.place {
            Place::Bucket(b, _) => {
                let key = self.key(rel);
                let listed = b.store.list(&key).await.map_err(|e| b.fail("listing the Elasticsearch repository", e))?;
                Ok(listed.into_iter().find(|(k, _)| *k == key).map(|(_, n)| n))
            }
            Place::Fs(root) => {
                let path = Self::fs_path(root, rel);
                tokio::task::spawn_blocking(move || std::fs::symlink_metadata(&path).ok().filter(|m| m.is_file()).map(|m| m.len()))
                    .await
                    .map_err(|e| EsError::internal(e.to_string()))
            }
        }
    }

    /// Exactly `len` bytes of `rel` from `offset`, paying the restore throttles.
    pub(super) async fn range(&self, rel: &str, offset: u64, len: usize) -> EsResult<Bytes> {
        let short = |got: usize| repository_exception(&self.repo, format!("[{rel}] gave [{got}] bytes at [{offset}], not the [{len}] wanted"));
        match &self.place {
            Place::Bucket(b, _) => {
                let end = offset.checked_add(len as u64).ok_or_else(|| repository_exception(&self.repo, format!("a read of [{rel}] past the end")))?;
                let Some(mut body) = b.store.open(&self.key(rel), Some((offset, end))).await.map_err(|e| b.fail("reading the Elasticsearch repository", e))? else {
                    return Err(repository_exception(&self.repo, format!("[{rel}] is missing from the repository")));
                };
                let mut out = Vec::with_capacity(len);
                while let Some(piece) = body.next().await.map_err(|e| b.fail("reading the Elasticsearch repository", e))? {
                    if out.len() + piece.len() > len {
                        return Err(short(out.len() + piece.len()));
                    }
                    b.io.read(&self.recovery, piece.len() as u64).await;
                    out.extend_from_slice(&piece);
                }
                if out.len() != len {
                    return Err(short(out.len()));
                }
                Ok(Bytes::from(out))
            }
            Place::Fs(root) => {
                use std::io::{Read, Seek};
                let path = Self::fs_path(root, rel);
                let repo = self.repo.clone();
                let out = tokio::task::spawn_blocking(move || -> Result<Vec<u8>, String> {
                    // Regular files only, as `get` reads them: a link could point anywhere.
                    match std::fs::symlink_metadata(&path) {
                        Ok(m) if m.is_file() => {}
                        Ok(_) => return Err(format!("[{}] isn't a regular file", path.display())),
                        Err(e) => return Err(format!("[{}]: {e}", path.display())),
                    }
                    let mut f = std::fs::File::open(&path).map_err(|e| format!("[{}]: {e}", path.display()))?;
                    f.seek(std::io::SeekFrom::Start(offset)).map_err(|e| e.to_string())?;
                    let mut out = Vec::with_capacity(len);
                    f.take(len as u64).read_to_end(&mut out).map_err(|e| e.to_string())?;
                    Ok(out)
                })
                .await
                .map_err(|e| EsError::internal(e.to_string()))?
                .map_err(|e| repository_exception(&repo, e))?;
                if out.len() != len {
                    return Err(short(out.len()));
                }
                self.recovery.pause(len as u64).await;
                Ok(Bytes::from(out))
            }
        }
    }
}

/// Repository `name`'s blobs when Elasticsearch wrote it (its base path holds `index.latest`);
/// `None` for roll's own repositories and empty ones.
pub(super) async fn open(cluster: &crate::Cluster, name: &str, r: &Repository) -> EsResult<Option<Source>> {
    if r.roll_managed() {
        return Ok(None);
    }
    let recovery = cluster.snapshot_data.recovery.clone();
    recovery.set(transfer::recovery_rate(&cluster.state()));
    let place = match r.ty.as_str() {
        "fs" => {
            let paths = cluster_repo_paths(cluster);
            let Some(loc) = resolve_location(&paths, r.settings.get("location").unwrap_or_default()) else { return Ok(None) };
            Place::Fs(std::path::PathBuf::from(loc))
        }
        _ if r.in_bucket() => {
            let bucket = s3::Bucket::open(cluster, name, r).await?;
            let base = r.settings.get("base_path").unwrap_or_default().trim_matches('/');
            Place::Bucket(Box::new(bucket), if base.is_empty() { String::new() } else { format!("{base}/") })
        }
        _ => return Ok(None),
    };
    let reach = |k: &str| r.settings.get(k).unwrap_or_default().to_string();
    let cache_key = match &place {
        Place::Fs(root) => format!("fs|{}", root.display()),
        Place::Bucket(..) => format!("{}|{}|{}|{}|{}", r.location_key(), reach("endpoint"), reach("client"), reach("account"), reach("protocol")),
    };
    let src = Source { repo: name.to_string(), location: r.location_key(), cache_key, place, recovery };
    match src.get("index.latest", 64).await? {
        Some(b) if b.len() == 8 => {}
        Some(_) => return Err(damaged(name, "index.latest", "it isn't an 8-byte generation")),
        None => {
            // Elasticsearch's repository is gone (the location wiped for roll): its copy goes too,
            // or it would list snapshots that aren't there and keep roll from writing.
            forget_copy(cluster, &src.location).await?;
            return Ok(None);
        }
    }
    // A location roll keeps snapshots at stays roll's: an `index.latest` someone put there
    // mustn't hide them (or stop a bucket's `roll/` from being read).
    if cluster.state().metadata.repository_data.get(&src.location).is_some_and(|d| !d.elastic && !d.snapshots.is_empty()) {
        return Ok(None);
    }
    if matches!(src.place, Place::Bucket(..)) && src.get("roll/index.latest", 64).await?.is_some() {
        return Ok(None);
    }
    Ok(Some(src))
}

/// One snapshot in `index-N`.
#[derive(Clone, Debug)]
pub(super) struct EsSnapshot {
    pub(super) name: String,
    pub(super) uuid: String,
    pub(super) state: i64,
    pub(super) start_ms: i64,
    pub(super) end_ms: i64,
    /// Index id → metadata identifier.
    pub(super) index_metadata: HashMap<String, String>,
}

/// One index in `index-N`.
#[derive(Clone, Debug)]
pub(super) struct EsIndex {
    pub(super) id: String,
}

/// `index-N`.
pub(super) struct EsData {
    pub(super) generation: i64,
    pub(super) uuid: String,
    pub(super) snapshots: Vec<EsSnapshot>,
    pub(super) indices: Vec<(String, EsIndex)>,
    /// Metadata identifier → metadata blob uuid.
    pub(super) identifiers: HashMap<String, String>,
}

impl EsData {
    /// About what the parsed data takes in memory.
    fn estimate(&self) -> u64 {
        let snapshots: usize = self.snapshots.iter().map(|s| 128 + s.name.len() + s.uuid.len() + s.index_metadata.iter().map(|(k, v)| 64 + k.len() + v.len()).sum::<usize>()).sum();
        let indices: usize = self.indices.iter().map(|(n, i)| 64 + n.len() + i.id.len()).sum();
        let identifiers: usize = self.identifiers.iter().map(|(k, v)| 64 + k.len() + v.len()).sum();
        (256 + snapshots + indices + identifiers) as u64
    }

    pub(super) fn snapshot(&self, name: &str) -> Option<&EsSnapshot> {
        self.snapshots.iter().find(|s| s.name == name)
    }

    pub(super) fn index(&self, name: &str) -> Option<&EsIndex> {
        self.indices.iter().find(|(n, _)| n == name).map(|(_, i)| i)
    }

    /// Where index `id`'s metadata as of snapshot `s` is.
    pub(super) fn index_metadata_key(&self, s: &EsSnapshot, id: &str) -> Option<String> {
        if !valid_name(id) {
            return None;
        }
        let blob = match s.index_metadata.get(id) {
            Some(identifier) => self.identifiers.get(identifier)?.clone(),
            // Repositories older than Elasticsearch 7.9 name it by the snapshot.
            None => s.uuid.clone(),
        };
        valid_name(&blob).then(|| format!("indices/{id}/meta-{blob}.dat"))
    }
}

/// The repository data parsed last, per repository place ([`Source::cache_key`]).
pub(super) struct CachedData {
    repo: String,
    generation: i64,
    /// `index-N`'s length: Elasticsearch numbers generations from 0 again after a wipe, so the
    /// generation alone doesn't name the data.
    len: u64,
    data: Arc<EsData>,
    /// What the parsed data holds, on the `request` breaker for as long as it's kept.
    _held: crate::breaker::Reservation,
    used: u64,
}

impl CachedData {
    pub(super) fn repo(&self) -> &str {
        &self.repo
    }
}

/// Repository data kept at most (one per repository place, least recently used dropped).
const CACHED: usize = 4;

/// Forgets what is kept of repository `repo`'s data (it was changed or unregistered).
pub(super) fn forget(cluster: &crate::Cluster, repo: &str) {
    cluster.snapshot_data.elastic_data.lock().retain(|_, c| c.repo != repo);
}

/// How much of `index-N` is read to find its uuid (`min_version` and `uuid` come first).
const HEAD_LEN: usize = 256;

/// The repository uuid at the start of an `index-N` Elasticsearch wrote, as its JSON starts
/// (`{"min_version":"7.12.0","uuid":"…"`); `None` when it isn't there.
fn head_uuid(head: &[u8]) -> Option<&str> {
    // The head may end inside a character.
    let text = match std::str::from_utf8(head) {
        Ok(t) => t,
        Err(e) => std::str::from_utf8(&head[..e.valid_up_to()]).ok()?,
    };
    let rest = &text[text.find("\"uuid\":\"")? + 8..];
    Some(&rest[..rest.find('"')?])
}

/// Reads `index.latest` and the newest `index-N`: the copy parsed last when the generation, the
/// blob's length and the repository uuid it starts with haven't changed.
pub(super) async fn read_data(cluster: &crate::Cluster, src: &Source) -> EsResult<Arc<EsData>> {
    let latest = src.get("index.latest", 64).await?.ok_or_else(|| repository_exception(&src.repo, "the Elasticsearch repository's [index.latest] is gone"))?;
    let mut generation = i64::from_be_bytes(latest[..8].try_into().map_err(|_| damaged(&src.repo, "index.latest", "short"))?);
    if !(0..i64::MAX - PROBES).contains(&generation) {
        return Err(damaged(&src.repo, "index.latest", format!("generation [{generation}]")));
    }
    let budget = cluster.breakers.request(cluster, &cluster.state());
    let mut held = crate::breaker::Reservation::new(budget.clone());
    // A newer generation a crash left unnamed wins.
    let mut raw: Option<Bytes> = None;
    for _ in 0..PROBES {
        let mut probe = crate::breaker::Reservation::new(budget.clone());
        match src.get_charged(&format!("index-{}", generation + 1), DATA_MAX, &mut probe).await? {
            Some(b) => {
                generation += 1;
                held = probe;
                raw = Some(b);
            }
            None => break,
        }
    }
    let what = format!("index-{generation}");
    let len = match &raw {
        Some(r) => Some(r.len() as u64),
        None => src.size(&what).await?,
    };
    // Generation and length alone don't tell a wiped and rewritten repository's `index-N` from
    // the one cached: its uuid, at the start, does.
    let candidate = {
        let cache = cluster.snapshot_data.elastic_data.lock();
        cache.get(&src.cache_key).filter(|c| c.generation == generation && Some(c.len) == len && !c.data.uuid.is_empty() && c.data.uuid != "_na_").map(|c| c.data.clone())
    };
    if let (Some(d), Some(len)) = (candidate, len) {
        // A file rewritten since its size was read is read whole below.
        let head = match &raw {
            Some(r) => Some(r.slice(..r.len().min(HEAD_LEN))),
            None => src.range(&what, 0, (len as usize).min(HEAD_LEN)).await.ok(),
        };
        if head.as_deref().and_then(head_uuid) == Some(d.uuid.as_str()) {
            let mut cache = cluster.snapshot_data.elastic_data.lock();
            let tick = cache.values().map(|c| c.used).max().unwrap_or(0) + 1;
            if let Some(c) = cache.get_mut(&src.cache_key) {
                c.used = tick;
            }
            return Ok(d);
        }
    }
    let raw = match raw {
        Some(r) => r,
        None => src.get_charged(&what, DATA_MAX, &mut held).await?.ok_or_else(|| repository_exception(&src.repo, format!("the Elasticsearch repository's [{what}] is missing")))?,
    };
    let len = raw.len() as u64;
    let text = if raw.starts_with(b"DFL\0") {
        inflate_charged(&raw[4..], DATA_MAX, &mut held).map_err(|e| match e {
            BlobError::Breaker(e) => e,
            BlobError::Damaged(why) => damaged(&src.repo, &what, why),
        })?
    } else {
        raw.to_vec()
    };
    drop(raw);
    let values = s3::json_values(&text);
    held.add((values * std::mem::size_of::<Value>() * 2 + text.len()) as u64, "<repository_data>")?;
    let v = crate::json::parse(&text).ok().flatten().ok_or_else(|| damaged(&src.repo, &what, "it isn't JSON"))?;
    drop(text);
    let data = Arc::new(parse_data(&v, generation).map_err(|e| damaged(&src.repo, &what, e))?);
    drop(v);
    drop(held);
    // What is kept is charged for as long as it's kept.
    let mut kept = crate::breaker::Reservation::new(budget);
    kept.add(data.estimate(), "<repository_data>")?;
    let mut cache = cluster.snapshot_data.elastic_data.lock();
    let tick = cache.values().map(|c| c.used).max().unwrap_or(0) + 1;
    cache.insert(src.cache_key.clone(), CachedData { repo: src.repo.clone(), generation, len, data: data.clone(), _held: kept, used: tick });
    while cache.len() > CACHED {
        let Some(oldest) = cache.iter().min_by_key(|(_, c)| c.used).map(|(k, _)| k.clone()) else { break };
        cache.remove(&oldest);
    }
    Ok(data)
}

fn parse_data(v: &Value, generation: i64) -> Result<EsData, String> {
    let s = |v: &Value, k: &str| v.get(k).and_then(Value::as_str).map(str::to_string);
    let mut snapshots = Vec::new();
    for snap in v.get("snapshots").and_then(Value::as_array).into_iter().flatten() {
        let (Some(name), Some(uuid)) = (s(snap, "name"), s(snap, "uuid")) else { return Err("a snapshot without a name or uuid".into()) };
        if !valid_name(&uuid) {
            return Err(format!("invalid snapshot uuid [{}]", uuid.escape_debug()));
        }
        let index_metadata = snap
            .get("index_metadata_lookup")
            .and_then(Value::as_object)
            .map(|m| m.iter().filter_map(|(k, v)| Some((k.clone(), v.as_str()?.to_string()))).collect())
            .unwrap_or_default();
        snapshots.push(EsSnapshot {
            name,
            uuid,
            state: snap.get("state").and_then(Value::as_i64).unwrap_or(1),
            start_ms: snap.get("start_time_millis").and_then(Value::as_i64).unwrap_or(0),
            end_ms: snap.get("end_time_millis").and_then(Value::as_i64).unwrap_or(0),
            index_metadata,
        });
    }
    if snapshots.len() > DATA_SNAPSHOTS_MAX {
        return Err(format!("it holds more than [{DATA_SNAPSHOTS_MAX}] snapshots"));
    }
    let mut indices = Vec::new();
    for (name, i) in v.get("indices").and_then(Value::as_object).into_iter().flatten() {
        let Some(id) = s(i, "id").filter(|id| valid_name(id)) else { return Err(format!("index [{name}] has no valid id")) };
        indices.push((name.clone(), EsIndex { id }));
    }
    if indices.len() > DATA_INDICES_MAX {
        return Err(format!("it holds more than [{DATA_INDICES_MAX}] indices"));
    }
    let identifiers = v.get("index_metadata_identifiers").and_then(Value::as_object).map(|m| m.iter().filter_map(|(k, v)| Some((k.clone(), v.as_str()?.to_string()))).collect()).unwrap_or_default();
    Ok(EsData { generation, uuid: s(v, "uuid").unwrap_or_default(), snapshots, indices, identifiers })
}

/// Raw DEFLATE, at most `max` bytes out (more is an error, not a bigger buffer), charging what
/// it inflates to `held` as it grows.
pub(super) fn inflate_charged(raw: &[u8], max: usize, held: &mut crate::breaker::Reservation) -> Result<Vec<u8>, BlobError> {
    use std::io::Read;
    let mut dec = flate2::read::DeflateDecoder::new(raw);
    let mut out = Vec::new();
    let mut buf = vec![0u8; 1 << 20];
    loop {
        let n = dec.read(&mut buf).map_err(|e| BlobError::Damaged(format!("inflating: {e}")))?;
        if n == 0 {
            return Ok(out);
        }
        if out.len() + n > max {
            return Err(BlobError::Damaged(format!("it inflates to more than [{max}] bytes")));
        }
        held.add(n as u64, "<snapshot_metadata>").map_err(BlobError::Breaker)?;
        out.extend_from_slice(&buf[..n]);
    }
}

/// A metadata blob's content: CodecUtil header (codec `codec`) and footer checked, inflated
/// when compressed, parsed (SMILE or JSON), charged to the `request` breaker as it is built.
/// Binary values come back raw through `binary`.
pub(super) async fn read_blob(cluster: &crate::Cluster, src: &Source, rel: &str, codec: &str, binary: &mut (dyn FnMut(&[u8]) -> Value + Send)) -> EsResult<Option<(Value, crate::breaker::Reservation)>> {
    let budget = cluster.breakers.request(cluster, &cluster.state());
    let mut held = crate::breaker::Reservation::new(budget);
    let Some(raw) = src.get_charged(rel, BLOB_MAX, &mut held).await? else { return Ok(None) };
    parse_blob(&raw, codec, held, binary).map(Some).map_err(|e| match e {
        BlobError::Breaker(e) => e,
        BlobError::Damaged(why) => damaged(&src.repo, rel, why),
    })
}

pub(super) enum BlobError {
    Breaker(EsError),
    Damaged(String),
}

/// [`read_blob`] on bytes already read and charged to `held`, which the result keeps (with what
/// the parsed value takes; the bytes as read and as inflated are released once it's parsed).
pub(super) fn parse_blob(raw: &[u8], codec: &str, mut held: crate::breaker::Reservation, binary: &mut dyn FnMut(&[u8]) -> Value) -> Result<(Value, crate::breaker::Reservation), BlobError> {
    let d = |e: String| BlobError::Damaged(e);
    search_lucene::codec::check_footer(raw).map_err(|e| d(e.to_string()))?;
    let body = search_lucene::codec::body(raw).map_err(|e| d(e.to_string()))?;
    let mut inp = search_lucene::input::Input::new(body);
    search_lucene::codec::check_header(&mut inp, codec, 1, 1).map_err(|e| d(e.to_string()))?;
    let content = &body[inp.pos()..];
    let inflated;
    let content = if let Some(rest) = content.strip_prefix(b"DFL\0") {
        inflated = inflate_charged(rest, INFLATED_MAX, &mut held)?;
        &inflated[..]
    } else {
        inflated = Vec::new();
        content
    };
    let transient = raw.len() + inflated.len();
    let mut tripped = None;
    let v = if content.first() == Some(&b'{') {
        held.add((s3::json_values(content) * std::mem::size_of::<Value>() * 2 + content.len()) as u64, "<snapshot_metadata>").map_err(BlobError::Breaker)?;
        crate::json::parse(content).ok().flatten().ok_or_else(|| d("it isn't JSON".into()))?
    } else {
        let mut charge = |n: usize| -> Result<(), String> {
            held.add(n as u64, "<snapshot_metadata>").map_err(|e| {
                tripped = Some(e);
                "breaker".to_string()
            })
        };
        let parsed = crate::xcontent::smile::parse_with_binary(content, crate::xcontent::smile::Limits { depth: SMILE_DEPTH, charge: &mut charge }, binary);
        match (parsed, tripped.take()) {
            (_, Some(e)) => return Err(BlobError::Breaker(e)),
            (Ok(v), None) => v,
            (Err(e), None) => return Err(d(e)),
        }
    };
    drop(inflated);
    held.release(transient as u64);
    Ok((v, held))
}

/// Binary values as base64 (what `GET` would show).
pub(super) fn base64_binary(raw: &[u8]) -> Value {
    use base64::Engine as _;
    Value::Str(base64::engine::general_purpose::STANDARD.encode(raw))
}

/// A snapshot as roll lists it, from Elasticsearch's `SnapshotInfo` (`snap-<uuid>.dat`).
fn snapshot_record(s: &EsSnapshot, info: &Value) -> Snapshot {
    let info = info.get("snapshot").unwrap_or(info);
    let strings = |k: &str| info.get(k).and_then(Value::as_array).map(|a| a.iter().filter_map(|x| x.as_str().map(str::to_string)).collect()).unwrap_or_default();
    let state = info.get("state").and_then(Value::as_str).map(str::to_string).unwrap_or_else(|| {
        match s.state {
            0 => "IN_PROGRESS",
            2 => "FAILED",
            3 => "PARTIAL",
            4 => "INCOMPATIBLE",
            _ => "SUCCESS",
        }
        .to_string()
    });
    Snapshot {
        name: s.name.clone(),
        uuid: s.uuid.clone(),
        indices: strings("indices"),
        data_streams: strings("data_streams"),
        include_global_state: info.get("include_global_state").and_then(Value::as_bool).unwrap_or(false),
        metadata: info.get("metadata").and_then(Value::as_object).cloned(),
        state,
        start_ms: info.get("start_time").and_then(Value::as_i64).unwrap_or(s.start_ms),
        end_ms: info.get("end_time").and_then(Value::as_i64).unwrap_or(s.end_ms),
        total_shards: info.get("total_shards").and_then(Value::as_i64).unwrap_or(0),
        successful_shards: info.get("successful_shards").and_then(Value::as_i64).unwrap_or(0),
        feature_states: info.get("feature_states").and_then(Value::as_array).cloned().unwrap_or_default(),
        index_meta: Default::default(),
        data_stream_meta: Default::default(),
        global: None,
        shard_files: Default::default(),
    }
}

/// When repository `name` is one Elasticsearch wrote: reads its data into the cluster's copy
/// (marked [`RepositoryData::elastic`], so nothing writes to it), reading the `SnapshotInfo` of
/// snapshots the copy doesn't have yet, and answers its blobs and data. `None` otherwise.
pub(super) async fn sync(cluster: &crate::Cluster, name: &str, r: &Repository) -> EsResult<Option<(Source, Arc<EsData>)>> {
    let Some(src) = open(cluster, name, r).await? else { return Ok(None) };
    let data = read_data(cluster, &src).await?;
    let key = r.location_key();
    let known: HashMap<String, Arc<Snapshot>> = cluster.state().metadata.repository_data.get(&key).filter(|d| d.elastic).map(|d| d.snapshots.iter().map(|s| (s.uuid.clone(), s.clone())).collect()).unwrap_or_default();
    let mut snapshots = Vec::with_capacity(data.snapshots.len());
    for s in &data.snapshots {
        match known.get(&s.uuid) {
            Some(k) if k.state != "IN_PROGRESS" => snapshots.push(k.clone()),
            _ => {
                let info = read_blob(cluster, &src, &format!("snap-{}.dat", s.uuid), "snapshot", &mut base64_binary).await?;
                let record = match &info {
                    Some((v, _)) => snapshot_record(s, v),
                    None => snapshot_record(s, &Value::Null),
                };
                snapshots.push(Arc::new(record));
            }
        }
    }
    let copy = RepositoryData { uuid: data.uuid.clone(), generation: data.generation, snapshots, elastic: true };
    if !store_copy(cluster, key, copy).await? {
        return Ok(None);
    }
    Ok(Some((src, data)))
}

/// Drops the cluster's copy of location `key` when it is Elasticsearch's (checked again inside
/// the update).
async fn forget_copy(cluster: &crate::Cluster, key: &str) -> EsResult<()> {
    let elastic = |st: &ClusterState, key: &str| st.metadata.repository_data.get(key).is_some_and(|d| d.elastic);
    if !elastic(&cluster.state(), key) {
        return Ok(());
    }
    let key = key.to_string();
    cluster
        .update(move |st| {
            if elastic(st, &key) {
                st.metadata.repository_data.remove(&key);
            }
            Ok(())
        })
        .await
}

/// Makes `copy` the cluster's copy of location `key`'s contents, unless roll keeps snapshots
/// there: decided in the same state update, so a roll snapshot recorded while the Elasticsearch
/// repository was being read (it took the check in [`open`] before) is never replaced. Whether
/// the copy is Elasticsearch's.
async fn store_copy(cluster: &crate::Cluster, key: String, copy: RepositoryData) -> EsResult<bool> {
    let st = cluster.state();
    if roll_owned(&st, &key) {
        return Ok(false);
    }
    if st.metadata.repository_data.get(&key).is_some_and(|d| d.elastic && d.uuid == copy.uuid && d.generation == copy.generation && d.snapshots.len() == copy.snapshots.len()) {
        return Ok(true);
    }
    drop(st);
    cluster.update(move |st| Ok(apply_copy(st, key, copy))).await
}

/// Whether roll's own snapshots are recorded at `key`.
fn roll_owned(st: &ClusterState, key: &str) -> bool {
    st.metadata.repository_data.get(key).is_some_and(|d| !d.elastic && !d.snapshots.is_empty())
}

/// The copy's insert, checked again inside the update: a roll snapshot recorded since
/// [`store_copy`] looked keeps its place, and the copy is dropped.
fn apply_copy(st: &mut ClusterState, key: String, copy: RepositoryData) -> bool {
    if roll_owned(st, &key) {
        return false;
    }
    st.metadata.repository_data.insert(key, Arc::new(copy));
    true
}

/// `_id` as Elasticsearch keeps it in Lucene, back to the id it was given (as observed:
/// `0xfe` then decimal digits two to a byte, `0xf` padding an odd count; `0xff` then UTF-8;
/// `0xfd` then, or otherwise just, the bytes of an id that was URL-safe base64 without
/// padding, which it is again).
pub fn decode_id(b: &[u8]) -> Option<String> {
    use base64::Engine as _;
    let b64 = |raw: &[u8]| base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(raw);
    match b.first()? {
        0xFE => {
            let mut s = String::with_capacity((b.len() - 1) * 2);
            for (i, &x) in b[1..].iter().enumerate() {
                let (hi, lo) = (x >> 4, x & 0xF);
                if hi > 9 {
                    return None;
                }
                s.push((b'0' + hi) as char);
                match lo {
                    0..=9 => s.push((b'0' + lo) as char),
                    0xF if i + 2 == b.len() => {}
                    _ => return None,
                }
            }
            (!s.is_empty()).then_some(s)
        }
        0xFF => std::str::from_utf8(&b[1..]).ok().filter(|s| !s.is_empty()).map(str::to_string),
        0xFD => (b.len() > 1).then(|| b64(&b[1..])),
        _ => Some(b64(b)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The race of a roll snapshot recorded between [`store_copy`]'s check and the copy's insert:
    /// the insert checks again, so roll's list stays.
    #[tokio::test]
    async fn a_copy_never_replaces_rolls_snapshots() {
        let c = crate::Cluster::in_memory("es-race").await;
        let roll = RepositoryData { uuid: "roll".into(), generation: 1, snapshots: vec![Arc::new(snapshot_record(&EsSnapshot { name: "mine".into(), uuid: "u".into(), state: 1, start_ms: 0, end_ms: 0, index_metadata: HashMap::new() }, &Value::Null))], elastic: false };
        let es = RepositoryData { uuid: "es".into(), generation: 3, snapshots: Vec::new(), elastic: true };
        // The update runs on a state that gained roll's snapshot after the first look.
        let mut st = (*c.state()).clone();
        assert!(!roll_owned(&st, "fs:/x"));
        st.metadata.repository_data.insert("fs:/x".into(), Arc::new(roll));
        assert!(!apply_copy(&mut st, "fs:/x".into(), es.clone()));
        let kept = &st.metadata.repository_data["fs:/x"];
        assert!(!kept.elastic && kept.snapshots[0].name == "mine");
        // With nothing of roll's there, the copy goes in.
        assert!(store_copy(&c, "fs:/x".into(), es).await.unwrap());
        assert!(c.state().metadata.repository_data["fs:/x"].elastic);
    }

    #[test]
    fn the_uuid_comes_from_index_ns_start() {
        let head = br#"{"min_version":"7.12.0","uuid":"KT2vatFNQMKN8UPw_S7RJQ","cluster_id":"ZIvC"#;
        assert_eq!(head_uuid(head), Some("KT2vatFNQMKN8UPw_S7RJQ"));
        // Cut inside a character after the uuid, or before its end.
        let mut cut = head.to_vec();
        cut.extend_from_slice(&"é".as_bytes()[..1]);
        assert_eq!(head_uuid(&cut), Some("KT2vatFNQMKN8UPw_S7RJQ"));
        assert_eq!(head_uuid(&head[..40]), None);
        assert_eq!(head_uuid(b"DFL\0\x01\x02"), None);
    }

    #[test]
    fn ids_decode_as_elasticsearch_wrote_them() {
        // Bytes read from a 9.5.5 shard's stored `_id`s.
        let cases: &[(&[u8], &str)] = &[
            (&[0xfe, 0x10, 0x00], "1000"),
            (&[0xfe, 0x0f], "0"),
            (&[0xfe, 0x04], "04"),
            (&[0xff, b'-', b'5'], "-5"),
            (&[0x69, 0xb7, 0x1d], "abcd"),
            (&[0x69, 0xb7], "abc"),
            (&[0xff, b'a', b'b'], "ab"),
            (&[0xfd, 0xfe, 0xff, 0x00], "_v8A"),
            (&[0xfd, 0xff, 0xff, 0x00], "__8A"),
            (&[0x08, 0x00, 0x00], "CAAA"),
            (&[0x00], "AA"),
            (&[0xff, 0xc3, 0xaf, b'd'], "ïd"),
            (&[0x0e, 0xa7, 0xa0, 0xfa, 0xa1, 0x01, 0xb5, 0x33, 0xfa, 0xb0, 0xe8, 0x2c, 0xac, 0x02, 0x2d], "Dqeg-qEBtTP6sOgsrAIt"),
        ];
        for (raw, id) in cases {
            assert_eq!(decode_id(raw).as_deref(), Some(*id), "{raw:x?}");
        }
        assert_eq!(decode_id(&[]), None);
        assert_eq!(decode_id(&[0xfe, 0xa1]), None);
        assert_eq!(decode_id(&[0xfe, 0x1f, 0x22]), None);
        assert_eq!(decode_id(&[0xff, 0xc3]), None);
    }
}
