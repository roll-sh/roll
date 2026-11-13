//! MySQL servers outside roll: the same guards as Postgres (a public address resolved here, TLS
//! verified against the URL's host, no socket), accounts `'<name>'@'%'`. MySQL has no
//! `VALID UNTIL`: a dynamic login ends when roll ends its lease. Passwords go in `IDENTIFIED BY`
//! (over TLS; MySQL rewrites them out of its logs).

use super::{TIMEOUT, resolve};
use mysql_async::prelude::Queryable;
use mysql_async::{Conn, OptsBuilder, SslOpts};

/// The URL's settings: `mysql://user:password@host:3306/db`, `ssl-mode` and nothing else.
pub(super) struct Url {
    host: String,
    port: u16,
    user: String,
    password: String,
    db: Option<String>,
    tls: bool,
}

pub(super) fn parse(url: &str, allow_private: bool) -> Result<Url, String> {
    let bad = || "the URL is mysql://user:password@host:3306/db".to_string();
    let u = url::Url::parse(url).map_err(|_| bad())?;
    let host = match u.host() {
        Some(url::Host::Domain(d)) => d.to_string(),
        Some(url::Host::Ipv4(ip)) => ip.to_string(),
        Some(url::Host::Ipv6(ip)) => ip.to_string(),
        None => return Err(bad()),
    };
    if u.scheme() != "mysql" || u.username().is_empty() {
        return Err(bad());
    }
    let mut tls = true;
    for (k, v) in u.query_pairs() {
        match (&*k, &*v.to_ascii_uppercase()) {
            ("ssl-mode", "DISABLED") if !allow_private => return Err("roll only sends a password over TLS: drop ssl-mode=DISABLED".into()),
            ("ssl-mode", "DISABLED") => tls = false,
            ("ssl-mode", _) => {}
            (k, _) => return Err(format!("the URL may set ssl-mode, but not {k}")),
        }
    }
    let decode = |s: &str| percent_decode(s).ok_or_else(bad);
    let db = u.path().trim_start_matches('/');
    Ok(Url { host, port: u.port().unwrap_or(3306), user: decode(u.username())?, password: decode(u.password().unwrap_or(""))?, db: (!db.is_empty()).then(|| db.to_string()), tls })
}

fn percent_decode(s: &str) -> Option<String> {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'%' {
            out.push(u8::from_str_radix(s.get(i + 1..i + 3)?, 16).ok()?);
            i += 3;
        } else {
            out.push(b[i]);
            i += 1;
        }
    }
    String::from_utf8(out).ok()
}

async fn connect(u: &Url, ca: Option<&str>, allow_private: bool) -> Result<Conn, String> {
    let addr = resolve(&u.host, u.port, allow_private).await?;
    let mut opts = OptsBuilder::default()
        .ip_or_hostname(addr.ip().to_string())
        .tcp_port(addr.port())
        .user(Some(u.user.clone()))
        .pass(Some(u.password.clone()))
        .db_name(u.db.clone())
        // Never reconnect over a socket the server names, nor take its packet size (up to 1 GiB).
        .prefer_socket(false)
        .max_allowed_packet(Some(4 << 20));
    if u.tls {
        let roots = ca.map(|pem| vec![pem.as_bytes().to_vec().into()]).unwrap_or_default();
        opts = opts.ssl_opts(SslOpts::default().with_root_certs(roots).with_danger_tls_hostname_override(Some(u.host.clone())));
    }
    Conn::new(opts).await.map_err(|e| format!("{}: {}", u.host, error(&e)))
}

/// The server's message (never the URL).
fn error(e: &mysql_async::Error) -> String {
    match e {
        mysql_async::Error::Server(s) => s.message.clone(),
        e => e.to_string(),
    }
}

/// `'name'@'%'` (roll-secrets checked the name: letters, digits and _).
fn account(name: &str) -> Result<String, String> {
    if name.is_empty() || !name.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_') {
        return Err(format!("{name} isn't a name roll uses"));
    }
    Ok(format!("'{name}'@'%'"))
}

/// A password roll made: letters and digits, nothing to escape.
fn literal(password: &str) -> Result<String, String> {
    if password.is_empty() || !password.bytes().all(|b| b.is_ascii_alphanumeric()) {
        return Err("a password roll can't send".into());
    }
    Ok(format!("'{password}'"))
}

pub(super) async fn run<T>(u: Url, ca: Option<String>, allow_private: bool, work: impl AsyncFnOnce(&mut Conn) -> Result<T, String>) -> Result<T, String> {
    tokio::time::timeout(TIMEOUT, async move {
        let mut c = connect(&u, ca.as_deref(), allow_private).await?;
        let out = work(&mut c).await;
        let _ = c.disconnect().await;
        out
    })
    .await
    .map_err(|_| "timed out".to_string())?
}

async fn exec(c: &mut Conn, sql: &str) -> Result<(), String> {
    c.query_drop(sql).await.map_err(|e| error(&e))
}

pub(super) async fn check(c: &mut Conn, roles: &[String], group: Option<&str>) -> Result<(), String> {
    for r in roles.iter().map(String::as_str).chain(group) {
        let found: Option<String> = c.exec_first("SELECT user FROM mysql.user WHERE host = '%' AND user = ? LIMIT 1", (r,)).await.map_err(|e| error(&e))?;
        if found.is_none() {
            return Err(format!("there's no account '{r}'@'%'"));
        }
    }
    Ok(())
}

pub(super) async fn set_password(c: &mut Conn, role: &str, password: &str) -> Result<(), String> {
    exec(c, &format!("ALTER USER {} IDENTIFIED BY {}", account(role)?, literal(password)?)).await
}

pub(super) async fn create_login(c: &mut Conn, name: &str, password: &str, group: &str) -> Result<(), String> {
    let (who, group) = (account(name)?, account(group)?);
    exec(c, &format!("CREATE USER {who} IDENTIFIED BY {}", literal(password)?)).await?;
    let granted = async {
        exec(c, &format!("GRANT {group} TO {who}")).await?;
        exec(c, &format!("SET DEFAULT ROLE {group} TO {who}")).await
    }
    .await;
    if granted.is_err() {
        let _ = exec(c, &format!("DROP USER IF EXISTS {who}")).await;
    }
    granted
}

pub(super) async fn drop_login(c: &mut Conn, name: &str) -> Result<(), String> {
    let who = account(name)?;
    // Its sessions end where the admin may (CONNECTION_ADMIN); dropping the account blocks new ones.
    let ids: Vec<u64> = c.exec("SELECT id FROM information_schema.processlist WHERE user = ?", (name,)).await.unwrap_or_default();
    for id in ids {
        let _ = exec(c, &format!("KILL {id}")).await;
    }
    exec(c, &format!("DROP USER IF EXISTS {who}")).await
}
