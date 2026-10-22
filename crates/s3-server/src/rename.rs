//! RenameObject: `PUT /<bucket>/<key>?renameObject` with `x-amz-rename-source`.
//!
//! As on S3 Express (2025): atomic and metadata only, with `x-amz-rename-source-if-*` on the
//! source and `If-Match`/`If-None-Match`/`If-(Un)Modified-Since` on the destination, all checked
//! inside the one transaction that moves the object (see `s3_store`'s `rename.rs`). A roll
//! extension: it works on general purpose buckets too (a versioned bucket keeps the source's
//! history behind a delete marker). It's free and instant whatever the size: the blob's
//! reference moves, no bytes do.
//!
//! `x-amz-client-token` makes a request idempotent: a retry with the same token and the same
//! source and destination succeeds without doing anything again; the same token with other
//! parameters is `IdempotencyParameterMismatch`. Tokens are remembered for 10 minutes, in memory
//! (a restart forgets them).

use crate::auth::Need;
use crate::ctx::Ctx;
use crate::error::StoreResult;
use crate::response::{self, Reply};
use axum::http::StatusCode;
use s3_proto::http::{percent_decode_utf8, validate_key};
use s3_proto::time::parse_http_date;
use s3_proto::{Code, S3Error};
use s3_store::{Bucket, Class, RenameCondition};
use std::collections::HashMap;
use std::sync::Mutex;

/// How long a client token is remembered, and how many at most.
const TOKEN_MS: i64 = 10 * 60 * 1000;
const MAX_TOKENS: usize = 100_000;

/// `(bucket, client token)`.
type TokenKey = (String, String);
/// `(source, destination, when)`.
type Renamed = (String, String, i64);

/// Recent renames by bucket and client token.
#[derive(Default)]
pub(crate) struct Tokens(Mutex<HashMap<TokenKey, Renamed>>);

impl Tokens {
    /// `Some(true)`: the same rename already succeeded; `Some(false)`: the token was used for
    /// another one.
    fn seen(&self, bucket: &str, token: &str, src: &str, dst: &str, now: i64) -> Option<bool> {
        let map = self.0.lock().unwrap();
        let (s, d, at) = map.get(&(bucket.to_string(), token.to_string()))?;
        (now - at < TOKEN_MS).then(|| s == src && d == dst)
    }

    fn remember(&self, bucket: &str, token: &str, src: &str, dst: &str, now: i64) {
        let mut map = self.0.lock().unwrap();
        if map.len() >= MAX_TOKENS {
            map.retain(|_, (_, _, at)| now - *at < TOKEN_MS);
            if map.len() >= MAX_TOKENS {
                map.clear();
            }
        }
        map.insert((bucket.to_string(), token.to_string()), (src.to_string(), dst.to_string(), now));
    }
}

/// The source key: `x-amz-rename-source` is `/<key>` or `<bucket>/<key>` (URL-encoded), in the
/// same bucket.
fn source_key(ctx: &Ctx, bucket: &str) -> Result<String, S3Error> {
    let raw = ctx.h("x-amz-rename-source").ok_or_else(|| S3Error::msg(Code::InvalidArgument, "x-amz-rename-source is required").with("ArgumentName", "x-amz-rename-source"))?;
    let decoded = percent_decode_utf8(raw.trim())?;
    let path = decoded.strip_prefix('/').unwrap_or(&decoded);
    let key = path.strip_prefix(bucket).and_then(|k| k.strip_prefix('/')).unwrap_or(path);
    validate_key(key)?;
    Ok(key.to_string())
}

fn date(ctx: &Ctx, name: &str) -> Result<Option<i64>, S3Error> {
    match ctx.h(name) {
        None => Ok(None),
        Some(v) => parse_http_date(v).map(|s| Some(s * 1000)).ok_or_else(|| S3Error::msg(Code::InvalidArgument, format!("{name} isn't an HTTP date")).with("ArgumentName", name.to_string())),
    }
}

fn condition(ctx: &Ctx, prefix: &str) -> Result<RenameCondition, S3Error> {
    let s = |n: &str| ctx.h(&format!("{prefix}{n}")).map(str::to_string);
    Ok(RenameCondition { if_match: s("if-match"), if_none_match: s("if-none-match"), if_modified_since: date(ctx, &format!("{prefix}if-modified-since"))?, if_unmodified_since: date(ctx, &format!("{prefix}if-unmodified-since"))? })
}

pub(crate) async fn rename(ctx: &mut Ctx, bucket: &Bucket) -> Reply {
    let dst = ctx.key_name().to_string();
    let src = source_key(ctx, &bucket.name)?;
    // Writing the destination, and reading and deleting the source.
    ctx.require(&bucket.name, Need::Write)?;
    ctx.require_source(&bucket.name, &src)?;
    crate::access::authorize(ctx, ctx.who(), &bucket.name, Some(&src), "s3:DeleteObject", Need::Write)?;
    let token = ctx.h("x-amz-client-token").map(str::to_string);
    if let Some(t) = &token
        && (t.is_empty() || t.len() > 64 || !t.bytes().all(|b| (33..=126).contains(&b)))
    {
        return Err(S3Error::msg(Code::InvalidArgument, "x-amz-client-token is 1-64 printable ASCII characters").with("ArgumentName", "x-amz-client-token"));
    }
    let src_cond = condition(ctx, "x-amz-rename-source-")?;
    if src_cond.if_none_match.as_deref().is_some_and(|m| m.trim() == "*") {
        return Err(S3Error::new(Code::PreconditionFailed));
    }
    let dst_cond = condition(ctx, "")?;
    ctx.meter(&bucket.id, Class::A);
    let now = ctx.store().now();
    if let Some(t) = &token {
        match ctx.server.renames.seen(&bucket.name, t, &src, &dst, now) {
            Some(true) => return Ok(response::empty(StatusCode::OK)),
            Some(false) => return Err(S3Error::new(Code::IdempotencyParameterMismatch)),
            None => {}
        }
    }
    ctx.store().rename_object(&bucket.name, &src, &dst, &src_cond, &dst_cond).await.s3()?;
    if let Some(t) = &token {
        ctx.server.renames.remember(&bucket.name, t, &src, &dst, now);
    }
    Ok(response::empty(StatusCode::OK))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tokens() {
        let t = Tokens::default();
        assert_eq!(t.seen("b", "t1", "a", "z", 0), None);
        t.remember("b", "t1", "a", "z", 0);
        assert_eq!(t.seen("b", "t1", "a", "z", 1), Some(true));
        assert_eq!(t.seen("b", "t1", "a", "y", 1), Some(false));
        assert_eq!(t.seen("other", "t1", "a", "z", 1), None);
        assert_eq!(t.seen("b", "t1", "a", "z", TOKEN_MS), None, "forgotten after 10 minutes");
    }
}
