//! The native code behind the extensions' `LANGUAGE C` functions, by library and symbol
//! (what `dlopen` / `dlsym` find in PostgreSQL).

use crate::sql::func::{FnCtx, FnImpl, SrfImpl};
use crate::types::Value;

/// A C function's implementation: a scalar one, or a set-returning one.
#[derive(Clone, Copy)]
pub struct Native {
    pub imp: FnImpl,
    pub srf: Option<SrfImpl>,
}

fn not_scalar(_: &[Value], _: &FnCtx) -> crate::Result<Value> {
    Err(crate::error::internal("set-valued function called in context that cannot accept a set"))
}

impl Native {
    pub const fn scalar(imp: FnImpl) -> Native {
        Native { imp, srf: None }
    }

    pub const fn set(srf: SrfImpl) -> Native {
        Native {
            imp: not_scalar,
            srf: Some(srf),
        }
    }
}

/// The library a function's `probin` names: `$libdir/pgcrypto` (or a path, with or without
/// the `.so` suffix) is `pgcrypto`.
pub fn library(probin: &str) -> &str {
    let base = probin.rsplit('/').next().unwrap_or(probin);
    base.strip_suffix(".so").unwrap_or(base)
}

/// Whether the engine has native code for library `lib` (so a symbol it lacks is an error,
/// as `dlsym` failing is).
pub fn has_library(lib: &str) -> bool {
    matches!(lib, "pgcrypto" | "uuid-ossp")
}

/// The implementation of `symbol` in the library `probin` names.
pub fn lookup(probin: &str, symbol: &str) -> Option<Native> {
    match library(probin) {
        "pgcrypto" => super::pgcrypto::symbol(symbol),
        "uuid-ossp" => super::uuid_ossp::symbol(symbol),
        _ => None,
    }
}
