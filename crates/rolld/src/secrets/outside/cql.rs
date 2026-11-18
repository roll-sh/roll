//! Cassandra and ScyllaDB outside roll, over CQL's native protocol (v4, which both speak) with
//! TLS: the same guards as Postgres (a public address resolved here, one connection to that
//! address, TLS verified against the URL's host with the system's roots and `ca`), and no driver
//! that would go looking for the cluster's other nodes. Frames are nosql-proto's (bounded
//! decoding), capped at [`FRAME_MAX`]; rows are walked lazily, at most [`ROWS_MAX`].
//!
//! Roles are `CREATE ROLE … WITH LOGIN = true AND HASHED PASSWORD = '…'` and `ALTER ROLE … WITH
//! HASHED PASSWORD = '…'`: neither database takes a password as a bound value, and a statement is
//! kept whole in tracing (`system_traces.sessions`) on both, so the statement holds bcrypt's hash
//! of the password (`$2a$`, cost 10, Cassandra's own), never the password. Only when the server
//! won't take that (a syntax error from one without `HASHED PASSWORD`; ScyllaDB 2026.3's refusal of
//! it from anyone but a superuser, and its server error on `ALTER ROLE … WITH HASHED PASSWORD`) does
//! the password go in the statement, over TLS (roll-secrets' passwords are letters and digits,
//! checked again here). A dynamic login is
//! granted the group. Neither has an expiry for a role: a dynamic login lasts until roll ends its
//! lease, which drops it. Names roll checked (letters, digits, _) are quoted, so their case holds.

use super::{TIMEOUT, resolve, tls_config};
use bytes::Bytes;
use futures::{SinkExt, StreamExt};
use nosql_proto::{ClientCodec, Consistency, Ctx, ErrorBody, Query, QueryParams, Request, RequestFrame, Response, ResponseFrame, ResultBody, Startup, Value, Version, code};
use std::sync::Arc;
use tokio::net::TcpStream;
use tokio_rustls::rustls::pki_types::ServerName;
use tokio_util::codec::Framed;

/// The largest frame read (the answers here are a few rows).
const FRAME_MAX: usize = 256 * 1024;
/// The most rows of one answer looked at.
const ROWS_MAX: usize = 1000;

/// How long [`settled`] waits for a new password to log in: past Cassandra's default
/// `credentials_validity` (2 s).
const SETTLE: std::time::Duration = std::time::Duration::from_secs(5);

/// The URL's settings: `cassandra://admin:password@host:9142/keyspace` (the keyspace optional).
#[derive(Clone)]
pub(super) struct Url {
    host: String,
    port: u16,
    user: String,
    password: String,
    keyspace: Option<String>,
}

pub(super) fn parse(url: &str) -> Result<Url, String> {
    let bad = || "the URL is cassandra://admin:password@host:9142/keyspace (the TLS port; the keyspace is optional)".to_string();
    let u = url::Url::parse(url).map_err(|_| bad())?;
    let host = match u.host() {
        Some(url::Host::Domain(d)) => d.to_string(),
        Some(url::Host::Ipv4(ip)) => ip.to_string(),
        Some(url::Host::Ipv6(ip)) => ip.to_string(),
        None => return Err(bad()),
    };
    if u.scheme() != "cassandra" || u.username().is_empty() || u.fragment().is_some() {
        return Err(bad());
    }
    if let Some((k, _)) = u.query_pairs().next() {
        return Err(format!("the URL takes no settings, not {k}"));
    }
    let decode = |s: &str| super::mysql::percent_decode(s).ok_or_else(bad);
    let keyspace = u.path().trim_start_matches('/');
    if !keyspace.is_empty() && !name_ok(keyspace) {
        return Err("a keyspace is letters, digits and _".into());
    }
    Ok(Url { host, port: u.port().unwrap_or(9142), user: decode(u.username())?, password: decode(u.password().unwrap_or(""))?, keyspace: (!keyspace.is_empty()).then(|| keyspace.to_string()) })
}

impl Url {
    /// The same login with `password`.
    pub(super) fn with_password(&self, password: &str) -> Url {
        Url { password: password.to_string(), ..self.clone() }
    }

    /// The same server, logging in as `user` with `password`.
    pub(super) fn as_user(&self, user: &str, password: &str) -> Url {
        Url { user: user.to_string(), ..self.with_password(password) }
    }
}

