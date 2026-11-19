//! Elasticsearch and OpenSearch outside roll, through their security REST APIs over HTTPS, as
//! Vault's Elasticsearch database plugin: the same guards as ClickHouse (a public address resolved
//! here and pinned, or a gateway's stream; TLS verified against the URL's host with the system's
//! roots and `ca`; no redirects, no proxy: https.rs). Users are the native realm's (Elasticsearch, `_security/user`) or the
//! security plugin's internal users (OpenSearch, `_plugins/_security/api/internalusers`); the
//! group is a role a dynamic user gets. Neither has an expiry for a user: a dynamic one lasts until
//! roll ends its lease, which deletes it (both clear their realm's cache for the user, so the next
//! request with it is refused). Answers are read up to [`ANSWER_MAX`].
//!
//! Elasticsearch echoes parts of a request's body in some errors, so a refused request that
//! carried a body (each one carries a password) is reported by its status and `error.type` alone.

use super::https::{Https, secret};
use super::{TIMEOUT, Via};
use hyper::Method;
use serde_json::{Value as Json, json};

/// The most of one answer read.
const ANSWER_MAX: usize = 64 * 1024;

#[derive(Clone, Copy, PartialEq)]
enum Flavor {
    Elasticsearch,
    OpenSearch,
}

/// The URL's settings: `elasticsearch://admin:password@host:9200` or `opensearch://…`, nothing else.
pub(super) struct Url {
    flavor: Flavor,
    host: String,
    port: u16,
    user: String,
    password: String,
}

pub(super) fn parse(url: &str) -> Result<Url, String> {
    let bad = || "the URL is elasticsearch://admin:password@host:9200 or opensearch://admin:password@host:9200 (the HTTPS port)".to_string();
    let u = url::Url::parse(url).map_err(|_| bad())?;
    let flavor = match u.scheme() {
        "elasticsearch" => Flavor::Elasticsearch,
        "opensearch" => Flavor::OpenSearch,
        _ => return Err(bad()),
    };
    let host = match u.host() {
        Some(url::Host::Domain(d)) => d.to_string(),
        Some(url::Host::Ipv4(ip)) => ip.to_string(),
        Some(url::Host::Ipv6(ip)) => ip.to_string(),
        None => return Err(bad()),
    };
    if u.username().is_empty() || !matches!(u.path(), "" | "/") || u.fragment().is_some() {
        return Err(bad());
    }
    if let Some((k, _)) = u.query_pairs().next() {
        return Err(format!("the URL takes no settings, not {k}"));
    }
    let decode = |s: &str| super::mysql::percent_decode(s).ok_or_else(bad);
    Ok(Url { flavor, host, port: u.port().unwrap_or(9200), user: decode(u.username())?, password: decode(u.password().unwrap_or(""))? })
}

pub(super) struct Conn {
    http: Https,
    /// `Basic …` for the admin.
    auth: String,
    /// The admin's password, which OpenSearch's account API wants to change it.
    password: String,
    flavor: Flavor,
}

fn connect(u: &Url, ca: Option<&str>, via: &Via) -> Result<Conn, String> {
    use base64::Engine as _;
    let auth = format!("Basic {}", base64::engine::general_purpose::STANDARD.encode(format!("{}:{}", u.user, u.password)));
    Ok(Conn { http: Https::new(&u.host, u.port, ca, via.clone())?, auth, password: u.password.clone(), flavor: u.flavor })
}

