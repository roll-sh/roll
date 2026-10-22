//! Inventory reports: each enabled inventory configuration (stored by
//! PutBucketInventoryConfiguration, `config.rs`) produces a report on its schedule, laid out as
//! S3 does in the destination bucket:
//!
//! ```text
//! <prefix>/<source bucket>/<config id>/data/<uuid>.csv.gz
//! <prefix>/<source bucket>/<config id>/<YYYY-MM-DDTHH-MMZ>/manifest.json
//! <prefix>/<source bucket>/<config id>/<YYYY-MM-DDTHH-MMZ>/manifest.checksum
//! <prefix>/<source bucket>/<config id>/hive/dt=<YYYY-MM-DD-HH-MM>/symlink.txt
//! ```
//!
//! Files are written through the normal put path, so they're durable once written and billed
//! to the destination bucket. Only CSV is written (ORC and Parquet are refused when the
//! configuration is put). A report that fails is retried at the next check.

use crate::Server;
use crate::notify::event_key;
use crate::replication::arn_bucket;
use flate2::Compression;
use flate2::write::GzEncoder;
use md5::{Digest, Md5};
use s3_proto::time::{civil_from_days, iso8601_ms};
use s3_proto::xml::{self, Node};
use s3_store::config::kind;
use s3_store::{Bucket, Class, InventoryEntry, ObjectHeaders, WriteCondition};
use std::io::Write;

/// S3's optional fields, in the order S3 writes them.
pub(crate) const FIELDS: [&str; 15] = [
    "Size",
    "LastModifiedDate",
    "ETag",
    "StorageClass",
    "IsMultipartUploaded",
    "ReplicationStatus",
    "EncryptionStatus",
    "ObjectLockRetainUntilDate",
    "ObjectLockMode",
    "ObjectLockLegalHoldStatus",
    "IntelligentTieringAccessTier",
    "BucketKeyStatus",
    "ChecksumAlgorithm",
    "ObjectAccessControlList",
    "ObjectOwner",
];

/// A data file is closed once this many compressed bytes are written (memory stays bounded).
const MAX_FILE_BYTES: usize = 16 << 20;
const DAY_MS: i64 = 86_400_000;

fn uuid() -> String {
    let mut b = [0u8; 16];
    getrandom::fill(&mut b).expect("os randomness");
    b[6] = (b[6] & 0x0f) | 0x40;
    b[8] = (b[8] & 0x3f) | 0x80;
    let h = hex::encode(b);
    format!("{}-{}-{}-{}-{}", &h[..8], &h[8..12], &h[12..16], &h[16..20], &h[20..])
}

/// `(YYYY-MM-DDTHH-MMZ, YYYY-MM-DD-HH-MM)`.
fn stamps(ms: i64) -> (String, String) {
    let secs = ms.div_euclid(1000);
    let (y, m, d) = civil_from_days(secs.div_euclid(86_400));
    let t = secs.rem_euclid(86_400);
    let (h, mi) = (t / 3600, t % 3600 / 60);
    (format!("{y:04}-{m:02}-{d:02}T{h:02}-{mi:02}Z"), format!("{y:04}-{m:02}-{d:02}-{h:02}-{mi:02}"))
}

fn csv_field(out: &mut String, v: &str) {
    out.push('"');
    out.push_str(&v.replace('"', "\"\""));
    out.push('"');
}

fn field(name: &str, e: &InventoryEntry, owner: &str) -> String {
    let data = !e.delete_marker;
    let s = |b: bool, v: String| if b { v } else { String::new() };
    match name {
        "Size" => s(data, e.size.to_string()),
        "LastModifiedDate" => iso8601_ms(e.modified_at),
        "ETag" => s(data, e.etag.clone()),
        "StorageClass" => s(data, "STANDARD".into()),
        "IsMultipartUploaded" => s(data, e.multipart.to_string()),
        "ReplicationStatus" => e.replication_status.clone().unwrap_or_default(),
        "EncryptionStatus" => s(data, e.encryption_status.clone()),
        "ObjectLockRetainUntilDate" => e.lock.retain_until.filter(|_| e.lock.mode.is_some()).map(iso8601_ms).unwrap_or_default(),
        "ObjectLockMode" => e.lock.mode.clone().unwrap_or_default(),
        "ObjectLockLegalHoldStatus" => s(data, if e.lock.legal_hold == Some(true) { "ON" } else { "OFF" }.into()),
        "BucketKeyStatus" => s(data, if e.bucket_key { "ENABLED" } else { "DISABLED" }.into()),
        "ChecksumAlgorithm" => e.checksum_algorithm.clone().unwrap_or_default(),
        "ObjectOwner" => owner.to_string(),
        _ => String::new(),
    }
}

