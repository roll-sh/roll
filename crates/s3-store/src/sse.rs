//! Server-side encryption in the metadata: which blobs are SSE-C or SSE-KMS (`blob_sse`), the
//! key checks every read and write of their bytes goes through, and copies that change
//! encryption.
//!
//! The backing object store does the crypto ([`crate::backend`]); the metadata only records how
//! each blob is encrypted. SSE-C keys are never stored: only their MD5, to tell the right key
//! from a wrong one before the backend is asked.
//!
//! A blob's encryption is fixed when it's written, so it's recorded per blob, and everything
//! sharing the blob (copies, branches, trash entries) shares it. A copy that changes encryption
//! can't share the blob: [`Store::copy_object_new_blob`] has the backend copy the bytes into a
//! new blob, server-side.

use crate::backend::{BlobAttrs, BlobSse, CopySource, KmsParams, SseUnsupported, UploadedPart};
use crate::error::{Result, StoreError};
use crate::ids;
use crate::model::*;
use crate::rows::{self, Row};
use crate::store::{Store, find_bucket};
use rusqlite::{Connection, OptionalExtension, params};
use serde::Serialize;

/// How a blob is encrypted, as the metadata records it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(tag = "algorithm")]
pub enum Encryption {
    /// SSE-C: the base64 MD5 of the customer's key (never the key).
    #[serde(rename = "SSE-C")]
    Customer { key_md5: String },
    /// SSE-KMS (or DSSE-KMS), as requested.
    #[serde(rename = "aws:kms")]
    Kms(KmsParams),
}

impl Encryption {
    pub fn of(sse: &BlobSse) -> Self {
        match sse {
            BlobSse::Customer(k) => Encryption::Customer { key_md5: k.md5_b64() },
            BlobSse::Kms(p) => Encryption::Kms(p.clone()),
        }
    }

    /// Whether a blob written with `sse` would be encrypted exactly like this one (so a copy
    /// may share the blob).
    pub fn same(this: Option<&Encryption>, sse: Option<&BlobSse>) -> bool {
        match (this, sse) {
            (None, None) => true,
            (Some(e), Some(s)) => *e == Encryption::of(s),
            _ => false,
        }
    }

    fn algorithm(&self) -> &'static str {
        match self {
            Encryption::Customer { .. } => "SSE-C",
            Encryption::Kms(p) if p.dsse => "aws:kms:dsse",
            Encryption::Kms(_) => "aws:kms",
        }
    }
}

pub(crate) fn load(c: &Connection, blob: &str) -> Result<Option<Encryption>> {
    let row = c
        .prepare_cached("SELECT algorithm, customer_key_md5, kms_key_id, kms_context, bucket_key FROM blob_sse WHERE blob_id = ?1")?
        .query_row([blob], |r| Ok((r.get::<_, String>(0)?, r.get::<_, Option<String>>(1)?, r.get::<_, Option<String>>(2)?, r.get::<_, Option<String>>(3)?, r.get::<_, Option<i64>>(4)?)))
        .optional()?;
    let Some((alg, md5, key_id, context, bucket_key)) = row else { return Ok(None) };
    Ok(Some(match alg.as_str() {
        "SSE-C" => Encryption::Customer { key_md5: md5.ok_or_else(|| StoreError::internal(format!("blob {blob}: SSE-C without a key MD5")))? },
        "aws:kms" | "aws:kms:dsse" => Encryption::Kms(KmsParams { dsse: alg == "aws:kms:dsse", key_id, context, bucket_key: bucket_key.map(|b| b != 0) }),
        other => return Err(StoreError::internal(format!("blob {blob}: unknown encryption {other}"))),
    }))
}

/// Record a blob's encryption (in the transaction that records the blob or its upload).
pub(crate) fn insert(c: &Connection, blob: &str, enc: &Encryption, now: i64) -> Result<()> {
    let (md5, kms) = match enc {
        Encryption::Customer { key_md5 } => (Some(key_md5.as_str()), None),
        Encryption::Kms(p) => (None, Some(p)),
    };
    c.prepare_cached("INSERT INTO blob_sse (blob_id, algorithm, customer_key_md5, kms_key_id, kms_context, bucket_key, created_at) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)")?.execute(params![
        blob,
        enc.algorithm(),
        md5,
        kms.and_then(|p| p.key_id.as_deref()),
        kms.and_then(|p| p.context.as_deref()),
        kms.and_then(|p| p.bucket_key).map(i64::from),
        now
    ])?;
    Ok(())
}

