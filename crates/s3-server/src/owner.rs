//! `x-amz-expected-bucket-owner` (and `x-amz-source-expected-bucket-owner` on copies): a
//! request naming an account other than the bucket owner's (its 12-digit account id) fails with
//! `AccessDenied`, as S3 does. A bucket that doesn't exist is left to the request itself
//! (`NoSuchBucket`).

use crate::ctx::Ctx;
use s3_proto::{Code, S3Error};

/// The bucket of `x-amz-copy-source` (`bucket/key` or `/bucket/key`, possibly percent-encoded).
fn copy_source_bucket(ctx: &Ctx) -> Option<String> {
    let src = ctx.h("x-amz-copy-source")?.trim().trim_start_matches('/');
    let b = src.split(['/', '?']).next()?;
    s3_proto::http::percent_decode_utf8(b).ok().filter(|b| !b.is_empty())
}

/// Refuse the request if it expects another bucket owner.
pub(crate) fn check(ctx: &Ctx) -> Result<(), S3Error> {
    for (h, bucket) in [("x-amz-expected-bucket-owner", ctx.bucket.clone()), ("x-amz-source-expected-bucket-owner", copy_source_bucket(ctx))] {
        let Some(v) = ctx.h(h) else { continue };
        let owner = match bucket.as_deref().map(|b| ctx.store().bucket(b)) {
            Some(Ok(b)) => b.account_id,
            // No such bucket: the request fails on its own. (A request without a bucket, like
            // ListBuckets, is the caller's account's.)
            Some(Err(_)) => continue,
            None => ctx.auth.as_ref().map_or_else(|| ctx.store().account_id(), |a| a.identity.account.id.clone()),
        };
        if v.trim() != owner {
            return Err(S3Error::new(Code::AccessDenied));
        }
    }
    Ok(())
}
