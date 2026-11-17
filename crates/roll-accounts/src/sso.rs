//! Enterprise SSO (docs/accounts.md, "SSO"): people sign in through their company's OpenID
//! Connect provider (Okta, Entra ID, Google Workspace, JumpCloud…), chosen by their email's
//! domain, as Linear's, Vercel's and GitHub's enterprise sign-in work.
//!
//! - A connection is the operator's: an issuer, the client roll is registered as there (its
//!   secret sealed with `ROLL_SECRET_KEY`), and the email domains it answers for. A domain has one
//!   connection.
//! - The provider is fetched only through a client that refuses private addresses
//!   (`roll_net::PublicOnly`, plus IP literals) and follows no redirects, every answer bounded.
//!   Its ID token is checked by roll-oidc against the keys its discovery document names: issuer
//!   exactly, audience (roll's client id), times, and the nonce this sign-in sent.
//! - Only an email the provider says it verified, in one of the connection's domains, counts.
//!   The provider is trusted for its domains: an account with that email is the person's
//!   (linked at first sign-in), and with `provision` a new one is made for them.
//! - `enforce`: people of those domains sign in only this way (an email code, GitHub or Google
//!   is refused with `sso_required`); the operator's token always works, so an outage at the
//!   provider never locks roll out.

use crate::oauth::{Finished, b64url, read_json};
use crate::{Accounts, Error, NewUser, Result, normalize_email, random, sha256_hex};
use code_sql::{OptionalExtension, params};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

const STATE_TTL_MS: i64 = 10 * 60_000;
/// A discovery document or a key set, at most.
const MAX_DOC: usize = 256 << 10;
/// Connections a deployment may have.
const MAX_CONNECTIONS: usize = 20;
/// How long a provider's discovery document is reused: `sso/start` is public, so a flood of
/// starts must not become a flood of fetches at the provider.
const DISCOVERY_TTL: std::time::Duration = std::time::Duration::from_secs(600);

/// A connection, as the operator sees it (never its secret).
#[derive(Clone, Debug, Serialize)]
pub struct Connection {
    pub id: String,
    pub name: String,
    pub issuer: String,
    pub client_id: String,
    pub domains: Vec<String>,
    /// People of these domains sign in only through it.
    pub enforce: bool,
    /// A verified person of these domains with no account gets one (never an admin).
    pub provision: bool,
    pub created_at: i64,
    /// What to register at the provider as the sign-in redirect.
    pub redirect_uri: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NewConnection {
    #[serde(default)]
    pub name: Option<String>,
    pub issuer: String,
    pub client_id: String,
    pub client_secret: String,
    pub domains: Vec<String>,
    #[serde(default)]
    pub enforce: bool,
    #[serde(default)]
    pub provision: bool,
}

#[derive(Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub struct ConnectionChange {
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub client_id: Option<String>,
    #[serde(default)]
    pub client_secret: Option<String>,
    #[serde(default)]
    pub domains: Option<Vec<String>>,
    #[serde(default)]
    pub enforce: Option<bool>,
    #[serde(default)]
    pub provision: Option<bool>,
}

/// The provider's endpoints, from its discovery document.
#[derive(Clone)]
pub(crate) struct Endpoints {
    authorize: String,
    token: String,
    jwks: String,
}

fn domains_of(raw: &[String]) -> Result<Vec<String>> {
    let ok = |d: &str| !d.is_empty() && d.len() <= 253 && d.contains('.') && d.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'.' || b == b'-');
    let mut out: Vec<String> = raw.iter().map(|d| d.trim().trim_start_matches('@').to_ascii_lowercase()).collect();
    out.sort();
    out.dedup();
    if out.is_empty() || out.len() > 50 || !out.iter().all(|d| ok(d)) {
        return Err(Error::BadRequest("domains are 1 to 50 email domains (acme.com)".into(), Some("domains")));
    }
    Ok(out)
}

