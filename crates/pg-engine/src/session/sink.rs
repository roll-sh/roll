//! Where a statement's rows go as the executor makes them (PostgreSQL's DestReceiver).
//!
//! A top-level SELECT (PORTAL_ONE_SELECT) hands each row to the session's `RowSink` as its
//! executor returns it, as PostgreSQL's printtup sends a DataRow per row: the client has the
//! rows made before an error, notices interleave with rows, and a result of any size takes
//! no more memory than a row. Without a sink the rows are collected into the statement's
//! `QueryResult`, as before. What PostgreSQL runs to completion before sending anything
//! (INSERT / UPDATE / DELETE / MERGE ... RETURNING, a SELECT with a data-modifying WITH:
//! PORTAL_ONE_RETURNING / PORTAL_ONE_MOD_WITH; EXECUTE and FETCH: PORTAL_UTIL_SELECT) keeps
//! collecting.

use crate::error::{Error, Result};
use crate::exec::{ExecCtx, Row};
use crate::sql::plan::OutCol;
use crate::types::text::FormatCtx;

/// A statement's rows as they are made: a server connection's DataRows, or an embedder's
/// callback (`Session::query_each`).
pub trait RowSink {
    /// ExecutorRun starts (the DestReceiver's rStartup): the rows that follow are `cols`'.
    /// A simple query's RowDescription goes out here; an error from here on follows it.
    fn begin(&mut self, cols: &[OutCol]) -> Result<()>;

    /// One row (receiveSlot). `fmt` renders values as text with the settings in force now
    /// (an output function reads them when it is called). An error fails the statement.
    fn row(&mut self, row: Row, cols: &[OutCol], fmt: &FormatCtx) -> Result<()>;

    /// Whether COPY TO STDOUT's data goes here as it is made (`copy_begin` / `copy_data` /
    /// `copy_end`); false: it is collected into `Outcome::CopyOut`.
    fn takes_copy(&self) -> bool {
        false
    }

    /// COPY TO STDOUT starts (CopyOutResponse), before its query runs.
    fn copy_begin(&mut self, _binary: bool, _ncols: usize) -> Result<()> {
        Err(crate::error::internal("this row sink takes no COPY data"))
    }

    /// One CopyData message.
    fn copy_data(&mut self, _data: &[u8]) -> Result<()> {
        Err(crate::error::internal("this row sink takes no COPY data"))
    }

    /// The data is complete (CopyDone); the query's AFTER triggers fire after it.
    fn copy_end(&mut self) -> Result<()> {
        Ok(())
    }
}

/// A callback as a sink (`Session::query_each`).
pub(crate) struct FnSink<F>(pub F);

impl<F: FnMut(&[OutCol], Option<Row>) -> Result<()>> RowSink for FnSink<F> {
    fn begin(&mut self, cols: &[OutCol]) -> Result<()> {
        (self.0)(cols, None)
    }

    fn row(&mut self, row: Row, cols: &[OutCol], _fmt: &FormatCtx) -> Result<()> {
        (self.0)(cols, Some(row))
    }
}

/// The rows of one statement on their way to a sink: counted, rendered with the settings
/// in force as each goes out.
pub(crate) struct Feed<'a> {
    pub sink: &'a mut dyn RowSink,
    pub cols: &'a [OutCol],
    fmt: Option<FormatCtx>,
    pub sent: usize,
}

impl<'a> Feed<'a> {
    pub fn new(sink: &'a mut dyn RowSink, cols: &'a [OutCol]) -> Self {
        Feed { sink, cols, fmt: None, sent: 0 }
    }

    pub fn row(&mut self, ctx: &ExecCtx, row: Row) -> Result<()> {
        // A cancel (or statement_timeout) while rows go out stops them, as the executor's
        // CHECK_FOR_INTERRUPTS would, whichever node makes them.
        ctx.check()?;
        let generation = ctx.sess.settings.borrow().generation();
        if self.fmt.as_ref().is_none_or(|f| f.settings != generation) {
            self.fmt = Some(format_ctx(ctx));
        }
        self.sent += 1;
        self.sink.row(row, self.cols, self.fmt.as_ref().unwrap())
    }
}

/// How a row's values are rendered now (`Session::format_ctx`, from a running statement).
pub(crate) fn format_ctx(ctx: &ExecCtx) -> FormatCtx {
    let mut f = ctx.sess.settings.borrow().format_ctx();
    f.names = Some(std::sync::Arc::new(crate::catalog::names::CatalogNames::new(
        ctx.catalog(),
        ctx.sess.search_path(),
    )));
    f
}

/// The error a sink returns when the client connection is gone (ClientConnectionLost).
pub fn connection_lost(e: &std::io::Error) -> Error {
    Error::new(
        "08006", // connection_failure
        format!("could not send data to client: {e}"),
    )
    .fatal()
}
