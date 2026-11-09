//! Derived images (image transformations): a transformed image is its own blob, cached by its
//! source blob and the transformation (`derived`), and metered by unique source per bucket per
//! month (`image_usage`).
//!
//! Source blobs are immutable, so a cached image never goes stale: an overwrite is a new source
//! blob, hence new entries, and the old entries go with the old blob. The rules:
//!
//! - A derived image is one blob, like every object (immutable, a fresh id per upload). Its
//!   `blobs` row counts the `derived` row pointing at it as its one reference, so the sweeper and
//!   the orphan scan treat it like any blob: never deleted while referenced, quarantined once it
//!   isn't.
//! - An entry is only added while its source is referenced (`refs > 0`): a source on its way out
//!   never gets new derived images. When the sweeper deletes a source blob, its entries go in
//!   the same transaction, and their blobs wait out the quarantine like any other.
//! - Entries unused for [`crate::StoreConfig::derived_expiry_days`] are dropped by the sweeper.
//!   `last_used_at` is written relaxed, at most once a day per entry (a lost update only means
//!   an earlier expiry, and the image is made again).
//! - A derived image of an SSE-KMS source is encrypted with the same KMS parameters (recorded in
//!   `blob_sse` like any blob's). SSE-C sources are never transformed: roll can't read them
//!   without the customer's key, and it never keeps one.

use crate::backend::{BlobSse, ByteStream};
use crate::error::{Result, StoreError};
use crate::ids;
use crate::rows;
use crate::sse::Encryption;
use crate::store::Store;
use rusqlite::{Connection, OptionalExtension, params};
use serde::Serialize;

const DAY_MS: i64 = 86_400_000;

/// A cached derived image.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct Derived {
    pub source_blob: String,
    /// The transformation's key (`s3_image::Plan::key`).
    pub params: String,
    pub blob_id: String,
    pub size: u64,
    pub content_type: String,
    pub created_at: i64,
    pub last_used_at: i64,
}

/// A new derived image, uploaded with [`Store::put_derived_blob`].
#[derive(Clone, Debug)]
pub struct NewDerived {
    pub source_blob: String,
    pub params: String,
    pub blob_id: String,
    pub size: u64,
    pub crc32c: u32,
    pub content_type: String,
}

/// One bucket's image transformations in one month.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize)]
pub struct ImageUsage {
    pub bucket_id: String,
    /// `YYYY-MM` (UTC).
    pub month: String,
    /// Unique source images transformed (each counts once however many variants were made).
    pub origin_images: u64,
    /// Derived images made (cache misses) for them: what the transformations cost us.
    pub variants: u64,
}

/// `YYYY-MM` of a Unix ms time.
pub fn month_of(ms: i64) -> String {
    ids::day_of(ms)[..7].to_string()
}

const COLS: &str = "source_blob, params, blob_id, size, content_type, created_at, last_used_at";

fn row(r: &rusqlite::Row) -> rusqlite::Result<Derived> {
    Ok(Derived {
        source_blob: r.get(0)?,
        params: r.get(1)?,
        blob_id: r.get(2)?,
        size: r.get::<_, i64>(3)?.max(0) as u64,
        content_type: r.get(4)?,
        created_at: r.get(5)?,
        last_used_at: r.get(6)?,
    })
}

/// Drop every derived image of `source` (the sweeper, in the transaction deleting the source's
/// row): their blobs lose their reference and wait out the quarantine.
pub(crate) fn forget_source(c: &Connection, source: &str, now: i64) -> Result<u64> {
    let blobs: Vec<String> = c.prepare_cached("SELECT blob_id FROM derived WHERE source_blob = ?1")?.query_map([source], |r| r.get(0))?.collect::<rusqlite::Result<_>>()?;
    for b in &blobs {
        c.prepare_cached("DELETE FROM derived WHERE blob_id = ?1")?.execute([b])?;
        rows::add_ref(c, b, -1, now)?;
    }
    Ok(blobs.len() as u64)
}