fn domain_of(email: &str) -> &str {
    email.rsplit_once('@').map(|(_, d)| d).unwrap_or_default()
}

fn text(s: &str, what: &str, max: usize) -> Result<String> {
    let s = s.trim();
    if s.is_empty() || s.len() > max || s.contains(char::is_control) {
        return Err(Error::BadRequest(format!("{what}: 1 to {max} characters"), None));
    }
    Ok(s.to_string())
}

impl Accounts {
    fn sso_redirect_uri(&self, id: &str) -> String {
        format!("{}/v1/accounts/sso/{id}/callback", self.config.api_url)
    }

    fn connection_row(&self, r: &code_sql::Row<'_>) -> code_sql::Result<Connection> {
        let id: String = r.get(0)?;
        let domains: String = r.get(4)?;
        Ok(Connection {
            redirect_uri: self.sso_redirect_uri(&id),
            id,
            name: r.get(1)?,
            issuer: r.get(2)?,
            client_id: r.get(3)?,
            domains: serde_json::from_str(&domains).unwrap_or_default(),
            enforce: r.get(5)?,
            provision: r.get(6)?,
            created_at: r.get(7)?,
        })
    }

    pub fn sso_connections(&self) -> Result<Vec<Connection>> {
        Ok(self.db.read(|c| c.query("SELECT id, name, issuer, client_id, domains, enforce, provision, created_at FROM sso_connections ORDER BY created_at", [], |r| self.connection_row(r)))?)
    }

    /// The connection that answers for `email`'s domain, if any.
    pub(crate) fn sso_for(&self, email: &str) -> Result<Option<Connection>> {
        let domain = domain_of(email);
        Ok(self.sso_connections()?.into_iter().find(|c| c.domains.iter().any(|d| d == domain)))
    }

    /// Refused when `user`'s email is in a domain whose connection enforces SSO: their sessions
    /// come only from [`Accounts::issue_sso`].
    pub(crate) fn sso_not_enforced(&self, user: &str) -> Result<()> {
        let email = self.db.read(|c| c.query_row("SELECT email FROM users WHERE id = ?", [user], |r| r.get::<_, String>(0)).optional())?;
        match email.map(|e| self.sso_for(&e)).transpose()?.flatten() {
            Some(conn) if conn.enforce => Err(Error::Forbidden(format!("sign in with {}: your organization requires it", conn.name), "sso_required")),
            _ => Ok(()),
        }
    }

    /// The same check for an address (an email code's start): no account lookup, the domain alone.
    pub(crate) fn sso_required_for(&self, email: &str) -> Result<Option<String>> {
        Ok(self.sso_for(email)?.filter(|c| c.enforce).map(|c| c.name))
    }

    /// Check a connection's settings, and that its issuer is an https URL roll may reach.
    fn check_issuer(&self, issuer: &str) -> Result<String> {
        let issuer = issuer.trim().trim_end_matches('/').to_string();
        let u = url::Url::parse(&issuer).map_err(|_| Error::BadRequest("the issuer is the provider's URL (https://acme.okta.com)".into(), Some("issuer")))?;
        if u.scheme() != "https" && !self.config.sso_allow_private || issuer.len() > 300 {
            return Err(Error::BadRequest("the issuer is an https URL".into(), Some("issuer")));
        }
        Ok(issuer)
    }

