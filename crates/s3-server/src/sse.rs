//! Server-side encryption on the wire, as S3 does it: what a write asks for (SSE-C, SSE-S3,
//! SSE-KMS, or the bucket's default), the key checks on reads, part uploads and copies, and the
//! response headers. The backing object store does the crypto (see `s3_store::backend`).
//!
//! S3 refuses SSE-C over plain HTTP; roll doesn't enforce that (it sits behind TLS-terminating
//! proxies), but clients should only send customer keys over HTTPS.

use crate::ctx::Ctx;
use crate::response::set;
use axum::response::Response;
use s3_proto::sse::{self, ServerSide};
use s3_proto::{Code, S3Error};
use s3_store::{BlobSse, Bucket, CustomerKey, Encryption, KmsParams, Store};

/// A bucket's default encryption rule (`PutBucketEncryption`).
#[derive(Clone, Debug, PartialEq, Eq)]
#[allow(dead_code)] // Made by [`bucket_default`] once bucket configuration lands.
pub(crate) enum DefaultEncryption {
    /// `AES256`.
    S3,
    Kms(KmsParams),
}

/// The bucket's default encryption, applied to writes that ask for none.
///
/// Bucket configuration (Get/Put/DeleteBucketEncryption) is owned by the bucket-config work,
/// which exposes `default_encryption(&Bucket)`. Until that's merged this stub says "none"
/// (everything is still encrypted at rest by the backing bucket). On merge, replace the body
/// with a call to that accessor, mapping `AES256` to [`DefaultEncryption::S3`] and
/// `aws:kms`/`aws:kms:dsse` (key id, bucket-key flag) to [`DefaultEncryption::Kms`].
pub(crate) fn bucket_default(_bucket: &Bucket) -> Option<DefaultEncryption> {
    None
}

/// The bucket default's encryption for writes that don't come through the S3 API (the control
/// plane's uploads): SSE-KMS if that's the default, else the backing bucket's.
pub(crate) fn default_sse(bucket: &Bucket) -> Option<BlobSse> {
    match bucket_default(bucket) {
        Some(DefaultEncryption::Kms(p)) => Some(BlobSse::Kms(p)),
        _ => None,
    }
}

/// What a write ends up encrypted with.
#[derive(Clone, Debug, Default)]
pub(crate) struct Target {
    /// What the backend applies: SSE-C or SSE-KMS.
    pub sse: Option<BlobSse>,
    /// SSE-S3 (`AES256`) was asked for, or is the bucket's default: echoed as S3 does.
    pub s3: bool,
}

impl Target {
    pub fn encryption(&self) -> Option<Encryption> {
        self.sse.as_ref().map(Encryption::of)
    }
}

fn kms_unavailable() -> S3Error {
    S3Error::msg(Code::NotImplemented, "SSE-KMS needs an AWS S3 backend; this deployment can't pass it through (ROLL_S3_KMS).").with("Header", "x-amz-server-side-encryption")
}

/// The encryption a write asks for in its headers (or form fields): `get` looks one up.
pub(crate) fn target_from<'a>(store: &Store, bucket: &Bucket, get: impl Fn(&str) -> Option<&'a str> + Copy) -> Result<Target, S3Error> {
    let req = sse::requested(get)?;
    if let Some(k) = req.customer {
        return Ok(Target { sse: Some(BlobSse::Customer(CustomerKey::new(k.key))), s3: false });
    }
    let kms = |p: KmsParams| -> Result<Target, S3Error> {
        if !store.kms_enabled() {
            return Err(kms_unavailable());
        }
        Ok(Target { sse: Some(BlobSse::Kms(p)), s3: false })
    };
    match req.server_side {
        Some(ServerSide::S3) => Ok(Target { sse: None, s3: true }),
        Some(ServerSide::Kms { dsse, key_id, context, bucket_key }) => kms(KmsParams { dsse, key_id, context, bucket_key }),
        None => match bucket_default(bucket) {
            None => Ok(Target::default()),
            Some(DefaultEncryption::S3) => Ok(Target { sse: None, s3: true }),
            Some(DefaultEncryption::Kms(p)) => kms(p),
        },
    }
}

/// The encryption a write (PUT, CreateMultipartUpload, a copy's destination) asks for.
pub(crate) fn target(ctx: &Ctx, bucket: &Bucket) -> Result<Target, S3Error> {
    target_from(ctx.store(), bucket, |n| ctx.h(n))
}

/// What a request reads or extends.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Use {
    /// GET, HEAD.
    Read,
    /// The source of a copy.
    CopySource,
    /// UploadPart, UploadPartCopy's destination.
    Part,
    /// CompleteMultipartUpload.
    Complete,
}

