//! Enterprise SSO (docs/accounts.md, "SSO"): people sign in through their company's OpenID
//! Connect provider (Okta, Entra ID, Google Workspace, JumpCloud…), chosen by their email's
//! domain, as Linear's, Vercel's and GitHub's enterprise sign-in work.
//!
//! - A connection is the admins': an issuer (as its discovery document states it, exactly), the
//!   client roll is registered as there (its secret sealed with `ROLL_SECRET_KEY`), and the email
//!   domains it claims. A claimed domain does nothing until a DNS TXT record proves the company
//!   holds it (`_roll-sso.<domain>`): only verified domains route sign-ins, link accounts,
//!   provision people or enforce. Public mail domains (gmail.com…) are never a connection's. A
//!   domain has one connection.
//! - The provider is fetched only through a client that refuses private addresses (and IP
//!   literals), uses no proxy, follows no redirects and bounds every answer. Its ID token is
//!   checked by roll-oidc against the keys its discovery document names: issuer exactly, audience
//!   (roll's client id), times; then the nonce this sign-in sent.
//! - Only an email the provider says it verified (Entra: `xms_edov`; Google: and `hd` one of the
//!   domains) in a verified domain of this connection counts. A person linked before signs in
//!   while their account's domain is still this connection's; an account with that email is
//!   linked at its first SSO sign-in, unless it's an admin's (admins link from their own session)
//!   or it's linked to another subject there already (an address reassigned at the provider).
//! - `enforce`: people of those domains sign in only this way. Other sign-ins are refused where
//!   sessions are issued (and at the second factor), and turning it on ends their other sessions;
//!   the operator's token always works, so an outage at the provider never locks roll out.

use crate::oauth::{Finished, b64url, read_json};
use crate::{Accounts, Error, NewUser, Result, normalize_email, random, sha256_hex};
use code_sql::{OptionalExtension, params};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::time::{Duration, Instant};

const STATE_TTL_MS: i64 = 10 * 60_000;
/// A discovery document or a key set, at most.
const MAX_DOC: usize = 256 << 10;
/// Connections a deployment may have.
const MAX_CONNECTIONS: usize = 20;
/// How long a provider's discovery document and keys are reused: `sso/start` is public, so a
/// flood of starts must not become a flood of fetches at the provider.
const DISCOVERY_TTL: Duration = Duration::from_secs(600);
/// A provider that didn't answer isn't asked again for this long.
const FAILURE_TTL: Duration = Duration::from_secs(60);
/// Sign-ins one address may start in ten minutes, and one connection in a minute.
const STARTS_PER_EMAIL: i64 = 10;
const STARTS_PER_CONNECTION: i64 = 600;

/// Domains anyone can have an address at: never a company's to claim.
const PUBLIC_MAIL: &[&str] = &[
    "gmail.com", "googlemail.com", "outlook.com", "hotmail.com", "live.com", "msn.com", "yahoo.com", "ymail.com", "icloud.com", "me.com", "mac.com", "aol.com",
    "proton.me", "protonmail.com", "pm.me", "gmx.com", "gmx.de", "gmx.net", "web.de", "mail.com", "yandex.com", "yandex.ru", "zoho.com", "fastmail.com", "hey.com",
    "qq.com", "163.com", "126.com", "naver.com", "mail.ru", "tutanota.com", "duck.com",
];

/// A connection, as admins see it (never its secret).
#[derive(Clone, Debug, Serialize)]
pub struct Connection {
    pub id: String,
    pub name: String,
    pub issuer: String,
    pub client_id: String,
    /// The domains it claims.
    pub domains: Vec<String>,
    /// Those a DNS TXT record proved: only these route, link, provision or enforce.
    pub verified: Vec<String>,
    /// People of its verified domains sign in only through it.
    pub enforce: bool,
    /// A verified person of its verified domains with no account gets one (never an admin).
    pub provision: bool,
    pub created_at: i64,
    /// What to register at the provider as the sign-in redirect.
    pub redirect_uri: String,
    /// The TXT record that proves a domain: `name` with the domain in place of `<domain>`.
    pub dns: DnsRecord,
    /// Its provider provisions people over SCIM (it has a live token).
    pub scim: bool,
    /// Its SCIM token was found leaked and revoked: where (until an admin makes a new one).
    pub scim_leaked: Option<String>,
    /// How long a session it signs in lasts at most, in hours (none: roll's 30 days), as Okta's
    /// and Google Workspace's session lengths: then the person signs in through it again.
    pub session_hours: Option<u32>,
}

#[derive(Clone, Debug, Serialize)]
pub struct DnsRecord {
    pub name: String,
    pub value: String,
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
    /// The longest a session it signs in lasts, in hours (1 to 720); 0 takes the cap off.
    #[serde(default)]
    pub session_hours: Option<u32>,
}

