//! The subscriber's side of the replication protocol (libpqwalreceiver.c): connections to a
//! publisher with `replication=database` (through the outbound allow-list, as dblink's
//! are), the slot commands, and the queries subscriptioncmds.c and tablesync.c run there.

use crate::remote::conn::{self, Conn, Env, Interrupt, PgResult, Status};
use crate::remote::conninfo;

/// A failure: the publisher's (or the connection's) error text, or an interrupt.
pub enum Fail {
    Remote { msg: String, sqlstate: Option<String> },
    Interrupt(crate::Error),
}

impl Fail {
    pub fn msg(&self) -> String {
        match self {
            Fail::Remote { msg, .. } => msg.clone(),
            Fail::Interrupt(e) => e.message.to_string(),
        }
    }
}

impl From<crate::Error> for Fail {
    fn from(e: crate::Error) -> Fail {
        Fail::Interrupt(e)
    }
}

fn remote(msg: impl Into<String>) -> Fail {
    Fail::Remote { msg: msg.into(), sqlstate: None }
}

/// walrcv_connect (logical): `conninfo` with `replication=database`, named `appname`.
pub fn connect(conninfo: &str, appname: &str, env: &Env) -> Result<Conn, String> {
    let mut info = conninfo::parse(conninfo)?;
    info.set("replication", "database");
    if info.get("fallback_application_name").is_none() {
        info.set("fallback_application_name", appname);
    }
    // libpqrcv_connect: values come in the formats the apply worker reads them in.
    info.set("options", "-c datestyle=ISO -c intervalstyle=postgres -c extra_float_digits=3");
    info.set("client_encoding", "UTF8");
    conn::connect(&info, env)
}

/// libpqrcv_connect's must_use_password (password_required, an owner who isn't superuser):
/// the publisher must have asked for a password.
pub fn password_check(c: &Conn, must_use_password: bool) -> crate::Result<()> {
    if must_use_password && !c.used_password {
        return Err(crate::Error::new("2F003", "password is required")
            .detail("Non-superuser cannot connect if the server does not request a password.")
            .hint("Target server's authentication method must be changed, or set password_required=false in the subscription parameters."));
    }
    Ok(())
}

/// walrcv_exec: a command or query, its result (the error as libpq reports it).
pub fn exec(c: &mut Conn, sql: &str, intr: Interrupt) -> Result<PgResult, Fail> {
    let r = c.exec(sql, intr)?;
    if r.status == Status::FatalError {
        let (msg, sqlstate) = match &r.error {
            Some(e) => (e.text().trim_end_matches('\n').to_string(), e.sqlstate().map(str::to_string)),
            None => (c.error_text(), None),
        };
        return Err(Fail::Remote { msg, sqlstate });
    }
    Ok(r)
}

/// quote_literal_cstr.
pub fn literal(s: &str) -> String {
    let body = s.replace('\'', "''");
    if s.contains('\\') {
        format!("E'{}'", body.replace('\\', "\\\\"))
    } else {
        format!("'{body}'")
    }
}

/// GetPublicationsStr: `'a', 'b'` (literals) or `"a", "b"`.
pub fn publications_str(pubs: &[String], literals: bool) -> String {
    pubs.iter()
        .map(|p| if literals { literal(p) } else { format!("\"{p}\"") })
        .collect::<Vec<_>>()
        .join(", ")
}

/// IDENTIFY_SYSTEM: the publisher's system identifier.
pub fn identify_system(c: &mut Conn, intr: Interrupt) -> Result<String, Fail> {
    let r = exec(c, "IDENTIFY_SYSTEM", intr).map_err(|e| remote(format!("could not receive database system identifier and timeline ID from the primary server: {}", e.msg())))?;
    Ok(r.rows.first().and_then(|r| r.first().cloned().flatten()).unwrap_or_default())
}