/// The SSE-C key a request must bring for something encrypted as `enc`, checked as S3 does: a
/// missing key is `InvalidRequest`, a wrong one `AccessDenied` (and a key MD5 that doesn't match
/// the key `InvalidArgument`, from parsing). SSE-KMS needs nothing; a key for something that
/// isn't SSE-C is refused.
pub(crate) fn key_for(ctx: &Ctx, enc: Option<&Encryption>, used: Use) -> Result<Option<BlobSse>, S3Error> {
    let given = sse::customer_key(|n| ctx.h(n), used == Use::CopySource)?;
    match (enc, given) {
        (Some(Encryption::Customer { key_md5 }), Some(k)) if k.key_md5 == *key_md5 => Ok(Some(BlobSse::Customer(CustomerKey::new(k.key)))),
        (Some(Encryption::Customer { .. }), Some(_)) => Err(S3Error::new(Code::AccessDenied)),
        (Some(Encryption::Customer { .. }), None) => Err(S3Error::msg(
            Code::InvalidRequest,
            match used {
                Use::Read => "The object was stored using a form of Server Side Encryption. The correct parameters must be provided to retrieve the object.",
                Use::CopySource => "The source object of the COPY operation is encrypted using Server Side Encryption with Customer provided keys. The correct parameters must be provided to retrieve the object.",
                Use::Part => "The multipart upload initiate requested encryption. Subsequent part requests must include the appropriate encryption parameters.",
                Use::Complete => "The multipart upload initiate requested encryption. The complete request must include the appropriate encryption parameters.",
            },
        )),
        (_, Some(_)) => Err(S3Error::msg(Code::InvalidRequest, "The encryption parameters are not applicable to this object.")),
        (_, None) => Ok(None),
    }
}

/// The response headers of an object (or upload) encrypted as `enc`; `s3`: SSE-S3 was asked
/// for.
pub(crate) fn set_headers(r: &mut Response, enc: Option<&Encryption>, s3: bool) {
    match enc {
        Some(Encryption::Customer { key_md5 }) => {
            set(r, "x-amz-server-side-encryption-customer-algorithm", sse::AES256);
            set(r, "x-amz-server-side-encryption-customer-key-MD5", key_md5);
        }
        Some(Encryption::Kms(p)) => {
            set(r, "x-amz-server-side-encryption", if p.dsse { sse::KMS_DSSE } else { sse::KMS });
            if let Some(k) = &p.key_id {
                set(r, "x-amz-server-side-encryption-aws-kms-key-id", k);
            }
            if let Some(c) = &p.context {
                set(r, "x-amz-server-side-encryption-context", c);
            }
            if p.bucket_key == Some(true) {
                set(r, "x-amz-server-side-encryption-bucket-key-enabled", "true");
            }
        }
        None if s3 => set(r, "x-amz-server-side-encryption", sse::AES256),
        None => {}
    }
}

/// An object's (or part's) ETag: the MD5 of its data, except for SSE-C and SSE-KMS, whose S3
/// ETags aren't (and mustn't reveal one): those get an opaque value of the same shape.
pub(crate) fn etag(enc: Option<&Encryption>, md5_hex: String) -> String {
    if enc.is_none() {
        return md5_hex;
    }
    let mut b = [0u8; 16];
    getrandom::fill(&mut b).expect("os randomness");
    hex::encode(b)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn opaque_etags_for_encrypted_objects() {
        let md5 = "5d41402abc4b2a76b9719d911017c592".to_string();
        assert_eq!(etag(None, md5.clone()), md5);
        let enc = Encryption::Customer { key_md5: "x".into() };
        let (a, b) = (etag(Some(&enc), md5.clone()), etag(Some(&enc), md5.clone()));
        assert_ne!(a, md5);
        assert_ne!(a, b);
        assert_eq!(a.len(), 32);
        assert!(a.bytes().all(|c| c.is_ascii_hexdigit()));
    }

    #[test]
    fn response_headers() {
        let mut r = crate::response::empty(axum::http::StatusCode::OK);
        set_headers(&mut r, Some(&Encryption::Customer { key_md5: "bWQ1".into() }), false);
        assert_eq!(r.headers()["x-amz-server-side-encryption-customer-algorithm"], "AES256");
        assert_eq!(r.headers()["x-amz-server-side-encryption-customer-key-md5"], "bWQ1");
        assert!(r.headers().get("x-amz-server-side-encryption").is_none());
        let mut r = crate::response::empty(axum::http::StatusCode::OK);
        set_headers(&mut r, Some(&Encryption::Kms(KmsParams { dsse: true, key_id: Some("arn:k".into()), context: None, bucket_key: Some(true) })), false);
        assert_eq!(r.headers()["x-amz-server-side-encryption"], "aws:kms:dsse");
        assert_eq!(r.headers()["x-amz-server-side-encryption-aws-kms-key-id"], "arn:k");
        assert_eq!(r.headers()["x-amz-server-side-encryption-bucket-key-enabled"], "true");
        let mut r = crate::response::empty(axum::http::StatusCode::OK);
        set_headers(&mut r, None, true);
        assert_eq!(r.headers()["x-amz-server-side-encryption"], "AES256");
        let mut r = crate::response::empty(axum::http::StatusCode::OK);
        set_headers(&mut r, None, false);
        assert!(r.headers().is_empty());
    }
}
