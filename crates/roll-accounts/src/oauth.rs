//! Sign-in with GitHub (OAuth 2.0) and Google (OpenID Connect), both with state and PKCE (S256).
//!
//! - The state row is single use, lives 10 minutes, and is bound to the browser that started
//!   the flow by a cookie (only its hash is stored): a callback URL carried to another browser
//!   does nothing.
//! - Only verified emails count: GitHub's primary verified email (`GET /user/emails`), Google's
//!   ID token with `email_verified` true. Google's ID token comes straight from its token
//!   endpoint over TLS, so its signature isn't checked (OpenID Connect Core 3.1.3.7); its
//!   issuer, audience, expiry and nonce are.
//! - An identity no account has never joins an account by email: linking needs the person
//!   signed in (`intent: link`).

use crate::{Accounts, Error, MINUTE, NewUser, Result, Secret, normalize_email, random, sha256_hex};
use base64::Engine as _;
use code_sql::{OptionalExtension, params};
use serde_json::Value;
use sha2::{Digest, Sha256};

const STATE_TTL_MS: i64 = 10 * MINUTE;
/// The most we read of a provider's answer.
const MAX_BODY: usize = 1 << 20;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Provider {
    Github,
    Google,
}

impl Provider {
    pub fn parse(s: &str) -> Option<Provider> {
        match s {
            "github" => Some(Provider::Github),
            "google" => Some(Provider::Google),
            _ => None,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Provider::Github => "github",
            Provider::Google => "google",
        }
    }
}

/// An OAuth client: its id, and its secret (never shown).
#[derive(Clone, Debug)]
pub struct OAuthApp {
    pub client_id: String,
    pub client_secret: Secret,
}

/// A provider's client and endpoints (the real ones by default; tests point them at fakes).
#[derive(Clone, Debug)]
pub struct ProviderConfig {
    pub provider: Provider,
    pub app: OAuthApp,
    pub authorize_url: String,
    pub token_url: String,
    /// GitHub: the REST API (`https://api.github.com`). Google: the ID token's issuer.
    pub api_url: String,
}

impl ProviderConfig {
    pub fn new(provider: Provider, app: OAuthApp) -> ProviderConfig {
        let (authorize_url, token_url, api_url) = match provider {
            Provider::Github => ("https://github.com/login/oauth/authorize", "https://github.com/login/oauth/access_token", "https://api.github.com"),
            Provider::Google => ("https://accounts.google.com/o/oauth2/v2/auth", "https://oauth2.googleapis.com/token", "https://accounts.google.com"),
        };
        ProviderConfig { provider, app, authorize_url: authorize_url.into(), token_url: token_url.into(), api_url: api_url.into() }
    }
}

/// A flow just started: where to send the browser, and the browser-binding cookie's value.
pub(crate) struct Started {
    pub url: String,
    pub browser: String,
}

/// How a callback ended well.
pub(crate) enum Finished {
    /// Sign the browser in as this user.
    SignIn { user: String, return_to: String },
    /// The identity is now linked to the signed-in user.
    Linked { return_to: String },
}

/// A callback's state row, used up.
struct State {
    intent: String,
    user_id: Option<String>,
    invite_id: Option<String>,
    verifier: String,
    nonce: String,
    return_to: String,
}

/// What the provider says about the person, with a verified email.
struct Verified {
    subject: String,
    email: String,
    login: String,
    name: Option<String>,
}

/// `return_to` if it's a path on the dashboard (`/…`, not `//…` or `/\…`), else `default`.
pub(crate) fn safe_return(return_to: Option<&str>, default: &str) -> String {
    match return_to.map(str::trim) {
        Some(p) if p.starts_with('/') && !p.starts_with("//") && !p.starts_with("/\\") && p.len() <= 512 && !p.bytes().any(|b| b.is_ascii_control()) => p.to_string(),
        _ => default.to_string(),
    }
}

fn b64url(b: &[u8]) -> String {
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(b)
}

