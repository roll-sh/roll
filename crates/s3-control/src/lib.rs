//! roll storage control plane: `/v1/storage/...`, merged into rolld's API.
//!
//! Buckets, branches, access keys, browsing, uploads and downloads for the dashboard and the CLI,
//! presigned URLs, the trash, usage and its cost at S3 list prices. Everything goes through
//! [`s3_store::Store`]; uploads take the same path as PutObject ([`s3_server::put_object`]).
//! The contract is in `docs/storage/architecture.md`.

pub mod api;
mod console;
pub mod control;
mod devapi;
pub mod roles;
pub mod settings;
mod targets;

pub use control::{Control, ControlConfig, ControlError, Pricing};
