//! SQL Server outside roll (and Azure SQL Managed Instance): the same guards as Postgres (a public
//! address resolved here, TLS verified against the URL's host, no redirect to another server).
//! Logins are server-wide, `CREATE LOGIN [name]`; a dynamic one gets a user in the URL's database
//! and joins the group there. SQL Server has no `VALID UNTIL`: a dynamic login ends when roll ends
//! its lease. Passwords go in `WITH PASSWORD` over TLS (SQL Server keeps them out of its traces).
//!
//! tiberius bounds what it preallocates from a server's lengths; on top of that, a conversation
//! reads at most [`READ_MAX`] bytes, whatever the server sends.

use super::{TIMEOUT, resolve};
use std::pin::Pin;
use std::task::{Context, Poll};
use tiberius::{AuthMethod, Client, Config, EncryptionLevel};
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};
use tokio::net::TcpStream;
use tokio_util::compat::{Compat, TokioAsyncWriteCompatExt};

/// The most one conversation reads: a few small result sets.
const READ_MAX: usize = 4 << 20;

pub(super) type Conn = Client<Compat<Bounded>>;

/// The URL's settings: `sqlserver://user:password@host:1433/db`, `encrypt` and nothing else.
pub(super) struct Url {
    host: String,
    port: u16,
    user: String,
    password: String,
    db: Option<String>,
}

pub(super) fn parse(url: &str) -> Result<Url, String> {
    let bad = || "the URL is sqlserver://user:password@host:1433/db".to_string();
    let u = url::Url::parse(url).map_err(|_| bad())?;
    let host = match u.host() {
        Some(url::Host::Domain(d)) => d.to_string(),
        Some(url::Host::Ipv4(ip)) => ip.to_string(),
        Some(url::Host::Ipv6(ip)) => ip.to_string(),
        None => return Err(bad()),
    };
    if u.scheme() != "sqlserver" || u.username().is_empty() {
        return Err(bad());
    }
    for (k, v) in u.query_pairs() {
        match (&*k, &*v.to_ascii_lowercase()) {
            ("encrypt", "true" | "mandatory" | "yes") => {}
            ("encrypt", _) => return Err("roll only sends a password over TLS: drop encrypt, or set it to true".into()),
            (k, _) => return Err(format!("the URL may set encrypt, but not {k}")),
        }
    }
    let decode = |s: &str| super::mysql::percent_decode(s).ok_or_else(bad);
    let db = u.path().trim_start_matches('/');
    Ok(Url { host, port: u.port().unwrap_or(1433), user: decode(u.username())?, password: decode(u.password().unwrap_or(""))?, db: (!db.is_empty()).then(|| decode(db)).transpose()? })
}

/// A TCP stream that refuses to read past [`READ_MAX`].
pub(super) struct Bounded {
    tcp: TcpStream,
    left: usize,
}

impl AsyncRead for Bounded {
    fn poll_read(mut self: Pin<&mut Self>, cx: &mut Context<'_>, buf: &mut ReadBuf<'_>) -> Poll<std::io::Result<()>> {
        if self.left == 0 {
            return Poll::Ready(Err(std::io::Error::other("the server sent more than roll reads")));
        }
        let cap = buf.remaining().min(self.left);
        let mut limited = ReadBuf::new(buf.initialize_unfilled_to(cap));
        let r = Pin::new(&mut self.tcp).poll_read(cx, &mut limited);
        let n = limited.filled().len();
        if let Poll::Ready(Ok(())) = r {
            buf.advance(n);
            self.left -= n;
        }
        r
    }
}

impl AsyncWrite for Bounded {
    fn poll_write(mut self: Pin<&mut Self>, cx: &mut Context<'_>, b: &[u8]) -> Poll<std::io::Result<usize>> {
        Pin::new(&mut self.tcp).poll_write(cx, b)
    }

    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        Pin::new(&mut self.tcp).poll_flush(cx)
    }

    fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        Pin::new(&mut self.tcp).poll_shutdown(cx)
    }
}

async fn connect(u: &Url, ca: Option<&str>, allow_private: bool) -> Result<Conn, String> {
    let addr = resolve(&u.host, u.port, allow_private).await?;
    let mut config = Config::new();
    config.host(&u.host);
    config.port(addr.port());
    config.hostname_in_certificate(&u.host);
    config.authentication(AuthMethod::sql_server(&u.user, &u.password));
    if let Some(db) = &u.db {
        config.database(db);
    }
    config.application_name("roll secrets");
    // Everything encrypted, the server's certificate verified (TDS 7.4; strict TDS 8 wasn't
    // accepted by SQL Server 2022 on Linux, so roll doesn't offer it untested).
    config.encryption(EncryptionLevel::Required);
    if let Some(pem) = ca {
        config.trust_cert_ca_bundle(pem.as_bytes().to_vec());
    }
    let tcp = TcpStream::connect(addr).await.map_err(|e| format!("{}: {e}", u.host))?;
    let _ = tcp.set_nodelay(true);
    // A redirect (Azure's gateway names another server) is an error here: roll connects only to
    // the address it checked.
    Client::connect(config, Bounded { tcp, left: READ_MAX }.compat_write()).await.map_err(|e| format!("{}: {}", u.host, error(&e)))
}

