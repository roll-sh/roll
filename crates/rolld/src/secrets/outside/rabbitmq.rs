//! RabbitMQ outside roll, through its management HTTPS API (port 15671), as Vault's and
//! Infisical's RabbitMQ engines: the same guards as Postgres (a public address resolved here and
//! pinned, TLS verified against the URL's host with the system's roots and `ca`, no redirects, no
//! proxy). Passwords go as RabbitMQ's own salted SHA-256 hash (`password_hash`), so the broker
//! never sees one. Rotation keeps a user's tags; a dynamic user copies a template user's (the
//! group's) permissions and topic permissions on the URL's vhost, and no tags. RabbitMQ has no
//! expiry for a user: a dynamic one lasts until roll ends its lease, which deletes it and closes
//! its connections. Answers are read up to [`ANSWER_MAX`].

use super::{TIMEOUT, resolve, tls_config};
use serde_json::{Value as Json, json};
use sha2::{Digest, Sha256};

/// The most of one answer read.
const ANSWER_MAX: usize = 256 * 1024;

/// The URL's settings: `rabbitmq://admin:password@host:15671/vhost` (no vhost: `/`).
pub(super) struct Url {
    host: String,
    port: u16,
    user: String,
    password: String,
    vhost: String,
}

pub(super) fn parse(url: &str) -> Result<Url, String> {
    let bad = || "the URL is rabbitmq://admin:password@host:15671/vhost (the management HTTPS port)".to_string();
    let u = url::Url::parse(url).map_err(|_| bad())?;
    let host = match u.host() {
        Some(url::Host::Domain(d)) => d.to_string(),
        Some(url::Host::Ipv4(ip)) => ip.to_string(),
        Some(url::Host::Ipv6(ip)) => ip.to_string(),
        None => return Err(bad()),
    };
    if u.scheme() != "rabbitmq" || u.username().is_empty() {
        return Err(bad());
    }
    if let Some((k, _)) = u.query_pairs().next() {
        return Err(format!("the URL takes no settings, not {k}"));
    }
    let decode = |s: &str| super::mysql::percent_decode(s).ok_or_else(bad);
    let vhost = u.path().strip_prefix('/').unwrap_or("");
    let vhost = if vhost.is_empty() { "/".to_string() } else { decode(vhost)? };
    Ok(Url { host, port: u.port().unwrap_or(15671), user: decode(u.username())?, password: decode(u.password().unwrap_or(""))?, vhost })
}

pub(super) struct Conn {
    client: reqwest::Client,
    base: url::Url,
    user: String,
    password: String,
    vhost: String,
}

async fn connect(u: &Url, ca: Option<&str>, allow_private: bool) -> Result<Conn, String> {
    let addr = resolve(&u.host, u.port, allow_private).await?;
    let client = reqwest::Client::builder()
        .use_preconfigured_tls(tls_config(ca)?)
        // The address rolld checked, whatever DNS says next; the certificate is still the host's.
        .resolve(&u.host, addr)
        .redirect(reqwest::redirect::Policy::none())
        .no_proxy()
        .timeout(TIMEOUT)
        .build()
        .map_err(|e| format!("{}: {e}", u.host))?;
    let host = if u.host.contains(':') { format!("[{}]", u.host) } else { u.host.clone() };
    let base = url::Url::parse(&format!("https://{host}:{}/api/", u.port)).map_err(|_| "a bad host".to_string())?;
    Ok(Conn { client, base, user: u.user.clone(), password: u.password.clone(), vhost: u.vhost.clone() })
}

/// A request's failure and its causes, never its URL or headers.
fn why(e: reqwest::Error) -> String {
    let e = e.without_url();
    let mut out = e.to_string();
    let mut cause = std::error::Error::source(&e);
    while let Some(c) = cause {
        out.push_str(": ");
        out.push_str(&c.to_string());
        cause = c.source();
    }
    out
}

/// `segments` joined as one API path, each percent-encoded (a vhost `/` is `%2F`).
fn path(segments: &[&str]) -> String {
    segments.iter().map(|s| urlencoding::encode(s).into_owned()).collect::<Vec<_>>().join("/")
}

/// One call; `None` for a 404, the answer's JSON (or null) otherwise.
async fn call(c: &Conn, method: reqwest::Method, segments: &[&str], body: Option<Json>) -> Result<Option<Json>, String> {
    let url = c.base.join(&path(segments)).map_err(|_| "a bad path".to_string())?;
    let host = c.base.host_str().unwrap_or_default().to_string();
    let mut req = c.client.request(method, url).basic_auth(&c.user, Some(&c.password));
    if let Some(b) = body {
        req = req.json(&b);
    }
    let mut resp = req.send().await.map_err(|e| format!("{host}: {}", why(e)))?;
    let status = resp.status();
    let mut bytes = Vec::new();
    while let Some(chunk) = resp.chunk().await.map_err(|e| format!("{host}: {}", why(e)))? {
        if bytes.len() + chunk.len() > ANSWER_MAX {
            return Err(format!("{host}: the answer is longer than roll reads"));
        }
        bytes.extend_from_slice(&chunk);
    }
    if status == reqwest::StatusCode::NOT_FOUND {
        return Ok(None);
    }
    if !status.is_success() {
        // RabbitMQ's own reason ({"error", "reason"}), clipped; never the request.
        let reason = serde_json::from_slice::<Json>(&bytes).ok().and_then(|v| v["reason"].as_str().map(String::from)).unwrap_or_else(|| status.to_string());
        return Err(format!("{host}: {}", reason.chars().take(300).collect::<String>()));
    }
    Ok(Some(if bytes.is_empty() { Json::Null } else { serde_json::from_slice(&bytes).unwrap_or(Json::Null) }))
}