impl Accounts {
    /// Start a sign-in (`link_user: None`) or a link to the signed-in `link_user`.
    pub(crate) fn oauth_start(&self, p: Provider, link_user: Option<&str>, invite: Option<&str>, return_to: Option<&str>) -> Result<Started> {
        let cfg = self.config.provider(p).ok_or_else(|| Error::NotFound(format!("sign-in with {} isn't configured", p.as_str())))?;
        let invite_id = match invite.map(str::trim).filter(|t| !t.is_empty()) {
            Some(t) => Some(self.valid_invite(t)?.ok_or_else(|| Error::BadRequest("the invite is used, revoked or expired".into(), Some("invite_invalid")))?.0),
            None => None,
        };
        let (state, verifier, nonce, browser) = (hex::encode(random::<32>()), b64url(&random::<32>()), hex::encode(random::<16>()), hex::encode(random::<32>()));
        let intent = if link_user.is_some() { "link" } else { "signin" };
        let return_to = safe_return(return_to, if link_user.is_some() { "/dashboard/account" } else { "/dashboard" });
        let now = self.db.now_ms()?;
        self.db.write(|c| {
            // Old, unused states go with each new one: the table stays small without a sweeper.
            c.execute("DELETE FROM oauth_states WHERE expires_at <= ?", [now])?;
            c.execute(
                "INSERT INTO oauth_states (state_hash, provider, intent, user_id, invite_id, verifier, nonce, browser_hash, return_to, created_at, expires_at) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
                params![sha256_hex(&state), p.as_str(), intent, link_user, invite_id, verifier, nonce, sha256_hex(&browser), return_to, now, now + STATE_TTL_MS],
            )
        })?;
        let mut url = url::Url::parse(&cfg.authorize_url).map_err(|e| Error::Internal(format!("{} authorize URL: {e}", p.as_str())))?;
        {
            let mut q = url.query_pairs_mut();
            q.append_pair("response_type", "code")
                .append_pair("client_id", &cfg.app.client_id)
                .append_pair("redirect_uri", &self.redirect_uri(p))
                .append_pair("state", &state)
                .append_pair("code_challenge", &b64url(&Sha256::digest(verifier.as_bytes())))
                .append_pair("code_challenge_method", "S256");
            match p {
                Provider::Github => q.append_pair("scope", "read:user user:email").append_pair("allow_signup", "false"),
                Provider::Google => q.append_pair("scope", "openid email profile").append_pair("nonce", &nonce).append_pair("prompt", "select_account"),
            };
        }
        Ok(Started { url: url.into(), browser })
    }

    fn redirect_uri(&self, p: Provider) -> String {
        format!("{}/v1/accounts/oauth/{}/callback", self.config.api_url, p.as_str())
    }

    /// Use up a state for `p`, if it's live and this browser started it.
    fn take_state(&self, p: Provider, state: &str, browser: Option<&str>) -> Result<State> {
        let now = self.db.now_ms()?;
        let bad = || Error::BadRequest("the sign-in expired or came from another browser; start again".into(), Some("state"));
        let hash = sha256_hex(state);
        let row = self.db.write(|c| {
            let row = c
                .query_row(
                    "SELECT intent, user_id, invite_id, verifier, nonce, browser_hash, return_to, expires_at FROM oauth_states WHERE state_hash = ? AND provider = ?",
                    [hash.as_str(), p.as_str()],
                    |r| {
                        Ok((
                            State { intent: r.get(0)?, user_id: r.get(1)?, invite_id: r.get(2)?, verifier: r.get(3)?, nonce: r.get(4)?, return_to: r.get(6)? },
                            r.get::<_, String>(5)?,
                            r.get::<_, i64>(7)?,
                        ))
                    },
                )
                .optional()?;
            if row.is_some() {
                c.execute("DELETE FROM oauth_states WHERE state_hash = ?", [hash.as_str()])?;
            }
            Ok::<_, Error>(row)
        })?;
        let Some((st, browser_hash, expires)) = row else { return Err(bad()) };
        let same_browser = browser.is_some_and(|b| roll_auth::constant_eq(sha256_hex(b).as_bytes(), browser_hash.as_bytes()));
        if expires <= now || !same_browser {
            return Err(bad());
        }
        Ok(st)
    }

