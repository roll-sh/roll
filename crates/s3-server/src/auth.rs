//! Who is asking (SigV4 with roll access keys) and what they may do.

use crate::ctx::Ctx;
use s3_proto::sigv4::{self, Verified};
use s3_proto::S3Error;
use s3_store::{AccessKey, Bucket};

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
    let verified = sigv4::authenticate(&req, ctx.now_s(), |id| match ctx.store().access_key(id) {
        Ok(Some(k)) => {
            let secret = k.secret.clone();
            found = Some(k);
            Some(secret)
        }
        Ok(None) => None,
        Err(e) => {
            tracing::warn!(error = %e, "looking up an access key failed");
            None
        }
    })?;
    Ok(match (verified, found) {
        (Some(verified), Some(key)) => Some(Auth { key, verified }),
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