/// TXT records, for proving domains (DNS over HTTPS by default; a table in tests).
pub trait TxtLookup: Send + Sync {
    fn txt<'a>(&'a self, host: &'a str) -> Pin<Box<dyn Future<Output = Vec<String>> + Send + 'a>>;
}

/// DNS over HTTPS (`application/dns-json`, Cloudflare's or `ROLL_ACCOUNTS_DOH`), through the
/// guarded client.
pub(crate) struct Doh {
    pub(crate) client: reqwest::Client,
    pub(crate) url: String,
}

impl TxtLookup for Doh {
    fn txt<'a>(&'a self, host: &'a str) -> Pin<Box<dyn Future<Output = Vec<String>> + Send + 'a>> {
        Box::pin(async move {
            let Ok(mut url) = url::Url::parse(&self.url) else { return Vec::new() };
            url.query_pairs_mut().append_pair("name", host).append_pair("type", "TXT");
            let Ok(resp) = self.client.get(url).header("accept", "application/dns-json").send().await else { return Vec::new() };
            let Ok(v) = read_bounded(resp, 64 << 10).await else { return Vec::new() };
            v["Answer"].as_array().into_iter().flatten().filter(|r| r["type"].as_i64() == Some(16)).filter_map(|r| r["data"].as_str()).map(|d| d.trim_matches('"').to_string()).collect()
        })
    }
}

/// A provider's endpoints and keys, from its discovery document.
#[derive(Clone)]
pub(crate) struct Endpoints {
    /// As its document states it (exactly: what its ID tokens carry).
    issuer: String,
    authorize: String,
    token: String,
    jwks_uri: String,
    /// The client authentication its token endpoint takes (`client_secret_post` unless it only
    /// says `client_secret_basic`).
    basic_auth: bool,
    /// Its keys, once read.
    jwks: Option<String>,
}

/// What the discovery cache holds per issuer: the endpoints, or that it didn't answer.
pub(crate) type Discovered = std::result::Result<Endpoints, String>;