fn name_ok(name: &str) -> bool {
    (1..=48).contains(&name.len()) && name.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_')
}

/// `"name"` (roll-secrets checked the name: letters, digits and _).
fn ident(name: &str) -> Result<String, String> {
    if name.is_empty() || name.len() > 256 || !name.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_') {
        return Err(format!("{name} isn't a name roll uses"));
    }
    Ok(format!("\"{name}\""))
}

/// `'password'`: roll's passwords are letters and digits, so nothing in one needs quoting.
fn literal(password: &str) -> Result<String, String> {
    if password.is_empty() || !password.bytes().all(|b| b.is_ascii_alphanumeric()) {
        return Err("a password roll didn't make".into());
    }
    Ok(format!("'{password}'"))
}

/// bcrypt's `$2a$` hash of `password` at cost 10: Cassandra's default cost, and the only prefix it
/// takes (`$2b$` and `$2y$` are "Invalid hashed password value. Please use jBcrypt."); ScyllaDB
/// takes any of them. Its characters (`$./`, letters, digits) need no quoting.
fn hashed(password: &str) -> Result<String, String> {
    literal(password)?;
    bcrypt::hash_with_result(password, 10).map(|h| h.format_for_version(bcrypt::Version::TwoA)).map_err(|e| format!("bcrypt: {e}"))
}

/// [`hashed`] off the runtime's threads (cost 10 is tens of milliseconds).
async fn hashed_blocking(password: &str) -> Result<String, String> {
    let password = password.to_string();
    tokio::task::spawn_blocking(move || hashed(&password)).await.map_err(|e| format!("bcrypt: {e}"))?
}

type Io = tokio_rustls::client::TlsStream<TcpStream>;

pub(super) struct Conn {
    io: Framed<Io, ClientCodec>,
    ctx: Ctx,
    host: String,
    user: String,
    keyspace: Option<String>,
}

/// The server's refusal: its code's name and message, except a statement holding a password,
/// whose syntax errors (which quote the statement) say only what kind they are, and whose other
/// messages lose any copy of it.
fn refused(e: &ErrorBody, secret: Option<&str>) -> String {
    let kind = match e.code {
        code::BAD_CREDENTIALS => "bad credentials",
        code::SYNTAX_ERROR => "syntax error",
        code::UNAUTHORIZED => "unauthorized",
        code::INVALID => "invalid request",
        code::ALREADY_EXISTS => "already exists",
        code::UNAVAILABLE => "unavailable",
        code::WRITE_TIMEOUT | code::READ_TIMEOUT => "timed out",
        _ => "error",
    };
    let message = match secret {
        Some(_) if e.code == code::SYNTAX_ERROR => return kind.to_string(),
        Some(s) => e.message.replace(s, "…"),
        None => e.message.clone(),
    };
    format!("{kind}: {}", message.chars().take(300).collect::<String>())
}

/// Send `req` on stream 0 and read its answer (the only one in flight).
async fn exchange(c: &mut Conn, req: Request) -> Result<Response, String> {
    let host = c.host.clone();
    c.io.send(RequestFrame::new(0, req).to_frame(c.ctx)).await.map_err(|e| format!("{host}: {e}"))?;
    loop {
        let frame = c.io.next().await.ok_or_else(|| format!("{host}: the server closed the connection"))?.map_err(|e| format!("{host}: {e}"))?;
        // Events (stream -1) aren't asked for, but a server may send them anyway.
        if frame.stream != 0 {
            continue;
        }
        return ResponseFrame::decode(c.ctx, &frame).map(|f| f.response).map_err(|e| format!("{host}: a malformed answer: {e}"));
    }
}

