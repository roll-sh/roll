//! Objects, listing, multipart uploads, tags and the trash.

use crate::backend::{BlobAttrs, BlobSse, ByteStream, UploadedPart};
use crate::error::{Result, StoreError};
use crate::ids;
use crate::model::*;
use crate::rows::{self, Entry, Merger, Row};
use crate::store::{Store, find_bucket};
use crate::validate::{self, bare_etag};
use crate::versioning;
use md5::{Digest, Md5};
use rusqlite::{Connection, OptionalExtension, params};
use std::collections::BTreeMap;
use std::sync::Arc;

/// Whether `etag` satisfies an `If-Match`/`If-None-Match` value (`*` matches anything).
pub(crate) fn etag_matches(cond: &str, etag: &str) -> bool {
    cond.split(',').map(bare_etag).any(|c| c == "*" || c == etag)
}

fn check_condition(cur: Option<&Row>, cond: &WriteCondition, key: &str) -> Result<()> {
    if let Some(m) = &cond.if_match {
        match cur {
            None => return Err(StoreError::NoSuchKey(key.to_string())),
            Some(r) if !etag_matches(m, &r.etag) => return Err(StoreError::PreconditionFailed),
            _ => {}
        }
    }
    if let Some(m) = &cond.if_none_match
        && let Some(r) = cur
        && etag_matches(m, &r.etag)
    {
        return Err(StoreError::PreconditionFailed);
    }
    Ok(())
}

const UPLOAD_COLS: &str = "id, bucket_id, key, blob_id, backend_upload_id, content_type, cache_control, content_disposition, content_encoding, \
     content_language, expires, metadata, tags, checksum_algorithm, checksum_type, created_at, acl, website_redirect, lock_mode, lock_until, \
     legal_hold";

fn upload_row(r: &rusqlite::Row) -> rusqlite::Result<Upload> {
    let json = |i: usize| -> rusqlite::Result<BTreeMap<String, String>> {
        let s: String = r.get(i)?;
        serde_json::from_str(&s).map_err(|e| rusqlite::Error::FromSqlConversionFailure(i, rusqlite::types::Type::Text, Box::new(e)))
    };
    Ok(Upload {
        id: r.get(0)?,
        bucket_id: r.get(1)?,
        key: r.get(2)?,
        blob_id: r.get(3)?,
        backend_upload_id: r.get(4)?,
        headers: ObjectHeaders {
            content_type: r.get(5)?,
            cache_control: r.get(6)?,
            content_disposition: r.get(7)?,
            content_encoding: r.get(8)?,
            content_language: r.get(9)?,
            expires: r.get(10)?,
            metadata: json(11)?,
            tags: json(12)?,
            acl: r.get(16)?,
            website_redirect: r.get(17)?,
        },
        checksum_algorithm: r.get(13)?,
        checksum_type: r.get(14)?,
        created_at: r.get(15)?,
        lock: ObjectLock { mode: r.get(18)?, retain_until: r.get(19)?, legal_hold: r.get::<_, Option<i64>>(20)?.map(|v| v != 0) },
    })
}

fn find_upload(c: &Connection, id: &str) -> Result<Upload> {
    c.prepare_cached(&format!("SELECT {UPLOAD_COLS} FROM uploads WHERE id = ?1"))?
        .query_row([id], upload_row)
        .optional()?
        .ok_or_else(|| StoreError::NoSuchUpload(id.to_string()))
}

fn part_row(r: &rusqlite::Row) -> rusqlite::Result<(Part, u32)> {
    Ok((
        Part {
            part_number: r.get::<_, i64>(0)? as u32,
            size: r.get::<_, i64>(1)? as u64,
            etag: r.get(2)?,
            checksum: r.get(3)?,
            backend_etag: r.get(4)?,
            created_at: r.get(5)?,
        },
        r.get::<_, i64>(6)? as u32,
    ))
}

const PART_COLS: &str = "part_number, size, etag, checksum, backend_etag, created_at, crc32c";

/// MD5 hex of a string (for ETags of parts copied server-side, whose bytes we don't see).
fn md5_hex(data: &[u8]) -> String {
    Md5::digest(data).iter().map(|b| format!("{b:02x}")).collect()
}

/// S3's multipart ETag: MD5 of the parts' binary MD5s, then `-<count>`.
fn multipart_etag(parts: &[Part]) -> String {
    multipart_etag_of(parts.iter().map(|p| p.etag.as_str()))
}

/// The ETag of a multipart object from its parts' ETags (unquoted), in order: S3's
/// `md5(md5s)-<parts>`.
pub fn multipart_etag_of<'a>(etags: impl ExactSizeIterator<Item = &'a str>) -> String {
    let n = etags.len();
    let mut h = Md5::new();
    for etag in etags {
        let bytes: Vec<u8> = (0..etag.len() / 2).filter_map(|i| u8::from_str_radix(etag.get(2 * i..2 * i + 2)?, 16).ok()).collect();
        if bytes.len() == 16 {
            h.update(&bytes);
        } else {
            h.update(etag.as_bytes());
        }
    }
    let digest: String = h.finalize().iter().map(|b| format!("{b:02x}")).collect();
    format!("{digest}-{n}")
}

impl Store {
    // ---- Objects ----

    /// The visible version of `key`.
    pub fn get_object(&self, bucket: &str, key: &str) -> Result<ObjectMeta> {
        self.read(|c| {
            let b = find_bucket(c, bucket)?;
            let chain = rows::chain(c, &b.id)?;
            match rows::resolve(c, &chain, key)? {
                Some((r, _)) => r.meta(),
                None => Err(StoreError::NoSuchKey(key.to_string())),
            }
        })
    }