/// One finished data file.
struct DataFile {
    key: String,
    size: u64,
    md5: String,
}

struct Report<'a> {
    server: &'a Server,
    dest: Bucket,
    base: String,
    files: Vec<DataFile>,
}

impl Report<'_> {
    async fn put(&self, key: &str, body: Vec<u8>, content_type: &str) -> Result<(), String> {
        let store = &self.server.store;
        let len = body.len() as u64;
        let headers = ObjectHeaders { content_type: Some(content_type.into()), ..Default::default() };
        crate::put_object(store, &self.dest, key, headers, s3_store::bytes_body(bytes::Bytes::from(body)), len, &WriteCondition::none()).await.map_err(|e| e.to_string())?;
        store.meter.request(&self.dest.id, Class::A);
        store.meter.bytes_in(&self.dest.id, len);
        Ok(())
    }

    async fn flush(&mut self, gz: GzEncoder<Vec<u8>>) -> Result<(), String> {
        let bytes = gz.finish().map_err(|e| e.to_string())?;
        let key = format!("{}/data/{}.csv.gz", self.base, uuid());
        let md5 = hex::encode(Md5::digest(&bytes));
        let size = bytes.len() as u64;
        self.put(&key, bytes, "application/x-gzip").await?;
        self.files.push(DataFile { key, size, md5 });
        Ok(())
    }
}

/// Write one report for `source`'s configuration `id` now.
async fn report(server: &Server, source: &Bucket, id: &str, cfg: &Node) -> Result<(), String> {
    let store = &server.store;
    let d = cfg.child("Destination").and_then(|d| d.child("S3BucketDestination")).ok_or("no destination")?;
    let arn = d.text_of("Bucket").unwrap_or_default();
    let dest_name = arn_bucket(arn).ok_or_else(|| format!("{arn} isn't an S3 bucket ARN"))?;
    let dest = store.bucket(dest_name).map_err(|e| format!("destination: {e}"))?;
    let format = d.text_of("Format").unwrap_or("CSV").trim();
    if format != "CSV" {
        return Err(format!("{format} inventories aren't supported"));
    }
    let prefix = d.text_of("Prefix").unwrap_or_default().trim_end_matches('/');
    let all = cfg.text_of("IncludedObjectVersions").map(str::trim) == Some("All");
    let filter = cfg.child("Filter").and_then(|f| f.text_of("Prefix")).unwrap_or_default().to_string();
    let wanted: Vec<String> = cfg.child("OptionalFields").map(|o| o.all("Field").map(|f| f.text.trim().to_string()).collect()).unwrap_or_default();
    let mut columns: Vec<&str> = vec!["Bucket", "Key"];
    if all {
        columns.extend(["VersionId", "IsLatest", "IsDeleteMarker"]);
    }
    columns.extend(FIELDS.iter().copied().filter(|f| wanted.iter().any(|w| w == f)));
    let now = store.now();
    let (folder, hive) = stamps(now);
    let base = if prefix.is_empty() { format!("{}/{id}", source.name) } else { format!("{prefix}/{}/{id}", source.name) };
    let owner = store.owner_id();
    let mut rep = Report { server, dest, base: base.clone(), files: Vec::new() };
    let mut gz = GzEncoder::new(Vec::new(), Compression::default());
    let mut rows = 0u64;
    let mut after = None;
    loop {
        let (page, next) = store.inventory_page(&source.name, all, &filter, after, 1000).map_err(|e| e.to_string())?;
        let mut text = String::new();
        for e in &page {
            for (i, c) in columns.iter().enumerate() {
                if i > 0 {
                    text.push(',');
                }
                let v = match *c {
                    "Bucket" => source.name.clone(),
                    "Key" => event_key(&e.key),
                    "VersionId" => e.version_id.clone(),
                    "IsLatest" => e.is_latest.to_string(),
                    "IsDeleteMarker" => e.delete_marker.to_string(),
                    f => field(f, e, &owner),
                };
                csv_field(&mut text, &v);
            }
            text.push('\n');
            rows += 1;
        }
        gz.write_all(text.as_bytes()).map_err(|e| e.to_string())?;
        if gz.get_ref().len() >= MAX_FILE_BYTES {
            let full = std::mem::replace(&mut gz, GzEncoder::new(Vec::new(), Compression::default()));
            rep.flush(full).await?;
            rows = 0;
        }
        match next {
            Some(n) => after = Some(n),
            None => break,
        }
    }
    if rows > 0 || rep.files.is_empty() {
        rep.flush(gz).await?;
    }
    let manifest = serde_json::json!({
        "sourceBucket": source.name,
        "destinationBucket": format!("arn:aws:s3:::{dest_name}"),
        "version": "2016-11-30",
        "creationTimestamp": now.to_string(),
        "fileFormat": "CSV",
        "fileSchema": columns.join(", "),
        "files": rep.files.iter().map(|f| serde_json::json!({ "key": f.key, "size": f.size, "MD5checksum": f.md5 })).collect::<Vec<_>>(),
    });
    let manifest = serde_json::to_vec_pretty(&manifest).map_err(|e| e.to_string())?;
    let checksum = hex::encode(Md5::digest(&manifest));
    let symlinks: String = rep.files.iter().map(|f| format!("s3://{dest_name}/{}\n", f.key)).collect();
    rep.put(&format!("{base}/hive/dt={hive}/symlink.txt"), symlinks.into_bytes(), "text/plain").await?;
    // The manifest last: once it's there, the report is complete.
    rep.put(&format!("{base}/{folder}/manifest.json"), manifest, "application/json").await?;
    rep.put(&format!("{base}/{folder}/manifest.checksum"), checksum.into_bytes(), "text/plain").await?;
    Ok(())
}