/// One call to `segments` (each percent-encoded); `None` for a read's 404, the answer's JSON (or
/// null) otherwise.
async fn call(c: &Conn, method: Method, segments: &[&str], body: Option<Json>) -> Result<Option<Json>, String> {
    let path = segments.iter().map(|s| urlencoding::encode(s).into_owned()).collect::<Vec<_>>().join("/");
    let host = c.http.host();
    let read = method == Method::GET;
    let carried = body.is_some();
    let mut headers = vec![("authorization", secret(&c.auth, "admin's login")?)];
    let body = match body {
        Some(b) => {
            headers.push(("content-type", hyper::header::HeaderValue::from_static("application/json")));
            serde_json::to_vec(&b).map_err(|e| e.to_string())?
        }
        None => vec![],
    };
    let (status, bytes) = c.http.send(method, &format!("/{path}"), headers, body, ANSWER_MAX).await?;
    if status == hyper::StatusCode::NOT_FOUND && read {
        return Ok(None);
    }
    if !status.is_success() {
        let v = serde_json::from_slice::<Json>(&bytes).unwrap_or(Json::Null);
        // Elasticsearch: {"error": {"type", "reason"}}; OpenSearch's security API: {"status", "message"}.
        let kind = v["error"]["type"].as_str().or_else(|| v["status"].as_str());
        let reason = if carried { None } else { v["error"]["reason"].as_str().or_else(|| v["message"].as_str()) };
        let mut out = format!("{host}: {status}");
        for s in [kind, reason].into_iter().flatten() {
            out.push_str(": ");
            out.extend(s.chars().take(300));
        }
        return Err(out);
    }
    Ok(Some(if bytes.is_empty() { Json::Null } else { serde_json::from_slice(&bytes).unwrap_or(Json::Null) }))
}

/// A user's path.
fn user<'a>(c: &Conn, name: &'a str) -> Vec<&'a str> {
    match c.flavor {
        Flavor::Elasticsearch => vec!["_security", "user", name],
        Flavor::OpenSearch => vec!["_plugins", "_security", "api", "internalusers", name],
    }
}

pub(super) async fn run<T>(u: Url, ca: Option<String>, via: Via, work: impl AsyncFnOnce(&Conn) -> Result<T, String>) -> Result<T, String> {
    tokio::time::timeout(TIMEOUT, async move {
        let c = connect(&u, ca.as_deref(), &via)?;
        work(&c).await
    })
    .await
    .map_err(|_| "timed out".to_string())?
}

pub(super) async fn check(c: &Conn, roles: &[String], group: Option<&str>) -> Result<(), String> {
    // HTTP connects on the first request: this one proves TLS and the login.
    let me: &[&str] = match c.flavor {
        Flavor::Elasticsearch => &["_security", "_authenticate"],
        Flavor::OpenSearch => &["_plugins", "_security", "authinfo"],
    };
    call(c, Method::GET, me, None).await?.ok_or("the server has no security API: turn security on")?;
    for r in roles {
        if call(c, Method::GET, &user(c, r), None).await?.is_none() {
            return Err(format!("there's no user {r}"));
        }
    }
    if let Some(g) = group {
        let role: &[&str] = match c.flavor {
            Flavor::Elasticsearch => &["_security", "role", g],
            Flavor::OpenSearch => &["_plugins", "_security", "api", "roles", g],
        };
        if call(c, Method::GET, role, None).await?.is_none() {
            return Err(format!("there's no role {g}"));
        }
    }
    Ok(())
}

pub(super) async fn set_password(c: &Conn, role: &str, password: &str) -> Result<(), String> {
    match c.flavor {
        Flavor::Elasticsearch => call(c, Method::POST, &["_security", "user", role, "_password"], Some(json!({ "password": password }))).await,
        // A patch, so the user's roles, backend roles and attributes stay as they are.
        Flavor::OpenSearch => call(c, Method::PATCH, &user(c, role), Some(json!([{ "op": "add", "path": "/password", "value": password }]))).await,
    }
    .map(|_| ())
}

/// The URL's own user gets `password`: Elasticsearch's change-password API as that user (every
/// user may change its own), OpenSearch's account API, which wants the current password too (the
/// URL's). Neither echoes a body roll can see: `call` drops a refused body's reason.
pub(super) async fn set_own_password(c: &Conn, password: &str) -> Result<(), String> {
    match c.flavor {
        Flavor::Elasticsearch => call(c, Method::POST, &["_security", "user", "_password"], Some(json!({ "password": password }))).await,
        Flavor::OpenSearch => call(c, Method::PUT, &["_plugins", "_security", "api", "account"], Some(json!({ "current_password": c.password, "password": password }))).await,
    }
    .map(|_| ())
}

pub(super) async fn create_login(c: &Conn, name: &str, password: &str, group: &str) -> Result<(), String> {
    let body = match c.flavor {
        Flavor::Elasticsearch => json!({ "password": password, "roles": [group] }),
        Flavor::OpenSearch => json!({ "password": password, "opendistro_security_roles": [group] }),
    };
    call(c, Method::PUT, &user(c, name), Some(body)).await.map(|_| ())
}

