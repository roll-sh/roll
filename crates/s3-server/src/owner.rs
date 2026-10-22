//! `x-amz-expected-bucket-owner` (and `x-amz-source-expected-bucket-owner` on copies): a
//! request naming an owner other than the deployment's fails with `AccessDenied`, as S3 does.

use crate::ctx::Ctx;
use s3_proto::{Code, S3Error};

/// The deployment's canonical owner id (what `Owner/ID` shows in listings and ACLs).
///
/// Owner ids are defined by the bucket-config work (ACLs, policies); until that's merged this
/// stub returns the `roll` id the responses already use. On merge, replace the body with its
/// canonical owner id accessor.
pub(crate) fn canonical_owner_id() -> &'static str {
    "roll"
}

/// Refuse the request if it expects another bucket owner.
pub(crate) fn check(ctx: &Ctx) -> Result<(), S3Error> {
    for h in ["x-amz-expected-bucket-owner", "x-amz-source-expected-bucket-owner"] {
        if let Some(v) = ctx.h(h)
            && v.trim() != canonical_owner_id()
        {
            return Err(S3Error::new(Code::AccessDenied));
        }
    }
    Ok(())
}