/// Forget a blob's encryption once neither a blob row nor an upload needs it.
pub(crate) fn forget_if_unused(c: &Connection, blob: &str) -> Result<()> {
    c.prepare_cached("DELETE FROM blob_sse WHERE blob_id = ?1 AND NOT EXISTS (SELECT 1 FROM blobs WHERE id = ?1) AND NOT EXISTS (SELECT 1 FROM uploads WHERE blob_id = ?1)")?.execute([blob])?;
    Ok(())
}

/// What to hand the backend for a blob encrypted as `enc`, given the key the request brought.
/// An SSE-C blob needs its own key; any other blob refuses one.
pub(crate) fn key_for(enc: Option<&Encryption>, given: Option<&BlobSse>, what: &str) -> Result<Option<BlobSse>> {
    let key = given.and_then(BlobSse::customer);
    match (enc, key) {
        (Some(Encryption::Customer { key_md5 }), Some(k)) if k.md5_b64() == *key_md5 => Ok(Some(BlobSse::Customer(k.clone()))),
        (Some(Encryption::Customer { .. }), Some(_)) => Err(StoreError::InvalidArgument(format!("{what} is encrypted with another customer key (SSE-C)"))),
        (Some(Encryption::Customer { .. }), None) => Err(StoreError::InvalidArgument(format!("{what} is encrypted with a customer-provided key (SSE-C); its key is needed"))),
        (_, Some(_)) => Err(StoreError::InvalidArgument(format!("{what} isn't encrypted with a customer-provided key"))),
        (Some(Encryption::Kms(p)), None) => Ok(Some(BlobSse::Kms(p.clone()))),
        (None, None) => Ok(None),
    }
}

fn random_etag() -> String {
    let mut b = [0u8; 16];
    getrandom::fill(&mut b).expect("os randomness");
    hex::encode(b)
}

impl Store {
    /// Whether SSE-KMS can be used: `StoreConfig::kms` (`ROLL_S3_KMS`), else whether the backend
    /// can pass it through (real AWS S3).
    pub fn kms_enabled(&self) -> bool {
        self.cfg.kms.unwrap_or_else(|| self.backend.supports_kms())
    }

    /// Refuse encryption the deployment can't do, before any bytes move.
    pub fn check_sse_supported(&self, sse: Option<&BlobSse>) -> Result<()> {
        if matches!(sse, Some(BlobSse::Kms(_))) && !self.kms_enabled() {
            return Err(StoreError::Internal(SseUnsupported("SSE-KMS isn't available: it needs an AWS S3 backend (set ROLL_S3_KMS to override)".into()).into()));
        }
        Ok(())
    }

    /// A blob's encryption (`None`: the backing bucket's default).
    pub fn blob_encryption(&self, blob: &str) -> Result<Option<Encryption>> {
        self.read(|c| load(c, blob))
    }

    /// The visible version of `key` and how its blob is encrypted, in one read.
    pub fn get_object_encryption(&self, bucket: &str, key: &str) -> Result<(ObjectMeta, Option<Encryption>)> {
        self.read(|c| {
            let b = find_bucket(c, bucket)?;
            let chain = rows::chain(c, &b.id)?;
            let Some((r, _)) = rows::resolve(c, &chain, key)? else { return Err(StoreError::NoSuchKey(key.to_string())) };
            let enc = load(c, r.blob()?)?;
            Ok((r.meta()?, enc))
        })
    }

    /// The backend's encryption for an upload's parts, checked against the request's key.
    pub(crate) fn upload_key(&self, upload: &Upload, given: Option<&BlobSse>) -> Result<Option<BlobSse>> {
        key_for(self.blob_encryption(&upload.blob_id)?.as_ref(), given, "the upload")
    }

    /// `UploadPartCopy` with encryption: `src_sse` is the source's SSE-C key, `sse` the
    /// upload's. The bytes are copied inside object storage, decrypted and re-encrypted there.
    #[allow(clippy::too_many_arguments)]
    pub(crate) async fn backend_part_copy(&self, upload: &Upload, part: u32, src: &ObjectMeta, range: Option<std::ops::Range<u64>>, src_sse: Option<&BlobSse>, sse: Option<&BlobSse>) -> Result<UploadedPart> {
        let src_key = key_for(self.blob_encryption(&src.blob_id)?.as_ref(), src_sse, "the copy source")?;
        let dst = self.upload_key(upload, sse)?;
        let blob_key = ids::blob_key(&src.blob_id);
        let from = CopySource { key: &blob_key, size: src.size, range, sse: src_key.as_ref() };
        Ok(self.backend.upload_part_copy(&ids::blob_key(&upload.blob_id), &upload.backend_upload_id, part, &from, dst.as_ref()).await?)
    }

