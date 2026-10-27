//! The CQL native protocol, versions 3, 4 and 5 (and Cassandra 5.0's 6-beta), both ways:
//! servers decode requests and encode responses, clients the reverse.
//!
//! - [`frame`]: the 9-byte envelope header, flags and opcodes; [`compress`]: v3/v4 body
//!   compression (lz4, snappy); [`segment`]: v5's CRC-protected outer framing.
//! - [`request`] / [`response`]: one type per message, encoded and decoded against a [`Ctx`]
//!   (version, [`Dialect`], negotiated ScyllaDB [`Features`]) that gates every field.
//!   [`result`], [`event`], [`error_body`] and [`types`] hold RESULT bodies, events, ERROR
//!   bodies and column types ([`ColType`], independent of `nosql-types`).
//! - [`codec`]: `tokio_util` codecs. The server decoder also answers bad envelopes the way
//!   each reference does (negotiation errors, size limits, corrupt segments).
//! - [`supported`]: the exact SUPPORTED maps of both references.
//! - `client` (feature `client`): an async client for the CLI and the differential harness.
//!
//! Decoding is bounded: nothing is allocated from a peer's count or length beyond the bytes
//! actually present, and malformed input becomes a [`DecodeError`] whose
//! [`to_error`](DecodeError::to_error) renders the reference's message. Values and row cells
//! are `Bytes` slices of the received body, never copies.

pub mod codec;
pub mod compress;
pub mod consistency;
pub mod error;
pub mod error_body;
pub mod event;
pub mod frame;
pub mod request;
pub mod response;
pub mod result;
pub mod segment;
pub mod supported;
pub mod types;
pub mod version;
pub mod wire;

#[cfg(feature = "client")]
pub mod client;

pub use codec::{ClientCodec, CodecError, Inbound, Limits, Phase, Rejection, ServerCodec, ServerDecoder, ServerEncoder};
pub use compress::Compression;
pub use consistency::Consistency;
pub use error::{DecodeError, Field};
pub use error_body::{ErrorBody, ErrorDetail, code};
pub use event::{Event, EventType, SchemaChange, SchemaChangeType, SchemaTarget, StatusChangeType, TopologyChangeType};
pub use frame::{Flags, Frame, Header, Opcode};
pub use request::{
    Batch, BatchQuery, BatchStatement, BatchType, CustomPayload, Execute, Prepare, Query, QueryParams, Request, RequestFrame,
    Startup,
};
pub use response::{Response, ResponseFrame};
pub use result::{Prepared, PreparedMetadata, ResultBody, Rows, RowsBuilder, RowsMetadata};
pub use supported::Supported;
pub use types::{ColSpec, ColType};
pub use version::{Ctx, Dialect, Direction, Features, Version};
pub use wire::{PutExt, Reader, Value};