    /// Commit an uploaded blob as `key`'s new version: the old one goes to the trash. Checks
    /// `cond` atomically. On a definite failure the blob is deleted; on `Unavailable` it's left
    /// to the orphan scan (the write may or may not have reached the log).
    pub async fn commit_put(&self, bucket: &str, key: &str, obj: NewObject, cond: &WriteCondition) -> Result<ObjectMeta> {
        self.commit_put_full(bucket, key, obj, cond, &ObjectLock::default(), None).await
    }

    /// [`Self::commit_put`] with the new version's Object Lock (empty: the bucket's default
    /// retention, if any).
    pub async fn commit_put_with(&self, bucket: &str, key: &str, obj: NewObject, cond: &WriteCondition, lock: &ObjectLock) -> Result<ObjectMeta> {
        self.commit_put_full(bucket, key, obj, cond, lock, None).await
    }

    /// [`Self::commit_put`] of a blob uploaded with server-side encryption `enc`.
    pub async fn commit_put_sse(&self, bucket: &str, key: &str, obj: NewObject, cond: &WriteCondition, enc: Option<&crate::sse::Encryption>) -> Result<ObjectMeta> {
        self.commit_put_full(bucket, key, obj, cond, &ObjectLock::default(), enc).await
    }

    /// [`Self::commit_put`] with both the version's Object Lock and the blob's encryption.
    pub async fn commit_put_full(&self, bucket: &str, key: &str, obj: NewObject, cond: &WriteCondition, lock: &ObjectLock, enc: Option<&crate::sse::Encryption>) -> Result<ObjectMeta> {
        let checked = validate::key(key).and_then(|_| validate::headers(&obj.headers));
        if let Err(e) = checked {
            self.discard_blob(&obj.blob_id).await;
            return Err(e);
        }
        let now = self.now();
        let row = Row {
            key: key.to_string(),
            blob_id: Some(obj.blob_id.clone()),
            size: obj.size,
            etag: obj.etag.clone(),
            headers: obj.headers.clone(),
            checksum: obj.checksum.clone(),
            parts: 0,
            part_sizes: Vec::new(),
            modified_at: now,
            lock: lock.clone(),
            ..Row::tombstone(key, now)
        };
        self.commit_row(bucket, row, obj.crc32c, cond, enc).await
    }

    /// Insert `row` (whose blob is new, encrypted as `enc`) as the key's version.
    pub(crate) async fn commit_row(&self, bucket: &str, row: Row, crc32c: u32, cond: &WriteCondition, enc: Option<&crate::sse::Encryption>) -> Result<ObjectMeta> {
        let blob = row.blob()?.to_string();
        // Whatever happens from here (even the caller's future being dropped), the blob stops
        // being in flight; an uncommitted one is then the orphan scan's.
        let _inflight = self.inflight_guard(&blob);
        let now = row.modified_at;
        self.log.check_open()?;
        let attempt = self.log.db.write(|c| {
            let b = find_bucket(c, bucket)?;
            let chain = rows::chain(c, &b.id)?;
            let cur = rows::resolve(c, &chain, &row.key)?.map(|(r, _)| r);
            check_condition(cur.as_ref(), cond, &row.key)?;
            rows::insert_blob(c, &blob, row.size, crc32c, now)?;
            if let Some(e) = enc {
                crate::sse::insert(c, &blob, e, now)?;
            }
            versioning::put_key(c, &b, &chain, &row.key, row.clone(), now)
        });
        let (stored, batch) = match attempt {
            Ok(x) => x,
            Err(e) => {
                // Rolled back: nothing references the blob.
                self.discard_blob(&blob).await;
                return Err(e);
            }
        };
        self.log.wait(batch).await?;
        stored.meta()
    }

    /// Server-side copy: the destination references the source's blob (no bytes move).
    pub async fn copy_object(&self, src_bucket: &str, src_key: &str, dst_bucket: &str, dst_key: &str, opts: &CopyOptions) -> Result<ObjectMeta> {
        validate::key(dst_key)?;
        if let Some(h) = &opts.headers {
            validate::headers(h)?;
        }
        if let Some(t) = &opts.tags {
            validate::tags(t)?;
        }
        let now = self.now();
        self.write(|c| {
            let sb = find_bucket(c, src_bucket)?;
            let schain = rows::chain(c, &sb.id)?;
            let src = match versioning::lookup(c, &schain, src_key, opts.source_version.as_deref()) {
                Ok((r, _)) => r,
                Err(StoreError::DeleteMarker { specific: true, .. }) => {
                    return Err(StoreError::InvalidRequest("The source of a copy request may not specifically refer to a delete marker by version id.".into()));
                }
                Err(StoreError::DeleteMarker { .. }) => return Err(StoreError::NoSuchKey(src_key.to_string())),
                Err(e) => return Err(e),
            };
            if let Some(m) = &opts.source_etag
                && !etag_matches(m, &src.etag)
            {
                return Err(StoreError::PreconditionFailed);
            }
            let db = find_bucket(c, dst_bucket)?;
            let dchain = rows::chain(c, &db.id)?;
            let cur = rows::resolve(c, &dchain, dst_key)?.map(|(r, _)| r);
            check_condition(cur.as_ref(), &opts.condition, dst_key)?;
            let mut row = src.clone();
            row.key = dst_key.to_string();
            row.modified_at = now;
            if let Some(h) = &opts.headers {
                let tags = std::mem::take(&mut row.headers.tags);
                row.headers = h.clone();
                row.headers.tags = tags;
            }
            if let Some(t) = &opts.tags {
                row.headers.tags = t.clone();
            }
            // A copy is a new version: its own lock (or the bucket's default), never the source's.
            row.lock = opts.lock.clone();
            rows::add_ref(c, src.blob()?, 1, now)?;
            versioning::put_key(c, &db, &dchain, dst_key, row, now)?.meta()
        })
        .await
    }