impl Store {
    /// The cached image of `source` transformed as `params`, if there is one. Marks it used
    /// (relaxed, at most daily).
    pub fn derived(&self, source: &str, params: &str) -> Result<Option<Derived>> {
        let d = self.read(|c| Ok(c.prepare_cached(&format!("SELECT {COLS} FROM derived WHERE source_blob = ?1 AND params = ?2"))?.query_row([source, params], row).optional()?))?;
        if let Some(d) = &d {
            let now = self.now();
            if now - d.last_used_at >= DAY_MS
                && let Err(e) = self.write_relaxed(|c| Ok(c.prepare_cached("UPDATE derived SET last_used_at = ?3 WHERE source_blob = ?1 AND params = ?2")?.execute(params![source, params, now])?))
            {
                tracing::debug!(error = %e, "marking a derived image used failed");
            }
        }
        Ok(d)
    }

    /// Every derived image of `source`.
    pub fn derived_of(&self, source: &str) -> Result<Vec<Derived>> {
        self.read(|c| Ok(c.prepare_cached(&format!("SELECT {COLS} FROM derived WHERE source_blob = ?1 ORDER BY params"))?.query_map([source], row)?.collect::<rusqlite::Result<_>>()?))
    }

    /// The encryption a derived image of `source` gets: the source's SSE-KMS, or none. An SSE-C
    /// source can't be transformed.
    pub fn derived_encryption(&self, source: &str) -> Result<Option<BlobSse>> {
        match self.blob_encryption(source)? {
            None => Ok(None),
            Some(Encryption::Kms(p)) => Ok(Some(BlobSse::Kms(p))),
            Some(Encryption::Customer { .. }) => Err(StoreError::InvalidRequest("objects encrypted with a customer-provided key (SSE-C) can't be transformed".into())),
        }
    }

    /// Upload a derived image's bytes as the blob `blob` (from [`Self::new_blob`]); record it
    /// with [`Self::commit_derived`]. `bucket_id` only goes in the blob's last-resort
    /// identification metadata.
    #[allow(clippy::too_many_arguments)]
    pub async fn put_derived_blob(&self, blob: &str, bucket_id: &str, source: &str, params: &str, bytes: bytes::Bytes, content_type: &str, sse: Option<&BlobSse>) -> Result<crate::backend::Uploaded> {
        let len = bytes.len() as u64;
        self.put_blob_sse(blob, bucket_id, &derived_label(source, params), crate::backend::bytes_body(bytes), len, Some(content_type), sse).await
    }

    /// Record an uploaded derived image (durable: it adds a blob). If another request recorded
    /// the same image first, that one is returned and `d`'s blob is left uncommitted (the caller
    /// discards it). Fails with `NotFound` if the source isn't referenced any more.
    pub async fn commit_derived(&self, d: &NewDerived, sse: Option<&BlobSse>) -> Result<(Derived, bool)> {
        let now = self.now();
        let enc = sse.map(Encryption::of);
        let out = self
            .write(|c| {
                let refs: Option<i64> = c.prepare_cached("SELECT refs FROM blobs WHERE id = ?1")?.query_row([&d.source_blob], |r| r.get(0)).optional()?;
                if !refs.is_some_and(|n| n > 0) {
                    return Err(StoreError::NotFound(format!("blob {} isn't referenced", d.source_blob)));
                }
                if let Some(existing) = c.prepare_cached(&format!("SELECT {COLS} FROM derived WHERE source_blob = ?1 AND params = ?2"))?.query_row([&d.source_blob, &d.params], row).optional()? {
                    return Ok((existing, false));
                }
                rows::insert_blob(c, &d.blob_id, d.size, d.crc32c, now)?;
                if let Some(e) = &enc {
                    crate::sse::insert(c, &d.blob_id, e, now)?;
                }
                c.prepare_cached(&format!("INSERT INTO derived ({COLS}) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?6)"))?.execute(params![d.source_blob, d.params, d.blob_id, d.size as i64, d.content_type, now])?;
                Ok((
                    Derived { source_blob: d.source_blob.clone(), params: d.params.clone(), blob_id: d.blob_id.clone(), size: d.size, content_type: d.content_type.clone(), created_at: now, last_used_at: now },
                    true,
                ))
            })
            .await;
        self.release_blob(&d.blob_id);
        out
    }

