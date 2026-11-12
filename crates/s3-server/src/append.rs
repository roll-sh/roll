//! Appends: PutObject with `x-amz-write-offset-bytes` (directory buckets, as on S3).
//!
//! The offset must be the object's current size (0 creates an object that doesn't exist):
//! checked first, and again inside the commit transaction (`InvalidWriteOffset`, or
//! `ConditionalRequestConflict` if the object was replaced by one of the same size meanwhile).
//! The appended object is a new blob holding the old bytes and the new ones, written
//! server-side in object storage; the old blob is never touched and goes to the trash as on any
//! overwrite (see `s3_store`'s `append.rs` for how and what it costs).
//!
//! - The object keeps its metadata and encryption; each append is one more part (at most
//!   10 000, `TooManyParts`), so `GET ?partNumber=` reads one appended piece.
//! - Its checksum: an object with a full-object CRC keeps one, combined with the new data's in
//!   the same algorithm (computed, or the client's, verified). Other checksums can't be
//!   extended, so the appended object has none unless the client sends the same CRC.
//! - Its ETag is new (directory buckets' ETags aren't MD5s of the data).

use crate::auth::Need;
use crate::body::{Incoming, requested_checksum};
use crate::ctx::Ctx;
use crate::error::{StoreResult, store_error};
use crate::object::{InflightBlob, MAX_PUT, object_headers, precheck, set_checksum, write_condition};
use crate::response::{self, Reply, set};
use crate::sse;
use axum::body::Body;
use axum::http::StatusCode;
use md5::{Digest, Md5};
use s3_proto::checksum::{Algorithm, b64, b64_decode, combine_crc, crc_bytes, crc_from_bytes};
use s3_proto::http::quote_etag;
use s3_proto::{Code, S3Error};
use s3_store::{Bucket, Checksum, Class, Encryption};

/// The full-object CRC an object has, if any.
fn full_crc(c: Option<&Checksum>) -> Option<(Algorithm, u64)> {
    let c = c?;
    let a = Algorithm::parse(&c.algorithm).filter(|a| a.is_crc())?;
    if c.kind != "FULL_OBJECT" {
        return None;
    }
    Some((a, crc_from_bytes(a, &b64_decode(&c.value)?)?))
}

pub(crate) async fn append(ctx: &mut Ctx, bucket: &Bucket, body: Body) -> Reply {
    if !crate::directory::is_directory(&bucket.name) {
        return Err(S3Error::msg(Code::NotImplemented, "Appending to objects (x-amz-write-offset-bytes) is only supported for directory buckets.").with("Header", "x-amz-write-offset-bytes"));
    }
    ctx.require(&bucket.name, Need::Write)?;
    let offset: u64 = ctx
        .h("x-amz-write-offset-bytes")
        .and_then(|v| v.trim().parse().ok())
        .ok_or_else(|| S3Error::msg(Code::InvalidArgument, "x-amz-write-offset-bytes must be a non-negative integer").with("ArgumentName", "x-amz-write-offset-bytes"))?;
    let key = ctx.key_name().to_string();
    let store = ctx.store().clone();
    let headers = object_headers(ctx)?;
    s3_store::validate::headers(&headers).s3()?;
    let cond = write_condition(ctx)?;
    let target = sse::target(ctx, bucket)?;
    let mut base = store.append_base(&bucket.name, &key, offset).s3()?;
    match &base.meta {
        // A new object is encrypted as any write.
        None => base.encryption = target.encryption(),
        // An existing one keeps its encryption: asking for another is S3's mismatch.
        Some(_) if ctx.h("x-amz-server-side-encryption").is_some() && !Encryption::same(base.encryption.as_ref(), target.sse.as_ref()) => {
            return Err(S3Error::new(Code::EncryptionTypeMismatch));
        }
        Some(_) => {}
    }
    let old_crc = base.meta.as_ref().and_then(|m| full_crc(m.checksum.as_ref()));
    let want = match (requested_checksum(ctx)?, old_crc) {
        (Some(r), _) => Some(r),
        (None, Some((a, _))) => Some((a, None)),
        // Every object gets a checksum, as on S3.
        (None, None) => Some((Algorithm::Crc64Nvme, None)),
    };
    let incoming = Incoming::new(ctx, body, want, None)?;
    let len = incoming.len().ok_or_else(|| S3Error::new(Code::MissingContentLength))?;
    if len > MAX_PUT {
        return Err(S3Error::new(Code::EntityTooLarge).with("ProposedSize", len.to_string()).with("MaxSizeAllowed", MAX_PUT.to_string()));
    }
    ctx.meter(&bucket.id, Class::A);
    precheck(ctx, &cond)?;
    let result = incoming.result();
    let guard = InflightBlob::new(&store);
    let blob = guard.id.clone();
    let prepared = store.append_blob(&blob, &bucket.id, &key, &base, incoming.into_stream(), len, headers.content_type.as_deref()).await;
    let (prepared, finished) = match (prepared, result.get()) {
        (Ok(p), Ok(f)) => (p, f),
        (Err(e), body) => return Err(body.err().unwrap_or_else(|| store_error(e))),
        (Ok(p), Err(e)) => {
            if let Some(b) = &p.blob {
                store.discard_blob(b).await;
            }
            return Err(e);
        }
    };
    store.meter.bytes_in(&bucket.id, len);
    let new_md5 = finished.digest.etag();
    let (etag, checksum) = match &base.meta {
        None => (sse::etag(base.encryption.as_ref(), new_md5), finished.checksum.map(|(a, v)| Checksum { algorithm: a.as_str().into(), kind: "FULL_OBJECT".into(), value: v })),
        Some(old) => {
            let mut h = Md5::new();
            h.update(format!("roll append\0{}\0{new_md5}\0{len}", old.etag).as_bytes());
            let etag = hex::encode(h.finalize());
            let combined = match (old_crc, &finished.checksum) {
                (Some((a, old_v)), Some((b, new_b64))) if a == *b => {
                    let new_v = b64_decode(new_b64).and_then(|raw| crc_from_bytes(a, &raw));
                    new_v.and_then(|n| combine_crc(a, &[(old_v, old.size), (n, len)])).map(|v| Checksum { algorithm: a.as_str().into(), kind: "FULL_OBJECT".into(), value: b64(&crc_bytes(a, v)) })
                }
                _ => None,
            };
            (etag, combined)
        }
    };
    let enc = base.encryption.clone();
    let meta = store.commit_append(&bucket.name, &key, prepared, etag, checksum, headers, enc.as_ref()).await.s3()?;
    drop(guard);
    let mut r = response::empty(StatusCode::OK);
    set(&mut r, "etag", &quote_etag(&meta.etag));
    set(&mut r, "x-amz-object-size", &meta.size.to_string());
    if let Some(c) = &meta.checksum {
        set_checksum(&mut r, c);
    }
    sse::set_headers(&mut r, enc.as_ref(), true);
    Ok(r)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_full_object_crcs_extend() {
        let c = |a: &str, k: &str, v: &str| Checksum { algorithm: a.into(), kind: k.into(), value: v.into() };
        assert_eq!(full_crc(Some(&c("CRC32", "FULL_OBJECT", "AAAAAQ=="))), Some((Algorithm::Crc32, 1)));
        assert_eq!(full_crc(Some(&c("CRC32", "COMPOSITE", "AAAAAQ=="))), None);
        assert_eq!(full_crc(Some(&c("SHA256", "FULL_OBJECT", "AAAAAQ=="))), None);
        assert_eq!(full_crc(None), None);
    }
}