pub(super) async fn drop_login(c: &Conn, name: &str) -> Result<(), String> {
    // A user that's gone already is fine.
    match call(c, Method::DELETE, &user(c, name), None).await {
        Ok(_) => Ok(()),
        Err(_) if call(c, Method::GET, &user(c, name), None).await?.is_none() => Ok(()),
        Err(e) => Err(e),
    }
}

#[cfg(test)]
mod tests {
    use super::super::tests::at;
    use super::*;
    use roll_secrets::outside::OutsideDb;

    /// Against a real Elasticsearch, run by scripts/secrets-outside-check.sh (security and TLS on,
    /// `ROLL_E2E_ES_URL` the `elastic` superuser's, `ROLL_E2E_OUTSIDE_CA` its CA).
    #[tokio::test(flavor = "multi_thread")]
    #[ignore]
    async fn a_real_elasticsearch_rotates_and_makes_and_deletes_users() {
        real("ROLL_E2E_ES_URL").await
    }

    /// The same against a real OpenSearch (`ROLL_E2E_OS_URL`, the demo `admin`).
    #[tokio::test(flavor = "multi_thread")]
    #[ignore]
    async fn a_real_opensearch_rotates_and_makes_and_deletes_users() {
        real("ROLL_E2E_OS_URL").await
    }

    fn pw() -> String {
        let mut b = [0u8; 18];
        getrandom::fill(&mut b).unwrap();
        b.iter().map(|x| format!("{x:02x}")).collect::<String>() + "Aa1"
    }

    /// `url` with another user and password.
    fn as_user(url: &str, user: &str, password: &str) -> String {
        let mut u = url::Url::parse(url).unwrap();
        u.set_username(user).unwrap();
        u.set_password(Some(password)).unwrap();
        u.to_string()
    }

    /// One call as whoever `url` names.
    async fn send(url: &str, ca: &str, method: Method, segments: &[&str], body: Option<Json>) -> Result<Option<Json>, String> {
        let segments: Vec<String> = segments.iter().map(|s| s.to_string()).collect();
        run(parse(url).unwrap(), Some(ca.to_string()), Via::Direct { allow_private: true }, async move |c| call(c, method, &segments.iter().map(String::as_str).collect::<Vec<_>>(), body).await).await
    }

