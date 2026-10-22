//! Store errors as S3 errors.

use s3_proto::{Code, S3Error};
use s3_store::StoreError;

/// The S3 error for a store error. Client mistakes map to 4xx; only real failures are 5xx.
pub fn store_error(e: StoreError) -> S3Error {
    match e {
        StoreError::NoSuchBucket(b) => S3Error::new(Code::NoSuchBucket).with("BucketName", b),
        StoreError::BucketExists(b) => S3Error::new(Code::BucketAlreadyOwnedByYou).with("BucketName", b),
        StoreError::BucketNotEmpty(b) => S3Error::new(Code::BucketNotEmpty).with("BucketName", b),
        StoreError::BucketHasBranches(b) => S3Error::msg(Code::BucketNotEmpty, format!("Bucket {b} has branches; delete them first.")).with("BucketName", b),
        StoreError::NoSuchKey(k) => S3Error::new(Code::NoSuchKey).with("Key", k),
        StoreError::NoSuchUpload(u) => S3Error::new(Code::NoSuchUpload).with("UploadId", u),
        StoreError::PreconditionFailed => S3Error::new(Code::PreconditionFailed),
        StoreError::InvalidPart(n) => S3Error::new(Code::InvalidPart).with("PartNumber", n.to_string()),
        StoreError::InvalidPartOrder => S3Error::new(Code::InvalidPartOrder),
        StoreError::EntityTooSmall(n) => S3Error::new(Code::EntityTooSmall).with("PartNumber", n.to_string()),
        StoreError::InvalidBucketName(why) => S3Error::msg(Code::InvalidBucketName, format!("The specified bucket is not valid: {why}")),
        StoreError::KeyTooLong => S3Error::new(Code::KeyTooLongError),
        StoreError::MetadataTooLarge => S3Error::new(Code::MetadataTooLarge),
        StoreError::InvalidArgument(why) => S3Error::msg(Code::InvalidArgument, why),
        StoreError::NotFound(why) => S3Error::msg(Code::NoSuchKey, why),
        StoreError::NoSuchVersion(k, v) => S3Error::new(Code::NoSuchVersion).with("Key", k).with("VersionId", v),
        // S3: 405 for a marker asked for by its id, else a 404; both say it's a marker.
        StoreError::DeleteMarker { key, version_id, modified_at, specific } => {
            let e = if specific { S3Error::new(Code::MethodNotAllowed) } else { S3Error::new(Code::NoSuchKey).with("Key", key) };
            e.with_header("x-amz-delete-marker", "true").with_header("x-amz-version-id", version_id).with_header("last-modified", s3_proto::time::http_date(modified_at))
        }
        StoreError::ObjectLocked(_) => S3Error::msg(Code::AccessDenied, "Access Denied because object protected by object lock."),
        StoreError::InvalidBucketState(why) => S3Error::msg(Code::InvalidBucketState, why),
        StoreError::InvalidRequest(why) | StoreError::Conflict(why) | StoreError::LimitExceeded(why) | StoreError::DeleteConflict(why) => S3Error::msg(Code::InvalidRequest, why),
        StoreError::Unavailable(why) => {
            tracing::error!(why, "storage unavailable");
            S3Error::new(Code::ServiceUnavailable)
        }
        StoreError::Internal(e) if e.downcast_ref::<s3_store::SseUnsupported>().is_some() => S3Error::msg(Code::NotImplemented, e.to_string()).with("Header", "x-amz-server-side-encryption"),
        StoreError::Internal(e) => {
            tracing::error!(error = %e, "storage error");
            S3Error::new(Code::InternalError)
        }
    }
}

/// `?` on store results inside handlers.
pub(crate) trait StoreResult<T> {
    fn s3(self) -> Result<T, S3Error>;
}

impl<T> StoreResult<T> for Result<T, StoreError> {
    fn s3(self) -> Result<T, S3Error> {
        self.map_err(store_error)
    }
}
