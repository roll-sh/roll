//! The `postgresql` connector: every table of a schema with a primary key, read in key order
//! through a server-side cursor in one read-only snapshot per table, a page at a time.
//!
//! The host is the tenant's choice: it is resolved here and refused when any of its addresses is
//! private (unless the node allows private hosts), and the connection goes to the address that
//! was checked. A row's `_id` is `{database}_{schema}_{table}_{key values joined by _}`; its
//! columns are fields `{schema}_{table}_{column}`, next to `database`, `schema`, `table` and
//! `_timestamp`. With an `incremental_column`, incremental syncs read only rows at or after the
//! cursor the last sync left (that column's maximum in its snapshot, per table).

use super::config::Values;
use super::{DOC_BYTES_MAX, PAGE_BYTES, PAGE_DOCS, Sink, Stop};
use crate::json::{Map, Value};
use std::net::{IpAddr, SocketAddr};
use std::sync::Arc;
use std::time::Duration;
use tokio_postgres::config::SslMode;
use tokio_rustls::rustls;
use tokio_rustls::rustls::pki_types::CertificateDer;
use tokio_rustls::rustls::pki_types::pem::PemObject;

const CONNECT_TIMEOUT: Duration = Duration::from_secs(15);
/// What one statement may take, and how long a transaction may sit idle (a sync that stalls
/// can't hold a snapshot open).
const SESSION: &str = "-c statement_timeout=300000 -c idle_in_transaction_session_timeout=600000 -c standard_conforming_strings=on";

/// `"name"`, quotes doubled.
fn ident(name: &str) -> String {
    format!("\"{}\"", name.replace('"', "\"\""))
}

/// `'text'`, quotes doubled (`standard_conforming_strings` is on for the session).
fn literal(text: &str) -> String {
    format!("'{}'", text.replace('\'', "''"))
}

/// The host's first address, refusing a private one.
async fn resolve(host: &str, port: u16, allow_private: bool) -> Result<SocketAddr, String> {
    let addrs: Vec<SocketAddr> = match host.parse::<IpAddr>() {
        Ok(ip) => vec![SocketAddr::new(ip, port)],
        Err(_) => tokio::net::lookup_host((host, port)).await.map_err(|_| format!("[{host}] doesn't resolve"))?.collect(),
    };
    let addr = addrs.first().copied().ok_or_else(|| format!("[{host}] doesn't resolve"))?;
    if !allow_private && addrs.iter().any(|a| roll_net::private_ip(a.ip())) {
        return Err(format!("[{host}] is a private address: connectors only reach public ones"));
    }
    Ok(addr)
}

fn tls_config(ca: Option<&str>) -> Result<rustls::ClientConfig, String> {
    let mut roots = rustls::RootCertStore::empty();
    for c in rustls_native_certs::load_native_certs().certs {
        let _ = roots.add(c);
    }
    if let Some(pem) = ca {
        for c in CertificateDer::pem_slice_iter(pem.as_bytes()) {
            roots.add(c.map_err(|_| "[ssl_ca] isn't PEM certificates".to_string())?).map_err(|e| format!("[ssl_ca]: {e}"))?;
        }
    }
    Ok(rustls::ClientConfig::builder_with_provider(Arc::new(rustls::crypto::ring::default_provider()))
        .with_safe_default_protocol_versions()
        .map_err(|e| format!("TLS: {e}"))?
        .with_root_certificates(roots)
        .with_no_client_auth())
}

/// The server's own message (never the connection's settings).
fn db_error(e: tokio_postgres::Error) -> String {
    e.as_db_error().map_or_else(|| e.to_string(), |d| d.message().to_string())
}

struct Conn {
    client: tokio_postgres::Client,
    task: tokio::task::JoinHandle<()>,
}

impl Drop for Conn {
    fn drop(&mut self) {
        self.task.abort();
    }
}