/// Write every inventory report that's due (or all enabled ones, with `force`). Returns how
/// many were written.
pub(crate) async fn run(server: &Server, force: bool) -> usize {
    let store = &server.store;
    let buckets = match store.buckets() {
        Ok(b) => b,
        Err(e) => {
            tracing::warn!(error = %e, "listing buckets for inventory reports failed");
            return 0;
        }
    };
    let mut written = 0;
    for b in buckets {
        let mut after: Option<String> = None;
        loop {
            let Ok(cfgs) = store.list_bucket_config(&b.name, kind::INVENTORY, after.as_deref(), 100) else { break };
            let Some(last) = cfgs.last().map(|(id, _)| id.clone()) else { break };
            for (id, v) in &cfgs {
                let Ok(n) = xml::parse(v.as_bytes()) else { continue };
                if !n.text_of("IsEnabled").is_some_and(|e| e.trim().eq_ignore_ascii_case("true")) {
                    continue;
                }
                let period = if n.child("Schedule").and_then(|s| s.text_of("Frequency")).map(str::trim) == Some("Weekly") { 7 * DAY_MS } else { DAY_MS };
                let job = format!("inventory:{}:{id}", b.id);
                let last_run: i64 = store.job_state(&job).ok().flatten().and_then(|v| v.parse().ok()).unwrap_or(0);
                let now = store.now();
                if !force && now - last_run < period {
                    continue;
                }
                match report(server, &b, id, &n).await {
                    Ok(()) => {
                        written += 1;
                        if let Err(e) = store.set_job_state(&job, &now.to_string()).await {
                            tracing::warn!(error = %e, "recording an inventory report failed");
                        }
                    }
                    Err(e) => tracing::warn!(bucket = b.name, config = id, error = e, "inventory report failed; retrying at the next check"),
                }
            }
            after = Some(last);
            if cfgs.len() < 100 {
                break;
            }
        }
    }
    written
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn formats() {
        assert_eq!(stamps(1_549_411_238_000), ("2019-02-06T00-00Z".to_string(), "2019-02-06-00-00".to_string()));
        let u = uuid();
        assert_eq!(u.len(), 36);
        assert_eq!(&u[14..15], "4");
        let mut s = String::new();
        csv_field(&mut s, "a\"b");
        assert_eq!(s, "\"a\"\"b\"");
    }
}