    /// Add a connection: its discovery document must answer first (a typo shows now, not at the
    /// first sign-in).
    pub async fn add_sso(&self, n: NewConnection) -> Result<Connection> {
        let sealer = self.sso_sealer()?;
        let issuer = self.check_issuer(&n.issuer)?;
        let name = match &n.name {
            Some(s) => text(s, "name", 100)?,
            None => url::Url::parse(&issuer).ok().and_then(|u| u.host_str().map(str::to_string)).unwrap_or_else(|| issuer.clone()),
        };
        let client_id = text(&n.client_id, "client_id", 300)?;
        let secret = text(&n.client_secret, "client_secret", 1000)?;
        let domains = domains_of(&n.domains)?;
        self.discover_now(&issuer).await?;
        let id = crate::new_id("sso");
        let sealed = sealer.seal(&id, &secret);
        let now = self.db.now_ms()?;
        let encoded = json!(domains).to_string();
        self.db.write(|c| {
            let n_conns: i64 = c.query_row("SELECT COUNT(*) FROM sso_connections", [], |r| r.get(0))?;
            if n_conns as usize >= MAX_CONNECTIONS {
                return Err(Error::Conflict(format!("at most {MAX_CONNECTIONS} SSO connections")));
            }
            taken(c, &domains, None)?;
            c.execute(
                "INSERT INTO sso_connections (id, name, issuer, client_id, client_secret, domains, enforce, provision, created_at) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?)",
                params![id, name, issuer, client_id, sealed, encoded, n.enforce, n.provision, now],
            )?;
            Ok(())
        })?;
        self.sso_connection(&id)
    }

    fn sso_connection(&self, id: &str) -> Result<Connection> {
        self.db
            .read(|c| c.query_row("SELECT id, name, issuer, client_id, domains, enforce, provision, created_at FROM sso_connections WHERE id = ?", [id], |r| self.connection_row(r)).optional())?
            .ok_or_else(|| Error::NotFound(format!("no SSO connection {id}")))
    }

    pub fn update_sso(&self, id: &str, ch: ConnectionChange) -> Result<Connection> {
        let name = ch.name.as_deref().map(|s| text(s, "name", 100)).transpose()?;
        let client_id = ch.client_id.as_deref().map(|s| text(s, "client_id", 300)).transpose()?;
        let sealed = match ch.client_secret.as_deref() {
            Some(s) => Some(self.sso_sealer()?.seal(id, &text(s, "client_secret", 1000)?)),
            None => None,
        };
        let domains = ch.domains.as_deref().map(domains_of).transpose()?;
        self.db.write(|c| {
            if c.query_row("SELECT 1 FROM sso_connections WHERE id = ?", [id], |_| Ok(())).optional()?.is_none() {
                return Err(Error::NotFound(format!("no SSO connection {id}")));
            }
            if let Some(d) = &domains {
                taken(c, d, Some(id))?;
                c.execute("UPDATE sso_connections SET domains = ? WHERE id = ?", params![json!(d).to_string(), id])?;
            }
            if let Some(n) = &name {
                c.execute("UPDATE sso_connections SET name = ? WHERE id = ?", params![n, id])?;
            }
            if let Some(cid) = &client_id {
                c.execute("UPDATE sso_connections SET client_id = ? WHERE id = ?", params![cid, id])?;
            }
            if let Some(s) = &sealed {
                c.execute("UPDATE sso_connections SET client_secret = ? WHERE id = ?", params![s, id])?;
            }
            if let Some(e) = ch.enforce {
                c.execute("UPDATE sso_connections SET enforce = ? WHERE id = ?", params![e, id])?;
            }
            if let Some(p) = ch.provision {
                c.execute("UPDATE sso_connections SET provision = ? WHERE id = ?", params![p, id])?;
            }
            Ok(())
        })?;
        self.sso_connection(id)
    }

    /// Remove a connection: its people's links go with it (their accounts stay, and sign in
    /// another way; an enforced domain is free again).
    pub fn remove_sso(&self, id: &str) -> Result<()> {
        let provider = format!("sso:{id}");
        self.db.write(|c| {
            if c.execute("DELETE FROM sso_connections WHERE id = ?", [id])? == 0 {
                return Err(Error::NotFound(format!("no SSO connection {id}")));
            }
            c.execute("DELETE FROM identities WHERE provider = ?", [provider.as_str()])?;
            c.execute("DELETE FROM oauth_states WHERE provider = ?", [provider.as_str()])?;
            Ok(())
        })
    }

