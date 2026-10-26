//! tsm_system_rows and tsm_system_time: TABLESAMPLE methods. The handlers only name the
//! method (`binder::select` resolves `TABLESAMPLE system_rows (...)` to the handler function
//! the extension made); the scan samples with `exec::scan::Sampler`.

use super::native::Native;
use crate::sql::func::FnCtx;
use crate::types::Value;

/// A tablesample method handler isn't called as a function (its argument is `internal`).
fn handler(_: &[Value], _: &FnCtx) -> crate::Result<Value> {
    Err(crate::error::internal("a tablesample method handler cannot be called directly"))
}

pub(super) fn symbol(sym: &str) -> Option<Native> {
    match sym {
        "tsm_system_rows_handler" | "tsm_system_time_handler" => Some(Native::scalar(handler)),
        _ => None,
    }
}

/// The TABLESAMPLE method a handler function of library `lib` (symbol `sym`) is.
pub fn method(lib: &str, sym: &str) -> Option<&'static str> {
    match (lib, sym) {
        ("tsm_system_rows", "tsm_system_rows_handler") => Some("system_rows"),
        ("tsm_system_time", "tsm_system_time_handler") => Some("system_time"),
        _ => None,
    }
}
