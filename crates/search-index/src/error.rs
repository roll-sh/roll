//! Errors of the shard engine.
//!
//! Lower layers fail with [`search_codec::Error`] or I/O errors; everything a user can cause
//! (a bad query, an immense term, a version conflict surfaced as an error) carries the
//! Elasticsearch exception `type` and `reason` text so `search-es` can render it unchanged.
//! Two variants are control flow, not failures: [`Error::CollectionTerminated`] ends the
//! collection of one segment early (Lucene's `CollectionTerminatedException`) and
//! [`Error::TimeExceeded`] ends a search whose `timeout` passed (partial results are kept).

use std::borrow::Cow;

/// Result alias of the crate.
pub type Result<T, E = Error> = std::result::Result<T, E>;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error(transparent)]
    Codec(#[from] search_codec::Error),
    #[error(transparent)]
    Io(#[from] std::io::Error),
    /// An Elasticsearch-typed failure (`illegal_argument_exception`, `too_many_clauses`, …).
    #[error("{reason}")]
    Es {
        kind: Cow<'static, str>,
        reason: String,
        status: u16,
    },
    /// The collector does not need more hits from the current segment.
    #[error("collection terminated")]
    CollectionTerminated,
    /// The search ran out of time; collected hits so far are valid (`timed_out: true`).
    #[error("time exceeded")]
    TimeExceeded,
    /// The search was cancelled by its caller.
    #[error("task cancelled")]
    Cancelled,
    /// The shard was closed.
    #[error("shard is closed")]
    Closed,
    /// A write that failed after it was given a sequence number (an immense term, a doc value
    /// too large, …): `source` is the failure. The number is used up, so the caller records
    /// it as a no-op in its WAL, as Elasticsearch writes a `NoOp` to the translog for a
    /// document-level Lucene failure; otherwise the local checkpoint stops below it after a
    /// recovery. Type and status are the source's.
    #[error("{source}")]
    Rejected { seq_no: i64, primary_term: i64, source: Box<Error> },
}

impl Error {
    /// `illegal_argument_exception` (400).
    pub fn illegal(reason: impl Into<String>) -> Error {
        Error::Es { kind: Cow::Borrowed("illegal_argument_exception"), reason: reason.into(), status: 400 }
    }

    /// `illegal_state_exception` (500).
    pub fn illegal_state(reason: impl Into<String>) -> Error {
        Error::Es { kind: Cow::Borrowed("illegal_state_exception"), reason: reason.into(), status: 500 }
    }

    /// An Elasticsearch exception of any type.
    pub fn es(kind: &'static str, reason: impl Into<String>, status: u16) -> Error {
        Error::Es { kind: Cow::Borrowed(kind), reason: reason.into(), status }
    }

    /// Lucene's `IndexSearcher.TooManyClauses` as Elasticsearch reports it.
    pub fn too_many_clauses(max: usize) -> Error {
        Error::es("too_many_clauses", format!("maxClauseCount is set to {max}"), 400)
    }

    /// Lucene's `IndexSearcher.TooManyNestedClauses`.
    pub fn too_many_nested_clauses(max: usize) -> Error {
        Error::es(
            "too_many_nested_clauses",
            format!("Query contains too many nested clauses; maxClauseCount is set to {max}"),
            400,
        )
    }

    /// The sequence number and term a failed write used up ([`Error::Rejected`]).
    pub fn seq_no(&self) -> Option<(i64, i64)> {
        match self {
            Error::Rejected { seq_no, primary_term, .. } => Some((*seq_no, *primary_term)),
            _ => None,
        }
    }

    /// The Elasticsearch exception type of this error.
    pub fn kind(&self) -> &str {
        match self {
            Error::Rejected { source, .. } => source.kind(),
            Error::Es { kind, .. } => kind,
            Error::Codec(search_codec::Error::InvalidArgument(_)) => "illegal_argument_exception",
            Error::Codec(_) => "corrupt_index_exception",
            Error::Io(_) => "i_o_exception",
            Error::CollectionTerminated => "collection_terminated_exception",
            Error::TimeExceeded => "time_exceeded_exception",
            Error::Cancelled => "task_cancelled_exception",
            Error::Closed => "already_closed_exception",
        }
    }

    /// HTTP status Elasticsearch answers with.
    pub fn status(&self) -> u16 {
        match self {
            Error::Rejected { source, .. } => source.status(),
            Error::Es { status, .. } => *status,
            Error::Codec(search_codec::Error::InvalidArgument(_)) => 400,
            Error::Cancelled => 400,
            _ => 500,
        }
    }

    /// Whether this is control flow ([`Error::CollectionTerminated`]).
    pub fn is_terminated(&self) -> bool {
        matches!(self, Error::CollectionTerminated)
    }
}

impl From<search_analysis::AnalysisError> for Error {
    fn from(e: search_analysis::AnalysisError) -> Error {
        Error::Es { kind: Cow::Borrowed(e.kind), reason: e.reason, status: e.status }
    }
}