    /// CopyObject into a new blob, for copies whose encryption differs from the source's (a
    /// shared blob has one encryption). The backend copies server-side: one copy up to
    /// `max_single_copy` (S3's 5 GiB limit), part by part beyond it. The new blob's CRC32C must
    /// equal the source's, or nothing is committed.
    ///
    /// The ETag: the source's for an unencrypted single-part destination, else a fresh opaque
    /// one (S3's SSE-C and SSE-KMS ETags aren't MD5s of the data either).
    #[allow(clippy::too_many_arguments)]
    pub async fn copy_object_new_blob(&self, src_bucket: &str, src_key: &str, dst_bucket: &str, dst_key: &str, opts: &CopyOptions, src_sse: Option<&BlobSse>, dst_sse: Option<&BlobSse>) -> Result<ObjectMeta> {
        crate::validate::key(dst_key)?;
        if let Some(h) = &opts.headers {
            crate::validate::headers(h)?;
        }
        if let Some(t) = &opts.tags {
            crate::validate::tags(t)?;
        }
        self.check_sse_supported(dst_sse)?;
        let (src, enc, src_crc, dst_bucket_id) = self.read(|c| {
            let sb = find_bucket(c, src_bucket)?;
            let chain = rows::chain(c, &sb.id)?;
            let Some((r, _)) = rows::resolve(c, &chain, src_key)? else { return Err(StoreError::NoSuchKey(src_key.to_string())) };
            let blob = r.blob()?.to_string();
            let crc: i64 = c.query_row("SELECT crc32c FROM blobs WHERE id = ?1", [&blob], |x| x.get(0))?;
            let enc = load(c, &blob)?;
            Ok((r, enc, crc as u32, find_bucket(c, dst_bucket)?.id))
        })?;
        if let Some(m) = &opts.source_etag
            && !crate::objects::etag_matches(m, &src.etag)
        {
            return Err(StoreError::PreconditionFailed);
        }
        let from_key = key_for(enc.as_ref(), src_sse, "the copy source")?;
        let blob = ids::blob_id();
        let _inflight = self.inflight_guard(&blob);
        let src_blob = ids::blob_key(src.blob()?);
        let dst_blob = ids::blob_key(&blob);
        let attrs = BlobAttrs { bucket_id: dst_bucket_id, key: dst_key.to_string(), content_type: opts.headers.as_ref().map_or(src.headers.content_type.clone(), |h| h.content_type.clone()), sse: dst_sse.cloned() };
        let copied = if src.size <= self.cfg.max_single_copy {
            let from = CopySource { key: &src_blob, size: src.size, range: None, sse: from_key.as_ref() };
            self.backend.copy(&from, &dst_blob, &attrs).await.map(|u| (u.size, u.crc32c, Vec::new()))
        } else {
            self.copy_in_parts(&src_blob, src.size, from_key.as_ref(), &dst_blob, &attrs).await
        };
        let (size, crc, part_sizes) = match copied {
            Ok(c) => c,
            Err(e) => {
                self.discard_blob(&blob).await;
                return Err(e.into());
            }
        };
        if size != src.size || crc != src_crc {
            tracing::error!(blob, src = src_blob, size, crc, src_crc, "a server-side copy doesn't match its source");
            self.discard_blob(&blob).await;
            return Err(StoreError::internal("the copied data doesn't match the source's checksum"));
        }
        let now = self.now();
        let mut row: Row = src.clone();
        row.key = dst_key.to_string();
        row.blob_id = Some(blob.clone());
        row.modified_at = now;
        if let Some(h) = &opts.headers {
            let tags = std::mem::take(&mut row.headers.tags);
            row.headers = h.clone();
            row.headers.tags = tags;
        }
        if let Some(t) = &opts.tags {
            row.headers.tags = t.clone();
        }
        let single = part_sizes.is_empty();
        row.etag = if dst_sse.is_none() && single { src.etag.clone() } else if single { random_etag() } else { format!("{}-{}", random_etag(), part_sizes.len()) };
        row.parts = part_sizes.len() as u32;
        row.part_sizes = part_sizes;
        // A composite checksum belongs to the old part layout; the CRC32C of the data doesn't.
        if !row.checksum.as_ref().is_some_and(|c| c.kind == "FULL_OBJECT") {
            use base64::Engine;
            row.checksum = Some(Checksum { algorithm: "CRC32C".into(), kind: "FULL_OBJECT".into(), value: base64::engine::general_purpose::STANDARD.encode(crc.to_be_bytes()) });
        }
        let dst_enc = dst_sse.map(Encryption::of);
        self.commit_row(dst_bucket, row, crc, &opts.condition, dst_enc.as_ref()).await
    }

