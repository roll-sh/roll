//! `POST /_snapshot/{repository}/_analyze` (`RepositoryAnalyzeAction`, Elasticsearch's
//! snapshot-repo-test-kit): writes, reads, overwrites, lists and deletes test blobs to check
//! that a repository's storage behaves as snapshots need.
//!
//! An `s3`, `gcs` or `azure` repository's bucket is really exercised, under `<base_path>/roll/temp-analysis-<id>/`
//! ([`analyze_bucket`]): a contended register (compare-and-exchange by `If-Match`, every
//! increment must land; a GCS generation with `ifGenerationMatch`), an uncontended one,
//! overwrite protection (of concurrent writers with `If-None-Match: *` exactly one wins), then
//! each blob is written (in parts past `buffer_size` or a GCS chunk or Azure block, as snapshot
//! contents are), sometimes read before its write, overwritten, or
//! written and aborted, and read back (whole or a range) byte for byte; the blobs must then all
//! be listed with their sizes, and gone once deleted. The first check that fails fails the
//! analysis with Elasticsearch's answer, after removing what it wrote. Writes and reads pay the
//! repository's throttles, so `timeout` bounds what an analysis can move as well as how long
//! it runs; it writes no more than `max_total_data_size` (each blob once, a rare overwrite
//! again), holds at most one part per blob in flight (charged to the `request` breaker), and
//! runs at most [`CONCURRENCY_MAX`] blobs at once.
//!
//! roll's other repositories keep snapshot contents in the cluster's own storage (the
//! repository is a namespace), so there is no third-party store whose consistency could be in
//! doubt: the analysis validates the request as Elasticsearch does, plans the same blobs and
//! reports a clean result in Elasticsearch's shape. The repository's throttles still bound it:
//! an analysis whose writes or reads couldn't finish within `timeout` times out as
//! Elasticsearch's does. `human` defaults to true, as `RestRepositoryAnalyzeAction` sets it.

use super::*;
use crate::units::{ByteSize, TimeValue};
use bytes::Bytes;
use std::time::{Duration, Instant};

/// Blobs analysed at once, whatever `concurrency` asks for.
const CONCURRENCY_MAX: usize = 16;
/// Attempts of one register increment before contention fails it.
const REGISTER_ATTEMPTS: usize = 1000;
/// Elasticsearch's docs on repository analysis and on S3-compatible storage, as its failures
/// cite them.
const ANALYZE_DOCS: &str = "https://www.elastic.co/docs/api/doc/elasticsearch/v9/operation/operation-snapshot-repository-analyze?version=9.5";
const S3_COMPATIBLE_DOCS: &str = "https://www.elastic.co/docs/deploy-manage/tools/snapshot-and-restore/s3-repository?version=9.5#repository-s3-compatible-services";

/// The request (`RepositoryAnalyzeAction.Request`), validated.
struct Params {
    blob_count: i64,
    concurrency: i64,
    register_ops: i64,
    read_node_count: i64,
    early_read_node_count: i64,
    max_blob: ByteSize,
    max_total: ByteSize,
    rare: f64,
    detailed: bool,
    abort_writes: bool,
    timeout: TimeValue,
    human: bool,
    seed: i64,
}

fn params(ctx: &Ctx) -> EsResult<Params> {
    let size = |p: &str, d: i64| -> EsResult<ByteSize> { ctx.query(p).map_or(Ok(ByteSize(d)), |v| ByteSize::parse(v, p)) };
    let p = Params {
        blob_count: ctx.int_param("blob_count", 100)?,
        concurrency: ctx.int_param("concurrency", 10)?,
        register_ops: ctx.int_param("register_operation_count", 10)?,
        read_node_count: ctx.int_param("read_node_count", 10)?,
        early_read_node_count: ctx.int_param("early_read_node_count", 2)?,
        max_blob: size("max_blob_size", 10 << 20)?,
        max_total: size("max_total_data_size", 1 << 30)?,
        rare: match ctx.query("rare_action_probability") {
            Some(v) => v.parse().map_err(|_| EsError::new(400, "number_format_exception", format!("For input string: \"{v}\"")))?,
            None => 0.02,
        },
        detailed: ctx.bool_param("detailed", false)?,
        abort_writes: ctx.bool_param("rarely_abort_writes", true)?,
        timeout: ctx.time_param("timeout", TimeValue::seconds(30))?,
        human: ctx.bool_param("human", true)?,
        seed: match ctx.query("seed") {
            Some(v) => crate::units::parse_java_long(v).ok_or_else(|| EsError::illegal_argument(format!("Failed to parse long parameter [seed] with value [{v}]")))?,
            None => i64::from(rand_seed()),
        },
    };
    // `RepositoryAnalyzeAction.Request.validate` order.
    let positive = |what: &str, v: i64| if v <= 0 { Err(EsError::illegal_argument(format!("{what} must be >0, but was [{v}]"))) } else { Ok(()) };
    positive("blobCount", p.blob_count)?;
    if p.blob_count > 100_000 {
        return Err(EsError::illegal_argument(format!("blobCount must be <= 100000, but was [{}]", p.blob_count)));
    }
    positive("concurrency", p.concurrency)?;
    positive("registerOperationCount", p.register_ops)?;
    if p.max_blob.bytes() <= 0 {
        return Err(EsError::illegal_argument(format!("maxBlobSize must be >0, but was [{}]", p.max_blob.human())));
    }
    if p.max_total.bytes() <= 0 {
        return Err(EsError::illegal_argument(format!("maxTotalDataSize must be >0, but was [{}]", p.max_total.human())));
    }
    positive("readNodeCount", p.read_node_count)?;
    if p.early_read_node_count < 0 {
        return Err(EsError::illegal_argument(format!("earlyReadNodeCount must be >=0, but was [{}]", p.early_read_node_count)));
    }
    if !(0.0..=1.0).contains(&p.rare) {
        return Err(EsError::illegal_argument(format!("rareActionProbability must be between 0 and 1, but was [{:?}]", p.rare)));
    }
    let min_total = p.max_blob.bytes() + p.blob_count - 1;
    if p.max_total.bytes() < min_total {
        return Err(EsError::illegal_argument(format!(
            "cannot satisfy max total bytes [{}B/{}]: must write at least one byte per blob and at least one max-sized blob which is [{min_total}B] in total",
            p.max_total.bytes(),
            p.max_total.human()
        )));
    }
    Ok(p)
}

