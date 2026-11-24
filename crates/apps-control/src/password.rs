//! A password for an environment (Vercel's and Netlify's password protection; docs/apps/
//! README.md, "A password for an environment"): `access: {password}` keeps its argon2id hash
//! only, roll's own page asks a visitor for it in place of the 401, and the right one sets a
//! host-only cookie signed for seven days (a changed password ends every session). Each client
//! gets ten tries every ten minutes per environment. The token link and signing in stay ways in.

use crate::Apps;
use argon2::password_hash::{PasswordHash, PasswordHasher, PasswordVerifier, SaltString};
use argon2::{Algorithm, Argon2, Params, Version};
use axum::http::{HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use std::sync::Arc;

/// Whose tries are counted: an environment and a client's address.
pub(crate) type Client = (String, Option<std::net::IpAddr>);

/// Where roll's page posts the password.
pub const PATH: &str = "/__roll/password";
const DAYS: i64 = 7;
const TRIES: u32 = 10;
const WINDOW_MS: i64 = 10 * 60 * 1000;

fn hasher() -> Argon2<'static> {
    Argon2::new(Algorithm::Argon2id, Version::V0x13, Params::new(19_456, 2, 1, None).expect("valid argon2 parameters"))
}

pub async fn hash(password: &str) -> Result<String, String> {
    let pw = zeroize::Zeroizing::new(password.to_string());
    tokio::task::spawn_blocking(move || {
        let mut salt = [0u8; 16];
        getrandom::fill(&mut salt).map_err(|e| format!("randomness: {e}"))?;
        let salt = SaltString::encode_b64(&salt).map_err(|e| e.to_string())?;
        hasher().hash_password(pw.as_bytes(), &salt).map(|h| h.to_string()).map_err(|e| e.to_string())
    })
    .await
    .map_err(|e| e.to_string())?
}

async fn verify(password: &str, hash: &str) -> bool {
    let pw = zeroize::Zeroizing::new(password.to_string());
    let hash = hash.to_string();
    tokio::task::spawn_blocking(move || PasswordHash::new(&hash).is_ok_and(|h| hasher().verify_password(pw.as_bytes(), &h).is_ok())).await.unwrap_or(false)
}

pub fn cookie(env: &str) -> String {
    format!("roll_pass_{env}")
}

fn session_mac(key: &[u8], service: &str, env: &str, until: i64, hash: &str) -> String {
    // The hash is in it: a new password ends the sessions of the old one.
    crate::challenge::mac(key, &["password", service, env, &until.to_string(), hash])
}

impl Apps {
    async fn pass_key(self: &Arc<Self>) -> Option<Vec<u8>> {
        let v = crate::model::random(32);
        self.db(move |s| s.state_or("password_key", &v)).await.ok().map(String::into_bytes)
    }

    /// Whether a session cookie's value is one of this environment's, for its password now.
    pub(crate) async fn password_session_ok(self: &Arc<Self>, service: &str, env: &str, hash: &str, value: &str) -> bool {
        let Some((until, mac)) = value.split_once('.') else { return false };
        let Ok(until) = until.parse::<i64>() else { return false };
        let Some(key) = self.pass_key().await else { return false };
        until > code_sql::now_ms() / 1000 && crate::public::same(&session_mac(&key, service, env, until, hash), mac)
    }