    async fn real(var: &str) {
        let env = |k: &str| std::env::var(k).unwrap_or_else(|_| panic!("{k}"));
        let (root, ca) = (env(var), std::fs::read_to_string(env("ROLL_E2E_OUTSIDE_CA")).unwrap());
        let os = root.starts_with("opensearch://");
        let put = Method::PUT;
        // Fixtures, as the superuser: two users that take turns, a role that reads `app`, and a
        // narrower admin roll registers with.
        let (admin_pw, start) = (pw(), pw());
        let reader = json!({ "cluster": [], "indices": [{ "names": ["app"], "privileges": ["read"] }] });
        let os_reader = json!({ "cluster_permissions": [], "index_permissions": [{ "index_patterns": ["app"], "allowed_actions": ["read"] }] });
        if os {
            send(&root, &ca, put.clone(), &["_plugins", "_security", "api", "roles", "reader"], Some(os_reader)).await.unwrap();
            for u in ["app_blue", "app_green"] {
                send(&root, &ca, put.clone(), &["_plugins", "_security", "api", "internalusers", u], Some(json!({ "password": start, "backend_roles": ["svc"], "opendistro_security_roles": ["reader"] }))).await.unwrap();
            }
            send(&root, &ca, put.clone(), &["_plugins", "_security", "api", "internalusers", "roll_admin"], Some(json!({ "password": admin_pw }))).await.unwrap();
            send(&root, &ca, put.clone(), &["_plugins", "_security", "api", "rolesmapping", "security_rest_api_access"], Some(json!({ "users": ["roll_admin"] }))).await.unwrap();
        } else {
            send(&root, &ca, put.clone(), &["_security", "role", "reader"], Some(reader)).await.unwrap();
            send(&root, &ca, put.clone(), &["_security", "role", "roll_admin"], Some(json!({ "cluster": ["manage_security"] }))).await.unwrap();
            for (u, p, r) in [("app_blue", &start, "reader"), ("app_green", &start, "reader"), ("roll_admin", &admin_pw, "roll_admin")] {
                send(&root, &ca, put.clone(), &["_security", "user", u], Some(json!({ "password": p, "roles": [r] }))).await.unwrap();
            }
        }
        send(&root, &ca, put.clone(), &["app", "_doc", "1"], Some(json!({ "n": 1 }))).await.unwrap();
        send(&root, &ca, put.clone(), &["other", "_doc", "1"], Some(json!({ "n": 1 }))).await.unwrap();
        let admin = as_user(&root, "roll_admin", &admin_pw);
        tokio::task::spawn_blocking(move || {
            let g = super::super::tests::direct();
            let roles = ["app_blue".to_string(), "app_green".to_string()];
            let logs_in = |user: &str, password: &str| g.check(&at(&as_user(&admin, user, password), Some(&ca)), &[], None);
            // Registering: the narrower admin, both users and the role; refusals for what isn't there.
            g.check(&at(&admin, Some(&ca)), &roles, Some("reader")).unwrap();
            assert!(g.check(&at(&admin, Some(&ca)), &["nobody".into()], None).unwrap_err().contains("there's no user nobody"));
            assert!(g.check(&at(&admin, Some(&ca)), &[], Some("nope")).unwrap_err().contains("there's no role nope"));
            let wrong = g.check(&at(&as_user(&admin, "roll_admin", "fake-wrong-pw"), Some(&ca)), &[], None).unwrap_err();
            assert!(wrong.contains("401") && !wrong.contains("fake-wrong-pw"), "{wrong}");
            let untrusted = g.check(&at(&admin, None), &[], None).unwrap_err();
            assert!(untrusted.to_lowercase().contains("certificate"), "{untrusted}");
            // Rotation: the new password logs in, the old one doesn't, and the user keeps its roles.
            let (first, second) = (pw(), pw());
            g.set_password(&at(&admin, Some(&ca)), "app_blue", &first).unwrap();
            logs_in("app_blue", &first).unwrap();
            assert!(logs_in("app_blue", &start).is_err(), "the starting password is refused");
            g.set_password(&at(&admin, Some(&ca)), "app_blue", &second).unwrap();
            logs_in("app_blue", &second).unwrap();
            assert!(logs_in("app_blue", &first).is_err(), "the old password is refused");
            let rt = tokio::runtime::Handle::current();
            let blue = as_user(&admin, "app_blue", &second);
            rt.block_on(send(&blue, &ca, Method::GET, &["app", "_search"], None)).expect("app_blue kept its role");
            // A dynamic user: made with the group, reads `app` and nothing else, then is gone (twice:
            // gone counts as deleted).
            let (name, upw) = ("roll_fakefakefakefake", pw());
            g.create_login(&at(&admin, Some(&ca)), name, &upw, "reader", 0).unwrap();
            let user = as_user(&admin, name, &upw);
            rt.block_on(send(&user, &ca, Method::GET, &["app", "_search"], None)).unwrap();
            let denied = rt.block_on(send(&user, &ca, Method::GET, &["other", "_search"], None)).unwrap_err();
            assert!(denied.contains("403"), "{denied}");
            let denied = rt.block_on(send(&user, &ca, Method::PUT, &["app", "_doc", "2"], Some(json!({ "n": 2 })))).unwrap_err();
            assert!(denied.contains("403"), "{denied}");
            g.drop_login(&at(&admin, Some(&ca)), name).unwrap();
            assert!(logs_in(name, &upw).is_err(), "a deleted user is refused");
            g.drop_login(&at(&admin, Some(&ca)), name).unwrap();
            // The admin's own password (rotate-admin): the new one logs in and still manages the
            // users, the old one is refused.
            let own = pw();
            g.set_own_password(&at(&admin, Some(&ca)), &own).unwrap();
            g.check(&at(&as_user(&admin, "roll_admin", &own), Some(&ca)), &roles, Some("reader")).unwrap();
            let old = g.check(&at(&admin, Some(&ca)), &[], None).unwrap_err();
            assert!(old.contains("401") && !old.contains(&own), "{old}");
        })
        .await
        .unwrap();
    }
}