    /// Finish a flow: check the state, trade the code, read the verified identity, and sign in
    /// or link. `Err` carries the `?error=` code; the second value is where to send the browser.
    pub(crate) async fn oauth_callback(self: &std::sync::Arc<Self>, p: Provider, code: &str, state: &str, browser: Option<String>) -> (Result<Finished>, String) {
        let fallback = "/signin".to_string();
        let this = self.clone();
        let (state, code_owned) = (state.to_string(), code.to_string());
        let st = match tokio::task::spawn_blocking(move || this.take_state(p, &state, browser.as_deref())).await {
            Ok(Ok(st)) => st,
            Ok(Err(e)) => return (Err(e), fallback),
            Err(e) => return (Err(Error::Internal(e.to_string())), fallback),
        };
        let back = if st.intent == "link" { st.return_to.clone() } else { fallback };
        let verified = match self.verified_identity(p, &code_owned, &st).await {
            Ok(v) => v,
            Err(e) => {
                tracing::info!(provider = p.as_str(), error = %e, "accounts: sign-in with the provider failed");
                return (Err(e), back);
            }
        };
        let this = self.clone();
        let done = tokio::task::spawn_blocking(move || this.finish(p, st, verified)).await.unwrap_or_else(|e| Err(Error::Internal(e.to_string())));
        (done, back)
    }

    fn finish(&self, p: Provider, st: State, v: Verified) -> Result<Finished> {
        let provider = p.as_str();
        let linked_to = self.db.read(|c| c.query_row("SELECT user_id FROM identities WHERE provider = ? AND subject = ?", [provider, v.subject.as_str()], |r| r.get::<_, String>(0)).optional())?;
        if st.intent == "link" {
            let user = st.user_id.ok_or_else(|| Error::BadRequest("sign in first".into(), Some("state")))?;
            match linked_to {
                Some(other) if other != user => return Err(Error::BadRequest("this identity belongs to another account".into(), Some("linked_elsewhere"))),
                Some(_) => {}
                None => {
                    let now = self.db.now_ms()?;
                    self.db.write(|c| {
                        // One identity per provider per user: a new link replaces the old one.
                        c.execute("DELETE FROM identities WHERE user_id = ? AND provider = ?", [user.as_str(), provider])?;
                        c.execute(
                            "INSERT INTO identities (provider, subject, user_id, email, login, created_at) VALUES (?, ?, ?, ?, ?, ?) ON CONFLICT (provider, subject) DO NOTHING",
                            params![provider, v.subject, user, v.email, v.login, now],
                        )
                    })?;
                }
            }
            return Ok(Finished::Linked { return_to: st.return_to });
        }
        if let Some(user) = linked_to {
            return Ok(Finished::SignIn { user, return_to: st.return_to });
        }
        if let Some(invite) = &st.invite_id {
            let hint = (p == Provider::Github).then_some(v.login.as_str());
            let user = self.create_user(NewUser { invite_id: invite, email: &v.email, hint, name: v.name.as_deref(), identity: Some((provider, &v.subject, &v.login)) })?;
            return Ok(Finished::SignIn { user, return_to: st.return_to });
        }
        if self.db.read(|c| c.query_row("SELECT 1 FROM users WHERE email = ?", [v.email.as_str()], |_| Ok(())).optional())?.is_some() {
            return Err(Error::BadRequest("an account has this email: sign in another way, then link it in settings".into(), Some("link_required")));
        }
        Err(Error::BadRequest("no account has this identity, and there was no invite".into(), Some("not_invited")))
    }

