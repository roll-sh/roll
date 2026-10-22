/// Everything the store can refuse. `s3-server` maps these to S3 error codes, `s3-control` to
/// HTTP statuses.
#[derive(Debug, thiserror::Error)]
pub enum StoreError {
    #[error("no bucket named {0}")]
    NoSuchBucket(String),
    #[error("a bucket named {0} already exists")]
    BucketExists(String),
    #[error("bucket {0} isn't empty")]
    BucketNotEmpty(String),
    #[error("bucket {0} has branches; delete them first")]
    BucketHasBranches(String),
    #[error("no such key {0}")]
    NoSuchKey(String),
    #[error("no such upload {0}")]
    NoSuchUpload(String),
    /// A conditional write's or read's condition didn't hold.
    #[error("precondition failed")]
    PreconditionFailed,
    #[error("part {0} wasn't uploaded, or its ETag doesn't match")]
    InvalidPart(u32),
    #[error("parts must be listed in ascending order")]
    InvalidPartOrder,
    #[error("part {0} is smaller than the minimum part size")]
    EntityTooSmall(u32),
    #[error("invalid bucket name: {0}")]
    InvalidBucketName(String),
    #[error("key is longer than 1024 bytes")]
    KeyTooLong,
    #[error("user metadata is larger than 2 KB")]
    MetadataTooLarge,
    #[error("{0}")]
    InvalidArgument(String),
    #[error("{0}")]
    NotFound(String),
    /// `(key, version id)`: no such version of the key.
    #[error("no version {1} of {0}")]
    NoSuchVersion(String, String),
    /// The version asked for is a delete marker: asked for by its id (`specific`), or as the
    /// key's current version.
    #[error("{key} is a delete marker")]
    DeleteMarker { key: String, version_id: String, modified_at: i64, specific: bool },
    /// Object Lock forbids it (retention or legal hold).
    #[error("{0} is protected by Object Lock")]
    ObjectLocked(String),
    /// Not allowed in the bucket's state (versioning, Object Lock).
    #[error("{0}")]
    InvalidBucketState(String),
    /// A well-formed request S3 refuses (`InvalidRequest`).
    #[error("{0}")]
    InvalidRequest(String),
    /// This node can't make writes durable any more (another node took over the metadata log,
    /// object storage failed, or it was closed). Nothing more was acknowledged; restart.
    #[error("storage unavailable: {0}")]
    Unavailable(String),
    #[error(transparent)]
    Internal(#[from] anyhow::Error),
}

impl StoreError {
    pub(crate) fn internal(msg: impl std::fmt::Display) -> Self {
        Self::Internal(anyhow::anyhow!("{msg}"))
    }
}

impl From<rusqlite::Error> for StoreError {
    fn from(e: rusqlite::Error) -> Self {
        Self::Internal(e.into())
    }
}

impl From<serde_json::Error> for StoreError {
    fn from(e: serde_json::Error) -> Self {
        Self::Internal(e.into())
    }
}

impl From<std::io::Error> for StoreError {
    fn from(e: std::io::Error) -> Self {
        Self::Internal(e.into())
    }
}

pub type Result<T, E = StoreError> = std::result::Result<T, E>;
