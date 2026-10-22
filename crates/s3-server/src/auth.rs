//! Who is asking (SigV4 with roll access keys) and what they may do.

use crate::ctx::Ctx;
use s3_proto::sigv4::{self, Verified};
use s3_proto::{Code, S3Error};
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

fn denied() -> S3Error {
    S3Error::new(Code::AccessDenied)
}

impl Ctx {
    /// The request's key may do `need` on `bucket`.
    pub fn require(&self, bucket: &str, need: Need) -> Result<&AccessKey, S3Error> {
        let Some(a) = &self.auth else { return Err(denied()) };
        let ok = match need {
            Need::Read => a.key.can_read(bucket),
            Need::Write => a.key.can_write(bucket),
            Need::Admin => a.key.is_admin() && a.key.covers(bucket),
        };
        if ok { Ok(&a.key) } else { Err(denied()) }
    }

    /// Reading an object: a key that may read the bucket, or anyone if the bucket is public.
    pub fn require_object_read(&self, bucket: &Bucket) -> Result<(), S3Error> {
        if self.auth.is_none() && bucket.public {
            return Ok(());
        }
        self.require(&bucket.name, Need::Read).map(|_| ())
    }
}