    /// A POST of roll's password page: the right password sets the session and goes on to `to`
    /// (a path on this site); a wrong one asks again; past the tries, a 429.
    pub(crate) async fn password_post(self: &Arc<Self>, service: &str, env: &str, hash: &str, client: Option<std::net::IpAddr>, body: &[u8]) -> Response {
        let (mut given, mut to) = (String::new(), "/".to_string());
        for (k, v) in url::form_urlencoded::parse(body) {
            match k.as_ref() {
                "password" => given = v.into_owned(),
                "to" => to = crate::visit::local_path(&v),
                _ => {}
            }
        }
        let now = code_sql::now_ms();
        let key = (crate::env_key(service, env), client);
        {
            let mut tries = self.password_tries.lock();
            tries.retain(|_, (at, _)| now - *at < WINDOW_MS);
            let t = tries.entry(key.clone()).or_insert((now, 0));
            if t.1 >= TRIES {
                let mut r = page(StatusCode::TOO_MANY_REQUESTS, &to, "Too many tries from your address. Try again in a few minutes.");
                if let Ok(v) = HeaderValue::from_str(&((WINDOW_MS - (now - t.0)) / 1000).max(1).to_string()) {
                    r.headers_mut().insert(header::RETRY_AFTER, v);
                }
                return r;
            }
            t.1 += 1;
        }
        if given.is_empty() || given.len() > 200 || !verify(&given, hash).await {
            return page(StatusCode::UNAUTHORIZED, &to, "Wrong password. Try again.");
        }
        self.password_tries.lock().remove(&key);
        let Some(k) = self.pass_key().await else { return StatusCode::SERVICE_UNAVAILABLE.into_response() };
        let until = now / 1000 + DAYS * 86_400;
        let secure = if self.cfg.scheme == "https" { "; Secure" } else { "" };
        let c = format!("{}={until}.{}; Path=/; Max-Age={}; HttpOnly; SameSite=Lax{secure}", crate::public::host_only(self, cookie(env)), session_mac(&k, service, env, until, hash), DAYS * 86_400);
        let mut r = StatusCode::SEE_OTHER.into_response();
        let h = r.headers_mut();
        if let (Ok(l), Ok(c)) = (HeaderValue::from_str(&to), HeaderValue::from_str(&c)) {
            h.insert(header::LOCATION, l);
            h.insert(header::SET_COOKIE, c);
        }
        h.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
        r
    }
}

/// roll's page asking for the password: a 401 (a page load), its form posting to [`PATH`], as
/// roll's other pages look.
pub fn page(status: StatusCode, to: &str, note: &str) -> Response {
    let to = crate::public::escape(to);
    let note = if note.is_empty() { String::new() } else { format!("<p class=note>{}</p>", crate::public::escape(note)) };
    let html = format!(
        "<!doctype html><meta charset=utf-8><meta name=viewport content=\"width=device-width\"><meta name=robots content=noindex><title>Password needed</title>\
         <style>:root{{color-scheme:dark light;--bg:#000;--fg:#ededed;--muted:#a1a1a1;--accent:#4c8dff;--line:#ffffff24;--danger:#e5484d}}\
         @media (prefers-color-scheme:light){{:root{{--bg:#fff;--fg:#0a0a0a;--muted:#666;--line:#00000024}}}}\
         body{{margin:0;min-height:100vh;display:grid;place-items:center;background:var(--bg);color:var(--fg);font:14px system-ui,sans-serif}}\
         main{{width:min(320px,90vw);padding:24px}}svg{{display:block;width:28px;height:28px;margin:0 0 16px}}\
         h1{{font-size:18px;font-weight:600;letter-spacing:-.04em;margin:0 0 8px}}p{{color:var(--muted);margin:0 0 16px;line-height:1.5}}.note{{color:var(--danger)}}\
         input{{box-sizing:border-box;width:100%;padding:8px 10px;border-radius:8px;border:1px solid var(--line);background:transparent;color:inherit;font:inherit}}\
         button{{margin-top:8px;width:100%;padding:8px;border:0;border-radius:8px;background:var(--accent);color:#fff;font:inherit;font-weight:500;cursor:pointer}}</style>\
         <main>{}<h1>Password needed</h1><p>This site asks for a password.</p>{note}<form method=post action=\"{PATH}\"><input type=hidden name=to value=\"{to}\">\
         <input type=password name=password autocomplete=current-password autofocus required aria-label=Password><button type=submit>Continue</button></form></main>",
        crate::public::HORIZON
    );
    let mut r = (status, [(header::CONTENT_TYPE, "text/html; charset=utf-8"), (header::CACHE_CONTROL, "no-store")], html).into_response();
    r.extensions_mut().insert(crate::public::RollPage::Error);
    r
}
