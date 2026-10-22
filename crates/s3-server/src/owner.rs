//! `x-amz-expected-bucket-owner` (and `x-amz-source-expected-bucket-owner` on copies): a
//! request naming an account other than the deployment's (its 12-digit account id, as in
//! `GET /v1/storage`) fails with `AccessDenied`, as S3 does.

use crate::ctx::Ctx;
use s3_proto::{Code, S3Error};

/// Refuse the request if it expects another bucket owner.
pub(crate) fn check(ctx: &Ctx) -> Result<(), S3Error> {
    for h in ["x-amz-expected-bucket-owner", "x-amz-source-expected-bucket-owner"] {
        if let Some(v) = ctx.h(h)
            && v.trim() != ctx.store().account_id()
        {
            return Err(S3Error::new(Code::AccessDenied));
        }
    }
    Ok(())
}