    /// A multipart copy of `size` bytes into `dst` (each part at most `copy_part_size`, at
    /// most 10 000 parts). Returns the size, the combined CRC32C and the part sizes.
    async fn copy_in_parts(&self, src: &str, size: u64, src_sse: Option<&BlobSse>, dst: &str, attrs: &BlobAttrs) -> anyhow::Result<(u64, u32, Vec<u64>)> {
        let part = self.cfg.copy_part_size.max(size.div_ceil(10_000)).max(1);
        let upload = self.backend.create_multipart(dst, attrs).await?;
        let result: anyhow::Result<(u64, u32, Vec<u64>)> = async {
            let mut tokens = Vec::new();
            let (mut crc, mut sizes, mut at) = (0u32, Vec::new(), 0u64);
            let mut n = 1u32;
            while at < size {
                let end = (at + part).min(size);
                let from = CopySource { key: src, size, range: Some(at..end), sse: src_sse };
                let p = self.backend.upload_part_copy(dst, &upload, n, &from, attrs.sse.as_ref()).await?;
                anyhow::ensure!(p.size == end - at, "part {n} of a copy has {} bytes, not {}", p.size, end - at);
                crc = crc32c::crc32c_combine(crc, p.crc32c, p.size as usize);
                sizes.push(p.size);
                tokens.push((n, p.etag));
                at = end;
                n += 1;
            }
            self.backend.complete_multipart(dst, &upload, &tokens, attrs.sse.as_ref()).await?;
            Ok((size, crc, sizes))
        }
        .await;
        if result.is_err() {
            let _ = self.backend.abort_multipart(dst, &upload).await;
        }
        result
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::backend::CustomerKey;

    #[test]
    fn keys_are_checked_against_the_blob() {
        let k = CustomerKey::new([1u8; 32]);
        let other = CustomerKey::new([2u8; 32]);
        let c = Encryption::of(&BlobSse::Customer(k.clone()));
        assert_eq!(c, Encryption::Customer { key_md5: k.md5_b64() });
        assert_eq!(key_for(Some(&c), Some(&BlobSse::Customer(k.clone())), "x").unwrap(), Some(BlobSse::Customer(k.clone())));
        for bad in [Some(BlobSse::Customer(other.clone())), None] {
            assert!(matches!(key_for(Some(&c), bad.as_ref(), "x"), Err(StoreError::InvalidArgument(_))));
        }
        assert!(matches!(key_for(None, Some(&BlobSse::Customer(k.clone())), "x"), Err(StoreError::InvalidArgument(_))), "a plain blob refuses a key");
        assert_eq!(key_for(None, None, "x").unwrap(), None);
        let kms = Encryption::Kms(KmsParams { key_id: Some("arn".into()), ..Default::default() });
        assert!(matches!(key_for(Some(&kms), None, "x").unwrap(), Some(BlobSse::Kms(_))));
        assert!(Encryption::same(None, None));
        assert!(Encryption::same(Some(&c), Some(&BlobSse::Customer(k.clone()))));
        assert!(!Encryption::same(Some(&c), Some(&BlobSse::Customer(other))));
        assert!(!Encryption::same(Some(&c), None));
        assert!(!Encryption::same(None, Some(&BlobSse::Customer(k))));
        assert!(!Encryption::same(Some(&kms), Some(&BlobSse::Kms(KmsParams::default()))), "another KMS key is another encryption");
    }

    #[test]
    fn rows_round_trip() {
        let c = Connection::open_in_memory().unwrap();
        crate::schema::migrate(&c, crate::schema::LATEST).unwrap();
        let encs = [
            Encryption::Customer { key_md5: "bWQ1".into() },
            Encryption::Kms(KmsParams::default()),
            Encryption::Kms(KmsParams { dsse: true, key_id: Some("arn:aws:kms:k".into()), context: Some("e30=".into()), bucket_key: Some(false) }),
        ];
        for (i, e) in encs.iter().enumerate() {
            insert(&c, &format!("b{i}"), e, 1).unwrap();
            assert_eq!(load(&c, &format!("b{i}")).unwrap().as_ref(), Some(e));
        }
        assert_eq!(load(&c, "none").unwrap(), None);
        assert!(insert(&c, "b0", &encs[0], 2).is_err(), "a blob's encryption is written once");
        forget_if_unused(&c, "b0").unwrap();
        assert_eq!(load(&c, "b0").unwrap(), None);
        // Kept while the blob or an upload is there.
        insert(&c, "b0", &encs[0], 3).unwrap();
        c.execute("INSERT INTO blobs (id, size, crc32c, refs, created_at) VALUES ('b0', 1, 0, 1, 0)", []).unwrap();
        forget_if_unused(&c, "b0").unwrap();
        assert!(load(&c, "b0").unwrap().is_some());
    }
}