async fn connect(u: &Url, ca: Option<&str>, allow_private: bool) -> Result<Conn, String> {
    let addr = resolve(&u.host, u.port, allow_private).await?;
    let tcp = TcpStream::connect(addr).await.map_err(|e| format!("{}: {e}", u.host))?;
    let _ = tcp.set_nodelay(true);
    let name = ServerName::try_from(u.host.clone()).map_err(|_| format!("{} isn't a host name", u.host))?;
    // The certificate is checked against the URL's host, whichever address it was reached at.
    let tls = tokio_rustls::TlsConnector::from(Arc::new(tls_config(ca)?)).connect(name, tcp).await.map_err(|e| format!("{}: TLS: {e}", u.host))?;
    let mut c = Conn { io: Framed::new(tls, ClientCodec::new().with_max_frame_size(FRAME_MAX)), ctx: Ctx::new(Version::V4), host: u.host.clone(), user: u.user.clone(), keyspace: u.keyspace.clone() };
    let mut resp = exchange(&mut c, Request::Startup(Startup::new("3.0.0"))).await?;
    let mut answered = false;
    loop {
        match resp {
            Response::Authenticate(_) if !answered => {
                answered = true;
                // SASL PLAIN, as PasswordAuthenticator and ScyllaDB's take it.
                let mut token = Vec::with_capacity(u.user.len() + u.password.len() + 2);
                token.push(0);
                token.extend_from_slice(u.user.as_bytes());
                token.push(0);
                token.extend_from_slice(u.password.as_bytes());
                resp = exchange(&mut c, Request::AuthResponse(Some(Bytes::from(token)))).await?;
            }
            Response::AuthSuccess(_) => return Ok(c),
            // A server that lets anyone in (AllowAllAuthenticator) has no passwords to rotate.
            Response::Ready => return Err(format!("{}: the server doesn't check passwords: turn on PasswordAuthenticator", u.host)),
            Response::Error(e) => return Err(format!("{}: {}", u.host, refused(&e, Some(&u.password)))),
            _ => return Err(format!("{}: the server asked for an authentication roll doesn't speak", u.host)),
        }
    }
}

pub(super) async fn run<T>(u: Url, ca: Option<String>, allow_private: bool, work: impl AsyncFnOnce(&mut Conn) -> Result<T, String>) -> Result<T, String> {
    tokio::time::timeout(TIMEOUT, async move {
        let mut c = connect(&u, ca.as_deref(), allow_private).await?;
        work(&mut c).await
    })
    .await
    .map_err(|_| "timed out".to_string())?
}

/// One statement with text values: its result, or the server's refusal.
async fn ask(c: &mut Conn, cql: String, values: &[&str]) -> Result<Result<ResultBody, ErrorBody>, String> {
    let mut params = QueryParams::new(Consistency::LocalQuorum);
    params.values = values.iter().map(|v| Value::Set(Bytes::copy_from_slice(v.as_bytes()))).collect();
    match exchange(c, Request::Query(Query { query: cql, params })).await? {
        Response::Result(r) => Ok(Ok(r)),
        Response::Error(e) => Ok(Err(e)),
        _ => Err(format!("{}: an answer that isn't a result", c.host)),
    }
}

/// One statement with text values; `secret` is a password it holds (kept out of errors).
async fn query(c: &mut Conn, cql: String, values: &[&str], secret: Option<&str>) -> Result<ResultBody, String> {
    ask(c, cql, values).await?.map_err(|e| format!("{}: {}", c.host, refused(&e, secret)))
}

/// `statement` with `HASHED PASSWORD = '<bcrypt>'` in it (see the module's doc), or with
/// `PASSWORD = '…'` once the server answers that with a syntax error, a server error or a refusal
/// (the hashed statement changed nothing then; a refusal for want of a permission comes again).
async fn with_password(c: &mut Conn, statement: impl Fn(&str) -> String, password: &str) -> Result<(), String> {
    let hash = hashed_blocking(password).await?;
    match ask(c, statement(&format!("HASHED PASSWORD = '{hash}'")), &[]).await? {
        Ok(_) => return Ok(()),
        Err(e) if [code::SYNTAX_ERROR, code::SERVER_ERROR, code::UNAUTHORIZED].contains(&e.code) => {}
        Err(e) => return Err(format!("{}: {}", c.host, refused(&e, Some(&hash)))),
    }
    query(c, statement(&format!("PASSWORD = {}", literal(password)?)), &[], Some(password)).await.map(|_| ())
}

