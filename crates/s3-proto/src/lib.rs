//! roll storage: the S3 wire protocol, with no I/O. SigV4 (headers, presigned URLs, presigned
//! POST policies, `aws-chunked` payloads with trailers), XML bodies, error codes, checksums and
//! ETags, ranges and conditional requests. `s3-server` does the networking and storage.
//!
//! Peer-sent sizes never size an allocation: the chunked decoder streams, and XML parsers are
//! capped at [`xml::MAX_XML_BYTES`].

pub mod checksum;
pub mod chunked;
pub mod error;
pub mod http;
pub mod sigv4;
pub mod time;
pub mod xml;

pub use error::{Code, S3Error};

/// The S3 XML namespace.
pub const XMLNS: &str = "http://s3.amazonaws.com/doc/2006-03-01/";