async fn connect(v: &Values, allow_private: bool) -> Result<Conn, String> {
    let host = v.need("host")?;
    let port = u16::try_from(v.int("port", 5432)?).map_err(|_| "the connector's [port] is out of range".to_string())?;
    let mut cfg = tokio_postgres::Config::new();
    cfg.host(&host).port(port).user(v.need("username")?).dbname(v.need("database")?).connect_timeout(CONNECT_TIMEOUT).application_name("roll search connector").options(SESSION);
    if let Some(p) = v.text("password") {
        cfg.password(p);
    }
    let addr = resolve(&host, port, allow_private).await?;
    cfg.hostaddr(addr.ip());
    let connected = if v.bool("ssl_enabled", true) {
        cfg.ssl_mode(SslMode::Require);
        let tls = tokio_postgres_rustls::MakeRustlsConnect::new(tls_config(v.text("ssl_ca").as_deref())?);
        tokio::time::timeout(CONNECT_TIMEOUT, cfg.connect(tls)).await.map_err(|_| format!("connecting to [{host}] timed out"))?.map(|(c, conn)| (c, tokio::spawn(async move { let _ = conn.await; })))
    } else {
        tokio::time::timeout(CONNECT_TIMEOUT, cfg.connect(tokio_postgres::NoTls)).await.map_err(|_| format!("connecting to [{host}] timed out"))?.map(|(c, conn)| (c, tokio::spawn(async move { let _ = conn.await; })))
    };
    let (client, task) = connected.map_err(|e| format!("connecting to [{host}] failed: {}", db_error(e)))?;
    Ok(Conn { client, task })
}

/// A table to read: its name, primary key and incremental column's type (when it has it).
struct Table {
    name: String,
    key: Vec<String>,
    incremental: Option<String>,
}

async fn tables(c: &tokio_postgres::Client, schema: &str, wanted: &[String], incremental: Option<&str>) -> Result<(Vec<Table>, Vec<String>), String> {
    let rows = c
        .query("SELECT c.relname FROM pg_catalog.pg_class c JOIN pg_catalog.pg_namespace n ON n.oid = c.relnamespace WHERE n.nspname = $1 AND c.relkind IN ('r', 'p') ORDER BY 1", &[&schema])
        .await
        .map_err(|e| format!("listing the tables of [{schema}] failed: {}", db_error(e)))?;
    let all: Vec<String> = rows.iter().map(|r| r.get(0)).collect();
    let names: Vec<String> = if wanted.is_empty() || wanted.iter().any(|t| t == "*") {
        all
    } else {
        if let Some(missing) = wanted.iter().find(|t| !all.contains(t)) {
            return Err(format!("the table [{schema}.{missing}] doesn't exist"));
        }
        wanted.to_vec()
    };
    let mut out = Vec::new();
    let mut skipped = Vec::new();
    for name in names {
        let key: Vec<String> = c
            .query(
                "SELECT a.attname FROM pg_catalog.pg_index i JOIN pg_catalog.pg_class c ON c.oid = i.indrelid JOIN pg_catalog.pg_namespace n ON n.oid = c.relnamespace \
                 CROSS JOIN LATERAL unnest(i.indkey::int2[]) WITH ORDINALITY AS k(attnum, ord) JOIN pg_catalog.pg_attribute a ON a.attrelid = c.oid AND a.attnum = k.attnum \
                 WHERE i.indisprimary AND n.nspname = $1 AND c.relname = $2 ORDER BY k.ord",
                &[&schema, &name],
            )
            .await
            .map_err(|e| format!("reading the primary key of [{schema}.{name}] failed: {}", db_error(e)))?
            .iter()
            .map(|r| r.get(0))
            .collect();
        if key.is_empty() {
            skipped.push(name);
            continue;
        }
        let incremental = match incremental {
            Some(col) => c
                .query_opt(
                    "SELECT format_type(a.atttypid, a.atttypmod) FROM pg_catalog.pg_attribute a JOIN pg_catalog.pg_class c ON c.oid = a.attrelid JOIN pg_catalog.pg_namespace n ON n.oid = c.relnamespace \
                     WHERE n.nspname = $1 AND c.relname = $2 AND a.attname = $3 AND a.attnum > 0 AND NOT a.attisdropped",
                    &[&schema, &name, &col],
                )
                .await
                .map_err(|e| format!("reading [{schema}.{name}] failed: {}", db_error(e)))?
                .map(|r| r.get::<_, String>(0)),
            None => None,
        };
        out.push(Table { name, key, incremental });
    }
    Ok((out, skipped))
}