    /// Delete `key` (to the trash, or a delete marker in a versioned bucket). `Ok(false)` if
    /// there was nothing to delete. `if_match`: only if the current version has that ETag.
    pub async fn delete_object(&self, bucket: &str, key: &str, if_match: Option<&str>) -> Result<bool> {
        Ok(self.delete_object_version(bucket, key, None, if_match, false).await?.existed)
    }

    /// Delete many keys in one durable write. Each result says whether the key existed.
    pub async fn delete_objects(&self, bucket: &str, keys: &[String]) -> Result<Vec<(String, Result<bool>)>> {
        let items: Vec<(String, Option<String>)> = keys.iter().map(|k| (k.clone(), None)).collect();
        let results = self.delete_versions(bucket, &items, false).await?;
        Ok(keys.iter().cloned().zip(results.into_iter().map(|r| r.map(|o| o.existed))).collect())
    }

    /// Lifecycle expiry: delete (to the trash, or behind a delete marker in a versioned
    /// bucket) each `(key, etag)` that is still that version and was written before `cutoff`,
    /// checked in the transaction, so a version written after the listing is never expired by
    /// it. With a `rule`, its filter is checked again too. Versions under Object Lock that
    /// would leave for good are skipped. Returns how many were deleted.
    #[cfg(test)]
    pub(crate) async fn expire_objects(&self, bucket: &str, keys: &[(String, String)], cutoff: i64) -> Result<u64> {
        self.expire_current(bucket, keys, cutoff, None).await
    }

    pub(crate) async fn expire_current(&self, bucket: &str, keys: &[(String, String)], cutoff: i64, rule: Option<&LifecycleRule>) -> Result<u64> {
        let now = self.now();
        self.write(|c| {
            let b = find_bucket(c, bucket)?;
            let chain = rows::chain(c, &b.id)?;
            let mut n = 0;
            for (key, etag) in keys {
                let Some((r, _)) = rows::resolve(c, &chain, key)? else { continue };
                if r.etag != *etag || r.modified_at >= cutoff || rule.is_some_and(|rule| !rule.matches(key, &r.headers.tags, r.size)) {
                    continue;
                }
                // A locked version that would leave for good is kept (refused before anything
                // changes, so the rest of the batch goes on).
                match versioning::delete_key(c, &b, &chain, key, now) {
                    Ok(o) if o.existed => n += 1,
                    Ok(_) | Err(StoreError::ObjectLocked(_)) => {}
                    Err(e) => return Err(e),
                }
            }
            Ok(n)
        })
        .await
    }

    /// Delete every visible key under `prefix` (to the trash). Returns how many.
    pub async fn delete_prefix(&self, bucket: &str, prefix: &str) -> Result<u64> {
        let mut n = 0;
        loop {
            let page = self.list_objects(bucket, &ListQuery { prefix: prefix.to_string(), max_keys: 1000, ..Default::default() })?;
            if page.objects.is_empty() {
                return Ok(n);
            }
            let keys: Vec<String> = page.objects.into_iter().map(|o| o.key).collect();
            for (_, r) in self.delete_objects(bucket, &keys).await? {
                if r? {
                    n += 1;
                }
            }
        }
    }

    /// One page of the bucket's view (S3's ListObjects semantics).
    pub fn list_objects(&self, bucket: &str, q: &ListQuery) -> Result<ListPage> {
        self.read(|c| {
            let b = find_bucket(c, bucket)?;
            let chain = rows::chain(c, &b.id)?;
            let mut m = Merger::new(c, &chain, &q.prefix, q.delimiter.as_deref(), q.start_after.as_deref());
            let mut page = ListPage::default();
            let mut n = 0;
            while n < q.max_keys {
                match m.next_entry()? {
                    Some(Entry::Object(r)) => {
                        page.next = Some(r.key.clone());
                        page.objects.push(r.meta()?);
                    }
                    Some(Entry::Prefix(p)) => {
                        page.next = Some(p.clone());
                        page.prefixes.push(p);
                    }
                    None => return Ok(page),
                }
                n += 1;
            }
            page.truncated = m.next_entry()?.is_some();
            if !page.truncated {
                page.next = None;
            }
            Ok(page)
        })
    }

    // ---- Tags ----

    pub fn get_tags(&self, bucket: &str, key: &str) -> Result<BTreeMap<String, String>> {
        Ok(self.get_object(bucket, key)?.headers.tags)
    }

    /// Replace an object's tags (not a new version: Last-Modified stays).
    pub async fn put_tags(&self, bucket: &str, key: &str, tags: BTreeMap<String, String>) -> Result<()> {
        self.put_version_tags(bucket, key, None, tags).await.map(|_| ())
    }

    // ---- Multipart ----

    /// Start a multipart upload (its blob id and backend upload are recorded durably first).
    pub async fn create_upload(&self, bucket: &str, key: &str, headers: ObjectHeaders, checksum_algorithm: Option<String>, checksum_type: Option<String>) -> Result<Upload> {
        self.create_upload_full(bucket, key, headers, checksum_algorithm, checksum_type, ObjectLock::default(), None).await
    }

    /// [`Self::create_upload`] with the object's Object Lock (applied when it completes).
    pub async fn create_upload_with(&self, bucket: &str, key: &str, headers: ObjectHeaders, checksum_algorithm: Option<String>, checksum_type: Option<String>, lock: ObjectLock) -> Result<Upload> {
        self.create_upload_full(bucket, key, headers, checksum_algorithm, checksum_type, lock, None).await
    }