/// walrcv_create_slot (pgoutput): the slot's consistent point.
pub fn create_slot(
    c: &mut Conn,
    name: &str,
    temporary: bool,
    two_phase: bool,
    failover: bool,
    snapshot: &str,
    intr: Interrupt,
) -> Result<u64, Fail> {
    let mut cmd = format!("CREATE_REPLICATION_SLOT \"{name}\"");
    if temporary {
        cmd.push_str(" TEMPORARY");
    }
    cmd.push_str(" LOGICAL pgoutput (");
    if two_phase {
        cmd.push_str("TWO_PHASE, ");
    }
    if failover {
        cmd.push_str("FAILOVER, ");
    }
    cmd.push_str(&format!("SNAPSHOT '{snapshot}')"));
    let r = exec(c, &cmd, intr).map_err(|e| match e {
        Fail::Remote { msg, sqlstate } => Fail::Remote { msg: format!("could not create replication slot \"{name}\": {msg}"), sqlstate },
        e => e,
    })?;
    let point = r.rows.first().and_then(|r| r.get(1).cloned().flatten()).unwrap_or_default();
    crate::types::text::parse_lsn(&point).map_err(|_| remote(format!("invalid consistent point \"{point}\" from the publisher")))
}

/// DROP_REPLICATION_SLOT ... WAIT.
pub fn drop_slot(c: &mut Conn, name: &str, intr: Interrupt) -> Result<(), Fail> {
    exec(c, &format!("DROP_REPLICATION_SLOT \"{name}\" WAIT"), intr).map(|_| ())
}

/// check_publications: the publications the publisher doesn't have.
pub fn missing_publications(c: &mut Conn, pubs: &[String], intr: Interrupt) -> Result<Vec<String>, Fail> {
    let sql = format!(
        "SELECT t.pubname FROM\n pg_catalog.pg_publication t WHERE\n t.pubname IN ({})",
        publications_str(pubs, true)
    );
    let r = exec(c, &sql, intr)
        .map_err(|e| remote(format!("could not receive list of publications from the publisher: {}", e.msg())))?;
    let have: Vec<String> = r.rows.iter().filter_map(|r| r.first().cloned().flatten()).collect();
    Ok(pubs.iter().filter(|p| !have.contains(p)).cloned().collect())
}

/// fetch_table_list: the published tables, (schema, name).
pub fn fetch_table_list(c: &mut Conn, pubs: &[String], intr: Interrupt) -> Result<Vec<(String, String)>, Fail> {
    let sql = format!(
        "SELECT DISTINCT n.nspname, c.relname, gpt.attrs\n       FROM pg_class c\n         JOIN pg_namespace n ON n.oid = c.relnamespace\n         JOIN ( SELECT (pg_get_publication_tables(VARIADIC array_agg(pubname::text))).*\n                FROM pg_publication\n                WHERE pubname IN ( {} )) AS gpt\n             ON gpt.relid = c.oid\n",
        publications_str(pubs, true)
    );
    let r = exec(c, &sql, intr)
        .map_err(|e| remote(format!("could not receive list of replicated tables from the publisher: {}", e.msg())))?;
    let mut out: Vec<(String, String)> = r
        .rows
        .iter()
        .filter_map(|r| Some((r.first()?.clone()?, r.get(1)?.clone()?)))
        .collect();
    out.dedup();
    Ok(out)
}