/// Whether `role` exists, and whether it can log in (`LIST ROLES OF` refuses a role that doesn't).
async fn role(c: &mut Conn, role: &str) -> Result<Option<bool>, String> {
    let rows = match query(c, format!("LIST ROLES OF {}", ident(role)?), &[], None).await {
        Ok(ResultBody::Rows(rows)) => rows,
        Ok(_) => return Err("LIST ROLES answered no rows".into()),
        Err(e) if e.contains("invalid request") => return Ok(None),
        Err(e) => return Err(e),
    };
    let col = |n: &str| rows.metadata.columns.iter().position(|c| c.name == n);
    let (Some(name_at), Some(login_at)) = (col("role"), col("login")) else { return Err("LIST ROLES answered without role and login".into()) };
    for row in rows.rows().take(ROWS_MAX) {
        let cells: Vec<Option<Bytes>> = row.take(rows.column_count()).collect();
        if cells.get(name_at).cloned().flatten().as_deref() == Some(role.as_bytes()) {
            return Ok(Some(cells.get(login_at).cloned().flatten().is_some_and(|b| b.first().is_some_and(|x| *x != 0))));
        }
    }
    Ok(None)
}

pub(super) async fn check(c: &mut Conn, roles: &[String], group: Option<&str>) -> Result<(), String> {
    if let Some(ks) = c.keyspace.clone() {
        match query(c, "SELECT keyspace_name FROM system_schema.keyspaces WHERE keyspace_name = ?".into(), &[&ks], None).await? {
            ResultBody::Rows(r) if !r.is_empty() => {}
            _ => return Err(format!("there's no keyspace {ks}")),
        }
    }
    for r in roles {
        if role(c, r).await? != Some(true) {
            return Err(format!("there's no role {r} that can log in"));
        }
    }
    if let Some(g) = group
        && role(c, g).await?.is_none()
    {
        return Err(format!("there's no role {g}"));
    }
    Ok(())
}

pub(super) async fn set_password(c: &mut Conn, role: &str, password: &str) -> Result<(), String> {
    let who = ident(role)?;
    with_password(c, |p| format!("ALTER ROLE {who} WITH {p}"), password).await
}

/// The URL's own role gets `password` (any role may change its own), hashed where the server takes
/// it (ScyllaDB's `ALTER ROLE` doesn't: see [`with_password`]).
pub(super) async fn set_own_password(c: &mut Conn, password: &str) -> Result<(), String> {
    let who = ident(&c.user)?;
    with_password(c, |p| format!("ALTER ROLE {who} WITH {p}"), password).await
}

/// Cassandra keeps a role's credentials cached (`credentials_validity`, 2 s by default) and takes
/// its old password, not the new one, until they expire: wait, at most [`SETTLE`], until `u` (the
/// new password) logs in. The change is made either way, so this never fails: a login that still
/// doesn't work is a warning naming the role.
pub(super) async fn settled(u: &Url, ca: Option<&str>, allow_private: bool) {
    let until = tokio::time::Instant::now() + SETTLE;
    loop {
        let why = match tokio::time::timeout(TIMEOUT, connect(u, ca, allow_private)).await {
            Ok(Ok(_)) => return,
            Ok(Err(e)) if e.contains("bad credentials") && tokio::time::Instant::now() < until => {
                tokio::time::sleep(std::time::Duration::from_millis(250)).await;
                continue;
            }
            // connect's errors never hold the password.
            Ok(Err(e)) => e,
            Err(_) => "timed out".to_string(),
        };
        tracing::warn!(role = %u.user, "roll secrets: {}'s new password didn't log in within {} s, kept anyway: {why}", u.user, SETTLE.as_secs());
        return;
    }
}

pub(super) async fn create_login(c: &mut Conn, name: &str, password: &str, group: &str) -> Result<(), String> {
    let (who, group) = (ident(name)?, ident(group)?);
    with_password(c, |p| format!("CREATE ROLE {who} WITH LOGIN = true AND {p}"), password).await?;
    if let Err(e) = query(c, format!("GRANT {group} TO {who}"), &[], None).await {
        let _ = query(c, format!("DROP ROLE IF EXISTS {who}"), &[], None).await;
        return Err(e);
    }
    Ok(())
}

pub(super) async fn drop_login(c: &mut Conn, name: &str) -> Result<(), String> {
    let who = ident(name)?;
    // Cassandra keeps a dropped role's credentials in its cache for up to `credentials_validity`
    // (2 s by default), but a new password takes at once: one nobody knows goes first. It's refused
    // within 5 s of the role's last change on Cassandra 5, and for a role that's gone already;
    // the drop goes ahead either way.
    let unknown = unknown_password();
    let _ = with_password(c, |p| format!("ALTER ROLE {who} WITH {p}"), &unknown).await;
    query(c, format!("DROP ROLE IF EXISTS {who}"), &[], None).await.map(|_| ())
}

