//! roll storage: metadata with a durable log in object storage, branching, refcounts, trash,
//! the blob backend and garbage collection. No HTTP: `s3-server` and `s3-control` sit on top.
//!
//! See `docs/storage/architecture.md`. In short:
//!
//! - One customer object is one immutable blob in object storage (`s3/blobs/…`).
//! - Metadata lives in SQLite, made durable by a log of group commits in object storage
//!   ([`log`]); the local file is a cache rebuilt on every start.
//! - Branches are copy-on-write; copies share blobs; deletes go to a trash, and unreferenced
//!   blobs wait out a quarantine before they're deleted.
//!
//! Writing an object (`s3-server`'s PutObject):
//!
//! ```ignore
//! let bucket = store.bucket(name)?;                       // NoSuchBucket
//! let blob = store.new_blob();
//! let up = store.put_blob(&blob, &bucket.id, key, body, len, content_type).await?;
//! let meta = store.commit_put(name, key, NewObject { blob_id: blob, size: up.size, crc32c: up.crc32c, etag, headers, checksum }, &cond).await?;
//! ```

pub mod access_points;
pub mod accounts;
mod append;
pub mod backend;
pub mod config;
mod db;
pub mod derived;
mod error;
pub mod events;
pub mod express;
pub mod iam;
mod ids;
mod jobs;
mod lifecycle;
pub mod log;
mod meter;
mod model;
mod objects;
mod rename;
mod rows;
mod schema;
pub mod sse;
mod store;
pub mod sts;
pub mod targets;
mod throttle;
pub mod torrent;
pub mod validate;
mod versioning;
mod versions;

pub use accounts::{Account, AccountUpdate};
pub use backend::{Backend, BlobAttrs, BlobSse, ByteStream, CopySource, CustomerKey, KmsParams, Listed, LocalBackend, S3Backend, S3Location, SseOp, SseUnsupported, Uploaded, UploadedPart, bytes_body, from_config, sse_c_unsupported, test_backend};
pub use config::{PublicAccessBlock, RedirectAll, Redirect, RoutingCondition, RoutingRule, WebsiteConfig};
pub use sse::Encryption;
pub use derived::{Derived, ImageUsage, NewDerived};
pub use error::{Result, StoreError};
pub use access_points::{AccessPoint, AccessPointKind, NewAccessPoint};
pub use events::{EventRecord, InventoryEntry, NotificationConfig, NotificationTarget, Origin, PendingEvent, ReplicaSource, ReplicaVersion, ReplicaWrite, ReplicateTask, ReplicationBacklog, ReplicationConfig, ReplicationRule, Replicated, RuleFilter};
pub use targets::{ReplicationTarget, Secret, TargetSpec};
pub use ids::{blob_key, day_of, is_temporary_key_id, version_id};
pub use log::LogStats;
pub use meter::{Class, Counters, Meter};
pub use model::*;
pub use append::{AppendBase, PreparedAppend};
pub use objects::multipart_etag_of;
pub use rename::RenameCondition;
pub use store::{Clock, Store, StoreConfig, StoreStats, system_clock};
