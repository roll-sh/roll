//! postgres_fdw (contrib/postgres_fdw): foreign tables on other PostgreSQL servers, over the
//! engine's own client (`crate::remote`). Outbound connections go only where the operator
//! allows (`remote::outbound`).

pub mod connection;
pub mod deparse;
pub mod direct;
pub mod import;
pub mod modify;
pub mod option;
pub mod pushdown;
pub mod scan;

use super::native::Native;
use crate::error::{Error, Result, code};
use crate::sql::func::{FnCtx, SrfIter};
use crate::types::Value;

pub(super) fn symbol(sym: &str) -> Option<Native> {
    Some(match sym {
        "postgres_fdw_handler" => Native::scalar(|_, _| Ok(Value::Null)),
        "postgres_fdw_validator" => Native::scalar(|_, _| Ok(Value::Null)),
        "postgres_fdw_get_connections" => Native::set(|a, c| get_connections(a, c, false)),
        "postgres_fdw_get_connections_1_2" => Native::set(|a, c| get_connections(a, c, true)),
        "postgres_fdw_disconnect" => Native::scalar(disconnect),
        "postgres_fdw_disconnect_all" => Native::scalar(|_, c| disconnect_cached(c, None)),
        _ => return None,
    })
}

/// postgres_fdw_get_connections: the cached connections (v1.2: with the user, whether used
/// in the transaction, closed when `check_conn`, and the remote backend's pid).
fn get_connections(a: &[Value], c: &FnCtx, v12: bool) -> Result<SrfIter> {
    let cat = c.exec.catalog();
    let check = v12 && a.first().and_then(Value::as_bool).unwrap_or(false);
    let mut r = c.exec.sess.remote.borrow_mut();
    connection::sweep(&mut r.fdw, &cat);
    let mut out = Vec::new();
    for (key, e) in &mut r.fdw.entries {
        let Some(conn) = e.conn.as_mut() else { continue };
        let server = cat.servers.get(&e.server).map_or(Value::Null, |s| Value::text(s.name.as_str()));
        let mut row = vec![server];
        if v12 {
            row.push(cat.user_mappings.get(key).map_or(Value::Null, |m| {
                Value::text(if m.user == 0 {
                    "public".to_string()
                } else {
                    cat.roles.get(&m.user).map_or(m.user.to_string(), |r| r.name.clone())
                })
            }));
        }
        row.push(Value::Bool(!e.invalidated));
        if v12 {
            row.push(Value::Bool(e.xact_depth > 0));
            row.push(if check { Value::Bool(!conn.consume_input()) } else { Value::Null });
            row.push(Value::Int4(conn.pid));
        }
        out.push(row);
    }
    Ok(Box::new(out.into_iter().map(Ok)))
}

fn disconnect(a: &[Value], c: &FnCtx) -> Result<Value> {
    let name = a[0].as_str().unwrap_or("").to_string();
    let cat = c.exec.catalog();
    let Some(srv) = cat.servers.values().find(|s| s.name == name) else {
        return Err(Error::new(code::UNDEFINED_OBJECT, format!("server \"{name}\" does not exist")));
    };
    disconnect_cached(c, Some(srv.oid))
}

/// disconnect_cached_connections: close the server's (or every) cached connection not in
/// use by the transaction.
fn disconnect_cached(c: &FnCtx, server: Option<u32>) -> Result<Value> {
    let cat = c.exec.catalog();
    let mut result = false;
    let mut warnings = Vec::new();
    {
        let mut r = c.exec.sess.remote.borrow_mut();
        for e in r.fdw.entries.values_mut() {
            if e.conn.is_none() || server.is_some_and(|s| s != e.server) {
                continue;
            }
            if e.xact_depth > 0 {
                warnings.push(match cat.servers.get(&e.server) {
                    Some(s) => format!("cannot close connection for server \"{}\" because it is still in use", s.name),
                    None => "cannot close dropped server connection because it is still in use".to_string(),
                });
            } else {
                e.conn = None;
                result = true;
            }
        }
    }
    for w in warnings {
        let mut e = Error::new(code::WARNING, w);
        e.severity = crate::error::Severity::Warning;
        c.exec.notice(e);
    }
    Ok(Value::Bool(result))
}
