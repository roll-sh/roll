//! Who is asking (SigV4, SigV4a or SigV2 with roll access keys) and what they may do.

use crate::ctx::Ctx;
use p256::ecdsa::VerifyingKey;
use s3_proto::auth::Context;
use s3_proto::sigv4::{self, Verified};
use s3_proto::{Code, S3Error, sigv4a};
use s3_store::{AccessKey, Bucket};
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::sync::{Mutex, OnceLock};

/// Access key → (SHA-256 of the secret, public key).
type KeyCache = Mutex<HashMap<String, ([u8; 32], VerifyingKey)>>;

/// SigV4a public keys by access key (with a hash of the secret they came from, so a changed
/// secret derives a new one). The derivation is a few HMACs and a scalar multiplication: cheap,
/// but not per request.
fn ecdsa_key(access_key: &str, secret: &str) -> VerifyingKey {
    const MAX: usize = 10_000;
    static CACHE: OnceLock<KeyCache> = OnceLock::new();
    let cache = CACHE.get_or_init(Default::default);
    let tag: [u8; 32] = Sha256::digest(secret.as_bytes()).into();
    if let Some((t, k)) = cache.lock().unwrap().get(access_key)
        && *t == tag
    {
        return *k;
    }
    let k = sigv4a::verifying_key(access_key, secret);
    let mut c = cache.lock().unwrap();
    if c.len() >= MAX {
        c.clear();
    }
    c.insert(access_key.to_string(), (tag, k));
    k
}

pub(crate) struct Auth {
    pub key: AccessKey,
    pub verified: Verified,
}

/// Verify the request's signature (header or presigned URL). `None`: anonymous.
pub(crate) fn authenticate(ctx: &Ctx) -> Result<Option<Auth>, S3Error> {
    let pairs = ctx.header_pairs();
    let headers: Vec<(&str, &str)> = pairs.iter().map(|(k, v)| (k.as_str(), v.as_str())).collect();
    let req = sigv4::Request { method: ctx.method.as_str(), path: &ctx.raw_path, query: &ctx.raw_query, headers: &headers };
    let mut found: Option<AccessKey> = None;
    // Why the key didn't resolve (an unknown key, or a bad session token), to report instead
    // of the generic `InvalidAccessKeyId`.
    let mut failure: Option<S3Error> = None;
    // S3 Express sessions (directory buckets) bring theirs in `x-amz-s3session-token`.
    let (token, express) = match crate::temporary::express_token(&ctx.headers, &ctx.query) {
        Some(t) => (Some(t), true),
        None => (crate::temporary::request_token(&ctx.headers, &ctx.query), false),
    };
    let cx = Context { now: ctx.now_s(), vhost_bucket: ctx.vhost.then(|| ctx.bucket_name()) };
    let verified = s3_proto::auth::authenticate(&req, &cx, |id| match crate::temporary::resolve_as(ctx.store(), id, token, express) {
        Ok(k) => {
            let secret = k.secret.clone();
            found = Some(k);
            Some(secret)
        }
        Err(e) => {
            failure = Some(e);
            None
        }
    }, ecdsa_key)
    .map_err(|e| match failure.take() {
        Some(f) if e.code == Code::InvalidAccessKeyId => f,
        _ => e,
    })?;
    Ok(match (verified, found) {
        (Some((_, verified)), Some(key)) => Some(Auth { key, verified }),
        _ => None,
    })
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Need {
    Read,
    Write,
    Admin,
}

impl Ctx {
    /// The request may do what it asks on `bucket`: its key has `need` there, or the bucket's
    /// policy, ACLs or `public` flag allow it (see `access.rs`).
    pub fn require(&self, bucket: &str, need: Need) -> Result<(), S3Error> {
        crate::access::authorize(self, self.who(), bucket, self.key.as_deref(), crate::access::action_of(self), need)
    }

    /// Reading the source of a copy (`s3:GetObject` on it).
    pub fn require_source(&self, bucket: &str, key: &str) -> Result<(), S3Error> {
        crate::access::authorize(self, self.who(), bucket, Some(key), "s3:GetObject", Need::Read)
    }

    /// Reading an object.
    pub fn require_object_read(&self, bucket: &Bucket) -> Result<(), S3Error> {
        self.require(&bucket.name, Need::Read)
    }
}
