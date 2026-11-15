//! Errors returned by the codec.
//!
//! Readers distinguish three failure classes: the bytes are not a valid segment (`Corrupt`, which
//! includes checksum mismatches), the caller asked for something the segment cannot do
//! (`InvalidArgument`), and plain I/O failures while writing. Metadata parsing always returns
//! these errors; hot decode loops (postings, doc values) trust data that passed open-time
//! validation and checksums, so they stay branch-light.

use std::io;

/// A codec error.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// The bytes do not form a valid segment, or a checksum does not match.
    #[error("corrupt segment: {0}")]
    Corrupt(String),
    /// A caller-supplied argument is invalid for this segment or structure.
    #[error("{0}")]
    InvalidArgument(String),
    /// An I/O failure while writing or mapping a file.
    #[error(transparent)]
    Io(#[from] io::Error),
}

/// `Result` with [`Error`].
pub type Result<T, E = Error> = std::result::Result<T, E>;

pub(crate) fn corrupt(msg: impl Into<String>) -> Error {
    Error::Corrupt(msg.into())
}

pub(crate) fn invalid(msg: impl Into<String>) -> Error {
    Error::InvalidArgument(msg.into())
}

impl From<Error> for io::Error {
    fn from(e: Error) -> io::Error {
        match e {
            Error::Io(e) => e,
            Error::Corrupt(m) => io::Error::new(io::ErrorKind::InvalidData, m),
            Error::InvalidArgument(m) => io::Error::new(io::ErrorKind::InvalidInput, m),
        }
    }
}