    /// The provider's endpoints, from `<issuer>/.well-known/openid-configuration`, which must
    /// name the same issuer.
    async fn discover(&self, issuer: &str) -> Result<Endpoints> {
        let cached = self.sso_discovery.lock().unwrap_or_else(|e| e.into_inner()).get(issuer).filter(|(_, at)| at.elapsed() < DISCOVERY_TTL).map(|(e, _)| e.clone());
        if let Some(e) = cached {
            return Ok(e);
        }
        let e = self.discover_now(issuer).await?;
        let mut cache = self.sso_discovery.lock().unwrap_or_else(|e| e.into_inner());
        cache.retain(|_, (_, at)| at.elapsed() < DISCOVERY_TTL);
        cache.insert(issuer.to_string(), (e.clone(), std::time::Instant::now()));
        Ok(e)
    }

    async fn discover_now(&self, issuer: &str) -> Result<Endpoints> {
        let failed = |what: String| Error::BadRequest(format!("the SSO provider: {what}"), Some("provider"));
        let doc = self.sso_get(&format!("{issuer}/.well-known/openid-configuration")).await.map_err(failed)?;
        let s = |k: &str| doc.get(k).and_then(Value::as_str).map(str::to_string);
        if s("issuer").as_deref().map(|i| i.trim_end_matches('/')) != Some(issuer) {
            return Err(failed(format!("its discovery document names another issuer than {issuer}")));
        }
        let (Some(authorize), Some(token), Some(jwks)) = (s("authorization_endpoint"), s("token_endpoint"), s("jwks_uri")) else {
            return Err(failed("its discovery document lacks an endpoint".into()));
        };
        for u in [&authorize, &token, &jwks] {
            self.sso_url(u).map_err(failed)?;
        }
        Ok(Endpoints { authorize, token, jwks })
    }

    /// A URL the SSO client may fetch: https (http only where private addresses are allowed, in
    /// development), and no private address written out.
    fn sso_url(&self, raw: &str) -> std::result::Result<url::Url, String> {
        let u = url::Url::parse(raw).map_err(|_| format!("{raw:?} isn't a URL"))?;
        if self.config.sso_allow_private {
            return Ok(u);
        }
        if u.scheme() != "https" || roll_net::literal_private_host(&u) {
            return Err(format!("{} isn't a public https URL", u.host_str().unwrap_or("?")));
        }
        Ok(u)
    }

    async fn sso_get(&self, raw: &str) -> std::result::Result<Value, String> {
        let u = self.sso_url(raw)?;
        let resp = self.sso_http.get(u).header("accept", "application/json").send().await.map_err(|e| e.without_url().to_string())?;
        read_bounded(resp, MAX_DOC).await
    }

    /// Start a sign-in through the connection that answers for `email`.
    pub(crate) async fn sso_start(&self, email: &str, invite: Option<&str>, return_to: Option<&str>) -> Result<crate::oauth::Started> {
        let email = normalize_email(email)?;
        let conn = self.sso_for(&email)?.ok_or_else(|| Error::NotFound("no SSO connection answers for that email's domain".into()))?;
        let ep = self.discover(&conn.issuer).await?;
        let invite_id = match invite.map(str::trim).filter(|t| !t.is_empty()) {
            Some(t) => Some(self.valid_invite(t)?.ok_or_else(|| Error::BadRequest("the invite is used, revoked or expired".into(), Some("invite_invalid")))?.0),
            None => None,
        };
        let (state, verifier, nonce, browser) = (hex::encode(random::<32>()), b64url(&random::<32>()), hex::encode(random::<16>()), hex::encode(random::<32>()));
        let return_to = crate::oauth::safe_return(return_to, "/dashboard");
        let now = self.db.now_ms()?;
        let provider = format!("sso:{}", conn.id);
        self.db.write(|c| {
            c.execute("DELETE FROM oauth_states WHERE expires_at <= ?", [now])?;
            c.execute(
                "INSERT INTO oauth_states (state_hash, provider, intent, user_id, invite_id, verifier, nonce, browser_hash, return_to, created_at, expires_at) VALUES (?, ?, 'signin', NULL, ?, ?, ?, ?, ?, ?, ?)",
                params![sha256_hex(&state), provider, invite_id, verifier, nonce, sha256_hex(&browser), return_to, now, now + STATE_TTL_MS],
            )
        })?;
        let mut url = url::Url::parse(&ep.authorize).map_err(|e| Error::Internal(format!("SSO authorize URL: {e}")))?;
        url.query_pairs_mut()
            .append_pair("response_type", "code")
            .append_pair("client_id", &conn.client_id)
            .append_pair("redirect_uri", &self.sso_redirect_uri(&conn.id))
            .append_pair("scope", "openid email profile")
            .append_pair("state", &state)
            .append_pair("nonce", &nonce)
            .append_pair("code_challenge", &b64url(&Sha256::digest(verifier.as_bytes())))
            .append_pair("code_challenge_method", "S256")
            .append_pair("login_hint", &email);
        Ok(crate::oauth::Started { url: url.into(), browser })
    }

