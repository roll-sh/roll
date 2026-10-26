//! Appends (`PutObject` with `x-amz-write-offset-bytes`, S3 Express directory buckets).
//!
//! Blobs are immutable and one object is one blob, so an append never touches the object's
//! blob. It writes a **new blob holding the old bytes followed by the new ones**, then swaps it
//! in like any overwrite:
//!
//! 1. [`Store::append_blob`] reads the key's current version (its blob, size and CRC32C) and
//!    writes the new blob:
//!    - old part at least `min_part_size` (5 MiB): a backend multipart upload whose part 1 is
//!      an `UploadPartCopy` of the old blob (server-side in S3: no bytes pass through roll) and
//!      part 2 the new data. The copied part's CRC32C must equal the old blob's.
//!    - smaller: one backend `PUT` streaming the old blob's bytes, then the new data (at most
//!      5 MiB are re-sent, never buffered). The old bytes' CRC32C must equal the old blob's,
//!      or the stream fails and nothing is stored.
//!
//!    The new blob's CRC32C is the combination of the two.
//! 2. [`Store::commit_append`] checks, inside the metadata transaction, that the key's current
//!    version is still the very blob that was copied (`InvalidWriteOffset` if its size moved,
//!    `ConditionalRequestConflict` if it was replaced by one of the same size), then writes the
//!    new version. The old one goes to the trash as on any overwrite (and from there, after
//!    `recover_days`, to the 24 h quarantine).
//!
//! A crash or failure anywhere before the commit leaves the old object exactly as it was: the
//! new blob is uncommitted (in flight until the request ends, then the orphan scan's), and an
//! unfinished backend multipart upload is aborted (or the backing bucket's lifecycle rule
//! cleans it up).
//!
//! **Cost.** An append costs a backend multipart upload (create, one part copy, one part,
//! complete: 4 requests) above 5 MiB, or one PUT of `old + new` bytes below it; plus a GET of
//! the old bytes below 5 MiB. The old blob stays billed as trash for `recover_days` like any
//! overwritten version, so many appends to a large object within the recovery window keep that
//! many copies until they expire.

use crate::backend::{BlobAttrs, BlobSse, ByteStream, CopySource};
use crate::error::{Result, StoreError};
use crate::events;
use crate::ids;
use crate::model::{Checksum, ObjectHeaders, ObjectMeta};
use crate::rows::{self, Row};
use crate::sse::Encryption;
use crate::store::{Store, find_bucket};
use crate::versioning;
use futures::StreamExt;
use rusqlite::OptionalExtension;

/// At most this many parts (each append is one), as on S3.
pub const MAX_PARTS: usize = 10_000;

/// The largest object (S3's 5 TiB).
const MAX_OBJECT: u64 = 5 << 40;

fn refused(code: &'static str, message: impl Into<String>) -> StoreError {
    StoreError::Refused { code, message: message.into() }
}

/// The version an append starts from.
#[derive(Clone, Debug)]
pub struct AppendBase {
    /// `None`: the key doesn't exist (an append at offset 0 creates it).
    pub meta: Option<ObjectMeta>,
    pub encryption: Option<Encryption>,
}

/// An appended blob waiting for [`Store::commit_append`].
#[derive(Debug)]
pub struct PreparedAppend {
    /// The new blob (`None`: nothing was appended to an existing object, so nothing changes).
    pub blob: Option<String>,
    /// The blob that was copied (`None`: there was no object).
    pub base_blob: Option<String>,
    /// The size of what was copied (the write offset).
    pub base_size: u64,
    pub size: u64,
    pub crc32c: u32,
    /// The new object's part sizes (each append adds one).
    pub part_sizes: Vec<u64>,
}

impl Store {
    /// The current version of `key` an append at `offset` would extend: refused unless
    /// `offset` is its size (0 for a key that doesn't exist).
    pub fn append_base(&self, bucket: &str, key: &str, offset: u64) -> Result<AppendBase> {
        let meta = match self.get_object(bucket, key) {
            Ok(m) => Some(m),
            Err(StoreError::NoSuchKey(_)) => None,
            Err(e) => return Err(e),
        };
        let size = meta.as_ref().map_or(0, |m| m.size);
        if offset != size {
            return Err(refused("InvalidWriteOffset", format!("The write offset value that you specified ({offset}) does not match the current object size ({size}).")));
        }
        let encryption = match &meta {
            Some(m) => self.blob_encryption(&m.blob_id)?,
            None => None,
        };
        if matches!(encryption, Some(Encryption::Customer { .. })) {
            return Err(refused("EncryptionTypeMismatch", "Objects encrypted with customer-provided keys can't be appended to."));
        }
        if let Some(m) = &meta
            && m.parts.max(1) as usize + 1 > MAX_PARTS
        {
            return Err(refused("TooManyParts", format!("The object already has {} parts, the maximum is {MAX_PARTS}.", m.parts.max(1))));
        }
        Ok(AppendBase { meta, encryption })
    }