/// `RepositoryAnalyzeAction.getBlobSizes`: powers of two up to `max_blob`, then `max_blob`,
/// repeated evenly while the count and the total allow, the rest spread by
/// `BlobCountCalculator` (which splits the largest sizes until every blob is placed), one
/// max-sized blob always among them. The caller checked that the total admits that.
pub(super) fn blob_sizes(blob_count: i64, max_total: i64, max_blob: i64) -> Vec<i64> {
    let mut sizes = Vec::new();
    let mut s = 1i64;
    while 0 < s && s < max_blob {
        sizes.push(s);
        s = s.wrapping_shl(1);
    }
    sizes.push(max_blob);
    let spread: i64 = sizes.iter().sum();
    let (mut count, mut total, mut repeats) = (blob_count, max_total, 0i64);
    while sizes.len() as i64 <= count && count - sizes.len() as i64 <= total - spread {
        repeats += 1;
        total -= spread;
        count -= sizes.len() as i64;
    }
    if repeats == 0 {
        count -= 1;
        total -= max_blob;
    }
    let mut out = BlobCount::new(count, total, &sizes).calculate();
    match repeats {
        0 => out.push(max_blob),
        r => {
            for &s in &sizes {
                out.extend(std::iter::repeat_n(s, r as usize));
            }
        }
    }
    out
}

/// `RepositoryAnalyzeAction.BlobCountCalculator`.
struct BlobCount<'a> {
    blob_count: i64,
    max_total: i64,
    sizes: &'a [i64],
    by_size: Vec<i64>,
    total_bytes: i64,
    total_blobs: i64,
}