/// Syncs the configured tables; the new cursor (`{"tables": {"schema.table": value}}`).
pub async fn sync(v: &Values, sink: &mut Sink<'_>, full: bool, cursor: &Value, allow_private: bool) -> Result<Value, Stop> {
    let database = v.need("database")?;
    let schema = v.text("schema").unwrap_or_else(|| "public".into());
    let incremental_col = v.text("incremental_column");
    let fetch = (v.int("fetch_size", 50)?.clamp(1, PAGE_DOCS as i64)) as usize;
    let mut conn = connect(v, allow_private).await?;
    let (tables, skipped) = tables(&conn.client, &schema, &v.list("tables"), incremental_col.as_deref()).await?;
    if !skipped.is_empty() {
        sink.counters.metadata.insert("skipped_tables".into(), Value::Arr(skipped.into_iter().map(Value::from).collect()));
    }
    let mut next = cursor.get("tables").and_then(Value::as_object).cloned().unwrap_or_default();
    let ts = super::now_nanos_string();
    for t in &tables {
        let key = format!("{schema}.{}", t.name);
        let since = if full { None } else { next.get(&key).and_then(Value::as_str).map(str::to_string) };
        if let Some(max) = read_table(&mut conn, sink, &database, &schema, t, incremental_col.as_deref(), since.as_deref(), fetch, &ts).await? {
            next.insert(key, max.into());
        }
    }
    Ok(crate::obj! { "tables" => Value::Obj(next) })
}

/// Reads one table in one snapshot: the incremental column's maximum there, when it has one.
#[allow(clippy::too_many_arguments)]
async fn read_table(conn: &mut Conn, sink: &mut Sink<'_>, database: &str, schema: &str, t: &Table, col: Option<&str>, since: Option<&str>, fetch: usize, ts: &str) -> Result<Option<String>, Stop> {
    let qualified = format!("{}.{}", ident(schema), ident(&t.name));
    let err = |what: &str, e: tokio_postgres::Error| Stop::Failed(format!("{what} [{schema}.{}] failed: {}", t.name, db_error(e)));
    let tx = conn.client.build_transaction().isolation_level(tokio_postgres::IsolationLevel::RepeatableRead).read_only(true).start().await.map_err(|e| err("reading", e))?;
    let (col, ty) = match (col, &t.incremental) {
        (Some(c), Some(ty)) => (Some(c), ty.clone()),
        _ => (None, String::new()),
    };
    let max: Option<String> = match col {
        Some(c) => tx.query_one(&format!("SELECT max({})::text FROM {qualified}", ident(c)), &[]).await.map_err(|e| err("reading", e))?.get(0),
        None => None,
    };
    let filter = match (col, since) {
        (Some(c), Some(s)) => format!(" WHERE t.{} >= {}::{ty}", ident(c), literal(s)),
        _ => String::new(),
    };
    let order = t.key.iter().map(|k| format!("t.{}", ident(k))).collect::<Vec<_>>().join(", ");
    // A row over the limit comes back as NULL: its size is counted, never its bytes read.
    let sql = format!(
        "DECLARE roll_sync NO SCROLL CURSOR FOR SELECT CASE WHEN octet_length(j) > {DOC_BYTES_MAX} THEN NULL ELSE j END FROM (SELECT row_to_json(t)::text AS j FROM {qualified} t{filter} ORDER BY {order}) q"
    );
    tx.batch_execute(&sql).await.map_err(|e| err("reading", e))?;
    let prefix = format!("{schema}_{}_", t.name);
    let mut n = fetch;
    loop {
        let asked = n;
        let rows = tx.query(&format!("FETCH {asked} FROM roll_sync"), &[]).await.map_err(|e| err("reading", e))?;
        if rows.is_empty() {
            break;
        }
        let mut docs = Vec::with_capacity(rows.len());
        let mut bytes = 0usize;
        for r in &rows {
            let Some(j) = r.get::<_, Option<String>>(0) else {
                sink.skip();
                continue;
            };
            bytes += j.len();
            let Ok(Some(Value::Obj(row))) = crate::json::parse(j.as_bytes()) else {
                sink.skip();
                continue;
            };
            let ids: Vec<String> = t.key.iter().map(|k| row.get(k).map(Value::to_es_string).unwrap_or_default()).collect();
            let mut doc = Map::new();
            doc.insert("_id".into(), format!("{database}_{schema}_{}_{}", t.name, ids.join("_")).into());
            doc.insert("_timestamp".into(), ts.into());
            doc.insert("database".into(), database.into());
            doc.insert("schema".into(), schema.into());
            doc.insert("table".into(), t.name.clone().into());
            for (k, v) in row {
                doc.insert(format!("{prefix}{k}"), v);
            }
            docs.push(doc);
        }
        // The next page aims at PAGE_BYTES from the size of these rows.
        let avg = (bytes / rows.len()).max(1);
        n = (PAGE_BYTES / avg).clamp(1, fetch);
        let short = rows.len() < asked;
        sink.page(docs).await?;
        if short {
            break;
        }
    }
    tx.commit().await.map_err(|e| err("reading", e))?;
    Ok(max)
}
