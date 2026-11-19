//! ClickHouse outside roll, over its HTTPS interface (port 8443, ClickHouse Cloud's too): the same
//! guards as Postgres (a public address resolved here and pinned, or a gateway's stream; TLS
//! verified against the URL's host with the system's roots and `ca`; no redirects, no proxy:
//! https.rs). Users are `CREATE USER`s with
//! `sha256_hash` passwords, so the server never sees one; a dynamic login is granted the group (a
//! role) as its default. ClickHouse has no expiry for a user: a dynamic login lasts until roll
//! ends its lease. Values go as query parameters, never into SQL; names roll checked (lowercase
//! letters, digits, _) are quoted as identifiers. Answers are read up to [`ANSWER_MAX`].

use super::https::{Https, secret};
use super::{TIMEOUT, Via};
use sha2::{Digest, Sha256};

/// The most of one answer read: counts and errors.
const ANSWER_MAX: usize = 64 * 1024;

/// The URL's settings: `clickhouse://user:password@host:8443/db`, nothing else.
pub(super) struct Url {
    host: String,
    port: u16,
    user: String,
    password: String,
    db: Option<String>,
}

pub(super) fn parse(url: &str) -> Result<Url, String> {
    let bad = || "the URL is clickhouse://user:password@host:8443/db (ClickHouse's HTTPS port)".to_string();
    let u = url::Url::parse(url).map_err(|_| bad())?;
    let host = match u.host() {
        Some(url::Host::Domain(d)) => d.to_string(),
        Some(url::Host::Ipv4(ip)) => ip.to_string(),
        Some(url::Host::Ipv6(ip)) => ip.to_string(),
        None => return Err(bad()),
    };
    if u.scheme() != "clickhouse" || u.username().is_empty() {
        return Err(bad());
    }
    if let Some((k, _)) = u.query_pairs().next() {
        return Err(format!("the URL takes no settings, not {k}"));
    }
    let decode = |s: &str| super::mysql::percent_decode(s).ok_or_else(bad);
    let db = u.path().trim_start_matches('/');
    Ok(Url { host, port: u.port().unwrap_or(8443), user: decode(u.username())?, password: decode(u.password().unwrap_or(""))?, db: (!db.is_empty()).then(|| decode(db)).transpose()? })
}

pub(super) struct Conn {
    http: Https,
    user: String,
    password: String,
    db: Option<String>,
}

fn connect(u: &Url, ca: Option<&str>, via: &Via) -> Result<Conn, String> {
    Ok(Conn { http: Https::new(&u.host, u.port, ca, via.clone())?, user: u.user.clone(), password: u.password.clone(), db: u.db.clone() })
}

/// Run one statement with `{name:String}` parameters; its answer (bounded).
async fn query(c: &Conn, sql: &str, params: &[(&str, &str)]) -> Result<String, String> {
    let mut q = url::form_urlencoded::Serializer::new(String::new());
    if let Some(db) = &c.db {
        q.append_pair("database", db);
    }
    for (k, v) in params {
        q.append_pair(&format!("param_{k}"), v);
    }
    let q = q.finish();
    let path = if q.is_empty() { "/".to_string() } else { format!("/?{q}") };
    let headers = vec![("x-clickhouse-user", secret(&c.user, "user")?), ("x-clickhouse-key", secret(&c.password, "password")?)];
    let (status, body) = c.http.send(hyper::Method::POST, &path, headers, sql.as_bytes().to_vec(), ANSWER_MAX).await?;
    let text = String::from_utf8_lossy(&body).trim().to_string();
    if !status.is_success() {
        // ClickHouse's message ("Code: 516. DB::Exception: …"), clipped.
        return Err(text.chars().take(300).collect());
    }
    Ok(text)
}

/// `` `name` `` (roll-secrets checked the name: lowercase letters, digits and _).
fn ident(name: &str) -> Result<String, String> {
    if name.is_empty() || !name.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_') {
        return Err(format!("{name} isn't a name roll uses"));
    }
    Ok(format!("`{name}`"))
}

/// What ClickHouse keeps for a password: its SHA-256, in hex.
fn hash(password: &str) -> String {
    Sha256::digest(password.as_bytes()).iter().map(|b| format!("{b:02x}")).collect()
}

pub(super) async fn run<T>(u: Url, ca: Option<String>, via: Via, work: impl AsyncFnOnce(&Conn) -> Result<T, String>) -> Result<T, String> {
    tokio::time::timeout(TIMEOUT, async move {
        let c = connect(&u, ca.as_deref(), &via)?;
        work(&c).await
    })
    .await
    .map_err(|_| "timed out".to_string())?
}

async fn count(c: &Conn, sql: &str, name: &str) -> Result<bool, String> {
    Ok(query(c, sql, &[("n", name)]).await? != "0")
}

pub(super) async fn check(c: &Conn, roles: &[String], group: Option<&str>) -> Result<(), String> {
    // HTTP connects on the first request: this one proves TLS and the login, whatever else is asked.
    query(c, "SELECT 1", &[]).await?;
    for r in roles {
        if !count(c, "SELECT count() FROM system.users WHERE name = {n:String}", r).await? {
            return Err(format!("there's no user {r}"));
        }
    }
    if let Some(g) = group
        && !count(c, "SELECT count() FROM system.roles WHERE name = {n:String}", g).await?
    {
        return Err(format!("there's no role {g}"));
    }
    Ok(())
}

pub(super) async fn set_password(c: &Conn, role: &str, password: &str) -> Result<(), String> {
    query(c, &format!("ALTER USER {} IDENTIFIED WITH sha256_hash BY '{}'", ident(role)?, hash(password)), &[]).await.map(|_| ())
}

pub(super) async fn create_login(c: &Conn, name: &str, password: &str, group: &str) -> Result<(), String> {
    let (who, group) = (ident(name)?, ident(group)?);
    query(c, &format!("CREATE USER {who} IDENTIFIED WITH sha256_hash BY '{}'", hash(password)), &[]).await?;
    let granted = async {
        query(c, &format!("GRANT {group} TO {who}"), &[]).await?;
        query(c, &format!("ALTER USER {who} DEFAULT ROLE {group}"), &[]).await
    }
    .await;
    if granted.is_err() {
        let _ = query(c, &format!("DROP USER IF EXISTS {who}"), &[]).await;
    }
    granted.map(|_| ())
}

pub(super) async fn drop_login(c: &Conn, name: &str) -> Result<(), String> {
    let who = ident(name)?;
    // Dropped first, so it can start nothing new; then what it's still running stops (KILL QUERY
    // matches by name, where the admin may).
    query(c, &format!("DROP USER IF EXISTS {who}"), &[]).await?;
    let _ = query(c, "KILL QUERY WHERE user = {n:String} ASYNC", &[("n", name)]).await;
    Ok(())
}