/// 40 letters and digits nobody is given.
fn unknown_password() -> String {
    const ALPHABET: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789";
    let mut b = [0u8; 40];
    getrandom::fill(&mut b).expect("system randomness");
    b.iter().map(|x| ALPHABET[(*x % 62) as usize] as char).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use roll_secrets::outside::OutsideDb;

    #[test]
    fn a_password_never_reaches_an_error() {
        let pw = "Abc123def456";
        let syntax = ErrorBody::new(code::SYNTAX_ERROR, format!("line 1:31 no viable alternative at input '{pw}'"));
        assert_eq!(refused(&syntax, Some(pw)), "syntax error");
        let other = ErrorBody::new(code::INVALID, format!("bad {pw}"));
        assert!(!refused(&other, Some(pw)).contains(pw));
        assert!(literal("it's").is_err() && ident("a\"b").is_err() && hashed("it's").is_err());
        let h = hashed(pw).unwrap();
        assert!(h.starts_with("$2a$10$") && !h.contains(pw) && bcrypt::verify(pw, &h).unwrap());
        assert!(parse("cassandra://a:b@db.example.com:9142/app?ssl=false").err().unwrap().contains("ssl"));
        assert!(parse("cassandra://a:b@db.example.com/a-b").is_err());
        assert_eq!(parse("cassandra://a:b@db.example.com").unwrap().port, 9142);
    }

    /// Against a real Cassandra, run by scripts/secrets-outside-check.sh (PasswordAuthenticator,
    /// CassandraAuthorizer and client TLS on; `ROLL_E2E_CASSANDRA_URL` the default superuser's,
    /// `ROLL_E2E_OUTSIDE_CA` its CA).
    #[tokio::test(flavor = "multi_thread")]
    #[ignore]
    async fn a_real_cassandra_rotates_and_makes_and_drops_roles() {
        real("ROLL_E2E_CASSANDRA_URL").await
    }

    /// The same against a real ScyllaDB (`ROLL_E2E_SCYLLA_URL`).
    #[tokio::test(flavor = "multi_thread")]
    #[ignore]
    async fn a_real_scylla_rotates_and_makes_and_drops_roles() {
        real("ROLL_E2E_SCYLLA_URL").await
    }

    fn pw() -> String {
        let mut b = [0u8; 18];
        getrandom::fill(&mut b).unwrap();
        b.iter().map(|x| format!("{x:02x}")).collect::<String>() + "Aa1"
    }

    /// `url` with another user and password, and `keyspace`.
    fn as_user(url: &str, user: &str, password: &str, keyspace: &str) -> String {
        let mut u = url::Url::parse(url).unwrap();
        u.set_username(user).unwrap();
        u.set_password(Some(password)).unwrap();
        u.set_path(keyspace);
        u.to_string()
    }

    /// Statements as whoever `url` names.
    async fn cql(url: &str, ca: &str, statements: &[&str]) -> Result<(), String> {
        let statements: Vec<String> = statements.iter().map(|s| s.to_string()).collect();
        run(parse(url).unwrap(), Some(ca.to_string()), true, async move |c| {
            for s in statements {
                query(c, s, &[], None).await?;
            }
            Ok(())
        })
        .await
    }

    /// Every cell of `select`'s rows as text, as whoever `url` names, at consistency ONE (the trace
    /// and audit keyspaces' replication outnumbers one node).
    async fn cells(url: &str, ca: &str, select: &str) -> Vec<String> {
        let select = select.to_string();
        run(parse(url).unwrap(), Some(ca.to_string()), true, async move |c| {
            let req = Request::Query(Query { query: select, params: QueryParams::new(Consistency::One) });
            let Response::Result(ResultBody::Rows(rows)) = exchange(c, req).await? else { return Err("no rows".into()) };
            Ok(rows.rows().flat_map(|row| row.take(rows.column_count()).collect::<Vec<_>>()).flatten().map(|b| String::from_utf8_lossy(&b).into_owned()).collect())
        })
        .await
        .unwrap()
    }

    async fn real(var: &str) {
        let env = |k: &str| std::env::var(k).unwrap_or_else(|_| panic!("{k}"));
        let (root, ca) = (env(var), std::fs::read_to_string(env("ROLL_E2E_OUTSIDE_CA")).unwrap());
        // Fixtures, as the superuser: a keyspace, two roles that take turns, a group that reads the
        // keyspace, and a narrower admin roll registers with (no superuser).
        let (admin_pw, start) = (pw(), pw());
        let admin_role = format!("CREATE ROLE IF NOT EXISTS roll_admin WITH LOGIN = true AND PASSWORD = '{admin_pw}'");
        let (blue, green) = (format!("CREATE ROLE IF NOT EXISTS app_blue WITH LOGIN = true AND PASSWORD = '{start}'"), format!("CREATE ROLE IF NOT EXISTS app_green WITH LOGIN = true AND PASSWORD = '{start}'"));
        cql(&root, &ca, &[
            // ScyllaDB's tablets refuse SimpleStrategy.
            "CREATE KEYSPACE IF NOT EXISTS app WITH replication = {'class': 'NetworkTopologyStrategy', 'replication_factor': 1}",
            "CREATE TABLE IF NOT EXISTS app.t (k int PRIMARY KEY, v text)",
            "INSERT INTO app.t (k, v) VALUES (1, 'one')",
            "CREATE ROLE IF NOT EXISTS reader",
            "GRANT SELECT ON KEYSPACE app TO reader",
            &blue,
            &green,
            &admin_role,
            "GRANT CREATE ON ALL ROLES TO roll_admin",
            "GRANT ALTER ON ALL ROLES TO roll_admin",
            "GRANT DROP ON ALL ROLES TO roll_admin",
            "GRANT AUTHORIZE ON ALL ROLES TO roll_admin",
            "GRANT DESCRIBE ON ALL ROLES TO roll_admin",
            "GRANT SELECT ON KEYSPACE system_schema TO roll_admin",
        ])
        .await
        .unwrap();
        let admin = as_user(&root, "roll_admin", &admin_pw, "app");
        let scylla = var == "ROLL_E2E_SCYLLA_URL";
        let (root2, ca2, root) = (root.clone(), ca.clone(), root);
        let sent = tokio::task::spawn_blocking(move || {
            let g = super::super::Guarded { allow_private: true };
            let roles = ["app_blue".to_string(), "app_green".to_string()];
            let rt = tokio::runtime::Handle::current();
            let logs_in = |user: &str, password: &str| rt.block_on(cql(&as_user(&admin, user, password, ""), &ca, &[]));
            // Registering: the keyspace, both roles and the group; refusals for what isn't there.
            g.check(&admin, Some(&ca), &roles, Some("reader")).unwrap();
            assert!(g.check(&admin, Some(&ca), &["nobody".into()], None).unwrap_err().contains("there's no role nobody"));
            assert!(g.check(&admin, Some(&ca), &["reader".into()], None).unwrap_err().contains("can log in"), "a role that can't log in");
            assert!(g.check(&admin, Some(&ca), &[], Some("nope")).unwrap_err().contains("there's no role nope"));
            assert!(g.check(&as_user(&admin, "roll_admin", &admin_pw, "nope"), Some(&ca), &[], None).unwrap_err().contains("no keyspace nope"));
            let wrong = g.check(&as_user(&admin, "roll_admin", "fakewrongpw", "app"), Some(&ca), &[], None).unwrap_err();
            assert!(wrong.contains("bad credentials") && !wrong.contains("fakewrongpw"), "{wrong}");
            let untrusted = g.check(&admin, None, &[], None).unwrap_err();
            assert!(untrusted.to_lowercase().contains("certificate"), "{untrusted}");
            // Rotation: the new password logs in and the old one doesn't. Cassandra 5 refuses a
            // role's second password change within 5 s (roll's turns are minutes apart at least).
            let settle = || std::thread::sleep(std::time::Duration::from_millis(5500));
            let (first, second) = (pw(), pw());
            settle();
            g.set_password(&admin, Some(&ca), "app_blue", &first).unwrap();
            logs_in("app_blue", &first).unwrap();
            assert!(logs_in("app_blue", &start).is_err(), "the starting password is refused");
            settle();
            g.set_password(&admin, Some(&ca), "app_blue", &second).unwrap();
            logs_in("app_blue", &second).unwrap();
            assert!(logs_in("app_blue", &first).is_err(), "the old password is refused");
            // A turn hands its password out as soon as the rotation returns, while an app still
            // logs in with the other role (which keeps Cassandra's credentials cache warm): the
            // handed-out password logs in at once, every time.
            let mut current = [second.clone(), start.clone()];
            for turn in 0..4 {
                let (i, next) = (turn % 2, pw());
                let role = roles[i].as_str();
                settle();
                logs_in(role, &current[i]).unwrap();
                g.set_password(&admin, Some(&ca), role, &next).unwrap();
                logs_in(role, &next).unwrap_or_else(|e| panic!("turn {turn}: {role}'s handed-out password was refused right after the rotation: {e}"));
                current[i] = next;
            }
            // A dynamic login: made in the group, reads the keyspace and can't write it, then is
            // gone (twice: gone counts as dropped).
            let (name, upw) = ("roll_fakefakefakefake", pw());
            g.create_login(&admin, Some(&ca), name, &upw, "reader", 0).unwrap();
            let user = as_user(&admin, name, &upw, "");
            rt.block_on(cql(&user, &ca, &["SELECT v FROM app.t WHERE k = 1"])).unwrap();
            let denied = rt.block_on(cql(&user, &ca, &["INSERT INTO app.t (k, v) VALUES (2, 'two')"])).unwrap_err();
            assert!(denied.contains("unauthorized"), "{denied}");
            // At once, though Cassandra's credentials cache would keep it for 2 s (a lease lasts
            // longer than the 5 s before its password may change).
            settle();
            g.drop_login(&admin, Some(&ca), name).unwrap();
            assert!(logs_in(name, &upw).is_err(), "a dropped role is refused");
            g.drop_login(&admin, Some(&ca), name).unwrap();
            // One made by the superuser, whom ScyllaDB lets send a hashed password.
            let (name, rootpw) = ("roll_fakefakefakeroot", pw());
            g.create_login(&root, Some(&ca), name, &rootpw, "reader", 0).unwrap();
            logs_in(name, &rootpw).unwrap();
            g.drop_login(&root, Some(&ca), name).unwrap();
            // The admin's own password (rotate-admin): the new one logs in and still manages the
            // roles, the old one is refused.
            let own = pw();
            settle();
            g.set_own_password(&admin, Some(&ca), &own).unwrap();
            g.check(&as_user(&admin, "roll_admin", &own, "app"), Some(&ca), &roles, Some("reader")).unwrap();
            assert!(logs_in("roll_admin", &admin_pw).is_err(), "the admin's old password is refused");
            (first, second, upw, rootpw, own)
        })
        .await
        .unwrap();
        // What the servers kept of the statements: the script turns on tracing for every request,
        // which keeps a statement whole on both. Every one went hashed on Cassandra. ScyllaDB 2026.3
        // takes a hashed password only from a superuser and only in CREATE ROLE (ALTER ROLE's is a
        // server error), so the rest went plain, which its audit log (on by default for DCL) masks.
        std::thread::sleep(std::time::Duration::from_secs(3));
        let traces = cells(&root2, &ca2, "SELECT parameters FROM system_traces.sessions").await.join("\n");
        assert!(traces.contains("HASHED PASSWORD = '$2a$10$"), "tracing is on and saw a hashed password");
        let (first, second, upw, rootpw, own) = sent;
        assert!(!traces.contains(&rootpw), "the superuser's login's password isn't in a statement");
        if scylla {
            let audit = cells(&root2, &ca2, "SELECT operation FROM audit.audit_log").await.join("\n");
            assert!(audit.contains("ALTER ROLE \"app_blue\" WITH PASSWORD = '***'"), "{audit}");
            assert!(audit.contains("ALTER ROLE \"roll_admin\" WITH PASSWORD = '***'"), "{audit}");
            assert!(![&first, &second, &upw, &own].iter().any(|p| audit.contains(p.as_str())), "ScyllaDB's audit log masks passwords");
        } else {
            assert!(![&first, &second, &upw, &own].iter().any(|p| traces.contains(p.as_str())), "no password roll sent is in a statement");
        }
    }
}