    /// [`Self::create_upload`] with server-side encryption (recorded with the upload).
    pub async fn create_upload_sse(&self, bucket: &str, key: &str, headers: ObjectHeaders, checksum_algorithm: Option<String>, checksum_type: Option<String>, sse: Option<&BlobSse>) -> Result<Upload> {
        self.create_upload_full(bucket, key, headers, checksum_algorithm, checksum_type, ObjectLock::default(), sse).await
    }

    /// [`Self::create_upload`] with both the object's Object Lock and its encryption.
    #[allow(clippy::too_many_arguments)]
    pub async fn create_upload_full(
        &self,
        bucket: &str,
        key: &str,
        headers: ObjectHeaders,
        checksum_algorithm: Option<String>,
        checksum_type: Option<String>,
        lock: ObjectLock,
        sse: Option<&BlobSse>,
    ) -> Result<Upload> {
        validate::key(key)?;
        validate::headers(&headers)?;
        self.check_sse_supported(sse)?;
        let b = self.read(|c| find_bucket(c, bucket))?;
        if b.object_lock.is_none() && !lock.is_empty() {
            return Err(StoreError::InvalidRequest("Bucket is missing Object Lock Configuration".into()));
        }
        let now = self.now();
        let blob = ids::blob_id();
        let enc = sse.map(crate::sse::Encryption::of);
        let attrs = BlobAttrs { bucket_id: b.id.clone(), key: key.to_string(), content_type: headers.content_type.clone(), sse: sse.cloned() };
        let backend_id = self.backend.create_multipart(&ids::blob_key(&blob), &attrs).await?;
        let up = Upload {
            id: ids::upload_id(now),
            bucket_id: b.id.clone(),
            key: key.to_string(),
            blob_id: blob.clone(),
            backend_upload_id: backend_id.clone(),
            headers,
            checksum_algorithm,
            checksum_type,
            created_at: now,
            lock,
        };
        let u = up.clone();
        let r = self
            .write(move |c| {
                find_bucket(c, bucket)?;
                c.execute(
                    &format!("INSERT INTO uploads ({UPLOAD_COLS}) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?17, ?18, ?19, ?20, ?21)"),
                    params![
                        u.id,
                        u.bucket_id,
                        u.key,
                        u.blob_id,
                        u.backend_upload_id,
                        u.headers.content_type,
                        u.headers.cache_control,
                        u.headers.content_disposition,
                        u.headers.content_encoding,
                        u.headers.content_language,
                        u.headers.expires,
                        rows::json(&u.headers.metadata)?,
                        rows::json(&u.headers.tags)?,
                        u.checksum_algorithm,
                        u.checksum_type,
                        u.created_at,
                        u.headers.acl,
                        u.headers.website_redirect,
                        u.lock.mode,
                        u.lock.retain_until,
                        u.lock.legal_hold.map(i64::from)
                    ],
                )?;
                if let Some(e) = &enc {
                    crate::sse::insert(c, &u.blob_id, e, now)?;
                }
                Ok(())
            })
            .await;
        if let Err(e) = r {
            if !matches!(e, StoreError::Unavailable(_)) {
                let _ = self.backend.abort_multipart(&ids::blob_key(&blob), &backend_id).await;
            }
            return Err(e);
        }
        Ok(up)
    }

    /// An upload in `bucket`.
    pub fn upload(&self, bucket: &str, upload_id: &str) -> Result<Upload> {
        self.read(|c| {
            let b = find_bucket(c, bucket)?;
            let u = find_upload(c, upload_id)?;
            if u.bucket_id != b.id {
                return Err(StoreError::NoSuchUpload(upload_id.to_string()));
            }
            Ok(u)
        })
    }

    /// Upload a part's bytes to the backend; commit it with [`Self::commit_part`].
    pub async fn put_part_data(&self, upload: &Upload, part: u32, body: ByteStream, len: u64) -> Result<UploadedPart> {
        self.put_part_data_sse(upload, part, body, len, None).await
    }

    /// [`Self::put_part_data`] with the request's SSE-C key (an SSE-C upload's parts need it).
    pub async fn put_part_data_sse(&self, upload: &Upload, part: u32, body: ByteStream, len: u64, sse: Option<&BlobSse>) -> Result<UploadedPart> {
        validate::part_number(part)?;
        let key = self.upload_key(upload, sse)?;
        Ok(self.backend.upload_part(&ids::blob_key(&upload.blob_id), &upload.backend_upload_id, part, body, len, key.as_ref()).await?)
    }