/// fetch_remote_table_info's columns: the published, non-generated columns of `nsp.rel`, in
/// attribute order.
pub fn published_columns(c: &mut Conn, nsp: &str, rel: &str, pubs: &[String], intr: Interrupt) -> Result<Vec<String>, Fail> {
    let sql = format!(
        "SELECT a.attname FROM pg_catalog.pg_attribute a\
         \n  JOIN pg_catalog.pg_class c ON c.oid = a.attrelid\
         \n  JOIN pg_catalog.pg_namespace n ON n.oid = c.relnamespace\
         \n WHERE n.nspname = {} AND c.relname = {}\
         \n   AND a.attnum > 0::pg_catalog.int2 AND NOT a.attisdropped AND a.attgenerated = ''\
         \n   AND a.attnum = ANY (SELECT unnest(gpt.attrs) FROM pg_catalog.pg_publication p,\
         \n         LATERAL pg_catalog.pg_get_publication_tables(p.pubname) gpt\
         \n         WHERE gpt.relid = c.oid AND p.pubname IN ( {} ))\
         \n ORDER BY a.attnum",
        literal(nsp),
        literal(rel),
        publications_str(pubs, true)
    );
    let r = exec(c, &sql, intr)
        .map_err(|e| remote(format!("could not fetch table info for table \"{nsp}.{rel}\" from publisher: {}", e.msg())))?;
    Ok(r.rows.iter().filter_map(|r| r.first().cloned().flatten()).collect())
}

/// fetch_remote_table_info's relkind and row filters of a published table: its publications'
/// filters (none when one of them publishes it without a filter).
pub fn table_filters(c: &mut Conn, nsp: &str, rel: &str, pubs: &[String], intr: Interrupt) -> Result<(char, Vec<String>), Fail> {
    let fail = |e: Fail| remote(format!("could not fetch table info for table \"{nsp}.{rel}\" from publisher: {}", e.msg()));
    let class = format!(
        "SELECT c.oid, c.relkind FROM pg_catalog.pg_class c JOIN pg_catalog.pg_namespace n ON n.oid = c.relnamespace \
         WHERE n.nspname = {} AND c.relname = {}",
        literal(nsp),
        literal(rel)
    );
    let r = exec(c, &class, intr).map_err(fail)?;
    let Some([Some(oid), Some(kind)]) = r.rows.first().map(|r| [r.first().cloned().flatten(), r.get(1).cloned().flatten()]) else {
        return Err(remote(format!("table \"{nsp}.{rel}\" not found on publisher")));
    };
    let sql = format!(
        "SELECT DISTINCT pg_catalog.pg_get_expr(gpt.qual, gpt.relid)\
         \n  FROM pg_catalog.pg_publication p,\
         \n  LATERAL pg_catalog.pg_get_publication_tables(p.pubname) gpt\
         \n  WHERE gpt.relid = {oid} AND p.pubname IN ( {} )",
        publications_str(pubs, true)
    );
    let r = exec(c, &sql, intr).map_err(fail)?;
    let mut quals = Vec::new();
    for row in &r.rows {
        match row.first().cloned().flatten() {
            Some(q) => quals.push(q),
            // (A publication without a filter: every row.)
            None => return Ok((kind.chars().next().unwrap_or('r'), Vec::new())),
        }
    }
    Ok((kind.chars().next().unwrap_or('r'), quals))
}

/// walrcv_startstreaming (pgoutput, protocol 4, text values): `streaming` 'on' / 'parallel'
/// (None: off), prepared transactions decoded at PREPARE with `two_phase`.
#[allow(clippy::too_many_arguments)]
pub fn start_streaming(
    c: &mut Conn,
    slot: &str,
    start: u64,
    pubs: &[String],
    origin: &str,
    streaming: Option<&str>,
    two_phase: bool,
    binary: bool,
    intr: Interrupt,
) -> Result<(), Fail> {
    let names = pubs.iter().map(|p| format!("\"{}\"", p.replace('"', "\"\""))).collect::<Vec<_>>().join(",");
    let mut cmd = format!("START_REPLICATION SLOT \"{slot}\" LOGICAL {} (proto_version '4'", super::lsn::fmt(start));
    if let Some(s) = streaming {
        cmd.push_str(&format!(", streaming '{s}'"));
    }
    if two_phase {
        cmd.push_str(", two_phase 'on'");
    }
    cmd.push_str(&format!(", origin '{origin}', publication_names {}", literal(&names)));
    if binary {
        cmd.push_str(", binary 'true'");
    }
    cmd.push(')');
    c.start_copy_both(&cmd, intr)?.map_err(|m| remote(format!("could not start WAL streaming: {m}")))
}