    /// Finish a sign-in: the state, the code traded, the ID token checked, then the person.
    pub(crate) async fn sso_callback(&self, id: &str, code: &str, state: &str, browser: Option<&str>) -> Result<Finished> {
        let st = self.take_state_named(&format!("sso:{id}"), state, browser)?;
        let conn = self.sso_connection(id)?;
        let secret = {
            let sealed = self.db.read(|c| c.query_row("SELECT client_secret FROM sso_connections WHERE id = ?", [id], |r| r.get::<_, String>(0)))?;
            self.sso_sealer()?.open(id, &sealed).ok_or_else(|| Error::Internal("the SSO client secret doesn't open (ROLL_SECRET_KEY changed?)".into()))?
        };
        let ep = self.discover(&conn.issuer).await?;
        let failed = |what: &str| Error::BadRequest(format!("the SSO provider: {what}"), Some("provider"));
        let redirect = self.sso_redirect_uri(id);
        let form = [
            ("grant_type", "authorization_code"),
            ("code", code),
            ("redirect_uri", redirect.as_str()),
            ("client_id", conn.client_id.as_str()),
            ("client_secret", secret.as_str()),
            ("code_verifier", st.verifier.as_str()),
        ];
        let token_url = self.sso_url(&ep.token).map_err(|e| failed(&e))?;
        let resp = self.sso_http.post(token_url).header("accept", "application/json").form(&form).send().await.map_err(|e| failed(&format!("token endpoint: {}", e.without_url())))?;
        let token = read_json(resp).await.map_err(|e| failed(&e))?;
        let id_token = token.get("id_token").and_then(Value::as_str).ok_or_else(|| failed("no ID token"))?.to_string();
        let jwks = self.sso_get(&ep.jwks).await.map_err(|e| failed(&e))?.to_string();
        let jwt = roll_oidc::Jwt::parse(&id_token).map_err(|e| failed(&e.to_string()))?;
        let now_s = self.db.now_ms()? / 1000;
        roll_oidc::Keys::default().verify(None, Some(&jwks), &jwt, &conn.issuer, &conn.client_id, now_s).map_err(|e| failed(&e.to_string()))?;
        if !jwt.str("nonce").is_some_and(|n| roll_auth::constant_eq(n.as_bytes(), st.nonce.as_bytes())) {
            return Err(failed("the ID token isn't for this sign-in"));
        }
        // Only an email the provider verified, in plain ASCII, in one of its domains.
        let verified = matches!(jwt.claims.get("email_verified"), Some(Value::Bool(true))) || jwt.str("email_verified") == Some("true");
        let email = jwt.str("email").filter(|e| verified && e.is_ascii()).ok_or_else(|| Error::BadRequest("the SSO provider has no verified email for this person".into(), Some("unverified_email")))?;
        let email = normalize_email(email)?;
        if !conn.domains.iter().any(|d| d == domain_of(&email)) {
            return Err(Error::BadRequest("that email's domain isn't one this SSO connection answers for".into(), Some("domain")));
        }
        let subject = jwt.str("sub").filter(|s| !s.is_empty() && s.len() <= 255).ok_or_else(|| failed("no subject"))?.to_string();
        let name = jwt.str("name").map(|n| n.chars().filter(|c| !c.is_control()).take(100).collect::<String>());
        self.sso_finish(&conn, st, &email, &subject, name.as_deref())
    }