impl<'a> BlobCount<'a> {
    fn new(blob_count: i64, max_total: i64, sizes: &'a [i64]) -> BlobCount<'a> {
        BlobCount { blob_count, max_total, sizes, by_size: vec![0; sizes.len()], total_bytes: 0, total_blobs: 0 }
    }

    fn calculate(mut self) -> Vec<i64> {
        self.add_roughly_evenly(self.sizes.len() as i64 - 1);
        while self.total_blobs < self.blob_count {
            let min_split = if self.by_size.iter().skip(1).all(|c| *c <= 1) { 1 } else { 2 };
            let Some(split) = (1..self.sizes.len()).rfind(|i| self.by_size[*i] >= min_split) else { break };
            self.by_size[split] -= 1;
            self.total_bytes -= self.sizes[split];
            self.total_blobs -= 1;
            self.add_roughly_evenly(split as i64 - 1);
        }
        self.sizes.iter().zip(&self.by_size).flat_map(|(s, n)| std::iter::repeat_n(*s, *n as usize)).collect()
    }

    fn add_roughly_evenly(&mut self, start: i64) {
        while self.total_blobs < self.blob_count && self.total_bytes < self.max_total {
            let mut progress = false;
            let mut i = start;
            while 0 <= i && self.total_blobs < self.blob_count && self.total_bytes < self.max_total {
                let s = self.sizes[i as usize];
                if self.total_bytes + s <= self.max_total {
                    progress = true;
                    self.by_size[i as usize] += 1;
                    self.total_blobs += 1;
                    self.total_bytes += s;
                }
                i -= 1;
            }
            if !progress {
                break;
            }
        }
    }
}

/// `POST /_snapshot/{repository}/_analyze`.
pub async fn analyze_repository(ctx: &Ctx<'_>) -> EsResult<Reply> {
    let name = ctx.path_param("repository").unwrap_or_default().to_string();
    let repo = {
        let st = ctx.state();
        let Some(repo) = st.metadata.repositories.get(&name) else { return Err(repository_missing(&name)) };
        repo.clone()
    };
    let p = params(ctx)?;
    if repo.read_only() || super::elastic::open(ctx.cluster, &name, &repo).await?.is_some() {
        return Err(EsError::illegal_argument(format!("repository [{name}] is read-only")));
    }
    let mut rng = SplitMix(p.seed as u64);
    let mut sizes = blob_sizes(p.blob_count, p.max_total.bytes(), p.max_blob.bytes());
    rng.shuffle(&mut sizes);
    let blob_path = format!("temp-analysis-{}", random_uuid());
    let outcome = match repo.in_bucket() {
        true => {
            let bucket = s3::Bucket::open(ctx.cluster, &name, &repo)?;
            let run = analyze_bucket(ctx, &bucket, &p, &sizes, &mut rng, &blob_path);
            match tokio::time::timeout(Duration::from_nanos(p.timeout.nanos().clamp(0, u64::MAX as i128) as u64), run).await {
                Ok(Ok(o)) => o,
                Ok(Err(e)) => {
                    remove_all(&bucket, &blob_path).await;
                    return Err(analysis_failed(&name, &repo, &blob_path, false, e));
                }
                Err(_) => {
                    remove_all(&bucket, &blob_path).await;
                    let cause = verification(&name, format!("analysis timed out after [{}]", p.timeout.string_rep()));
                    return Err(analysis_failed(&name, &repo, &blob_path, true, cause));
                }
            }
        }
        false => {
            planned_timeout(&name, &repo, &p, sizes.iter().sum())?;
            Outcome::planned(&sizes, &mut rng)
        }
    };
    Reply::ok(answer(ctx, &name, &p, &blob_path, &outcome))
}

/// A planned analysis (a repository in the cluster's own storage) that the repository's
/// throttles couldn't finish within `timeout`.
fn planned_timeout(name: &str, repo: &Repository, p: &Params, total: i64) -> EsResult<()> {
    let throttle = |key: &str| repo.settings.get(key).and_then(|v| ByteSize::parse(v, key).ok()).map(|b| b.bytes()).filter(|b| *b > 0);
    let too_slow = |rate: Option<i64>| rate.is_some_and(|r| i128::from(total) * 1_000_000_000 / i128::from(r) > p.timeout.nanos());
    if too_slow(throttle("max_snapshot_bytes_per_sec")) || too_slow(throttle("max_restore_bytes_per_sec")) {
        let cause = verification(name, format!("analysis timed out after [{}]", p.timeout.string_rep()));
        return Err(verification(
            name,
            format!("Repository analysis timed out. Consider specifying a longer timeout using the [?timeout] request parameter. See [{ANALYZE_DOCS}] for more information."),
        )
        .caused_by(cause));
    }
    Ok(())
}

fn verification(name: &str, msg: impl std::fmt::Display) -> EsError {
    EsError::new(500, "repository_verification_exception", format!("[{name}] {msg}"))
}

/// The failure an analysis of a bucket answers (`RepositoryAnalyzeAction`'s, with
/// `S3Repository`'s detail on S3-compatible storage; `BlobStoreRepository`'s on the others),
/// having tried to remove what it wrote.
fn analysis_failed(name: &str, repo: &Repository, blob_path: &str, timed_out: bool, cause: EsError) -> EsError {
    let left = format!("Elasticsearch attempted to remove the data it wrote at [{blob_path}] but may have left some behind. If so, please now remove this data manually.");
    let msg = match timed_out {
        true => format!("Repository analysis timed out. Consider specifying a longer timeout using the [?timeout] request parameter. See [{ANALYZE_DOCS}] for more information. {left}"),
        false if repo.ty != "s3" => format!(
            "Elasticsearch observed the storage system underneath this repository behaved incorrectly which indicates it is not suitable for use with Elasticsearch snapshots. See [{ANALYZE_DOCS}] for further information. {left}"
        ),
        false => format!(
            "Elasticsearch observed the storage system underneath this repository behaved incorrectly which indicates it is not suitable for use with Elasticsearch snapshots. Typically this happens when using storage other than AWS S3 which incorrectly claims to be S3-compatible. If so, please report this incompatibility to your storage supplier. Do not report Elasticsearch issues involving storage systems which claim to be S3-compatible unless you can demonstrate that the same issue exists when using a genuine AWS S3 repository. See [{ANALYZE_DOCS}] for further information about repository analysis, and [{S3_COMPATIBLE_DOCS}] for further information about support for S3-compatible repository implementations. {left}"
        ),
    };
    verification(name, msg).caused_by(cause)
}

/// Deletes everything under the analysis's path, as far as it can.
async fn remove_all(bucket: &s3::Bucket, blob_path: &str) {
    let dir = format!("{}{blob_path}/", bucket.prefix);
    let cleanup = async {
        if let Ok(listed) = bucket.store.list(&dir).await {
            for (k, _) in listed {
                let _ = bucket.store.delete(&k).await;
            }
        }
        if let Ok(uploads) = bucket.store.list_uploads(&dir, 10_000).await {
            for (k, id) in uploads {
                let _ = bucket.store.abort(&k, &id).await;
            }
        }
    };
    let _ = tokio::time::timeout(Duration::from_secs(30), cleanup).await;
}

/// A seeded generator (SplitMix64) for blob sizes' order, contents and the rare actions.
pub(super) struct SplitMix(u64);

impl SplitMix {
    fn next(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        mix(self.0)
    }

    fn below(&mut self, n: u64) -> u64 {
        if n == 0 { 0 } else { self.next() % n }
    }