    /// Record an uploaded part. `etag` is the part's MD5 hex (S3's part ETag), `checksum` its
    /// `x-amz-checksum-*` value if the client sent one.
    pub async fn commit_part(&self, upload: &Upload, part: u32, data: &UploadedPart, etag: &str, checksum: Option<String>) -> Result<Part> {
        validate::part_number(part)?;
        let now = self.now();
        let p = Part { part_number: part, size: data.size, etag: etag.to_string(), checksum, backend_etag: data.etag.clone(), created_at: now };
        let q = p.clone();
        let crc = data.crc32c;
        self.write(move |c| {
            find_upload(c, &upload.id)?;
            c.execute(
                &format!("INSERT INTO parts (upload_id, {PART_COLS}) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8) ON CONFLICT (upload_id, part_number) DO UPDATE SET
                  size = excluded.size, etag = excluded.etag, checksum = excluded.checksum, backend_etag = excluded.backend_etag,
                  created_at = excluded.created_at, crc32c = excluded.crc32c"),
                params![upload.id, q.part_number as i64, q.size as i64, q.etag, q.checksum, q.backend_etag, q.created_at, crc as i64],
            )?;
            Ok(())
        })
        .await?;
        Ok(p)
    }

    /// `UploadPartCopy`: a part from (a range of) another object, copied inside object storage.
    /// The part's ETag is the source's when it's a whole single-PUT object, else an MD5 of
    /// where the bytes came from (we never see them).
    pub async fn copy_part(&self, upload: &Upload, part: u32, src_bucket: &str, src_key: &str, range: Option<std::ops::Range<u64>>, source_etag: Option<&str>) -> Result<Part> {
        self.copy_part_full(upload, part, src_bucket, src_key, None, range, source_etag, None, None).await
    }

    /// [`Self::copy_part`] from a version of the source (`None`: its current one).
    #[allow(clippy::too_many_arguments)]
    pub async fn copy_part_version(
        &self,
        upload: &Upload,
        part: u32,
        src_bucket: &str,
        src_key: &str,
        src_version: Option<&str>,
        range: Option<std::ops::Range<u64>>,
        source_etag: Option<&str>,
    ) -> Result<Part> {
        self.copy_part_full(upload, part, src_bucket, src_key, src_version, range, source_etag, None, None).await
    }

    /// [`Self::copy_part`] with the source's SSE-C key (`src_sse`) and the upload's (`sse`).
    #[allow(clippy::too_many_arguments)]
    pub async fn copy_part_sse(&self, upload: &Upload, part: u32, src_bucket: &str, src_key: &str, range: Option<std::ops::Range<u64>>, source_etag: Option<&str>, src_sse: Option<&BlobSse>, sse: Option<&BlobSse>) -> Result<Part> {
        self.copy_part_full(upload, part, src_bucket, src_key, None, range, source_etag, src_sse, sse).await
    }

    /// [`Self::copy_part`] from a version of the source, with both sides' SSE-C keys.
    #[allow(clippy::too_many_arguments)]
    pub async fn copy_part_full(
        &self,
        upload: &Upload,
        part: u32,
        src_bucket: &str,
        src_key: &str,
        src_version: Option<&str>,
        range: Option<std::ops::Range<u64>>,
        source_etag: Option<&str>,
        src_sse: Option<&BlobSse>,
        sse: Option<&BlobSse>,
    ) -> Result<Part> {
        validate::part_number(part)?;
        let src = match self.get_object_version(src_bucket, src_key, src_version) {
            Err(StoreError::DeleteMarker { specific: true, .. }) => {
                return Err(StoreError::InvalidRequest("The source of a copy request may not specifically refer to a delete marker by version id.".into()));
            }
            Err(StoreError::DeleteMarker { .. }) => return Err(StoreError::NoSuchKey(src_key.to_string())),
            r => r?,
        };
        if let Some(m) = source_etag
            && !etag_matches(m, &src.etag)
        {
            return Err(StoreError::PreconditionFailed);
        }
        if let Some(r) = &range
            && (r.start >= r.end || r.end > src.size)
        {
            return Err(StoreError::InvalidArgument("copy range outside the source object".into()));
        }
        let data = self.backend_part_copy(upload, part, &src, range.clone(), src_sse, sse).await?;
        let whole = range.as_ref().is_none_or(|r| r.start == 0 && r.end == src.size);
        let etag = if whole && src.parts == 0 { src.etag.clone() } else { md5_hex(format!("{}:{:?}", src.blob_id, range).as_bytes()) };
        self.commit_part(upload, part, &data, &etag, None).await
    }

    /// Parts after `marker`, up to `max`, and whether there are more.
    pub fn list_parts(&self, bucket: &str, upload_id: &str, marker: u32, max: usize) -> Result<(Vec<Part>, bool)> {
        self.upload(bucket, upload_id)?;
        self.read(|c| {
            let mut parts = c
                .prepare(&format!("SELECT {PART_COLS} FROM parts WHERE upload_id = ?1 AND part_number > ?2 ORDER BY part_number LIMIT ?3"))?
                .query_map(params![upload_id, marker as i64, max as i64 + 1], part_row)?
                .map(|r| r.map(|(p, _)| p))
                .collect::<rusqlite::Result<Vec<_>>>()?;
            let more = parts.len() > max;
            parts.truncate(max);
            Ok((parts, more))
        })
    }

    /// In-progress uploads of a bucket (S3's ListMultipartUploads), by key then age.
    pub fn list_uploads(&self, bucket: &str, q: &UploadsQuery) -> Result<UploadsPage> {
        self.read(|c| {
            let b = find_bucket(c, bucket)?;
            let mut st = c.prepare(&format!("SELECT {UPLOAD_COLS} FROM uploads WHERE bucket_id = ?1 AND key >= ?2 ORDER BY key, id"))?;
            let rows = st.query_map(params![b.id, q.prefix], upload_row)?;
            let mut page = UploadsPage::default();
            let mut n = 0;
            let mut skip_prefix: Option<String> = None;
            let delimiter = q.delimiter.as_deref().filter(|d| !d.is_empty());
            for r in rows {
                let u = r?;
                if !u.key.starts_with(&q.prefix) {
                    break;
                }
                if let Some(km) = &q.key_marker {
                    let after = match &q.upload_id_marker {
                        Some(um) => u.key > *km || (u.key == *km && u.id > *um),
                        None => u.key > *km,
                    };
                    if !after {
                        continue;
                    }
                }
                if let Some(sp) = &skip_prefix
                    && u.key.starts_with(sp.as_str())
                {
                    continue;
                }
                if n == q.max_uploads {
                    page.truncated = true;
                    break;
                }
                if let Some(cp) = rows::common_prefix(&u.key, &q.prefix, delimiter) {
                    let cp = cp.to_string();
                    page.next_key_marker = Some(cp.clone());
                    page.next_upload_id_marker = None;
                    skip_prefix = Some(cp.clone());
                    page.prefixes.push(cp);
                } else {
                    page.next_key_marker = Some(u.key.clone());
                    page.next_upload_id_marker = Some(u.id.clone());
                    page.uploads.push(u);
                }
                n += 1;
            }
            if !page.truncated {
                page.next_key_marker = None;
                page.next_upload_id_marker = None;
            }
            Ok(page)
        })
    }

    /// Complete an upload: validate the part list, assemble the parts in object storage, then
    /// commit the object. `checksum`: the whole object's checksum, if the caller computed one.
    ///
    /// Completes of one upload run one at a time. A retry that finds the upload already
    /// completed into the current version of its key gets that version back (S3 SDKs retry a
    /// slow Complete), and a failed attempt never deletes a blob the metadata knows.
    pub async fn complete_upload(&self, bucket: &str, upload_id: &str, parts: &[(u32, String)], checksum: Option<Checksum>, cond: &WriteCondition) -> Result<ObjectMeta> {
        self.complete_upload_sse(bucket, upload_id, parts, checksum, cond, None).await
    }

    /// [`Self::complete_upload`] with the request's SSE-C key (S3 needs it to complete an SSE-C
    /// upload whose parts carry checksums, as the backend's always do).
    pub async fn complete_upload_sse(&self, bucket: &str, upload_id: &str, parts: &[(u32, String)], checksum: Option<Checksum>, cond: &WriteCondition, sse: Option<&BlobSse>) -> Result<ObjectMeta> {
        let up = self.upload(bucket, upload_id)?;
        let key = self.upload_key(&up, sse)?;
        let lock = self.completing.lock().unwrap().entry(upload_id.to_string()).or_default().clone();
        let result = {
            let _serial = lock.lock().await;
            self.complete_locked(bucket, &up, parts, checksum, cond, key.as_ref()).await
        };
        {
            let mut map = self.completing.lock().unwrap();
            // Two references: the map's and ours. Nobody else is waiting.
            if Arc::strong_count(&lock) == 2 {
                map.remove(upload_id);
            }
        }
        result
    }

    async fn complete_locked(&self, bucket: &str, up: &Upload, parts: &[(u32, String)], checksum: Option<Checksum>, cond: &WriteCondition, sse: Option<&BlobSse>) -> Result<ObjectMeta> {
        let upload_id = up.id.as_str();
        // An earlier attempt (this one's retry target) may have finished while this one waited.
        match self.read(|c| find_upload(c, upload_id)) {
            Ok(_) => {}
            Err(StoreError::NoSuchUpload(_)) => return self.completed_already(bucket, up),
            Err(e) => return Err(e),
        }
        if parts.is_empty() {
            return Err(StoreError::InvalidArgument("a multipart upload needs at least one part".into()));
        }
        if parts.windows(2).any(|w| w[0].0 >= w[1].0) {
            return Err(StoreError::InvalidPartOrder);
        }
        let stored: BTreeMap<u32, (Part, u32)> = self.read(|c| {
            Ok(c.prepare(&format!("SELECT {PART_COLS} FROM parts WHERE upload_id = ?1"))?
                .query_map([upload_id], part_row)?
                .collect::<rusqlite::Result<Vec<_>>>()?
                .into_iter()
                .map(|(p, crc)| (p.part_number, (p, crc)))
                .collect())
        })?;
        let mut chosen = Vec::with_capacity(parts.len());
        for (i, (n, etag)) in parts.iter().enumerate() {
            let Some((p, crc)) = stored.get(n) else { return Err(StoreError::InvalidPart(*n)) };
            if bare_etag(etag) != p.etag {
                return Err(StoreError::InvalidPart(*n));
            }
            if i + 1 < parts.len() && p.size < self.cfg.min_part_size {
                return Err(StoreError::EntityTooSmall(*n));
            }
            chosen.push((p.clone(), *crc));
        }
        // Fail early on the condition (it's checked again atomically below).
        let current = self.get_object(bucket, &up.key).ok().map(|m| m.etag);
        {
            let cur = current.map(|etag| Row { etag, ..Row::tombstone(&up.key, 0) });
            check_condition(cur.as_ref(), cond, &up.key)?;
        }
        let size: u64 = chosen.iter().map(|(p, _)| p.size).sum();
        let mut crc = 0u32;
        for (p, c) in &chosen {
            crc = crc32c::crc32c_combine(crc, *c, p.size as usize);
        }
        let part_list: Vec<Part> = chosen.iter().map(|(p, _)| p.clone()).collect();
        let etag = multipart_etag(&part_list);
        let _inflight = self.inflight_guard(&up.blob_id);
        let backend_parts: Vec<(u32, String)> = part_list.iter().map(|p| (p.part_number, p.backend_etag.clone())).collect();
        self.backend.complete_multipart(&ids::blob_key(&up.blob_id), &up.backend_upload_id, &backend_parts, sse).await?;
        let now = self.now();
        let row = Row {
            key: up.key.clone(),
            blob_id: Some(up.blob_id.clone()),
            size,
            etag,
            headers: up.headers.clone(),
            checksum,
            parts: part_list.len() as u32,
            part_sizes: part_list.iter().map(|p| p.size).collect(),
            modified_at: now,
            lock: up.lock.clone(),
            ..Row::tombstone(&up.key, now)
        };
        self.log.check_open()?;
        let attempt = self.log.db.write(|c| {
            let b = find_bucket(c, bucket)?;
            find_upload(c, upload_id)?;
            let chain = rows::chain(c, &b.id)?;
            let cur = rows::resolve(c, &chain, &row.key)?.map(|(r, _)| r);
            check_condition(cur.as_ref(), cond, &row.key)?;
            rows::insert_blob(c, &up.blob_id, size, crc, now)?;
            let stored = versioning::put_key(c, &b, &chain, &row.key, row.clone(), now)?;
            c.execute("DELETE FROM parts WHERE upload_id = ?1", [upload_id])?;
            c.execute("DELETE FROM uploads WHERE id = ?1", [upload_id])?;
            Ok(stored)
        });
        let (stored, batch) = match attempt {
            Ok(x) => x,
            Err(StoreError::NoSuchUpload(_)) => return self.completed_already(bucket, up),
            Err(e) => {
                // Rolled back. If the metadata knows the blob after all, something else committed
                // it: never delete it.
                if self.blob_committed(&up.blob_id)? {
                    return Err(e);
                }
                // The backend upload is complete, so the upload can't be retried: drop it.
                let dropped = self
                    .write(|c| {
                        c.execute("DELETE FROM parts WHERE upload_id = ?1", [upload_id])?;
                        c.execute("DELETE FROM uploads WHERE id = ?1", [upload_id])?;
                        crate::sse::forget_if_unused(c, &up.blob_id)?;
                        Ok(())
                    })
                    .await;
                // Only once the upload is durably gone: if dropping it didn't commit (a deposed
                // node), the upload still references the blob, and a retry of the Complete on
                // the new writer would commit an object whose bytes were deleted.
                if dropped.is_ok() && !self.blob_committed(&up.blob_id)? {
                    self.discard_blob(&up.blob_id).await;
                }
                return Err(e);
            }
        };
        self.log.wait(batch).await?;
        stored.meta()
    }

    /// The upload is gone: if it became the key's current version, that's a completed retry.
    fn completed_already(&self, bucket: &str, up: &Upload) -> Result<ObjectMeta> {
        match self.get_object(bucket, &up.key) {
            Ok(m) if m.blob_id == up.blob_id => Ok(m),
            Ok(_) | Err(StoreError::NoSuchKey(_)) => Err(StoreError::NoSuchUpload(up.id.clone())),
            Err(e) => Err(e),
        }
    }

    /// Abort an upload: forget it durably, then abort it in object storage.
    pub async fn abort_upload(&self, upload_id: &str) -> Result<()> {
        let up = self
            .write(|c| {
                let u = find_upload(c, upload_id)?;
                c.execute("DELETE FROM parts WHERE upload_id = ?1", [upload_id])?;
                c.execute("DELETE FROM uploads WHERE id = ?1", [upload_id])?;
                crate::sse::forget_if_unused(c, &u.blob_id)?;
                Ok(u)
            })
            .await?;
        if let Err(e) = self.backend.abort_multipart(&ids::blob_key(&up.blob_id), &up.backend_upload_id).await {
            tracing::warn!(error = %e, upload = upload_id, "aborting a backend upload failed; the backing bucket's lifecycle rule cleans it up");
        }
        Ok(())
    }

    /// Abort an upload in `bucket` (S3's AbortMultipartUpload).
    pub async fn abort_bucket_upload(&self, bucket: &str, upload_id: &str) -> Result<()> {
        self.upload(bucket, upload_id)?;
        self.abort_upload(upload_id).await
    }

    // ---- Trash ----

    /// Deleted and overwritten versions still restorable, by key then newest first. `after`:
    /// the `(key, deleted_at)` of the previous page's last entry.
    pub fn list_trash(&self, bucket: &str, prefix: &str, after: Option<(&str, i64)>, limit: usize) -> Result<(Vec<TrashEntry>, bool)> {
        self.read(|c| {
            let b = find_bucket(c, bucket)?;
            let keep = b.recover_days as i64 * 86_400_000;
            let (ak, ad) = after.map_or((prefix.to_string(), i64::MAX), |(k, d)| (k.to_string(), d));
            let mut st = c.prepare(
                "SELECT key, size, etag, deleted_at, vid, is_null, lock_mode, lock_until, legal_hold FROM trash
                 WHERE bucket_id = ?1 AND (key > ?2 OR (key = ?2 AND deleted_at < ?3)) AND key >= ?4
                 ORDER BY key, deleted_at DESC LIMIT ?5",
            )?;
            let mut out = st
                .query_map(params![b.id, ak, ad, prefix, limit as i64 + 1], |r| {
                    let deleted_at: i64 = r.get(3)?;
                    let version_id = if r.get::<_, i64>(5)? != 0 { "null".to_string() } else { r.get(4)? };
                    let lock = ObjectLock { mode: r.get(6)?, retain_until: r.get(7)?, legal_hold: r.get::<_, Option<i64>>(8)?.map(|v| v != 0) };
                    // A version under Object Lock outlives the recovery window until its lock ends.
                    let held = if lock.mode.is_some() { lock.retain_until.unwrap_or(0) } else { 0 };
                    Ok(TrashEntry { key: r.get(0)?, size: r.get::<_, i64>(1)? as u64, etag: r.get(2)?, deleted_at, expires_at: (deleted_at + keep).max(held), version_id })
                })?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            out.retain(|e| e.key.starts_with(prefix));
            let more = out.len() > limit;
            out.truncate(limit);
            Ok((out, more))
        })
    }

    /// Bring a version back from the trash. In an unversioned bucket it becomes the current one
    /// (the current one, if any, goes to the trash in its place); in a versioned one it goes back
    /// to its place in the key's history (see `versioning::restore`).
    pub async fn restore(&self, bucket: &str, key: &str, deleted_at: i64) -> Result<ObjectMeta> {
        let now = self.now();
        self.write(|c| {
            let b = find_bucket(c, bucket)?;
            let Some(row) = rows::trash_row(c, &b.id, key, deleted_at)? else {
                return Err(StoreError::NotFound(format!("{key} deleted at {deleted_at} isn't in the trash")));
            };
            // The trash row's reference moves to the restored version.
            rows::trash_delete(c, &b.id, key, deleted_at, row.size)?;
            let chain = rows::chain(c, &b.id)?;
            versioning::restore(c, &b, &chain, key, row, now)?.meta()
        })
        .await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::StoreConfig;

    #[test]
    fn etag_conditions() {
        assert!(etag_matches("*", "abc"));
        assert!(etag_matches("\"abc\"", "abc"));
        assert!(etag_matches("\"x\", W/\"abc\"", "abc"));
        assert!(!etag_matches("\"abcd\"", "abc"));
        let cur = Row { etag: "e1".into(), ..Row::tombstone("k", 0) };
        let cond = |m: Option<&str>, n: Option<&str>| WriteCondition { if_match: m.map(str::to_string), if_none_match: n.map(str::to_string) };
        assert!(check_condition(None, &cond(None, None), "k").is_ok());
        assert!(check_condition(None, &cond(None, Some("*")), "k").is_ok());
        assert!(matches!(check_condition(None, &cond(Some("*"), None), "k"), Err(StoreError::NoSuchKey(_))));
        assert!(matches!(check_condition(Some(&cur), &cond(None, Some("*")), "k"), Err(StoreError::PreconditionFailed)));
        assert!(matches!(check_condition(Some(&cur), &cond(None, Some("\"e1\"")), "k"), Err(StoreError::PreconditionFailed)));
        assert!(check_condition(Some(&cur), &cond(None, Some("\"e2\"")), "k").is_ok());
        assert!(check_condition(Some(&cur), &cond(Some("e1"), None), "k").is_ok());
        assert!(matches!(check_condition(Some(&cur), &cond(Some("e2"), None), "k"), Err(StoreError::PreconditionFailed)));
        assert!(matches!(check_condition(Some(&cur), &cond(Some("e1"), Some("e1")), "k"), Err(StoreError::PreconditionFailed)));
    }

    #[test]
    fn multipart_etags() {
        let part = |etag: &str| Part { part_number: 1, size: 1, etag: etag.into(), checksum: None, backend_etag: String::new(), created_at: 0 };
        let a = md5_hex(b"a");
        let mut h = Md5::new();
        h.update(Md5::digest(b"a"));
        let want: String = h.finalize().iter().map(|b| format!("{b:02x}")).collect();
        assert_eq!(multipart_etag(&[part(&a)]), format!("{want}-1"));
        // Non-MD5 ETags (copied parts) still give a stable, distinct result.
        assert_ne!(multipart_etag(&[part("zz")]), multipart_etag(&[part("zy")]));
        assert!(multipart_etag(&[part(&a), part(&a)]).ends_with("-2"));
    }

    async fn store() -> Arc<Store> {
        let dir = tempfile::tempdir().unwrap().keep();
        let mut cfg = StoreConfig::new(dir).manual();
        cfg.commit_ms = 2;
        Store::open(cfg, crate::backend::test_backend("unit-expire").await).await.unwrap()
    }

    async fn put(s: &Store, bucket: &str, key: &str, data: &'static [u8]) -> ObjectMeta {
        let b = s.bucket(bucket).unwrap();
        let blob = s.new_blob();
        let up = s.put_blob(&blob, &b.id, key, crate::backend::bytes_body(bytes::Bytes::from_static(data)), data.len() as u64, None).await.unwrap();
        let obj = NewObject { blob_id: blob, size: up.size, crc32c: up.crc32c, etag: md5_hex(data), headers: ObjectHeaders::default(), checksum: None };
        s.commit_put(bucket, key, obj, &WriteCondition::none()).await.unwrap()
    }

    /// Lifecycle lists first and deletes later: the delete re-checks each version itself.
    #[tokio::test]
    async fn expiry_rechecks_the_version_inside_the_delete() {
        let s = store().await;
        s.create_bucket("exp", BucketOptions::default()).await.unwrap();
        let old = put(&s, "exp", "a", b"old").await;
        let fresh = put(&s, "exp", "b", b"b").await;
        let later = s.now() + 1;
        // Overwritten since the listing: a different ETag.
        put(&s, "exp", "a", b"new").await;
        assert_eq!(s.expire_objects("exp", &[("a".into(), old.etag.clone())], later).await.unwrap(), 0);
        // Same ETag but written after the cutoff (rewritten with identical bytes).
        put(&s, "exp", "b", b"b").await;
        assert_eq!(s.expire_objects("exp", &[("b".into(), fresh.etag.clone())], fresh.modified_at + 1).await.unwrap(), 0);
        // Gone since the listing.
        assert_eq!(s.expire_objects("exp", &[("gone".into(), "x".into())], later).await.unwrap(), 0);
        assert_eq!(s.get_object("exp", "a").unwrap().etag, md5_hex(b"new"));
        assert_eq!(s.expire_objects("exp", &[("b".into(), fresh.etag.clone())], s.now() + 1).await.unwrap(), 1);
        assert_eq!(s.list_trash("exp", "", None, 10).unwrap().0.len(), 3, "expiry is a normal delete: recoverable");
    }
}