    async fn verified_identity(&self, p: Provider, code: &str, st: &State) -> Result<Verified> {
        let cfg = self.config.provider(p).ok_or_else(|| Error::NotFound(format!("sign-in with {} isn't configured", p.as_str())))?;
        let failed = |what: &str| Error::BadRequest(format!("{}: {what}", p.as_str()), Some("provider"));
        let redirect = self.redirect_uri(p);
        let form = [
            ("grant_type", "authorization_code"),
            ("code", code),
            ("redirect_uri", redirect.as_str()),
            ("client_id", cfg.app.client_id.as_str()),
            ("client_secret", cfg.app.client_secret.expose()),
            ("code_verifier", st.verifier.as_str()),
        ];
        let resp = self.http.post(&cfg.token_url).header("accept", "application/json").form(&form).send().await.map_err(|e| failed(&format!("token endpoint: {}", e.without_url())))?;
        let token = read_json(resp).await.map_err(|e| failed(&e))?;
        let access = token.get("access_token").and_then(Value::as_str).filter(|t| !t.is_empty()).ok_or_else(|| failed("no access token"))?;
        match p {
            Provider::Github => {
                let get = |path: &str| self.http.get(format!("{}{path}", cfg.api_url.trim_end_matches('/'))).bearer_auth(access).header("accept", "application/vnd.github+json").send();
                let user = read_json(get("/user").await.map_err(|e| failed(&e.without_url().to_string()))?).await.map_err(|e| failed(&e))?;
                let emails = read_json(get("/user/emails").await.map_err(|e| failed(&e.without_url().to_string()))?).await.map_err(|e| failed(&e))?;
                let subject = user.get("id").and_then(Value::as_u64).ok_or_else(|| failed("no user id"))?.to_string();
                let login = user.get("login").and_then(Value::as_str).unwrap_or_default().to_string();
                let primary = emails.as_array().into_iter().flatten().find(|e| e.get("primary").and_then(Value::as_bool) == Some(true) && e.get("verified").and_then(Value::as_bool) == Some(true));
                let email = primary.and_then(|e| e.get("email")).and_then(Value::as_str).ok_or_else(|| Error::BadRequest("GitHub has no verified primary email for this account".into(), Some("unverified_email")))?;
                let name = user.get("name").and_then(Value::as_str).map(str::to_string);
                Ok(Verified { subject, email: normalize_email(email)?, login, name })
            }
            Provider::Google => {
                let id_token = token.get("id_token").and_then(Value::as_str).ok_or_else(|| failed("no ID token"))?;
                let claims = id_token.split('.').nth(1).and_then(|p| base64::engine::general_purpose::URL_SAFE_NO_PAD.decode(p.trim_end_matches('=')).ok()).and_then(|b| serde_json::from_slice::<Value>(&b).ok()).ok_or_else(|| failed("an unreadable ID token"))?;
                let s = |k: &str| claims.get(k).and_then(Value::as_str);
                let issuer = cfg.api_url.trim_end_matches('/');
                let iss_ok = s("iss").is_some_and(|i| i == issuer || Some(i) == issuer.strip_prefix("https://"));
                let aud_ok = match claims.get("aud") {
                    Some(Value::String(a)) => *a == cfg.app.client_id,
                    Some(Value::Array(a)) => a.iter().any(|x| x.as_str() == Some(cfg.app.client_id.as_str())),
                    _ => false,
                };
                let now_s = self.db.now_ms()? / 1000;
                let exp_ok = claims.get("exp").and_then(Value::as_i64).is_some_and(|e| e > now_s);
                let nonce_ok = s("nonce").is_some_and(|n| roll_auth::constant_eq(n.as_bytes(), st.nonce.as_bytes()));
                if !(iss_ok && aud_ok && exp_ok && nonce_ok) {
                    return Err(failed("the ID token isn't for this sign-in"));
                }
                let verified = matches!(claims.get("email_verified"), Some(Value::Bool(true))) || s("email_verified") == Some("true");
                let email = s("email").filter(|_| verified).ok_or_else(|| Error::BadRequest("Google has no verified email for this account".into(), Some("unverified_email")))?;
                let subject = s("sub").filter(|x| !x.is_empty()).ok_or_else(|| failed("no subject"))?.to_string();
                let email = normalize_email(email)?;
                Ok(Verified { subject, login: email.clone(), email, name: s("name").map(str::to_string) })
            }
        }
    }
}

/// A provider's JSON answer, at most [`MAX_BODY`] bytes; a non-2xx status or an `error` field
/// is an error (its text, never a token).
async fn read_json(mut resp: reqwest::Response) -> std::result::Result<Value, String> {
    let status = resp.status();
    let mut body = Vec::new();
    while let Some(chunk) = resp.chunk().await.map_err(|e| e.without_url().to_string())? {
        if body.len() + chunk.len() > MAX_BODY {
            return Err("the answer is too big".into());
        }
        body.extend_from_slice(&chunk);
    }
    let v: Value = serde_json::from_slice(&body).map_err(|_| format!("HTTP {status}: not JSON"))?;
    if !status.is_success() {
        return Err(format!("HTTP {status}"));
    }
    if let Some(e) = v.get("error").and_then(Value::as_str) {
        return Err(format!("refused: {}", e.chars().take(100).collect::<String>()));
    }
    Ok(v)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn return_paths_stay_on_the_dashboard() {
        assert_eq!(safe_return(Some("/dashboard/code?x=1"), "/d"), "/dashboard/code?x=1");
        for bad in ["//evil.example", "https://evil.example", "/\\evil.example", "dashboard", ""] {
            assert_eq!(safe_return(Some(bad), "/d"), "/d", "{bad}");
        }
    }
}