    fn chance(&mut self, p: f64) -> bool {
        ((self.next() >> 11) as f64 / (1u64 << 53) as f64) < p
    }

    fn shuffle<T>(&mut self, v: &mut [T]) {
        for i in (1..v.len()).rev() {
            v.swap(i, self.below(i as u64 + 1) as usize);
        }
    }
}

fn mix(mut z: u64) -> u64 {
    z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    z ^ (z >> 31)
}

/// Bytes `offset..offset + out.len()` of the blob with `seed`: any range regenerates alone, so
/// a read is checked without holding what was written.
fn content(seed: u64, offset: u64, out: &mut [u8]) {
    for (i, b) in out.iter_mut().enumerate() {
        let pos = offset + i as u64;
        *b = (mix(seed ^ (pos >> 3).wrapping_mul(0x2545_F491_4F6C_DD1D)) >> ((pos & 7) * 8)) as u8;
    }
}

/// One blob's analysis (`BlobAnalyzeAction`).
struct Blob {
    name: String,
    size: i64,
    seed: u64,
    read_early: bool,
    overwrite: bool,
    abort: bool,
    /// The range read back.
    range: (u64, u64),
}

impl Blob {
    /// `BlobAnalyzeAction.Request.getDescription`, as failures name it.
    fn description(&self, repo: &str, blob_path: &str) -> String {
        format!(
            "blob analysis [{repo}:{blob_path}/{}, length={}, seed={}, readEarly={}, writeAndOverwrite={}, abortWrite={}, copyBlobName=null]",
            self.name, self.size, self.seed as i64, self.read_early, self.overwrite, self.abort
        )
    }
}

/// One read of a blob.
#[derive(Clone, Copy)]
struct ReadRun {
    found: bool,
    first_byte: u64,
    elapsed: u64,
    throttled: u64,
    bytes: i64,
}

/// What one blob's write and reads took.
#[derive(Default)]
struct BlobRun {
    write_elapsed: u64,
    write_throttled: u64,
    written: i64,
    reads: Vec<ReadRun>,
}

/// An analysis's results.
#[derive(Default)]
struct Outcome {
    blobs: Vec<(Blob, BlobRun)>,
    listing_elapsed: u64,
    delete_elapsed: u64,
}

impl Outcome {
    /// What a repository in the cluster's own storage answers: every blob written once and read
    /// back whole, instantly.
    fn planned(sizes: &[i64], rng: &mut SplitMix) -> Outcome {
        let uuid = random_uuid();
        let blobs = sizes
            .iter()
            .enumerate()
            .map(|(i, &size)| {
                let blob = Blob { name: format!("test-blob-{i}-{uuid}"), size, seed: rng.next(), read_early: false, overwrite: false, abort: false, range: (0, size as u64) };
                let read = ReadRun { found: true, first_byte: 0, elapsed: 0, throttled: 0, bytes: size };
                (blob, BlobRun { written: size, reads: vec![read], ..Default::default() })
            })
            .collect();
        Outcome { blobs, ..Default::default() }
    }
}

fn plan_blobs(p: &Params, sizes: &[i64], rng: &mut SplitMix) -> Vec<Blob> {
    let uuid = random_uuid();
    sizes
        .iter()
        .enumerate()
        .map(|(i, &size)| {
            let size_u = size as u64;
            let abort = p.abort_writes && rng.chance(p.rare);
            let overwrite = !abort && p.concurrency > 1 && rng.chance(p.rare);
            let read_early = p.early_read_node_count > 0 && rng.chance(p.rare);
            let range = match rng.next() & 1 {
                0 => (0, size_u),
                _ => {
                    let start = rng.below(size_u);
                    (start, start + 1 + rng.below(size_u - start))
                }
            };
            Blob { name: format!("test-blob-{i}-{uuid}"), size, seed: rng.next(), read_early, overwrite, abort, range }
        })
        .collect()
}

/// Analyses `bucket` under `blob_path`.
async fn analyze_bucket(ctx: &Ctx<'_>, bucket: &s3::Bucket, p: &Params, sizes: &[i64], rng: &mut SplitMix, blob_path: &str) -> EsResult<Outcome> {
    let name = bucket.name.as_str();
    let dir = format!("{}{blob_path}/", bucket.prefix);
    let node = crate::api::nodes::discovery_node_string(ctx.cluster);
    let fail = |what: &str, e: crate::s3::Error| verification(name, format!("{what}: {e}"));
    let lanes = (p.concurrency as usize).clamp(1, CONCURRENCY_MAX);

    register_checks(bucket, &dir, p, lanes).await?;

    let blobs = plan_blobs(p, sizes, rng);
    let lane = |l: usize| {
        let (blobs, dir, node) = (&blobs, &dir, &node);
        async move {
            let mut out = Vec::new();
            for i in (l..blobs.len()).step_by(lanes) {
                let b = &blobs[i];
                let run = analyze_blob(ctx, bucket, dir, node, b).await.map_err(|e| verification(name, format!("failure processing [{}]", b.description(name, blob_path))).caused_by(e))?;
                out.push((i, run));
            }
            Ok::<_, EsError>(out)
        }
    };
    let mut runs: Vec<Option<BlobRun>> = blobs.iter().map(|_| None).collect();
    for lane in crate::api::by_query::join_all((0..lanes).map(lane).collect()).await {
        for (i, run) in lane? {
            runs[i] = Some(run);
        }
    }

    // Every blob written is listed with its size, and a prefix nothing has lists nothing.
    let started = Instant::now();
    let listed: std::collections::BTreeMap<String, u64> = bucket.store.list(&dir).await.map_err(|e| fail("listing the analysis blobs", e))?.into_iter().collect();
    let listing_elapsed = started.elapsed().as_nanos() as u64;
    let key = |b: &Blob| format!("{dir}{}", b.name);
    let missing: Vec<&str> = blobs.iter().filter(|b| !b.abort && !listed.contains_key(&key(b))).map(|b| b.name.as_str()).collect();
    if !missing.is_empty() {
        return Err(verification(name, format!("expected blobs [{}] missing in [{name}:{blob_path}]", missing.join(", "))));
    }
    for b in blobs.iter().filter(|b| !b.abort) {
        let got = listed[&key(b)];
        if got != b.size as u64 {
            return Err(verification(name, format!("listBlobsByPrefix(\"test-blob-\") returned incorrect size for blob [{}]: expected {} but got {got}", b.name, b.size)));
        }
    }
    let aborted: Vec<&str> = blobs.iter().filter(|b| b.abort && listed.contains_key(&key(b))).map(|b| b.name.as_str()).collect();
    if !aborted.is_empty() {
        return Err(verification(name, format!("listBlobsByPrefix(\"test-blob-\") returned incorrect results: missing=[], unexpected=[{}]", aborted.join(", "))));
    }
    let nothing = bucket.store.list(&format!("{dir}nonexistent-prefix-")).await.map_err(|e| fail("listing the analysis blobs", e))?;
    if !nothing.is_empty() {
        let keys: Vec<&str> = nothing.iter().map(|(k, _)| k.strip_prefix(dir.as_str()).unwrap_or(k)).collect();
        return Err(verification(name, format!("listBlobsByPrefix(\"nonexistent-prefix-\") should return empty results but returned [{}]", keys.join(", "))));
    }

    // Deleting them all leaves nothing.
    let started = Instant::now();
    for k in listed.keys() {
        bucket.store.delete(k).await.map_err(|e| fail("deleting the analysis blobs", e))?;
    }
    let delete_elapsed = started.elapsed().as_nanos() as u64;
    let left = bucket.store.list(&dir).await.map_err(|e| fail("listing the analysis blobs", e))?;
    if !left.is_empty() {
        let keys: Vec<&str> = left.iter().map(|(k, _)| k.strip_prefix(dir.as_str()).unwrap_or(k)).collect();
        return Err(verification(name, format!("failed to clean up blobs [{}]", keys.join(", "))));
    }
    let blobs = blobs.into_iter().zip(runs).map(|(b, r)| (b, r.unwrap_or_default())).collect();
    Ok(Outcome { blobs, listing_elapsed, delete_elapsed })
}

/// The register checks (`ContendedRegisterAnalyzeAction`, `UncontendedRegisterAnalyzeAction`)
/// and overwrite protection (`BlobOverwriteAction`).
async fn register_checks(bucket: &s3::Bucket, dir: &str, p: &Params, lanes: usize) -> EsResult<()> {
    let name = bucket.name.as_str();
    let fail = |what: &str, e: crate::s3::Error| verification(name, format!("{what}: {e}"));
    // `AzureBlobContainer`: a register read from a secondary location may be stale.
    if let crate::blob::Store::Azure(c) = &bucket.store
        && c.location_mode() != crate::azure::LocationMode::PrimaryOnly
    {
        return Err(verification(name, format!("consistent register operations are not supported with location_mode [{}]", c.location_mode().name())));
    }
    let uuid = random_uuid();
    // Contended: `register_operation_count` increments by compare-and-exchange, all landing.
    let contended = &format!("{dir}test-register-contended-{uuid}");
    let increment = |_: &usize| async move {
        for _ in 0..REGISTER_ATTEMPTS {
            let (value, etag) = match bucket.store.get_tagged(contended, 64).await.map_err(|e| fail("reading a register", e))? {
                Some((b, etag)) => (register_value(&b).ok_or_else(|| verification(name, "register holds unexpected value [corrupt]"))?, Some(etag)),
                None => (0, None),
            };
            let next = Bytes::from((value + 1).to_be_bytes().to_vec());
            if bucket.store.put_if(contended, next, etag.as_deref()).await.map_err(|e| fail("writing a register", e))?.is_some() {
                return Ok(());
            }
        }
        Err(verification(name, "register read failed due to contention"))
    };
    let ops: Vec<usize> = (0..p.register_ops as usize).collect();
    for batch in ops.chunks(lanes) {
        for r in crate::api::by_query::join_all(batch.iter().map(increment).collect()).await {
            r?;
        }
    }
    let value = bucket.store.get_tagged(contended, 64).await.map_err(|e| fail("reading a register", e))?.and_then(|(b, _)| register_value(&b));
    if value != Some(ops.len() as u64) {
        let shown = value.map_or("absent".to_string(), |v| v.to_string());
        return Err(verification(name, format!("register holds unexpected value [{shown}]")));
    }
    // Uncontended: a fresh register is written once, and only once.
    let uncontended = format!("{dir}test-register-uncontended-{uuid}");
    let first = bucket.store.put_if(&uncontended, Bytes::from(1u64.to_be_bytes().to_vec()), None).await.map_err(|e| fail("writing a register", e))?;
    let again = bucket.store.put_if(&uncontended, Bytes::from(2u64.to_be_bytes().to_vec()), None).await.map_err(|e| fail("writing a register", e))?;
    if first.is_none() || again.is_some() {
        return Err(verification(name, format!("register holds unexpected value [{}]", if first.is_none() { 0 } else { 2 })));
    }
    // Overwrite protection: of concurrent creators of one blob exactly one wins.
    let blob = format!("test-overwrite-blob-{uuid}");
    let key = format!("{dir}{blob}");
    let wins = crate::api::by_query::join_all((0..lanes.max(2)).map(|i| bucket.store.put_new(&key, Bytes::from(format!("writer {i}")))).collect()).await;
    let mut won = 0;
    for w in wins {
        won += usize::from(w.map_err(|e| fail("writing an overwrite-protected blob", e))?);
    }
    match won {
        0 => Err(verification(name, "all overwrite protection checks failed")),
        1 => Ok(()),
        _ => Err(verification(name, format!("multiple writes succeeded to overwrite-protected blob {}/{blob}", dir.trim_end_matches('/')))),
    }
}

/// A register's value: 8 bytes, big-endian (an empty object is 0).
fn register_value(b: &[u8]) -> Option<u64> {
    match b.len() {
        0 => Some(0),
        8 => Some(u64::from_be_bytes(b.try_into().ok()?)),
        _ => None,
    }
}

/// Writes blob `b` and reads it back.
async fn analyze_blob(ctx: &Ctx<'_>, bucket: &s3::Bucket, dir: &str, node: &str, b: &Blob) -> EsResult<BlobRun> {
    let name = bucket.name.as_str();
    let key = format!("{dir}{}", b.name);
    let fail = |what: &str, e: crate::s3::Error| verification(name, format!("{what}: {e}"));
    let mut run = BlobRun::default();
    if b.read_early {
        // Before its write the blob can't be there.
        let early = read_range(ctx, bucket, &key, b.seed, (0, b.size as u64)).await.map_err(|e| fail("reading a blob", e))?;
        if early.found {
            return Err(verification(name, format!("node [{node}] read blob [{}] before it was written", b.name)));
        }
        run.reads.push(ReadRun { found: false, first_byte: early.first_byte, elapsed: early.elapsed, throttled: early.throttled, bytes: 0 });
    }
    let started = Instant::now();
    if b.abort {
        // A write abandoned partway leaves nothing behind.
        let mut up = transfer::Upload::new(&bucket.store, &bucket.io, bucket.budget.clone(), key.clone(), bucket.sizes.buffer);
        write_content(&mut up, b.seed, b.size as u64).await.map_err(|e| failed_write(bucket, &key, e))?;
        run.write_throttled += up.throttled_nanos;
        up.abort().await;
        if bucket.store.open(&key, None).await.map_err(|e| fail("reading a blob", e))?.is_some() {
            return Err(verification(name, format!("node [{node}] found blob [{}] after its write was aborted", b.name)));
        }
        run.write_elapsed = started.elapsed().as_nanos() as u64;
        return Ok(run);
    }
    write_blob(bucket, &key, b.seed, b.size as u64, true, &mut run).await?;
    let mut seed = b.seed;
    if b.overwrite {
        seed = b.seed.wrapping_add(1);
        write_blob(bucket, &key, seed, b.size as u64, false, &mut run).await?;
    }
    run.written = b.size;
    run.write_elapsed = started.elapsed().as_nanos() as u64;
    let read = read_range(ctx, bucket, &key, seed, b.range).await.map_err(|e| fail("reading a blob", e))?;
    if !read.found {
        return Err(verification(name, format!("node [{node}] reported blob not found after it was written")));
    }
    let want = b.range.1 - b.range.0;
    if read.bytes != want || read.checksum != read.expected {
        return Err(verification(
            name,
            format!(
                "node [{node}] failed during analysis: expected to read [{}-{}], [{want}] bytes, with checksum [{}] but read [GetRepositoryBlobChecksumResponse{{bytesRead={}, checksum={}, firstByteNanos={}, elapsedNanos={}, throttleNanos={}}}]",
                b.range.0, b.range.1, read.expected, read.bytes, read.checksum, read.first_byte, read.elapsed, read.throttled
            ),
        ));
    }
    run.reads.push(ReadRun { found: true, first_byte: read.first_byte, elapsed: read.elapsed, throttled: read.throttled, bytes: want as i64 });
    Ok(run)
}

fn failed_write(bucket: &s3::Bucket, key: &str, f: transfer::Failed) -> EsError {
    match f {
        transfer::Failed::Store(e) => verification(&bucket.name, format!("write failed for [{key}]: {e}")),
        transfer::Failed::Breaker(e) => e,
    }
}

/// Writes blob `key` (`size` bytes of `seed`'s content): one request with `If-None-Match` when it
/// fits in a part (`create`: it must not exist yet), otherwise a multipart upload.
async fn write_blob(bucket: &s3::Bucket, key: &str, seed: u64, size: u64, create: bool, run: &mut BlobRun) -> EsResult<()> {
    let name = bucket.name.as_str();
    if size <= bucket.sizes.buffer {
        let mut held = crate::breaker::Reservation::new(bucket.budget.clone());
        held.add(size, "<s3_analysis_blob>")?;
        let mut body = vec![0u8; size as usize];
        content(seed, 0, &mut body);
        run.write_throttled += bucket.io.write(size).await;
        let fresh = match create {
            true => bucket.store.put_new(key, Bytes::from(body)).await,
            false => bucket.store.put(key, Bytes::from(body)).await.map(|_| true),
        };
        return match fresh.map_err(|e| verification(name, format!("write failed for [{key}]: {e}")))? {
            true => Ok(()),
            false => Err(verification(name, format!("blob [{key}] already existed before it was written"))),
        };
    }
    let mut up = transfer::Upload::new(&bucket.store, &bucket.io, bucket.budget.clone(), key.to_string(), bucket.sizes.buffer);
    write_content(&mut up, seed, size).await.map_err(|e| failed_write(bucket, key, e))?;
    up.finish().await.map_err(|e| failed_write(bucket, key, e))?;
    run.write_throttled += up.throttled_nanos;
    Ok(())
}

async fn write_content(up: &mut transfer::Upload<'_>, seed: u64, size: u64) -> Result<(), transfer::Failed> {
    let mut piece = vec![0u8; (1 << 20).min(size as usize)];
    let mut at = 0u64;
    while at < size {
        let n = (size - at).min(piece.len() as u64) as usize;
        content(seed, at, &mut piece[..n]);
        up.write(&piece[..n]).await?;
        at += n as u64;
    }
    Ok(())
}

/// A read back: whether the blob was there, what it held, and how long it took.
struct Read {
    found: bool,
    bytes: u64,
    checksum: u32,
    expected: u32,
    first_byte: u64,
    elapsed: u64,
    throttled: u64,
}

/// Reads `range` of `key`, checking it against `seed`'s content as it arrives (paying the
/// repository's restore throttles).
async fn read_range(ctx: &Ctx<'_>, bucket: &s3::Bucket, key: &str, seed: u64, range: (u64, u64)) -> Result<Read, crate::s3::Error> {
    let started = Instant::now();
    let mut out = Read { found: false, bytes: 0, checksum: 0, expected: 0, first_byte: 0, elapsed: 0, throttled: 0 };
    let Some(mut body) = bucket.store.open(key, Some(range)).await? else {
        out.elapsed = started.elapsed().as_nanos() as u64;
        return Ok(out);
    };
    out.found = true;
    let want = range.1 - range.0;
    let (mut got, mut expected) = (crc32fast::Hasher::new(), crc32fast::Hasher::new());
    let mut scratch = Vec::new();
    while let Some(piece) = body.next().await? {
        if out.bytes == 0 {
            out.first_byte = started.elapsed().as_nanos() as u64;
        }
        out.throttled += bucket.io.read(&ctx.cluster.snapshot_data.recovery, piece.len() as u64).await;
        // Read no further than one byte past the range: more is wrong already.
        let take = piece.len().min((want + 1).saturating_sub(out.bytes) as usize);
        got.update(&piece[..take]);
        let fits = take.min(want.saturating_sub(out.bytes) as usize);
        scratch.resize(fits, 0);
        content(seed, range.0 + out.bytes, &mut scratch);
        expected.update(&scratch);
        out.bytes += take as u64;
        if out.bytes > want {
            break;
        }
    }
    // The rest of the expected range, for the checksum a short read is compared with.
    let mut at = out.bytes.min(want);
    while at < want {
        let n = (want - at).min(1 << 20) as usize;
        scratch.resize(n, 0);
        content(seed, range.0 + at, &mut scratch);
        expected.update(&scratch);
        at += n as u64;
    }
    out.checksum = got.finalize();
    out.expected = expected.finalize();
    out.elapsed = started.elapsed().as_nanos() as u64;
    Ok(out)
}

/// `RepositoryAnalyzeAction.Response.toXContent`.
fn answer(ctx: &Ctx, name: &str, p: &Params, blob_path: &str, o: &Outcome) -> Value {
    let node = &ctx.cluster.node;
    let human = p.human;
    let time = |m: &mut Map, k: &str, nanos: u64| {
        if human {
            m.insert(k.into(), Value::from(TimeValue { duration: nanos as i64, unit: crate::units::TimeUnit::Nanos }.human()));
        }
        m.insert(format!("{k}_nanos"), Value::from(nanos as i64));
    };
    let sized = |m: &mut Map, k: &str, v: i64| {
        if human {
            m.insert(k.into(), Value::from(ByteSize(v).human()));
        }
        m.insert(format!("{k}_bytes"), Value::from(v));
    };
    let reads = || o.blobs.iter().flat_map(|(_, r)| r.reads.iter().filter(|r| r.found));
    let (mut w, mut r) = (Map::new(), Map::new());
    w.insert("count".into(), Value::from(p.blob_count));
    sized(&mut w, "total_size", o.blobs.iter().map(|(_, r)| r.written).sum());
    time(&mut w, "total_throttled", o.blobs.iter().map(|(_, r)| r.write_throttled).sum());
    time(&mut w, "total_elapsed", o.blobs.iter().map(|(_, r)| r.write_elapsed).sum());
    r.insert("count".into(), Value::from(reads().count() as i64));
    sized(&mut r, "total_size", reads().map(|r| r.bytes).sum());
    time(&mut r, "total_wait", reads().map(|r| r.first_byte).sum());
    time(&mut r, "max_wait", reads().map(|r| r.first_byte).max().unwrap_or(0));
    time(&mut r, "total_throttled", reads().map(|r| r.throttled).sum());
    time(&mut r, "total_elapsed", reads().map(|r| r.elapsed).sum());
    let who = || obj! { "id" => node.id.clone(), "name" => node.name.clone() };
    let mut m = Map::new();
    m.insert("coordinating_node".into(), who());
    m.insert("repository".into(), name.into());
    m.insert("blob_count".into(), p.blob_count.into());
    m.insert("concurrency".into(), p.concurrency.into());
    m.insert("read_node_count".into(), p.read_node_count.into());
    m.insert("early_read_node_count".into(), p.early_read_node_count.into());
    sized(&mut m, "max_blob_size", p.max_blob.bytes());
    sized(&mut m, "max_total_data_size", p.max_total.bytes());
    m.insert("seed".into(), Value::from(p.seed));
    m.insert("rare_action_probability".into(), Value::from(p.rare));
    m.insert("blob_path".into(), Value::from(blob_path));
    m.insert("check_overwrite_protection".into(), Value::Bool(true));
    m.insert("issues_detected".into(), Value::Arr(Vec::new()));
    m.insert("summary".into(), obj! { "write" => Value::Obj(w), "read" => Value::Obj(r) });
    if p.detailed {
        let details = o
            .blobs
            .iter()
            .map(|(b, run)| {
                let mut blob = Map::new();
                blob.insert("name".into(), b.name.clone().into());
                sized(&mut blob, "size", b.size);
                blob.insert("read_start".into(), Value::from(b.range.0 as i64));
                blob.insert("read_end".into(), Value::from(b.range.1 as i64));
                blob.insert("read_early".into(), Value::Bool(b.read_early));
                blob.insert("overwritten".into(), Value::Bool(b.overwrite));
                let reads = run
                    .reads
                    .iter()
                    .map(|rd| {
                        let mut m = Map::new();
                        m.insert("node".into(), who());
                        m.insert("found".into(), Value::Bool(rd.found));
                        if rd.found {
                            time(&mut m, "first_byte_time", rd.first_byte);
                            time(&mut m, "elapsed", rd.elapsed);
                            time(&mut m, "throttled", rd.throttled);
                        }
                        Value::Obj(m)
                    })
                    .collect();
                let mut e = Map::new();
                e.insert("blob".into(), Value::Obj(blob));
                e.insert("writer_node".into(), who());
                time(&mut e, "write_elapsed", run.write_elapsed);
                time(&mut e, "write_throttled", run.write_throttled);
                e.insert("reads".into(), Value::Arr(reads));
                Value::Obj(e)
            })
            .collect();
        m.insert("details".into(), Value::Arr(details));
    }
    time(&mut m, "listing_elapsed", o.listing_elapsed);
    time(&mut m, "delete_elapsed", o.delete_elapsed);
    Value::Obj(m)
}

/// A seed for the analysis (`Randomness.get().nextLong()` in Elasticsearch; any value will do).
fn rand_seed() -> i32 {
    let nanos = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.subsec_nanos());
    (nanos >> 1) as i32
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Elasticsearch 9.5.5's answers: `summary.write.total_size_bytes` for these requests.
    #[test]
    fn blob_sizes_are_elasticsearchs() {
        let total = |count, max_total, max_blob| blob_sizes(count, max_total, max_blob).iter().sum::<i64>();
        assert_eq!(blob_sizes(4, 1 << 30, 64 << 10), [16384, 32768, 65536, 65536]);
        assert_eq!(total(10, 4 << 20, 1 << 20), 3_141_632);
        assert_eq!(total(100, 64 << 20, 10 << 20), 66_584_576);
        assert_eq!(total(50, 1 << 30, 1 << 20), 6_283_262);
        assert_eq!(total(200, 4 << 20, 4 << 10), 130_801);
        for (count, max_total, max_blob) in [(100, 1 << 30, 10 << 20), (5, (64 << 10) + 4, 64 << 10), (1, 10, 10), (200, 4 << 20, 4096)] {
            let s = blob_sizes(count, max_total, max_blob);
            assert_eq!(s.len() as i64, count, "{count} {max_total} {max_blob}");
            assert!(s.iter().sum::<i64>() <= max_total && s.contains(&max_blob) && s.iter().all(|b| (1..=max_blob).contains(b)));
        }
    }

    #[test]
    fn content_regenerates_from_any_offset() {
        let mut whole = vec![0u8; 100];
        content(7, 0, &mut whole);
        let mut part = vec![0u8; 37];
        content(7, 13, &mut part);
        assert_eq!(&whole[13..50], &part[..]);
        assert_ne!(whole[..8], [0; 8]);
    }
}