    /// Write the appended blob `blob` (from [`Store::new_blob`]): `base`'s bytes, then `len`
    /// bytes of `body`. Nothing is committed; see the module docs.
    #[allow(clippy::too_many_arguments)]
    pub async fn append_blob(&self, blob: &str, bucket_id: &str, key: &str, base: &AppendBase, body: ByteStream, len: u64, content_type: Option<&str>) -> Result<PreparedAppend> {
        let sse = match &base.encryption {
            Some(Encryption::Kms(p)) => Some(BlobSse::Kms(p.clone())),
            _ => None,
        };
        let attrs = BlobAttrs { bucket_id: bucket_id.to_string(), key: key.to_string(), content_type: content_type.map(str::to_string), sse: sse.clone() };
        let Some(old) = &base.meta else {
            // No object yet: the append creates it.
            let up = self.put_blob_sse(blob, bucket_id, key, body, len, content_type, sse.as_ref()).await?;
            return Ok(PreparedAppend { blob: Some(blob.to_string()), base_blob: None, base_size: 0, size: up.size, crc32c: up.crc32c, part_sizes: Vec::new() });
        };
        let mut part_sizes = if old.parts == 0 { vec![old.size] } else { old.part_sizes.clone() };
        if len == 0 {
            self.release_blob(blob);
            return Ok(PreparedAppend { blob: None, base_blob: Some(old.blob_id.clone()), base_size: old.size, size: old.size, crc32c: 0, part_sizes });
        }
        let size = old.size.checked_add(len).filter(|s| *s <= MAX_OBJECT).ok_or_else(|| refused("EntityTooLarge", "Your proposed upload exceeds the maximum allowed object size."))?;
        part_sizes.push(len);
        let old_crc: u32 = self.read(|c| {
            c.query_row("SELECT crc32c FROM blobs WHERE id = ?1", [&old.blob_id], |r| r.get::<_, i64>(0))
                .optional()?
                .map(|v| v as u32)
                .ok_or_else(|| StoreError::internal(format!("blob {} has no metadata", old.blob_id)))
        })?;
        if let Err(e) = self.check_sse_supported(sse.as_ref()) {
            self.release_blob(blob);
            return Err(e);
        }
        let new_key = ids::blob_key(blob);
        let old_key = ids::blob_key(&old.blob_id);
        let result = if old.size >= self.cfg.min_part_size {
            self.append_by_part_copy(&new_key, &old_key, old.size, old_crc, body, len, &attrs).await
        } else {
            self.append_by_put(&new_key, &old_key, old.size, old_crc, body, len, &attrs).await
        };
        match result {
            Ok(crc) => Ok(PreparedAppend { blob: Some(blob.to_string()), base_blob: Some(old.blob_id.clone()), base_size: old.size, size, crc32c: crc, part_sizes }),
            Err(e) => {
                // Nothing references the new blob; a partial one is the orphan scan's otherwise.
                self.discard_blob(blob).await;
                Err(e)
            }
        }
    }

    /// Part 1 copied server-side from the old blob, part 2 the new bytes. Returns the CRC32C.
    #[allow(clippy::too_many_arguments)]
    async fn append_by_part_copy(&self, new_key: &str, old_key: &str, old_size: u64, old_crc: u32, body: ByteStream, len: u64, attrs: &BlobAttrs) -> Result<u32> {
        let sse = attrs.sse.as_ref();
        let upload = self.backend.create_multipart(new_key, attrs).await?;
        let attempt = async {
            let from = CopySource { key: old_key, size: old_size, range: None, sse: None };
            let first = self.backend.upload_part_copy(new_key, &upload, 1, &from, sse).await?;
            if first.size != old_size || first.crc32c != old_crc {
                return Err(StoreError::internal(format!("the copy of {old_key} for an append doesn't match it (crc32c {:08x}, expected {old_crc:08x})", first.crc32c)));
            }
            let second = self.backend.upload_part(new_key, &upload, 2, body, len, sse).await?;
            if second.size != len {
                return Err(StoreError::internal("the appended part has the wrong size"));
            }
            self.backend.complete_multipart(new_key, &upload, &[(1, first.etag.clone()), (2, second.etag.clone())], sse).await?;
            Ok(crc32c::crc32c_combine(first.crc32c, second.crc32c, len as usize))
        }
        .await;
        if attempt.is_err() {
            let _ = self.backend.abort_multipart(new_key, &upload).await;
        }
        attempt
    }