    fn sso_finish(&self, conn: &Connection, st: crate::oauth::State, email: &str, subject: &str, name: Option<&str>) -> Result<Finished> {
        let provider = format!("sso:{}", conn.id);
        let now = self.db.now_ms()?;
        // Linked already, or the account with this email (the provider is trusted for its
        // domains): linked now, in one write.
        let user = self.db.write(|c| {
            if let Some(u) = c.query_row("SELECT user_id FROM identities WHERE provider = ? AND subject = ?", [provider.as_str(), subject], |r| r.get::<_, String>(0)).optional()? {
                return Ok::<_, Error>(Some(u));
            }
            let Some(u) = c.query_row("SELECT id FROM users WHERE email = ?", [email], |r| r.get::<_, String>(0)).optional()? else { return Ok(None) };
            c.execute("DELETE FROM identities WHERE user_id = ? AND provider = ?", [u.as_str(), provider.as_str()])?;
            c.execute(
                "INSERT INTO identities (provider, subject, user_id, email, login, created_at) VALUES (?, ?, ?, ?, ?, ?) ON CONFLICT (provider, subject) DO NOTHING",
                params![provider, subject, u, email, email, now],
            )?;
            Ok(Some(u))
        })?;
        if let Some(user) = user {
            return Ok(Finished::SignIn { user, return_to: st.return_to });
        }
        // No account: an invite, or a connection that provisions its people (never as admins).
        let invite = match &st.invite_id {
            Some(i) => Some(i.clone()),
            None if conn.provision => None,
            None => return Err(Error::BadRequest("no account has this email, and there was no invite".into(), Some("not_invited"))),
        };
        let user = match &invite {
            Some(i) => self.create_user(NewUser { invite_id: i, email, hint: None, name, identity: Some((&provider, subject, email)) })?,
            None => self.provision_user(email, name, (&provider, subject))?,
        };
        Ok(Finished::SignIn { user, return_to: st.return_to })
    }
}

/// Another connection holds one of `domains` (`except`: the one being changed).
fn taken(c: &code_sql::Conn<'_>, domains: &[String], except: Option<&str>) -> Result<()> {
    let rows: Vec<(String, String)> = c.query("SELECT id, domains FROM sso_connections", [], |r| Ok((r.get(0)?, r.get(1)?)))?;
    for (id, raw) in rows {
        if Some(id.as_str()) == except {
            continue;
        }
        let theirs: Vec<String> = serde_json::from_str(&raw).unwrap_or_default();
        if let Some(d) = domains.iter().find(|d| theirs.contains(d)) {
            return Err(Error::Conflict(format!("{d} has an SSO connection already ({id})")));
        }
    }
    Ok(())
}

/// JSON of at most `limit` bytes; a non-2xx status is an error.
async fn read_bounded(mut resp: reqwest::Response, limit: usize) -> std::result::Result<Value, String> {
    let status = resp.status();
    let mut buf = Vec::new();
    while let Some(chunk) = resp.chunk().await.map_err(|e| e.without_url().to_string())? {
        if buf.len() + chunk.len() > limit {
            return Err(format!("its answer is over {} KiB", limit >> 10));
        }
        buf.extend_from_slice(&chunk);
    }
    if !status.is_success() {
        return Err(format!("answered {status}"));
    }
    serde_json::from_slice(&buf).map_err(|_| "its answer isn't JSON".to_string())
}