    /// A derived image's bytes.
    pub async fn open_derived(&self, d: &Derived) -> Result<(u64, ByteStream)> {
        let key = crate::sse::key_for(self.blob_encryption(&d.blob_id)?.as_ref(), None, "the derived image")?;
        match self.backend.get_blob(&ids::blob_key(&d.blob_id), None, key.as_ref()).await? {
            Some(x) => Ok(x),
            None => {
                tracing::error!(blob = d.blob_id, source = d.source_blob, "derived image missing from object storage");
                Err(StoreError::internal("a derived image is missing from object storage"))
            }
        }
    }

    /// Count `source` as transformed for `bucket_id` this month, and a new variant made of it
    /// if `made` (a cache miss). The first time in a month it's a durable write (it's billed);
    /// later ones only count variants, relaxed. `true` if the source was new this month.
    pub async fn record_transform(&self, bucket_id: &str, source: &str, made: bool) -> Result<bool> {
        let now = self.now();
        let month = month_of(now);
        let seen = self.read(|c| Ok(c.prepare_cached("SELECT 1 FROM image_usage WHERE bucket_id = ?1 AND month = ?2 AND source_blob = ?3")?.exists(params![bucket_id, month, source])?))?;
        if seen {
            if made {
                self.write_relaxed(|c| Ok(c.prepare_cached("UPDATE image_usage SET variants = variants + 1 WHERE bucket_id = ?1 AND month = ?2 AND source_blob = ?3")?.execute(params![bucket_id, month, source])?))?;
            }
            return Ok(false);
        }
        self.write(|c| {
            Ok(c.prepare_cached(
                "INSERT INTO image_usage (bucket_id, month, source_blob, first_at, variants) VALUES (?1, ?2, ?3, ?4, ?5)
                 ON CONFLICT DO UPDATE SET variants = variants + excluded.variants",
            )?
            .execute(params![bucket_id, month, source, now, made as i64])?
                > 0)
        })
        .await
    }

    /// Unique source images transformed per bucket and month, from `since_month` (`YYYY-MM`) on,
    /// of one bucket or all.
    pub fn image_usage(&self, bucket_id: Option<&str>, since_month: &str) -> Result<Vec<ImageUsage>> {
        self.read(|c| {
            Ok(c.prepare_cached(
                "SELECT bucket_id, month, count(*), sum(variants) FROM image_usage WHERE month >= ?1 AND (?2 IS NULL OR bucket_id = ?2) GROUP BY bucket_id, month ORDER BY month, bucket_id",
            )?
            .query_map(params![since_month, bucket_id], |r| {
                Ok(ImageUsage { bucket_id: r.get(0)?, month: r.get(1)?, origin_images: r.get::<_, i64>(2)?.max(0) as u64, variants: r.get::<_, i64>(3)?.max(0) as u64 })
            })?
            .collect::<rusqlite::Result<_>>()?)
        })
    }

    /// Drop derived images unused for `derived_expiry_days` (the sweeper). Their blobs lose their
    /// reference and wait out the quarantine.
    pub(crate) async fn expire_derived(&self) -> Result<u64> {
        let now = self.now();
        let cutoff = now - self.cfg.derived_expiry_days as i64 * DAY_MS;
        let mut total = 0;
        loop {
            let n = self
                .write(|c| {
                    let due: Vec<String> = c.prepare_cached("SELECT blob_id FROM derived WHERE last_used_at < ?1 LIMIT 1000")?.query_map([cutoff], |r| r.get(0))?.collect::<rusqlite::Result<_>>()?;
                    for b in &due {
                        c.prepare_cached("DELETE FROM derived WHERE blob_id = ?1")?.execute([b])?;
                        rows::add_ref(c, b, -1, now)?;
                    }
                    Ok(due.len() as u64)
                })
                .await?;
            total += n;
            if n < 1000 {
                return Ok(total);
            }
        }
    }
}

/// The key a derived blob's last-resort identification metadata names.
pub fn derived_label(source: &str, params: &str) -> String {
    format!(".roll/derived/{source}/{params}")
}