    /// One PUT of the old bytes (checked against the old blob's CRC32C as they stream) followed
    /// by the new ones. Returns the CRC32C.
    #[allow(clippy::too_many_arguments)]
    async fn append_by_put(&self, new_key: &str, old_key: &str, old_size: u64, old_crc: u32, body: ByteStream, len: u64, attrs: &BlobAttrs) -> Result<u32> {
        let prefix: ByteStream = if old_size == 0 {
            futures::stream::empty().boxed()
        } else {
            let Some((n, stream)) = self.backend.get_blob(old_key, None, None).await? else {
                tracing::error!(blob = old_key, "blob missing from object storage");
                return Err(StoreError::internal(format!("the data of {old_key} is missing from object storage")));
            };
            if n != old_size {
                return Err(StoreError::internal(format!("{old_key} has {n} bytes, the metadata says {old_size}")));
            }
            checked_prefix(stream, old_size, old_crc)
        };
        let up = self.backend.put(new_key, prefix.chain(body).boxed(), old_size + len, attrs).await?;
        Ok(up.crc32c)
    }

    /// Commit an appended blob as the key's new version, if the key's current version is still
    /// the one it extends. `etag` and `checksum` are the new object's; `headers` are used only
    /// when the append creates the object (an existing one keeps its own).
    #[allow(clippy::too_many_arguments)]
    pub async fn commit_append(&self, bucket: &str, key: &str, prepared: PreparedAppend, etag: String, checksum: Option<Checksum>, headers: ObjectHeaders, encryption: Option<&Encryption>) -> Result<ObjectMeta> {
        let Some(blob) = prepared.blob.clone() else {
            // Nothing appended: the object is unchanged, if it's still the one checked.
            let m = self.get_object(bucket, key)?;
            if Some(&m.blob_id) != prepared.base_blob.as_ref() {
                return Err(refused("ConditionalRequestConflict", "The object changed while it was being appended to."));
            }
            return Ok(m);
        };
        let _inflight = self.inflight_guard(&blob);
        let now = self.now();
        self.log.check_open()?;
        let attempt = self.log.db.write(|c| {
            let b = find_bucket(c, bucket)?;
            let chain = rows::chain(c, &b.id)?;
            let cur = rows::resolve(c, &chain, key)?.map(|(r, _)| r);
            let base_size = prepared.base_size;
            match (&cur, &prepared.base_blob) {
                (None, None) => {}
                (Some(r), Some(base)) if r.blob_id.as_ref() == Some(base) => {}
                (Some(r), _) if r.size != base_size => {
                    return Err(refused("InvalidWriteOffset", format!("The write offset value that you specified ({base_size}) does not match the current object size ({}).", r.size)));
                }
                (None, Some(_)) => return Err(refused("InvalidWriteOffset", format!("The write offset value that you specified ({base_size}) does not match the current object size (0)."))),
                _ => return Err(refused("ConditionalRequestConflict", "The object changed while it was being appended to.")),
            }
            let (parts, part_sizes) = if prepared.base_blob.is_some() { (prepared.part_sizes.len() as u32, prepared.part_sizes.clone()) } else { (0, Vec::new()) };
            let row = match &cur {
                Some(r) => Row { blob_id: Some(blob.clone()), size: prepared.size, etag: etag.clone(), checksum: checksum.clone(), parts, part_sizes, part_checksums: Vec::new(), modified_at: now, lock: Default::default(), ..r.clone() },
                None => Row { blob_id: Some(blob.clone()), size: prepared.size, etag: etag.clone(), checksum: checksum.clone(), headers: headers.clone(), modified_at: now, ..Row::tombstone(key, now) },
            };
            rows::insert_blob(c, &blob, prepared.size, prepared.crc32c, now)?;
            if let Some(e) = encryption {
                crate::sse::insert(c, &blob, e, now)?;
            }
            let stored = versioning::put_key(c, &b, &chain, key, row, now)?;
            events::emit(c, &b, events::Change::version(&events::created(), &stored), now)?;
            Ok(stored)
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
}

/// The old blob's bytes, failing the stream (so the backend stores nothing) unless they are
/// exactly `size` bytes with CRC32C `crc`.
fn checked_prefix(inner: ByteStream, size: u64, crc: u32) -> ByteStream {
    let state = (inner, 0u64, 0u32, false);
    futures::stream::unfold(state, move |(mut inner, seen, acc, done)| async move {
        if done {
            return None;
        }
        match inner.next().await {
            Some(Ok(chunk)) => {
                let seen = seen + chunk.len() as u64;
                if seen > size {
                    return Some((Err(std::io::Error::new(std::io::ErrorKind::InvalidData, "the object's blob is longer than recorded")), (inner, seen, acc, true)));
                }
                let acc = crc32c::crc32c_append(acc, &chunk);
                Some((Ok(chunk), (inner, seen, acc, false)))
            }
            Some(Err(e)) => Some((Err(e), (inner, seen, acc, true))),
            None if seen != size || acc != crc => {
                tracing::error!(seen, size, crc = acc, expected = crc, "an object's blob doesn't match its recorded CRC32C; refusing to append to it");
                Some((Err(std::io::Error::new(std::io::ErrorKind::InvalidData, "the object's blob doesn't match its recorded checksum")), (inner, seen, acc, true)))
            }
            None => None,
        }
    })
    .boxed()
}