/// RabbitMQ's `rabbit_password_hashing_sha256`: base64(salt ‖ SHA-256(salt ‖ password)).
fn password_hash(password: &str) -> String {
    use base64::Engine as _;
    let mut salt = [0u8; 4];
    getrandom::fill(&mut salt).expect("system randomness");
    let mut h = Sha256::new();
    h.update(salt);
    h.update(password.as_bytes());
    let mut out = salt.to_vec();
    out.extend_from_slice(&h.finalize());
    base64::engine::general_purpose::STANDARD.encode(out)
}

pub(super) async fn run<T>(u: Url, ca: Option<String>, allow_private: bool, work: impl AsyncFnOnce(&Conn) -> Result<T, String>) -> Result<T, String> {
    tokio::time::timeout(TIMEOUT, async move {
        let c = connect(&u, ca.as_deref(), allow_private).await?;
        work(&c).await
    })
    .await
    .map_err(|_| "timed out".to_string())?
}

pub(super) async fn check(c: &Conn, roles: &[String], group: Option<&str>) -> Result<(), String> {
    // Proves TLS and the login, and that it may manage users.
    let me = call(c, reqwest::Method::GET, &["whoami"], None).await?.ok_or("the management API didn't answer whoami")?;
    let tags = match &me["tags"] {
        Json::Array(t) => t.iter().filter_map(Json::as_str).map(String::from).collect::<Vec<_>>(),
        Json::String(t) => t.split(',').map(|s| s.trim().to_string()).collect(),
        _ => vec![],
    };
    if !tags.iter().any(|t| t == "administrator") {
        return Err("the admin user needs the administrator tag".into());
    }
    if call(c, reqwest::Method::GET, &["vhosts", &c.vhost], None).await?.is_none() {
        return Err(format!("there's no vhost {}", c.vhost));
    }
    for r in roles {
        if call(c, reqwest::Method::GET, &["users", r], None).await?.is_none() {
            return Err(format!("there's no user {r}"));
        }
    }
    if let Some(g) = group
        && call(c, reqwest::Method::GET, &["permissions", &c.vhost, g], None).await?.is_none()
    {
        return Err(format!("the template user {g} has no permissions on {}", c.vhost));
    }
    Ok(())
}

pub(super) async fn set_password(c: &Conn, role: &str, password: &str) -> Result<(), String> {
    // PUT replaces the user: keep its tags.
    let user = call(c, reqwest::Method::GET, &["users", role], None).await?.ok_or_else(|| format!("there's no user {role}"))?;
    let tags = match &user["tags"] {
        Json::Array(t) => t.iter().filter_map(Json::as_str).collect::<Vec<_>>().join(","),
        Json::String(t) => t.clone(),
        _ => String::new(),
    };
    call(c, reqwest::Method::PUT, &["users", role], Some(json!({ "password_hash": password_hash(password), "hashing_algorithm": "rabbit_password_hashing_sha256", "tags": tags }))).await.map(|_| ())
}

pub(super) async fn create_login(c: &Conn, name: &str, password: &str, group: &str) -> Result<(), String> {
    let perms = call(c, reqwest::Method::GET, &["permissions", &c.vhost, group], None).await?.ok_or_else(|| format!("the template user {group} has no permissions on {}", c.vhost))?;
    let topics = call(c, reqwest::Method::GET, &["topic-permissions", &c.vhost, group], None).await?.unwrap_or(Json::Null);
    call(c, reqwest::Method::PUT, &["users", name], Some(json!({ "password_hash": password_hash(password), "hashing_algorithm": "rabbit_password_hashing_sha256", "tags": "" }))).await?;
    let granted = async {
        call(c, reqwest::Method::PUT, &["permissions", &c.vhost, name], Some(json!({ "configure": perms["configure"], "write": perms["write"], "read": perms["read"] }))).await?;
        for t in topics.as_array().into_iter().flatten().take(100) {
            call(c, reqwest::Method::PUT, &["topic-permissions", &c.vhost, name], Some(json!({ "exchange": t["exchange"], "write": t["write"], "read": t["read"] }))).await?;
        }
        Ok::<_, String>(())
    }
    .await;
    if granted.is_err() {
        let _ = call(c, reqwest::Method::DELETE, &["users", name], None).await;
    }
    granted
}

pub(super) async fn drop_login(c: &Conn, name: &str) -> Result<(), String> {
    // Deleted first, so it can open nothing new; then its connections close (where the broker
    // has the route, 3.11 and later).
    call(c, reqwest::Method::DELETE, &["users", name], None).await?;
    let _ = call(c, reqwest::Method::DELETE, &["connections", "username", name], None).await;
    Ok(())
}