fn claimed_domains(raw: &[String]) -> Result<Vec<String>> {
    let ok = |d: &str| !d.is_empty() && d.len() <= 253 && d.contains('.') && d.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'.' || b == b'-');
    let mut out: Vec<String> = raw.iter().map(|d| d.trim().trim_start_matches('@').to_ascii_lowercase()).collect();
    out.sort();
    out.dedup();
    if out.is_empty() || out.len() > 50 || !out.iter().all(|d| ok(d)) {
        return Err(Error::BadRequest("domains are 1 to 50 email domains (acme.com)".into(), Some("domains")));
    }
    if let Some(d) = out.iter().find(|d| PUBLIC_MAIL.contains(&d.as_str())) {
        return Err(Error::BadRequest(format!("{d} is a public mail domain: anyone can have an address there"), Some("public_domain")));
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

/// Google's issuer: its tokens vouch for consumer accounts too, so a Workspace domain needs `hd`.
fn is_google(issuer: &str) -> bool {
    issuer.trim_end_matches('/') == "https://accounts.google.com"
}

/// Entra ID's issuers: `email` there isn't verified unless `xms_edov` (the domain's owner
/// verified it) says so.
fn is_entra(issuer: &str) -> bool {
    url::Url::parse(issuer).ok().and_then(|u| u.host_str().map(str::to_ascii_lowercase)).is_some_and(|h| h == "login.microsoftonline.com" || h == "sts.windows.net")
}

const COLUMNS: &str = "id, name, issuer, client_id, domains, verified, enforce, provision, created_at, challenge, (SELECT COUNT(*) FROM scim_tokens t WHERE t.connection_id = sso_connections.id AND t.leaked IS NULL) AS scim, (SELECT leaked FROM scim_tokens t WHERE t.connection_id = sso_connections.id) AS scim_leaked, session_hours";

impl Accounts {
    fn sso_redirect_uri(&self, id: &str) -> String {
        format!("{}/v1/accounts/sso/{id}/callback", self.config.api_url)
    }

    fn connection_row(&self, r: &code_sql::Row<'_>) -> code_sql::Result<Connection> {
        let id: String = r.get(0)?;
        let list = |s: String| serde_json::from_str::<Vec<String>>(&s).unwrap_or_default();
        let challenge: String = r.get(9)?;
        Ok(Connection {
            redirect_uri: self.sso_redirect_uri(&id),
            id,
            name: r.get(1)?,
            issuer: r.get(2)?,
            client_id: r.get(3)?,
            domains: list(r.get(4)?),
            verified: list(r.get(5)?),
            enforce: r.get(6)?,
            provision: r.get(7)?,
            created_at: r.get(8)?,
            dns: DnsRecord { name: "_roll-sso.<domain>".into(), value: format!("roll-sso={challenge}") },
            scim: r.get::<_, i64>(10)? > 0,
            scim_leaked: r.get(11)?,
            session_hours: r.get::<_, Option<i64>>(12)?.and_then(|h| u32::try_from(h).ok()).filter(|h| *h > 0),
        })
    }

    pub fn sso_connections(&self) -> Result<Vec<Connection>> {
        Ok(self.db.read(|c| c.query(&format!("SELECT {COLUMNS} FROM sso_connections ORDER BY created_at"), (), |r| self.connection_row(r)))?)
    }

    fn sso_connection(&self, id: &str) -> Result<Connection> {
        self.db
            .read(|c| c.query_row(&format!("SELECT {COLUMNS} FROM sso_connections WHERE id = ?"), [id], |r| self.connection_row(r)).optional())?
            .ok_or_else(|| Error::NotFound(format!("no SSO connection {id}")))
    }

    /// The connection that answers for `email`'s domain: one whose verified domains include it.
    pub(crate) fn sso_for(&self, email: &str) -> Result<Option<Connection>> {
        let domain = domain_of(email);
        Ok(self.sso_connections()?.into_iter().find(|c| c.verified.iter().any(|d| d == domain)))
    }

    /// Refused when `user`'s email is in a domain whose connection enforces SSO: their sessions
    /// come only from an SSO sign-in.
    pub(crate) fn sso_not_enforced(&self, user: &str) -> Result<()> {
        let email = self.db.read(|c| c.query_row("SELECT email FROM users WHERE id = ?", [user], |r| r.get::<_, String>(0)).optional())?;
        match email.map(|e| self.sso_for(&e)).transpose()?.flatten() {
            Some(conn) if conn.enforce => Err(Error::Forbidden(format!("sign in with {}: your organization requires it", conn.name), "sso_required")),
            _ => Ok(()),
        }
    }

    /// The same check for an address (an email code's start): the domain alone decides.
    pub(crate) fn sso_required_for(&self, email: &str) -> Result<Option<String>> {
        Ok(self.sso_for(email)?.filter(|c| c.enforce).map(|c| c.name))
    }

    /// An https issuer roll may reach, with no query, fragment or credentials.
    fn check_issuer(&self, issuer: &str) -> Result<String> {
        let bad = || Error::BadRequest("the issuer is the provider's https URL (https://acme.okta.com)".into(), Some("issuer"));
        let issuer = issuer.trim().to_string();
        let u = url::Url::parse(&issuer).map_err(|_| bad())?;
        let scheme_ok = u.scheme() == "https" || self.config.sso_allow_private && u.scheme() == "http";
        if !scheme_ok || issuer.len() > 300 || u.query().is_some() || u.fragment().is_some() || !u.username().is_empty() || u.password().is_some() {
            return Err(bad());
        }
        Ok(issuer)
    }

    /// Add a connection: its discovery document must answer and name the issuer first (a typo
    /// shows now, not at the first sign-in). Its domains wait for their TXT records.
    pub async fn add_sso(self: &Arc<Self>, n: NewConnection) -> Result<Connection> {
        let sealer = self.sso_sealer()?;
        let issuer = self.check_issuer(&n.issuer)?;
        let name = match &n.name {
            Some(s) => text(s, "name", 100)?,
            None => url::Url::parse(&issuer).ok().and_then(|u| u.host_str().map(str::to_string)).unwrap_or_else(|| issuer.clone()),
        };
        let client_id = text(&n.client_id, "client_id", 300)?;
        let secret = text(&n.client_secret, "client_secret", 1000)?;
        let domains = claimed_domains(&n.domains)?;
        let ep = self.discover_now(&issuer).await.map_err(|e| Error::BadRequest(format!("the SSO provider: {e}"), Some("provider")))?;
        let id = crate::new_id("sso");
        let sealed = sealer.seal(&id, &secret);
        let challenge = hex::encode(random::<16>());
        let (encoded, id2) = (json!(domains).to_string(), id.clone());
        blocking(self, move |a| {
            let now = a.db.now_ms()?;
            a.db.write(|c| {
                let n_conns: i64 = c.query_row("SELECT COUNT(*) FROM sso_connections", (), |r| r.get(0))?;
                if n_conns as usize >= MAX_CONNECTIONS {
                    return Err(Error::Conflict(format!("at most {MAX_CONNECTIONS} SSO connections")));
                }
                taken(c, &domains, None)?;
                c.execute(
                    "INSERT INTO sso_connections (id, name, issuer, client_id, client_secret, domains, verified, enforce, provision, created_at, challenge) VALUES (?, ?, ?, ?, ?, ?, '[]', ?, ?, ?, ?)",
                    params![id2, name, ep.issuer, client_id, sealed, encoded, n.enforce, n.provision, now, challenge],
                )?;
                Ok(())
            })
        })
        .await?;
        tracing::info!(connection = %id, "accounts: SSO connection added");
        blocking(self, move |a| a.sso_connection(&id)).await
    }

    pub fn update_sso(&self, id: &str, ch: ConnectionChange) -> Result<Connection> {
        let name = ch.name.as_deref().map(|s| text(s, "name", 100)).transpose()?;
        let client_id = ch.client_id.as_deref().map(|s| text(s, "client_id", 300)).transpose()?;
        let sealed = match ch.client_secret.as_deref() {
            Some(s) => Some(self.sso_sealer()?.seal(id, &text(s, "client_secret", 1000)?)),
            None => None,
        };
        let domains = ch.domains.as_deref().map(claimed_domains).transpose()?;
        let provider = format!("sso:{id}");
        self.db.write(|c| {
            let Some((old_verified, was_enforced)) = c
                .query_row("SELECT verified, enforce FROM sso_connections WHERE id = ?", [id], |r| Ok((r.get::<_, String>(0)?, r.get::<_, bool>(1)?)))
                .optional()?
            else {
                return Err(Error::NotFound(format!("no SSO connection {id}")));
            };
            let mut verified: Vec<String> = serde_json::from_str(&old_verified).unwrap_or_default();
            if let Some(d) = &domains {
                taken(c, d, Some(id))?;
                // A domain it no longer claims is no longer verified, and its people's links here go.
                verified.retain(|v| d.contains(v));
                c.execute("UPDATE sso_connections SET domains = ?, verified = ? WHERE id = ?", params![json!(d).to_string(), json!(verified).to_string(), id])?;
                drop_links_outside(c, &provider, &verified)?;
                // And what SCIM kept about people now outside its domains.
                let kept: Vec<(String, String)> = c.query("SELECT s.user_id, u.email FROM scim_users s JOIN users u ON u.id = s.user_id WHERE s.connection_id = ?", [id], |r| Ok((r.get(0)?, r.get(1)?)))?;
                for (user, email) in kept {
                    if !verified.iter().any(|v| v == domain_of(&email)) {
                        c.execute("DELETE FROM scim_users WHERE connection_id = ? AND user_id = ?", [id, user.as_str()])?;
                    }
                }
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
            if let Some(p) = ch.provision {
                c.execute("UPDATE sso_connections SET provision = ? WHERE id = ?", params![p, id])?;
            }
            if let Some(h) = ch.session_hours {
                if h > 720 {
                    return Err(Error::BadRequest("a session lasts 1 to 720 hours (0 takes the cap off)".into(), None));
                }
                c.execute("UPDATE sso_connections SET session_hours = ? WHERE id = ?", params![(h > 0).then_some(h as i64), id])?;
                // A shorter cap applies to the sessions it already signed in, now.
                if h > 0 {
                    let cap = h as i64 * 3_600_000;
                    c.execute(
                        "UPDATE sessions SET cap_ms = ?, expires_at = CASE WHEN created_at + ? < expires_at THEN created_at + ? ELSE expires_at END \
                         WHERE method = 'sso' AND user_id IN (SELECT user_id FROM identities WHERE provider = ?)",
                        params![cap, cap, cap, provider],
                    )?;
                }
            }
            if let Some(e) = ch.enforce {
                c.execute("UPDATE sso_connections SET enforce = ? WHERE id = ?", params![e, id])?;
                // Turned on: its people's sessions that didn't come from SSO end now.
                if e && !was_enforced {
                    end_other_sessions(c, &verified)?;
                }
            }
            Ok(())
        })?;
        self.sso_connection(id)
    }

    /// Check each claimed domain's `_roll-sso.<domain>` TXT record for this connection's value;
    /// the ones that have it are verified (and with `enforce` on, their people's other sessions
    /// end). A domain whose record went away stays verified until it's no longer claimed.
    pub async fn verify_sso(self: &Arc<Self>, id: &str) -> Result<Connection> {
        let conn = {
            let id = id.to_string();
            blocking(self, move |a| a.sso_connection(&id)).await?
        };
        let lookup = self.txt_lookup();
        let mut proven = Vec::new();
        for d in &conn.domains {
            if lookup.txt(&format!("_roll-sso.{d}")).await.iter().any(|v| v.trim() == conn.dns.value) {
                proven.push(d.clone());
            }
        }
        let id2 = id.to_string();
        blocking(self, move |a| {
            a.db.write(|c| {
                let Some((raw, claimed, enforce)) = c
                    .query_row("SELECT verified, domains, enforce FROM sso_connections WHERE id = ?", [id2.as_str()], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?, r.get::<_, bool>(2)?)))
                    .optional()?
                else {
                    return Err(Error::NotFound(format!("no SSO connection {id2}")));
                };
                let claimed: Vec<String> = serde_json::from_str(&claimed).unwrap_or_default();
                let mut verified: Vec<String> = serde_json::from_str(&raw).unwrap_or_default();
                let new: Vec<String> = proven.iter().filter(|d| claimed.contains(d) && !verified.contains(d)).cloned().collect();
                verified.extend(new.iter().cloned());
                verified.sort();
                c.execute("UPDATE sso_connections SET verified = ? WHERE id = ?", params![json!(verified).to_string(), id2])?;
                if enforce {
                    end_other_sessions(c, &new)?;
                }
                Ok(())
            })?;
            a.sso_connection(&id2)
        })
        .await
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
            c.execute("DELETE FROM scim_tokens WHERE connection_id = ?", [id])?;
            c.execute("DELETE FROM scim_users WHERE connection_id = ?", [id])?;
            Ok(())
        })
    }

    /// The provider's endpoints (and keys, once read), reused for [`DISCOVERY_TTL`]; one that
    /// failed isn't asked again for [`FAILURE_TTL`].
    async fn discover(&self, issuer: &str) -> std::result::Result<Endpoints, String> {
        let cached = self.sso_discovery.lock().unwrap_or_else(|e| e.into_inner()).get(issuer).cloned();
        match cached {
            Some((Ok(e), at)) if at.elapsed() < DISCOVERY_TTL => return Ok(e),
            Some((Err(e), at)) if at.elapsed() < FAILURE_TTL => return Err(e),
            _ => {}
        }
        let found = self.discover_now(issuer).await;
        self.remember(issuer, found.clone());
        found
    }

    fn remember(&self, issuer: &str, found: Discovered) {
        let mut cache = self.sso_discovery.lock().unwrap_or_else(|e| e.into_inner());
        cache.retain(|_, (_, at)| at.elapsed() < DISCOVERY_TTL);
        cache.insert(issuer.to_string(), (found, Instant::now()));
    }

    /// `<issuer>/.well-known/openid-configuration`, which must name the same issuer.
    async fn discover_now(&self, issuer: &str) -> std::result::Result<Endpoints, String> {
        let doc = self.sso_get(&format!("{}/.well-known/openid-configuration", issuer.trim_end_matches('/'))).await?;
        let s = |k: &str| doc.get(k).and_then(Value::as_str).map(str::to_string);
        let stated = s("issuer").unwrap_or_default();
        if stated.trim_end_matches('/') != issuer.trim_end_matches('/') {
            return Err(format!("its discovery document names another issuer than {issuer}"));
        }
        let (Some(authorize), Some(token), Some(jwks_uri)) = (s("authorization_endpoint"), s("token_endpoint"), s("jwks_uri")) else {
            return Err("its discovery document lacks an endpoint".into());
        };
        for u in [&authorize, &token, &jwks_uri] {
            self.sso_url(u)?;
        }
        let methods: Vec<String> = doc.get("token_endpoint_auth_methods_supported").and_then(Value::as_array).map(|a| a.iter().filter_map(Value::as_str).map(str::to_string).collect()).unwrap_or_default();
        let basic_auth = !methods.is_empty() && !methods.iter().any(|m| m == "client_secret_post") && methods.iter().any(|m| m == "client_secret_basic");
        Ok(Endpoints { issuer: stated, authorize, token, jwks_uri, basic_auth, jwks: None })
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

    /// The provider's keys: cached with its endpoints, read again once when `fresh` (a token
    /// signed by a key roll hasn't seen: the provider rotated).
    async fn keys(&self, issuer: &str, ep: &Endpoints, fresh: bool) -> std::result::Result<String, String> {
        if let (Some(k), false) = (&ep.jwks, fresh) {
            return Ok(k.clone());
        }
        let jwks = self.sso_get(&ep.jwks_uri).await?.to_string();
        self.remember(issuer, Ok(Endpoints { jwks: Some(jwks.clone()), ..ep.clone() }));
        Ok(jwks)
    }

    /// Start a sign-in through the connection that answers for `email` (`link_user`: link the
    /// signed-in person's account instead, through the connection for their own email).
    pub(crate) async fn sso_start(self: &Arc<Self>, email: &str, invite: Option<&str>, return_to: Option<&str>, link_user: Option<String>) -> Result<crate::oauth::Started> {
        let (email, invite) = (email.to_string(), invite.map(str::to_string));
        let return_to = crate::oauth::safe_return(return_to, if link_user.is_some() { "/dashboard/account" } else { "/dashboard" });
        let (conn, invite_id, email) = blocking(self, move |a| {
            let email = match &link_user {
                Some(u) => a.user(u)?.email,
                None => normalize_email(&email)?,
            };
            let conn = a.sso_for(&email)?.ok_or_else(|| Error::BadRequest("no SSO connection answers for that email's domain".into(), Some("no_sso")))?;
            // A flood of starts is refused before it costs the provider anything.
            if !a.attempt(&format!("sso-start:{email}"), STARTS_PER_EMAIL, 10 * 60_000)? || !a.attempt(&format!("sso-start:{}", conn.id), STARTS_PER_CONNECTION, 60_000)? {
                return Err(Error::BadRequest("too many sign-ins started; wait a few minutes".into(), Some("throttled")));
            }
            let invite_id = match invite.as_deref().map(str::trim).filter(|t| !t.is_empty()) {
                Some(t) => Some(a.valid_invite(t)?.ok_or_else(|| Error::BadRequest("the invite is used, revoked or expired".into(), Some("invite_invalid")))?.0),
                None => None,
            };
            Ok((conn, invite_id, (email, link_user)))
        })
        .await?;
        let (email, link_user) = email;
        let ep = self.discover(&conn.issuer).await.map_err(|e| {
            tracing::info!(connection = %conn.id, error = %e, "accounts: an SSO provider didn't answer");
            Error::BadRequest(format!("{} didn't answer; try again in a minute", conn.name), Some("provider"))
        })?;
        let (state, verifier, nonce, browser) = (hex::encode(random::<32>()), b64url(&random::<32>()), hex::encode(random::<16>()), hex::encode(random::<32>()));
        let (provider, intent) = (format!("sso:{}", conn.id), if link_user.is_some() { "link" } else { "signin" });
        let (s2, v2, n2, b2) = (sha256_hex(&state), verifier.clone(), nonce.clone(), sha256_hex(&browser));
        blocking(self, move |a| {
            let now = a.db.now_ms()?;
            a.db.write(|c| {
                c.execute("DELETE FROM oauth_states WHERE expires_at <= ?", [now])?;
                c.execute(
                    "INSERT INTO oauth_states (state_hash, provider, intent, user_id, invite_id, verifier, nonce, browser_hash, return_to, created_at, expires_at) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
                    params![s2, provider, intent, link_user, invite_id, v2, n2, b2, return_to, now, now + STATE_TTL_MS],
                )?;
                Ok(())
            })
        })
        .await?;
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
    pub(crate) async fn sso_callback(self: &Arc<Self>, id: &str, code: &str, state: &str, browser: Option<String>) -> Result<Finished> {
        let (id2, state2) = (id.to_string(), state.to_string());
        let (st, conn, secret) = blocking(self, move |a| {
            let st = a.take_state_named(&format!("sso:{id2}"), &state2, browser.as_deref())?;
            let conn = a.sso_connection(&id2)?;
            let sealed = a.db.read(|c| c.query_row("SELECT client_secret FROM sso_connections WHERE id = ?", [id2.as_str()], |r| r.get::<_, String>(0)))?;
            let secret = a.sso_sealer()?.open(&id2, &sealed).ok_or_else(|| Error::Internal("the SSO client secret doesn't open (ROLL_SECRET_KEY changed?)".into()))?;
            Ok((st, conn, secret))
        })
        .await?;
        let failed = |what: &str| Error::BadRequest(format!("the SSO provider: {what}"), Some("provider"));
        let ep = self.discover(&conn.issuer).await.map_err(|e| failed(&e))?;
        let redirect = self.sso_redirect_uri(id);
        let mut form = vec![("grant_type", "authorization_code"), ("code", code), ("redirect_uri", redirect.as_str()), ("code_verifier", st.verifier.as_str())];
        if !ep.basic_auth {
            form.extend([("client_id", conn.client_id.as_str()), ("client_secret", secret.as_str())]);
        }
        let token_url = self.sso_url(&ep.token).map_err(|e| failed(&e))?;
        let mut req = self.sso_http.post(token_url).header("accept", "application/json");
        if ep.basic_auth {
            req = req.basic_auth(&conn.client_id, Some(secret.as_str()));
        }
        let resp = req.form(&form).send().await.map_err(|e| failed(&format!("token endpoint: {}", e.without_url())))?;
        let token = read_json(resp).await.map_err(|e| failed(&e))?;
        let id_token = token.get("id_token").and_then(Value::as_str).ok_or_else(|| failed("no ID token"))?.to_string();
        let jwt = roll_oidc::Jwt::parse(&id_token).map_err(|e| failed(&e.to_string()))?;
        let now_s = chrono_now_s();
        let check = |jwks: &str| roll_oidc::Keys::default().verify(None, Some(jwks), &jwt, &ep.issuer, &conn.client_id, now_s);
        let jwks = self.keys(&conn.issuer, &ep, false).await.map_err(|e| failed(&e))?;
        if let Err(first) = check(&jwks) {
            // A key it rotated in since roll read them: once more, fresh.
            let fresh = self.keys(&conn.issuer, &ep, true).await.map_err(|e| failed(&e))?;
            check(&fresh).map_err(|_| failed(&first.to_string()))?;
        }
        if !jwt.str("nonce").is_some_and(|n| roll_auth::constant_eq(n.as_bytes(), st.nonce.as_bytes())) {
            return Err(failed("the ID token isn't for this sign-in"));
        }
        // An aud of several needs azp to be roll's client (OpenID Connect Core 3.1.3.7).
        if jwt.claims.get("aud").and_then(Value::as_array).is_some_and(|a| a.len() > 1) && jwt.str("azp") != Some(conn.client_id.as_str()) {
            return Err(failed("the ID token is for another client"));
        }
        // Only an email the provider verified, in plain ASCII, in one of its verified domains.
        let verified = matches!(jwt.claims.get("email_verified"), Some(Value::Bool(true)))
            || jwt.str("email_verified") == Some("true")
            || is_entra(&ep.issuer) && (matches!(jwt.claims.get("xms_edov"), Some(Value::Bool(true))) || jwt.str("xms_edov") == Some("true"));
        let email = jwt.str("email").filter(|e| verified && e.is_ascii()).ok_or_else(|| Error::BadRequest("the SSO provider has no verified email for this person".into(), Some("unverified_email")))?;
        let email = normalize_email(email)?;
        if !conn.verified.iter().any(|d| d == domain_of(&email)) {
            return Err(Error::BadRequest("that email's domain isn't one this SSO connection answers for".into(), Some("domain")));
        }
        // Google's tokens vouch for consumer accounts too: a Workspace domain needs its hd.
        if is_google(&ep.issuer) && !jwt.str("hd").is_some_and(|hd| conn.verified.iter().any(|d| d == hd)) {
            return Err(Error::BadRequest("that Google account isn't one of the company's Workspace accounts".into(), Some("domain")));
        }
        let subject = jwt.str("sub").filter(|s| !s.is_empty() && s.len() <= 255).ok_or_else(|| failed("no subject"))?.to_string();
        let name = jwt.str("name").map(|n| n.chars().filter(|c| !c.is_control()).take(100).collect::<String>());
        let id = id.to_string();
        blocking(self, move |a| a.sso_finish(&id, st, &email, &subject, name.as_deref())).await
    }

    fn sso_finish(&self, id: &str, st: crate::oauth::State, email: &str, subject: &str, name: Option<&str>) -> Result<Finished> {
        let provider = format!("sso:{id}");
        let now = self.db.now_ms()?;
        let linked_elsewhere = || Error::BadRequest("that SSO identity belongs to another roll account".into(), Some("linked_elsewhere"));
        enum Next {
            SignIn(String, Option<&'static str>),
            Linked,
            Create { provision: bool },
        }
        // One write: the connection as it is now (removed, its domains or provisioning changed
        // since the callback began), the links, and the account.
        let next = self.db.write(|c| {
            let Some((verified, provision)) = c.query_row("SELECT verified, provision FROM sso_connections WHERE id = ?", [id], |r| Ok((r.get::<_, String>(0)?, r.get::<_, bool>(1)?))).optional()? else {
                return Err(Error::BadRequest("the SSO connection was removed".into(), Some("provider")));
            };
            let verified: Vec<String> = serde_json::from_str(&verified).unwrap_or_default();
            let in_domain = |e: &str| verified.iter().any(|d| d == domain_of(e));
            if !in_domain(email) {
                return Err(Error::BadRequest("that email's domain isn't one this SSO connection answers for".into(), Some("domain")));
            }
            let holder = c.query_row("SELECT user_id FROM identities WHERE provider = ? AND subject = ?", [provider.as_str(), subject], |r| r.get::<_, String>(0)).optional()?;
            if st.intent == "link" {
                let user = st.user_id.clone().ok_or_else(|| Error::BadRequest("sign in first".into(), Some("state")))?;
                let mine = c.query_row("SELECT email FROM users WHERE id = ? AND disabled = 0", [user.as_str()], |r| r.get::<_, String>(0)).optional()?;
                if mine.as_deref() != Some(email) {
                    return Err(Error::BadRequest("the SSO provider signed in another address than your account's".into(), Some("link_email")));
                }
                if holder.as_ref().is_some_and(|h| *h != user) {
                    return Err(linked_elsewhere());
                }
                c.execute("DELETE FROM identities WHERE user_id = ? AND provider = ?", [user.as_str(), provider.as_str()])?;
                c.execute("INSERT INTO identities (provider, subject, user_id, email, login, created_at) VALUES (?, ?, ?, ?, ?, ?)", params![provider, subject, user, email, email, now])?;
                return Ok(Next::Linked);
            }
            if let Some(user) = holder {
                // Linked before: only while the account's own address is still this connection's.
                let theirs = c.query_row("SELECT email FROM users WHERE id = ?", [user.as_str()], |r| r.get::<_, String>(0)).optional()?;
                if !theirs.as_deref().is_some_and(in_domain) {
                    return Err(Error::BadRequest("that account's email isn't in this SSO connection's domains".into(), Some("domain")));
                }
                return Ok(Next::SignIn(user, None));
            }
            let Some((user, admin)) = c.query_row("SELECT id, admin FROM users WHERE email = ?", [email], |r| Ok((r.get::<_, String>(0)?, r.get::<_, bool>(1)?))).optional()? else {
                return Ok(Next::Create { provision });
            };
            // An admin links from their own session; an account linked to another subject here
            // (an address reassigned at the provider) waits for that link to be removed.
            if admin {
                return Err(Error::BadRequest("an admin links SSO from their own account settings first".into(), Some("link_required")));
            }
            if c.query_row("SELECT 1 FROM identities WHERE user_id = ? AND provider = ?", [user.as_str(), provider.as_str()], |_| Ok(())).optional()?.is_some() {
                return Err(linked_elsewhere());
            }
            c.execute("INSERT INTO identities (provider, subject, user_id, email, login, created_at) VALUES (?, ?, ?, ?, ?, ?)", params![provider, subject, user, email, email, now])?;
            Ok(Next::SignIn(user, Some("linked")))
        })?;
        match next {
            Next::SignIn(user, new) => {
                tracing::info!(user = %user, connection = %id, "accounts: SSO sign-in");
                Ok(Finished::SignIn { user, return_to: st.return_to, new })
            }
            Next::Linked => Ok(Finished::Linked { return_to: st.return_to }),
            Next::Create { provision } => {
                let (user, new) = match &st.invite_id {
                    Some(i) => (self.create_user(NewUser { invite_id: i, email, hint: None, name, identity: Some((&provider, subject, email)) })?, "invite"),
                    None if provision => (self.provision_user(email, name, Some((&provider, subject)), &provider)?, "provisioned"),
                    None => return Err(Error::BadRequest("no account has this email, and there was no invite".into(), Some("not_invited"))),
                };
                Ok(Finished::SignIn { user, return_to: st.return_to, new: Some(new) })
            }
        }
    }
}

/// Another connection claims one of `domains` (`except`: the one being changed).
fn taken(c: &code_sql::Conn<'_>, domains: &[String], except: Option<&str>) -> Result<()> {
    let rows: Vec<(String, String)> = c.query("SELECT id, domains FROM sso_connections", (), |r| Ok((r.get(0)?, r.get(1)?)))?;
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

/// Links at `provider` whose account's email isn't in `domains` go.
fn drop_links_outside(c: &code_sql::Conn<'_>, provider: &str, domains: &[String]) -> Result<()> {
    let links: Vec<(String, String)> = c.query("SELECT i.subject, u.email FROM identities i JOIN users u ON u.id = i.user_id WHERE i.provider = ?", [provider], |r| Ok((r.get(0)?, r.get(1)?)))?;
    for (subject, email) in links {
        if !domains.iter().any(|d| d == domain_of(&email)) {
            c.execute("DELETE FROM identities WHERE provider = ? AND subject = ?", [provider, subject.as_str()])?;
        }
    }
    Ok(())
}

/// The sessions of people in `domains` that didn't come from SSO end (enforcement turned on).
fn end_other_sessions(c: &code_sql::Conn<'_>, domains: &[String]) -> Result<()> {
    for d in domains {
        c.execute(
            "DELETE FROM sessions WHERE (method IS NULL OR method <> 'sso') AND user_id IN (SELECT id FROM users WHERE email LIKE ?)",
            [format!("%@{d}")],
        )?;
    }
    Ok(())
}

fn chrono_now_s() -> i64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs() as i64).unwrap_or_default()
}

/// Run `f` off the async runtime: the store blocks on its database.
async fn blocking<T: Send + 'static>(a: &Arc<Accounts>, f: impl FnOnce(&Accounts) -> Result<T> + Send + 'static) -> Result<T> {
    let a = a.clone();
    tokio::task::spawn_blocking(move || f(&a)).await.map_err(|e| Error::Internal(e.to_string()))?
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
