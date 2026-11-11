//! Connections the engine makes to PostgreSQL servers (dblink, postgres_fdw): libpq's
//! connection strings (`conninfo`), a blocking wire-protocol client (`conn`) and the
//! operator's allow-list of destinations (`outbound`).

pub mod conn;
pub mod conninfo;
pub mod outbound;

use crate::error::{Error, Result, code};
use crate::exec::ExecCtx;
use crate::session::state::SessionState;

/// A session's connections to other servers.
#[derive(Default)]
pub struct SessionConns {
    pub dblink: crate::ext::dblink::Conns,
    pub fdw: crate::ext::postgres_fdw::connection::Conns,
}

/// Where this session's outbound connections may go and how the engine itself is reached.
pub fn env(sess: &SessionState) -> conn::Env {
    let local_port = u16::try_from(sess.settings.borrow().int("port")).unwrap_or(5432);
    conn::Env {
        outbound: sess.cluster.outbound().cloned(),
        local: sess.cluster.listen_addr(),
        local_port,
        default_user: sess.cluster.bootstrap_superuser(),
    }
}

/// CHECK_FOR_INTERRUPTS while waiting for a remote server: a cancel request or the
/// statement's timeout.
pub fn interrupts<'a>(ctx: &'a ExecCtx<'_>) -> impl Fn() -> Result<()> + 'a {
    move || {
        if let Ok(t) = ctx.txn.try_borrow() {
            t.check_cancel()?;
        }
        if let Some(d) = ctx.deadline
            && std::time::Instant::now() > d
        {
            return Err(Error::new(code::QUERY_CANCELED, "canceling statement due to statement timeout"));
        }
        Ok(())
    }
}

/// A remote error's SQLSTATE as the engine's (interned once).
pub fn sqlstate(s: Option<&str>, default: &'static str) -> &'static str {
    match s {
        Some(s) if s.len() == 5 => crate::plpgsql::intern_sqlstate(s),
        _ => default,
    }
}