/// The server's message (never the URL).
fn error(e: &tiberius::error::Error) -> String {
    match e {
        tiberius::error::Error::Server(t) => t.message().to_string(),
        tiberius::error::Error::Routing { .. } => "the server redirects to another one: register that server's address".into(),
        // tiberius's own advice names its API: keep the reason.
        tiberius::error::Error::Tls(m) => match m.split_once("during the TLS handshake: ").and_then(|(_, r)| r.split_once('.')) {
            Some((why, _)) => format!("its certificate isn't trusted ({why}): add its CA certificate as ca"),
            None => format!("TLS: {m}"),
        },
        e => e.to_string(),
    }
}

/// `[name]` (roll-secrets checked the name: lowercase letters, digits and _).
fn ident(name: &str) -> Result<String, String> {
    if name.is_empty() || !name.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_') {
        return Err(format!("{name} isn't a name roll uses"));
    }
    Ok(format!("[{name}]"))
}

/// A password roll made: letters and digits, nothing to escape.
fn literal(password: &str) -> Result<String, String> {
    if password.is_empty() || !password.bytes().all(|b| b.is_ascii_alphanumeric()) {
        return Err("a password roll can't send".into());
    }
    Ok(format!("N'{password}'"))
}

pub(super) async fn run<T>(u: Url, ca: Option<String>, allow_private: bool, work: impl AsyncFnOnce(&mut Conn) -> Result<T, String>) -> Result<T, String> {
    tokio::time::timeout(TIMEOUT, async move {
        let mut c = connect(&u, ca.as_deref(), allow_private).await?;
        let out = work(&mut c).await;
        let _ = c.close().await;
        out
    })
    .await
    .map_err(|_| "timed out".to_string())?
}

async fn exec(c: &mut Conn, sql: &str) -> Result<(), String> {
    c.execute(sql, &[]).await.map(|_| ()).map_err(|e| error(&e))
}

async fn exists(c: &mut Conn, sql: &str, name: &str) -> Result<bool, String> {
    let row = c.query(sql, &[&name]).await.map_err(|e| error(&e))?.into_row().await.map_err(|e| error(&e))?;
    Ok(row.is_some())
}

pub(super) async fn check(c: &mut Conn, roles: &[String], group: Option<&str>) -> Result<(), String> {
    // What the admin login needs: to change passwords and make logins, and, for dynamic logins,
    // to see and end their sessions when a lease ends (or a dropped login's session lingers).
    let mut needs = vec!["ALTER ANY LOGIN"];
    if group.is_some() {
        needs.extend(["VIEW SERVER STATE", "ALTER ANY CONNECTION"]);
    }
    for p in needs {
        let row = c.query("SELECT HAS_PERMS_BY_NAME(NULL, NULL, @P1)", &[&p]).await.map_err(|e| error(&e))?.into_row().await.map_err(|e| error(&e))?;
        if row.and_then(|r| r.get::<i32, _>(0)) != Some(1) {
            return Err(format!("the admin login needs {p} on the server"));
        }
    }
    for r in roles {
        if !exists(c, "SELECT 1 FROM sys.sql_logins WHERE name = @P1 AND is_disabled = 0", r).await? {
            return Err(format!("there's no SQL login {r} that can log in"));
        }
    }
    if let Some(g) = group
        && !exists(c, "SELECT 1 FROM sys.database_principals WHERE name = @P1 AND type = 'R'", g).await?
    {
        return Err(format!("there's no database role {g}"));
    }
    Ok(())
}

pub(super) async fn set_password(c: &mut Conn, role: &str, password: &str) -> Result<(), String> {
    exec(c, &format!("ALTER LOGIN {} WITH PASSWORD = {}", ident(role)?, literal(password)?)).await
}

pub(super) async fn create_login(c: &mut Conn, name: &str, password: &str, group: &str) -> Result<(), String> {
    let (who, group) = (ident(name)?, ident(group)?);
    exec(c, &format!("CREATE LOGIN {who} WITH PASSWORD = {}", literal(password)?)).await?;
    let granted = async {
        exec(c, &format!("CREATE USER {who} FOR LOGIN {who}")).await?;
        exec(c, &format!("ALTER ROLE {group} ADD MEMBER {who}")).await
    }
    .await;
    if granted.is_err() {
        let _ = exec(c, &format!("DROP USER IF EXISTS {who}")).await;
        let _ = exec(c, &format!("DROP LOGIN {who}")).await;
    }
    granted
}

pub(super) async fn drop_login(c: &mut Conn, name: &str) -> Result<(), String> {
    let who = ident(name)?;
    let exists_login = exists(c, "SELECT 1 FROM sys.server_principals WHERE name = @P1", name).await?;
    // Disabled first: that works while it's connected and stops new sessions at once, so a drop
    // that fails (a session that reconnected in between) leaves nothing that can log in.
    if exists_login {
        exec(c, &format!("ALTER LOGIN {who} DISABLE")).await?;
    }
    // Then its sessions end (VIEW SERVER STATE and ALTER ANY CONNECTION, checked at registration).
    let ids: Vec<i16> = match c.query("SELECT session_id FROM sys.dm_exec_sessions WHERE login_name = @P1", &[&name]).await {
        Ok(s) => s.into_first_result().await.unwrap_or_default().iter().filter_map(|r| r.get::<i16, _>(0)).collect(),
        Err(_) => vec![],
    };
    for id in ids {
        let _ = exec(c, &format!("KILL {id}")).await;
    }
    exec(c, &format!("DROP USER IF EXISTS {who}")).await?;
    if exists_login {
        exec(c, &format!("DROP LOGIN {who}")).await?;
    }
    Ok(())
}
