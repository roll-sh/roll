//! sslinfo (`contrib/sslinfo`): the connection's SSL details. The engine's server declines
//! SSLRequest (connections are unencrypted: TLS is the operator's proxy's), so every
//! connection is one without SSL: ssl_is_used() is false and the rest NULL, no extension
//! rows, as sslinfo.c answers for a non-SSL connection.

use super::native::Native;
use crate::sql::func::{FnCtx, SrfIter};
use crate::types::Value;

fn not_used(_: &[Value], _: &FnCtx) -> crate::Result<Value> {
    Ok(Value::Bool(false))
}

fn null(_: &[Value], _: &FnCtx) -> crate::Result<Value> {
    Ok(Value::Null)
}

fn no_rows(_: &[Value], _: &FnCtx) -> crate::Result<SrfIter> {
    Ok(Box::new(std::iter::empty()))
}

pub(super) fn symbol(sym: &str) -> Option<Native> {
    Some(match sym {
        "ssl_is_used" | "ssl_client_cert_present" => Native::scalar(not_used),
        "ssl_version" | "ssl_cipher" | "ssl_client_serial" | "ssl_client_dn_field" | "ssl_issuer_field"
        | "ssl_client_dn" | "ssl_issuer_dn" => Native::scalar(null),
        "ssl_extension_info" => Native::set(no_rows),
        _ => return None,
    })
}
