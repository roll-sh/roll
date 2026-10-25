//! hstore's index support functions (hstore_gist.c, hstore_gin.c) and its subscripting
//! handler (hstore_subs.c). They take and return `internal`, so SQL never calls them; the
//! engine's own index code answers hstore's operators (the operators' results are what an
//! index scan of either opclass returns after its recheck).

use crate::Result;
use crate::error::{Error, code};
use crate::ext::native::Native;
use crate::sql::func::FnCtx;
use crate::types::Value;

fn internal(_: &[Value], _: &FnCtx) -> Result<Value> {
    Err(Error::new(code::FEATURE_NOT_SUPPORTED, "cannot call a function taking type internal"))
}

/// `ghstore_in`.
fn ghstore_in(_: &[Value], _: &FnCtx) -> Result<Value> {
    Err(Error::new(code::FEATURE_NOT_SUPPORTED, "cannot accept a value of type ghstore"))
}

/// `ghstore_out`.
fn ghstore_out(_: &[Value], _: &FnCtx) -> Result<Value> {
    Err(Error::new(code::FEATURE_NOT_SUPPORTED, "cannot display a value of type ghstore"))
}

pub(super) fn symbol(sym: &str) -> Option<Native> {
    Some(Native::scalar(match sym {
        "ghstore_in" => ghstore_in,
        "ghstore_out" => ghstore_out,
        "ghstore_compress" | "ghstore_decompress" | "ghstore_penalty" | "ghstore_picksplit"
        | "ghstore_union" | "ghstore_same" | "ghstore_consistent" | "ghstore_options"
        | "gin_extract_hstore" | "gin_extract_hstore_query" | "gin_consistent_hstore"
        | "hstore_subscript_handler" => internal,
        _ => return None,
    }))
}
