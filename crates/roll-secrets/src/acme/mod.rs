//! ACME (RFC 8555) on a project's CAs (docs/secrets.md, "ACME"), as Vault's PKI gives: cert-manager,
//! certbot, Caddy and lego get certificates from a CA with no roll client and no roll token.
//!
//! - Per CA, under `/v1/secrets/acme/{ca}/…` ([`routes`]). Requests are flattened JWS signed with an
//!   account key (ES256 or RS256), each with a nonce used once and a `url` that must be the
//!   request's own.
//! - An account needs External Account Binding, always: a binding (`acme_bindings`) is a key id
//!   and an HMAC key an admin of the CA's project made, tied to the CA and optionally to narrower
//!   names and an end. Every request of an account looks its binding up (never cached): revoking
//!   it, its end, or its maker no longer reaching the project stops the account at once.
//! - An order's names are within the CA's policy and the binding's names. Each is then proven with
//!   `http-01`, `dns-01` or `tls-alpn-01` through rolld's [`Checks`] (outside any transaction), or,
//!   where the CA pre-authorizes, valid at once: the binding's key is then the whole proof. A name
//!   the account already proved on the CA reuses that authorization until it ends (never extended).
//! - Finalize checks the names against the CA's policy and the binding again (either may have
//!   changed since the order), then signs the CSR through the same path as `…/cas/{id}/sign`
//!   (`source: "acme"`): the CA's list, revocation, the CRL and OCSP all have it. `revoke-cert`
//!   takes the ordering account's signature or the certificate's own key's.
//! - Nonces, accounts, orders, authorizations and challenges live in the database, so any node of
//!   a fleet answers any request; each read-then-write (using a nonce, claiming a challenge or an
//!   order, issuing) is one write. Expired nonces, orders and certificates go in the sweep.

mod bindings;
mod http;
mod jws;

pub use bindings::{Binding, NewBinding, NewKey, Settings};
pub(crate) use bindings::forget_ca;
use bindings::{binding_allows, eab_aad};
pub(crate) use http::routes;

use crate::access::Caller;
use crate::pki::{self, Policy};
use crate::{Error, Result, Secrets, Value, audit, names};
use code_sql::{Conn, OptionalExtension, params};
use jws::{Jwk, Jws, b64, unb64};
use serde::{Deserialize, Serialize};
use serde_json::{Value as Json, json};
use std::sync::{Arc, OnceLock};

pub(crate) const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS acme_bindings (
    id TEXT PRIMARY KEY,
    ca_id TEXT NOT NULL,
    project_id TEXT NOT NULL,
    hmac BLOB NOT NULL,
    label TEXT NOT NULL,
    names TEXT,
    expires_at INTEGER,
    created_at INTEGER NOT NULL,
    created_by TEXT NOT NULL,
    revoked_at INTEGER,
    revoked_by TEXT,
    used_at INTEGER
) WITHOUT ROWID;
CREATE INDEX IF NOT EXISTS acme_bindings_ca ON acme_bindings (ca_id);
CREATE TABLE IF NOT EXISTS acme_accounts (
    id TEXT PRIMARY KEY,
    ca_id TEXT NOT NULL,
    binding_id TEXT NOT NULL,
    thumbprint TEXT NOT NULL,
    jwk TEXT NOT NULL,
    contact TEXT NOT NULL,
    status TEXT NOT NULL,
    created_at INTEGER NOT NULL,
    UNIQUE (ca_id, thumbprint)
) WITHOUT ROWID;
CREATE INDEX IF NOT EXISTS acme_accounts_binding ON acme_accounts (binding_id);
CREATE TABLE IF NOT EXISTS acme_orders (
    id TEXT PRIMARY KEY,
    ca_id TEXT NOT NULL,
    account_id TEXT NOT NULL,
    identifiers TEXT NOT NULL,
    status TEXT NOT NULL,
    expires_at INTEGER NOT NULL,
    claimed_at INTEGER,
    serial TEXT,
    created_at INTEGER NOT NULL
) WITHOUT ROWID;
CREATE INDEX IF NOT EXISTS acme_orders_account ON acme_orders (account_id, created_at);
CREATE INDEX IF NOT EXISTS acme_orders_expiry ON acme_orders (expires_at);
CREATE TABLE IF NOT EXISTS acme_authz (
    id TEXT PRIMARY KEY,
    order_id TEXT NOT NULL,
    ca_id TEXT NOT NULL,
    account_id TEXT NOT NULL,
    kind TEXT NOT NULL,
    value TEXT NOT NULL,
    wildcard INTEGER NOT NULL,
    status TEXT NOT NULL,
    expires_at INTEGER NOT NULL
) WITHOUT ROWID;
CREATE INDEX IF NOT EXISTS acme_authz_order ON acme_authz (order_id);
CREATE INDEX IF NOT EXISTS acme_authz_reuse ON acme_authz (account_id, value);
CREATE INDEX IF NOT EXISTS acme_authz_expiry ON acme_authz (expires_at);
CREATE TABLE IF NOT EXISTS acme_order_authz (
    order_id TEXT NOT NULL,
    authz_id TEXT NOT NULL,
    PRIMARY KEY (order_id, authz_id)
) WITHOUT ROWID;
CREATE INDEX IF NOT EXISTS acme_order_authz_authz ON acme_order_authz (authz_id);
CREATE TABLE IF NOT EXISTS acme_challenges (
    id TEXT PRIMARY KEY,
    authz_id TEXT NOT NULL,
    ca_id TEXT NOT NULL,
    type TEXT NOT NULL,
    token TEXT NOT NULL,
    status TEXT NOT NULL,
    claimed_at INTEGER,
    validated_at INTEGER,
    error TEXT
) WITHOUT ROWID;
CREATE INDEX IF NOT EXISTS acme_challenges_authz ON acme_challenges (authz_id);
CREATE TABLE IF NOT EXISTS acme_certs (
    ca_id TEXT NOT NULL,
    serial TEXT NOT NULL,
    account_id TEXT NOT NULL,
    binding_id TEXT NOT NULL,
    order_id TEXT NOT NULL,
    chain TEXT NOT NULL,
    not_after INTEGER NOT NULL,
    PRIMARY KEY (ca_id, serial)
) WITHOUT ROWID;
CREATE TABLE IF NOT EXISTS acme_nonces (
    nonce TEXT PRIMARY KEY,
    expires_at INTEGER NOT NULL
) WITHOUT ROWID;
CREATE INDEX IF NOT EXISTS acme_nonces_expiry ON acme_nonces (expires_at);
CREATE TABLE IF NOT EXISTS acme_keys (
    name TEXT PRIMARY KEY,
    secret BLOB NOT NULL
) WITHOUT ROWID;
";

/// The largest request (a JWS; a CSR rides inside one).
pub(crate) const JWS_MAX: usize = 64 * 1024;
/// Names in one order, and in one binding.
const NAMES_MAX: usize = 100;
const BINDING_NAMES_MAX: usize = 50;
/// Orders one account makes in a day, accounts one binding makes, live bindings on one CA.
const ORDERS_PER_DAY: i64 = 300;
const ACCOUNTS_PER_BINDING: i64 = 50;
const BINDINGS_MAX: i64 = 200;
/// How long a nonce, an order and its authorizations last; how long an order's row is kept after.
const NONCE_MS: i64 = 60 * 60_000;
const ORDER_MS: i64 = 24 * 60 * 60_000;
const KEEP_MS: i64 = 24 * 60 * 60_000;
/// A claim on a challenge or an order that a node took and never finished is taken over after this.
const CLAIM_MS: i64 = 2 * 60_000;
/// A valid authorization is reused by a new order only while it has this long left (an order
/// ends with the earliest of its authorizations).
const REUSE_MIN_MS: i64 = 60 * 60_000;
/// The most of an http-01 answer read.
const ANSWER_MAX: usize = 8 * 1024;
/// TXT records read at one name.
const TXT_MAX: usize = 32;

/// What validating a challenge reaches outside roll. rolld's resolves through public DNS and
/// fetches and connects through its guarded checks (`roll_net::PublicOnly`, no redirects, the
/// literal-host check, bounded reads); tests inject fakes.
pub trait Checks: Send + Sync {
    /// The TXT records at `name` (each record's strings joined), within a timeout.
    fn txt(&self, name: &str) -> std::result::Result<Vec<String>, String>;
    /// GET `url` (`http://`, port 80) without following redirects: a 2xx answer's first `limit`
    /// bytes at most.
    fn get(&self, url: &str, limit: usize) -> std::result::Result<Vec<u8>, String>;
    /// tls-alpn-01 (RFC 8737): a TLS handshake with `name` on port 443, SNI `name`, offering only
    /// `acme-tls/1`. The server chooses `acme-tls/1` and presents a certificate whose one SAN is
    /// `name`, with the critical `id-pe-acmeIdentifier` extension holding `digest` (SHA-256 of the
    /// key authorization).
    fn tls_alpn(&self, name: &str, digest: &[u8; 32]) -> std::result::Result<(), AlpnFailure>;
}

/// Why a tls-alpn-01 check failed.
#[derive(Debug)]
pub enum AlpnFailure {
    /// No `acme-tls/1` handshake: the name didn't resolve to an address roll may reach, the
    /// connection or handshake failed, or the server chose another protocol.
    Tls(String),
    /// The handshake's certificate doesn't prove the name.
    Incorrect(String),
}

/// ACME's state on a node: rolld's checks once it says, and the nonce key once read.
#[derive(Default)]
pub(crate) struct State {
    checks: OnceLock<Arc<dyn Checks>>,
    nonce_key: OnceLock<[u8; 32]>,
}

/// An RFC 8555 problem document (`urn:ietf:params:acme:error:<kind>`). Never key material.
#[derive(Debug)]
pub struct Problem {
    pub(crate) kind: &'static str,
    pub(crate) status: u16,
    pub(crate) detail: String,
    algorithms: bool,
    /// An existing account's URL (409 on a key change to a key in use).
    pub(crate) location: Option<String>,
}

impl Problem {
    pub(crate) fn new(kind: &'static str, status: u16, detail: impl Into<String>) -> Problem {
        Problem { kind, status, detail: detail.into(), algorithms: false, location: None }
    }
    pub(crate) fn malformed(detail: impl Into<String>) -> Problem {
        Problem::new("malformed", 400, detail)
    }
    pub(crate) fn unauthorized(detail: impl Into<String>) -> Problem {
        Problem::new("unauthorized", 403, detail)
    }
    pub(crate) fn bad_algorithm(alg: &str) -> Problem {
        Problem::new("badSignatureAlgorithm", 400, format!("{alg} isn't accepted: sign with ES256 or RS256")).algorithms()
    }
    pub(crate) fn algorithms(mut self) -> Problem {
        self.algorithms = true;
        self
    }
    fn not_found(what: &str) -> Problem {
        Problem::new("malformed", 404, format!("no such {what}"))
    }

    pub(crate) fn body(&self) -> Json {
        let mut b = json!({ "type": format!("urn:ietf:params:acme:error:{}", self.kind), "detail": self.detail, "status": self.status });
        if self.algorithms {
            b["algorithms"] = json!(jws::ALGORITHMS);
        }
        b
    }
}

impl From<Error> for Problem {
    fn from(e: Error) -> Problem {
        match e {
            Error::BadRequest(m) => Problem::malformed(m),
            Error::NotFound(m) => Problem::new("malformed", 404, m),
            Error::Unauthorized(m) | Error::Forbidden(m) => Problem::unauthorized(m),
            Error::Conflict(_, m) | Error::TooMany(m) => Problem::new("rateLimited", 429, m),
            e => {
                tracing::error!("roll secrets ACME: {e}");
                Problem::new("serverInternal", 500, "roll couldn't answer this request; try again")
            }
        }
    }
}

impl From<code_sql::Error> for Problem {
    fn from(e: code_sql::Error) -> Problem {
        Problem::from(Error::from(e))
    }
}

type Acme<T> = std::result::Result<T, Problem>;

// ── the protocol ──

/// What a POST is to.
pub(crate) enum Ep {
    NewAccount,
    Account(String),
    Orders(String),
    NewOrder,
    Order(String),
    Finalize(String),
    Authz(String),
    Chall(String),
    Cert(String),
    Revoke,
    KeyChange,
}

pub(crate) enum Body {
    Json(Json),
    Pem(String),
    Empty,
}

pub(crate) struct Answer {
    pub status: u16,
    pub body: Body,
    pub location: Option<String>,
    /// `Link: <…>;rel="up"` (a challenge's authorization).
    pub up: Option<String>,
}

fn ok(body: Json) -> Answer {
    Answer { status: 200, body: Body::Json(body), location: None, up: None }
}

/// An account, as stored.
struct Account {
    id: String,
    binding: String,
    jwk: Jwk,
    thumbprint: String,
    status: String,
    contact: Vec<String>,
}

/// A live binding: whom an account acts as.
struct Live {
    id: String,
    project: String,
    label: String,
    names: Option<Vec<String>>,
    created_by: String,
    policy: Policy,
    ca_name: String,
    pre_authorize: bool,
}

impl Live {
    /// Whether an account with this binding may have `value`: within the CA's policy and the
    /// binding's names. Checked at new-order and again at finalize.
    fn takes(&self, value: &str) -> std::result::Result<(), String> {
        if !self.policy.allows(value) {
            let ips = if self.policy.allow_ip { " and IP addresses" } else { "" };
            return Err(format!("{} doesn't sign for {value}: it takes names under {}{ips}", self.ca_name, self.policy.domains.join(", ")));
        }
        if let Some(names) = &self.names
            && !binding_allows(names, value)
        {
            return Err(format!("this account's binding doesn't take {value}: it takes {}", names.join(", ")));
        }
        Ok(())
    }
}

/// A request in progress: its CA, its URLs, who's asking.
struct Req<'a> {
    ca: &'a str,
    base: String,
    url: String,
    ip: Option<String>,
}

impl Req<'_> {
    fn account_url(&self, id: &str) -> String {
        format!("{}/acct/{id}", self.base)
    }
}

/// RFC 3339, as ACME's `expires` is.
fn rfc3339(ms: i64) -> String {
    match time::OffsetDateTime::from_unix_timestamp(ms.div_euclid(1000)) {
        Ok(t) => format!("{:04}-{:02}-{:02}T{:02}:{:02}:{:02}Z", t.year(), u8::from(t.month()), t.day(), t.hour(), t.minute(), t.second()),
        Err(_) => String::new(),
    }
}

fn token() -> String {
    let mut raw = [0u8; 32];
    getrandom::fill(&mut raw).expect("system randomness");
    b64(&raw)
}

/// An identifier from an order: `dns` (a name or a wildcard one level down) or `ip`, normalized.
fn identifier(v: &Json) -> Acme<(String, String)> {
    let kind = v["type"].as_str().unwrap_or_default();
    let value = v["value"].as_str().unwrap_or_default().trim();
    if value.is_empty() || value.len() > 253 {
        return Err(Problem::malformed("an identifier is {type, value}"));
    }
    match kind {
        "dns" => {
            let name = value.trim_end_matches('.').to_ascii_lowercase();
            if !pki::is_domain(&name) || name.parse::<std::net::IpAddr>().is_ok() {
                return Err(Problem::new("rejectedIdentifier", 400, format!("{value} isn't a DNS name")));
            }
            Ok(("dns".into(), name))
        }
        "ip" => value.parse::<std::net::IpAddr>().map(|ip| ("ip".into(), ip.to_string())).map_err(|_| Problem::new("rejectedIdentifier", 400, format!("{value} isn't an IP address"))),
        _ => Err(Problem::new("unsupportedIdentifier", 400, format!("identifiers are dns or ip, not {kind}"))),
    }
}

impl Secrets {
    fn nonce_key(&self) -> Result<[u8; 32]> {
        if let Some(k) = self.acme.nonce_key.get() {
            return Ok(*k);
        }
        // One key for every node (any node checks a nonce another gave out). It only proves that
        // roll made a nonce; a used nonce is refused by its row, whoever made it.
        let mut fresh = [0u8; 32];
        getrandom::fill(&mut fresh).expect("system randomness");
        let stored: Vec<u8> = self.db.write(|c| {
            c.execute("INSERT INTO acme_keys (name, secret) VALUES ('nonce', ?) ON CONFLICT (name) DO NOTHING", params![fresh.to_vec()])?;
            Ok::<_, Error>(c.query_row("SELECT secret FROM acme_keys WHERE name = 'nonce'", (), |r| r.get(0))?)
        })?;
        let key: [u8; 32] = stored.try_into().map_err(|_| Error::Internal("ACME's nonce key isn't 32 bytes".into()))?;
        Ok(*self.acme.nonce_key.get_or_init(|| key))
    }

    /// A fresh nonce (nothing is written until it's used).
    pub(crate) fn acme_nonce(&self) -> Result<String> {
        Ok(jws::new_nonce(&self.nonce_key()?, code_sql::now_ms()))
    }

    /// Use a nonce: one roll made within the hour, never used before (one write).
    fn use_nonce(&self, nonce: Option<&str>) -> Acme<()> {
        let bad = |d: &str| Problem::new("badNonce", 400, d.to_string());
        let n = nonce.ok_or_else(|| bad("the protected header has no nonce"))?;
        let now = code_sql::now_ms();
        let made = jws::nonce_time(&self.nonce_key()?, n).filter(|t| now - NONCE_MS < *t && *t <= now + 60_000).ok_or_else(|| bad("that nonce isn't one of roll's, or it's over an hour old: get a new one"))?;
        let fresh = self.db.write(|c| Ok::<_, Error>(c.execute("INSERT INTO acme_nonces (nonce, expires_at) VALUES (?, ?) ON CONFLICT (nonce) DO NOTHING", params![n, made + NONCE_MS])? == 1))?;
        if fresh { Ok(()) } else { Err(bad("that nonce was used: each is good for one request")) }
    }

    /// The directory (RFC 8555 §7.1.1). External Account Binding is always required.
    pub(crate) fn acme_directory(&self, ca: &str) -> Acme<Json> {
        self.ca_of(None, ca).map_err(|_| Problem::not_found("CA"))?;
        let b = self.acme_base(ca)?;
        Ok(json!({
            "newNonce": format!("{b}/new-nonce"),
            "newAccount": format!("{b}/new-account"),
            "newOrder": format!("{b}/new-order"),
            "revokeCert": format!("{b}/revoke-cert"),
            "keyChange": format!("{b}/key-change"),
            "meta": { "externalAccountRequired": true },
        }))
    }

    fn account(&self, req: &Req<'_>, kid: &str) -> Acme<Account> {
        let gone = || Problem::new("accountDoesNotExist", 400, "no account with that URL on this CA");
        let id = kid.strip_prefix(&format!("{}/acct/", req.base)).ok_or_else(gone)?;
        let row: Option<(String, String, String, String, String)> = self.db.read(|c| {
            c.query_row("SELECT id, binding_id, jwk, status, contact FROM acme_accounts WHERE id = ? AND ca_id = ?", [id, req.ca], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?))).optional()
        })?;
        let (id, binding, jwk, status, contact) = row.ok_or_else(gone)?;
        let jwk = Jwk::parse(&serde_json::from_str(&jwk).unwrap_or_default())?;
        Ok(Account { id, binding, thumbprint: jwk.thumbprint(), jwk, status, contact: crate::from_json(&contact) })
    }

    /// The binding an account acts with, looked up on every request: revoked, past its end, made
    /// by someone who can no longer reach the project, or of a deleted project, and the account
    /// stops.
    fn live(&self, ca: &str, binding: &str) -> Acme<Live> {
        type Row = (String, String, Option<String>, Option<i64>, Option<i64>, String, String, String, Option<i64>, i64);
        let row: Option<Row> = self.db.read(|c| {
            c.query_row(
                "SELECT b.project_id, b.label, b.names, b.expires_at, b.revoked_at, b.created_by, a.policy, a.name, a.acme_preauthorize,
                        (SELECT COUNT(*) FROM projects p WHERE p.project_id = b.project_id AND p.forgotten_at IS NULL)
                 FROM acme_bindings b JOIN cas a ON a.id = b.ca_id WHERE b.id = ? AND b.ca_id = ?",
                [binding, ca],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?, r.get(5)?, r.get(6)?, r.get(7)?, r.get(8)?, r.get(9)?)),
            )
            .optional()
        })?;
        let (project, label, names, expires_at, revoked_at, created_by, policy, ca_name, pre, live) = row.ok_or_else(|| Problem::unauthorized("no such external account binding on this CA"))?;
        // A deleted project's CA issues nothing (its CRL is still served); it issues again if the
        // project is recovered.
        if live == 0 {
            return Err(Problem::unauthorized("the CA's project was deleted"));
        }
        if revoked_at.is_some() {
            return Err(Problem::unauthorized("the external account binding was revoked"));
        }
        if expires_at.is_some_and(|t| t <= code_sql::now_ms()) {
            return Err(Problem::unauthorized("the external account binding has expired"));
        }
        // An external account binding is the project admin's: it works while its maker still is one.
        if !self.maker_reaches(&created_by, &[(crate::access::Action::Admin, crate::access::Scope::whole(&project))])? {
            return Err(Problem::unauthorized("whoever made the external account binding can no longer reach its project"));
        }
        let policy = serde_json::from_str(&policy).map_err(|_| Error::Internal("a CA's policy isn't JSON".into()))?;
        Ok(Live { id: binding.to_string(), project, label, names: names.and_then(|n| serde_json::from_str(&n).ok()), created_by, policy, ca_name, pre_authorize: pre.unwrap_or(0) != 0 })
    }

    /// Who an ACME request is in the audit log: `principal` and the client's address.
    fn acme_caller(principal: &str, ip: &Option<String>) -> Caller {
        let mut c = Caller::consumer(principal);
        c.runtime = false;
        c.ip = ip.clone();
        c
    }

    /// A POST to an ACME resource: the JWS checked (its key, signature, `url` and nonce), then the
    /// resource's own work.
    pub(crate) fn acme_post(&self, ca: &str, ep: Ep, path: &str, content_type: Option<&str>, ip: Option<String>, body: &[u8]) -> Acme<Answer> {
        self.ca_of(None, ca).map_err(|_| Problem::not_found("CA"))?;
        let base = self.acme_base(ca)?;
        if content_type.map(|t| t.split(';').next().unwrap_or_default().trim()) != Some("application/jose+json") {
            return Err(Problem::new("malformed", 415, "ACME requests are application/jose+json"));
        }
        let jws = Jws::parse(body)?;
        let url = format!("{}{path}", base.split("/v1/secrets/acme/").next().unwrap_or_default());
        if jws.header.url != url {
            return Err(Problem::unauthorized(format!("the JWS's url isn't this request's ({url})")));
        }
        if !jws::ALGORITHMS.contains(&jws.header.alg.as_str()) {
            return Err(Problem::bad_algorithm(&jws.header.alg));
        }
        let req = Req { ca, base, url, ip };
        let by_jwk = matches!(ep, Ep::NewAccount | Ep::Revoke);
        let (account, key) = match (&jws.header.jwk, &jws.header.kid) {
            (Some(j), None) if by_jwk => (None, Jwk::parse(j)?),
            (None, Some(kid)) if !matches!(ep, Ep::NewAccount) => {
                let a = self.account(&req, kid)?;
                let k = a.jwk.clone();
                (Some(a), k)
            }
            (Some(_), None) => return Err(Problem::malformed("this request is signed by an account: kid, not jwk")),
            (None, Some(_)) => return Err(Problem::malformed("a new account is signed by its key: jwk, not kid")),
            _ => return Err(Problem::malformed("the protected header has exactly one of jwk and kid")),
        };
        key.verify(&jws.header.alg, &jws.signing_input, &jws.signature)?;
        self.use_nonce(jws.header.nonce.as_deref())?;
        // An account acts while it's valid and its binding is live.
        let acting = match account {
            Some(a) => {
                if a.status != "valid" {
                    return Err(Problem::unauthorized(format!("the account is {}", a.status)));
                }
                let live = self.live(ca, &a.binding)?;
                Some((a, live))
            }
            None => None,
        };
        let need = || acting.as_ref().ok_or_else(|| Problem::malformed("this request is signed by an account"));
        match ep {
            Ep::NewAccount => self.new_account(&req, &jws, key),
            Ep::Account(id) => self.update_account(&req, &jws, need()?, &id),
            Ep::Orders(id) => self.orders(&req, need()?, &id),
            Ep::NewOrder => self.new_order(&req, &jws, need()?),
            Ep::Order(id) => Ok(ok(self.order_json(&req, &need()?.0, &id)?)),
            Ep::Finalize(id) => self.finalize(&req, &jws, need()?, &id),
            Ep::Authz(id) => self.authz(&req, &jws, need()?, &id),
            Ep::Chall(id) => self.challenge(&req, &jws, need()?, &id),
            Ep::Cert(serial) => self.certificate(&req, need()?, &serial),
            Ep::Revoke => self.revoke_cert(&req, &jws, acting.as_ref(), &key),
            Ep::KeyChange => self.key_change(&req, &jws, need()?),
        }
    }

    fn account_json(req: &Req<'_>, id: &str, status: &str, contact: &[String]) -> Json {
        json!({ "status": status, "contact": contact, "orders": format!("{}/orders", req.account_url(id)) })
    }

    /// The account this key already has on the CA.
    fn existing_account(&self, req: &Req<'_>, thumbprint: &str) -> Acme<Option<Answer>> {
        let row: Option<(String, String, String)> =
            self.db.read(|c| c.query_row("SELECT id, status, contact FROM acme_accounts WHERE ca_id = ? AND thumbprint = ?", [req.ca, thumbprint], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?))).optional())?;
        Ok(row.map(|(id, status, contact)| Answer { status: 200, body: Body::Json(Secrets::account_json(req, &id, &status, &crate::from_json::<Vec<String>>(&contact))), location: Some(req.account_url(&id)), up: None }))
    }

    /// newAccount (RFC 8555 §7.3): the key's account if it has one; otherwise a new one, with an
    /// External Account Binding whose MAC is over this very key.
    fn new_account(&self, req: &Req<'_>, jws: &Jws, key: Jwk) -> Acme<Answer> {
        let p = jws.json()?;
        let thumbprint = key.thumbprint();
        if let Some(a) = self.existing_account(req, &thumbprint)? {
            return Ok(a);
        }
        if p["onlyReturnExisting"] == true {
            return Err(Problem::new("accountDoesNotExist", 400, "no account has this key on this CA"));
        }
        let eab = p.get("externalAccountBinding").ok_or_else(|| Problem::new("externalAccountRequired", 403, "this CA needs an external account binding: ask an admin of its project for a key id and HMAC key (roll secrets cas acme eab add)"))?;
        let inner = Jws::from_json(eab)?;
        let kid = inner.header.kid.as_deref().ok_or_else(|| Problem::malformed("the external account binding names its key id (kid)"))?;
        if inner.header.nonce.is_some() || inner.header.jwk.is_some() || inner.header.url != req.url {
            return Err(Problem::malformed("the external account binding's header is {alg, kid, url} with this request's url, and no nonce"));
        }
        let bound = serde_json::from_slice::<Json>(&inner.payload).ok().and_then(|j| Jwk::parse(&j).ok());
        if bound.as_ref() != Some(&key) {
            return Err(Problem::unauthorized("the external account binding isn't over this account's key"));
        }
        let live = self.live(req.ca, kid)?;
        let mac_key = self.binding_key(&live)?;
        jws::verify_mac(&inner.header.alg, &unb64(mac_key.expose(), "the binding's key")?, &inner.signing_input, &inner.signature)?;
        let contact = contact_of(&p)?;
        let id = names::new_id("acct", 16);
        let now = self.db.now_ms()?;
        let caller = Secrets::acme_caller(&format!("acme:{}", live.id), &req.ip);
        let made = self.db.write(|c| {
            let n: i64 = c.query_row("SELECT COUNT(*) FROM acme_accounts WHERE binding_id = ?", [live.id.as_str()], |r| r.get(0))?;
            if n >= ACCOUNTS_PER_BINDING {
                return Err(Error::TooMany(format!("binding {} has made {ACCOUNTS_PER_BINDING} accounts: that's the most one makes", live.id)));
            }
            let made = c.execute(
                "INSERT INTO acme_accounts (id, ca_id, binding_id, thumbprint, jwk, contact, status, created_at) VALUES (?, ?, ?, ?, ?, ?, 'valid', ?) ON CONFLICT (ca_id, thumbprint) DO NOTHING",
                params![id, req.ca, live.id, thumbprint, key.canonical(), crate::json(&contact), now],
            )? == 1;
            if made {
                c.execute("UPDATE acme_bindings SET used_at = ? WHERE id = ?", params![now, live.id])?;
                audit::record(c, self.keys()?, &live.project, &caller, "ca.acme.account", None, None, &[], &format!("{}: ACME account {id} made with binding {} (made by {})", live.ca_name, live.id, live.created_by), now)?;
            }
            Ok(made)
        })?;
        if !made {
            // Another node made this key's account at the same moment.
            return self.existing_account(req, &thumbprint)?.ok_or_else(|| Problem::new("serverInternal", 500, "try again"));
        }
        Ok(Answer { status: 201, body: Body::Json(Secrets::account_json(req, &id, "valid", &contact)), location: Some(req.account_url(&id)), up: None })
    }

    fn binding_key(&self, live: &Live) -> Acme<Value> {
        let dek = self.project(&live.project)?;
        let sealed: Vec<u8> = self.db.read(|c| c.query_row("SELECT hmac FROM acme_bindings WHERE id = ?", [live.id.as_str()], |r| r.get(0)))?;
        Ok(dek.open(&eab_aad(&live.project, &live.id), &sealed).map_err(Error::from)?)
    }

    /// The account's own URL: POST-as-GET reads it; `{contact}` replaces its contacts;
    /// `{status: "deactivated"}` ends it for good.
    fn update_account(&self, req: &Req<'_>, jws: &Jws, (a, _): &(Account, Live), id: &str) -> Acme<Answer> {
        if a.id != id {
            return Err(Problem::unauthorized("an account reads and changes only itself"));
        }
        if jws.post_as_get() {
            return Ok(ok(Secrets::account_json(req, &a.id, &a.status, &a.contact)));
        }
        let p = jws.json()?;
        let mut status = a.status.clone();
        let mut contact = a.contact.clone();
        if p.get("contact").is_some() {
            contact = contact_of(&p)?;
        }
        match p["status"].as_str() {
            None => {}
            Some("deactivated") => status = "deactivated".into(),
            Some(s) => return Err(Problem::malformed(format!("an account's status changes only to deactivated, not {s}"))),
        }
        self.db.write(|c| c.execute("UPDATE acme_accounts SET status = ?, contact = ? WHERE id = ?", params![status, crate::json(&contact), a.id]))?;
        Ok(ok(Secrets::account_json(req, &a.id, &status, &contact)))
    }

    fn orders(&self, req: &Req<'_>, (a, _): &(Account, Live), id: &str) -> Acme<Answer> {
        if a.id != id {
            return Err(Problem::unauthorized("an account lists only its own orders"));
        }
        let now = self.db.now_ms()?;
        let ids = self.db.read(|c| c.query("SELECT id FROM acme_orders WHERE account_id = ? AND expires_at > ? ORDER BY created_at DESC LIMIT 1000", params![a.id, now], |r| r.get::<_, String>(0)))?;
        Ok(ok(json!({ "orders": ids.iter().map(|o| format!("{}/order/{o}", req.base)).collect::<Vec<_>>() })))
    }

    /// newOrder (RFC 8555 §7.4): every name within the CA's policy and the binding's names; each
    /// gets an authorization, valid at once where the CA pre-authorizes, else with challenges. A
    /// name the account already proved on this CA reuses that authorization (same identifier and
    /// wildcard, valid, an hour or more left), as Let's Encrypt does: it keeps its own end, and the
    /// order ends with the earliest of its authorizations.
    fn new_order(&self, req: &Req<'_>, jws: &Jws, (a, live): &(Account, Live)) -> Acme<Answer> {
        let p = jws.json()?;
        let list = p["identifiers"].as_array().filter(|l| !l.is_empty() && l.len() <= NAMES_MAX).ok_or_else(|| Problem::malformed(format!("an order names 1–{NAMES_MAX} identifiers")))?;
        let mut ids: Vec<(String, String)> = vec![];
        for v in list {
            let (kind, value) = identifier(v)?;
            live.takes(&value).map_err(|m| Problem::new("rejectedIdentifier", 400, m))?;
            if !ids.iter().any(|(_, v)| *v == value) {
                ids.push((kind, value));
            }
        }
        if !live.pre_authorize && self.acme.checks.get().is_none() {
            return Err(Problem::new("rejectedIdentifier", 400, "this server validates no challenges: an admin of the CA's project can pre-authorize its bindings' names"));
        }
        let now = self.db.now_ms()?;
        let order = names::new_id("ord", 16);
        self.db.write(|c| {
            let recent: i64 = c.query_row("SELECT COUNT(*) FROM acme_orders WHERE account_id = ? AND created_at > ?", params![a.id, now - 86_400_000], |r| r.get(0))?;
            if recent >= ORDERS_PER_DAY {
                return Err(Error::TooMany(format!("this account made {ORDERS_PER_DAY} orders in the last day: that's the most")));
            }
            let mut expires = now + ORDER_MS;
            let mut reused: Vec<Option<String>> = vec![];
            for (kind, value) in &ids {
                let bare = value.strip_prefix("*.").unwrap_or(value);
                let found: Option<(String, i64)> = c
                    .query_row(
                        "SELECT id, expires_at FROM acme_authz WHERE account_id = ? AND ca_id = ? AND kind = ? AND value = ? AND wildcard = ? AND status = 'valid' AND expires_at > ? ORDER BY expires_at DESC LIMIT 1",
                        params![a.id, req.ca, kind, bare, i64::from(value.starts_with("*.")), now + REUSE_MIN_MS],
                        |r| Ok((r.get(0)?, r.get(1)?)),
                    )
                    .optional()?;
                if let Some((_, end)) = &found {
                    expires = expires.min(*end);
                }
                reused.push(found.map(|(id, _)| id));
            }
            let idents: Vec<Ident> = ids.iter().map(|(k, v)| Ident { r#type: k.clone(), value: v.clone() }).collect();
            c.execute(
                "INSERT INTO acme_orders (id, ca_id, account_id, identifiers, status, expires_at, created_at) VALUES (?, ?, ?, ?, 'pending', ?, ?)",
                params![order, req.ca, a.id, crate::json(&idents), expires, now],
            )?;
            for ((kind, value), reuse) in ids.iter().zip(&reused) {
                if let Some(az) = reuse {
                    c.execute("INSERT INTO acme_order_authz (order_id, authz_id) VALUES (?, ?)", params![order, az])?;
                    continue;
                }
                let az = names::new_id("az", 16);
                let wildcard = value.starts_with("*.");
                let bare = value.strip_prefix("*.").unwrap_or(value);
                let status = if live.pre_authorize { "valid" } else { "pending" };
                c.execute(
                    "INSERT INTO acme_authz (id, order_id, ca_id, account_id, kind, value, wildcard, status, expires_at) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?)",
                    params![az, order, req.ca, a.id, kind, bare, i64::from(wildcard), status, now + ORDER_MS],
                )?;
                c.execute("INSERT INTO acme_order_authz (order_id, authz_id) VALUES (?, ?)", params![order, az])?;
                if live.pre_authorize {
                    continue;
                }
                // http-01 and tls-alpn-01 can't prove a wildcard; dns-01 and tls-alpn-01 (here)
                // can't prove an IP address.
                let types: &[&str] = match (kind.as_str(), wildcard) {
                    ("dns", true) => &["dns-01"],
                    ("dns", false) => &["http-01", "dns-01", "tls-alpn-01"],
                    _ => &["http-01"],
                };
                for t in types {
                    c.execute("INSERT INTO acme_challenges (id, authz_id, ca_id, type, token, status) VALUES (?, ?, ?, ?, ?, 'pending')", params![names::new_id("ch", 16), az, req.ca, t, token()])?;
                }
            }
            Ok::<_, Error>(())
        })?;
        Ok(Answer { status: 201, body: Body::Json(self.order_json(req, a, &order)?), location: Some(format!("{}/order/{order}", req.base)), up: None })
    }

    fn order_json(&self, req: &Req<'_>, a: &Account, id: &str) -> Acme<Json> {
        let now = self.db.now_ms()?;
        let o = self.db.read(|c| order_state(c, &a.id, id, now))?.ok_or_else(|| Problem::not_found("order"))?;
        Ok(o.json(req, id))
    }

    /// finalize (RFC 8555 §7.4): the CSR names exactly the order's identifiers; it's signed through
    /// the CA's own path (`source: "acme"`), recorded, audited and the order made valid in one write.
    fn finalize(&self, req: &Req<'_>, jws: &Jws, (a, live): &(Account, Live), id: &str) -> Acme<Answer> {
        let p = jws.json()?;
        let der = unb64(p["csr"].as_str().ok_or_else(|| Problem::malformed("finalize takes {csr}: the CSR's DER, base64url"))?, "csr")?;
        let pem = format!("-----BEGIN CERTIFICATE REQUEST-----\n{}\n-----END CERTIFICATE REQUEST-----\n", base64_lines(&der));
        let (csr, mut names) = pki::read_csr(&pem).map_err(|e| Problem::new("badCSR", 400, Problem::from(e).detail))?;
        names.sort();
        names.dedup();
        let now = self.db.now_ms()?;
        // Claimed in one write: no two nodes sign one order.
        let claimed = self.db.write(|c| {
            let o = order_state(c, &a.id, id, now)?.ok_or_else(|| Error::NotFound("no such order".into()))?;
            let stale = o.stored == "processing" && o.claimed_at.is_some_and(|t| t < now - CLAIM_MS) && o.expires_at > now;
            if o.status != "ready" && !stale {
                return Ok(Err(o.status));
            }
            // The CA's policy or the binding may have narrowed since the order (or the
            // authorization it reuses) was made.
            for i in &o.identifiers {
                live.takes(&i.value).map_err(Error::Forbidden)?;
            }
            let mut wanted: Vec<String> = o.identifiers.iter().map(|i| i.value.clone()).collect();
            wanted.sort();
            if wanted != names {
                return Err(Error::BadRequest(format!("the CSR's names are exactly the order's: {}", wanted.join(", "))));
            }
            c.execute("UPDATE acme_orders SET status = 'processing', claimed_at = ? WHERE id = ?", params![now, id])?;
            Ok(Ok(()))
        });
        match claimed {
            Ok(Ok(())) => {}
            Ok(Err(status)) => return Err(Problem::new("orderNotReady", 403, format!("the order is {status}: finalize it once it's ready"))),
            Err(Error::BadRequest(m)) => return Err(Problem::new("badCSR", 400, m)),
            Err(e) => return Err(e.into()),
        }
        let caller = Secrets::acme_caller(&format!("acme:{}", live.id), &req.ip);
        let principal = caller.principal.clone();
        let signed = self.sign_read(&live.project, req.ca, csr, names, None, "acme", &principal, |c, s, at| {
            let chain = format!("{}{}", s.certificate, s.ca_chain);
            c.execute(
                "INSERT INTO acme_certs (ca_id, serial, account_id, binding_id, order_id, chain, not_after) VALUES (?, ?, ?, ?, ?, ?, ?)",
                params![req.ca, s.serial, a.id, live.id, id, chain, s.not_after],
            )?;
            c.execute("UPDATE acme_orders SET status = 'valid', serial = ?, claimed_at = NULL WHERE id = ?", params![s.serial, id])?;
            let label = if live.label.is_empty() { String::new() } else { format!(" {}", live.label) };
            let summary = format!("{} issued {} over ACME for {} (binding {}{label}, made by {})", live.ca_name, s.serial, s.names.join(", "), live.id, live.created_by);
            audit::record(c, self.keys()?, &live.project, &caller, "ca.acme.issue", None, None, &[], &summary, at)
        });
        if let Err(e) = signed {
            // Let go of the order: it can be finalized again.
            let _ = self.db.write(|c| c.execute("UPDATE acme_orders SET status = 'pending', claimed_at = NULL WHERE id = ? AND status = 'processing'", [id]));
            return Err(match e {
                Error::BadRequest(m) => Problem::new("badCSR", 400, m),
                e => e.into(),
            });
        }
        Ok(Answer { status: 200, body: Body::Json(self.order_json(req, a, id)?), location: Some(format!("{}/order/{id}", req.base)), up: None })
    }

    fn authz(&self, req: &Req<'_>, jws: &Jws, (a, _): &(Account, Live), id: &str) -> Acme<Answer> {
        if !jws.post_as_get() {
            if jws.json()?["status"].as_str() != Some("deactivated") {
                return Err(Problem::malformed("an authorization's status changes only to deactivated"));
            }
            self.db.write(|c| c.execute("UPDATE acme_authz SET status = 'deactivated' WHERE id = ? AND account_id = ? AND status IN ('pending', 'valid')", [id, a.id.as_str()]))?;
        }
        let now = self.db.now_ms()?;
        let row: Option<(String, String, i64, String, i64)> = self.db.read(|c| {
            c.query_row("SELECT kind, value, wildcard, status, expires_at FROM acme_authz WHERE id = ? AND account_id = ? AND ca_id = ?", [id, a.id.as_str(), req.ca], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?))).optional()
        })?;
        let (kind, value, wildcard, status, expires_at) = row.ok_or_else(|| Problem::not_found("authorization"))?;
        let challenges = self.db.read(|c| c.query("SELECT id, type, token, status, validated_at, error FROM acme_challenges WHERE authz_id = ? ORDER BY type DESC", [id], challenge_row))?;
        let status = if (status == "pending" || status == "valid") && expires_at <= now { "expired".to_string() } else { status };
        let mut v = json!({
            "identifier": { "type": kind, "value": value },
            "status": status,
            "expires": rfc3339(expires_at),
            "challenges": challenges.iter().map(|c| c.json(req)).collect::<Vec<_>>(),
        });
        if wildcard != 0 {
            v["wildcard"] = json!(true);
        }
        Ok(ok(v))
    }

    /// A challenge: POST-as-GET reads it; any JSON object asks roll to validate it now. The claim,
    /// then the check outside any transaction, then the result in one write.
    fn challenge(&self, req: &Req<'_>, jws: &Jws, (a, _): &(Account, Live), id: &str) -> Acme<Answer> {
        let row: Option<(String, String, String, String, String)> = self.db.read(|c| {
            c.query_row(
                "SELECT h.authz_id, h.type, h.token, z.kind, z.value FROM acme_challenges h JOIN acme_authz z ON z.id = h.authz_id WHERE h.id = ? AND z.account_id = ? AND h.ca_id = ?",
                [id, a.id.as_str(), req.ca],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?)),
            )
            .optional()
        })?;
        let (authz, typ, tok, kind, value) = row.ok_or_else(|| Problem::not_found("challenge"))?;
        if !jws.post_as_get() && jws.json()?.is_object() {
            let now = self.db.now_ms()?;
            let claimed = self.db.write(|c| {
                let row: Option<(String, i64, String, Option<i64>)> = c
                    .query_row("SELECT z.status, z.expires_at, h.status, h.claimed_at FROM acme_challenges h JOIN acme_authz z ON z.id = h.authz_id WHERE h.id = ?", [id], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)))
                    .optional()?;
                let Some((az, expires_at, ch, at)) = row else { return Ok::<_, Error>(false) };
                let open = ch == "pending" || (ch == "processing" && at.is_some_and(|t| t < now - CLAIM_MS));
                if az != "pending" || expires_at <= now || !open {
                    return Ok(false);
                }
                c.execute("UPDATE acme_challenges SET status = 'processing', claimed_at = ? WHERE id = ?", params![now, id])?;
                Ok(true)
            })?;
            if claimed {
                let outcome = self.validate(&typ, &kind, &value, &tok, &format!("{tok}.{}", a.thumbprint));
                let at = self.db.now_ms()?;
                let (status, error) = match &outcome {
                    Ok(()) => ("valid", None),
                    Err(p) => ("invalid", Some(p.body().to_string())),
                };
                self.db.write(|c| {
                    if c.execute("UPDATE acme_challenges SET status = ?, validated_at = ?, error = ? WHERE id = ? AND status = 'processing'", params![status, at, error, id])? == 1 {
                        c.execute("UPDATE acme_authz SET status = ? WHERE id = ? AND status = 'pending'", params![status, authz])?;
                    }
                    Ok::<_, Error>(())
                })?;
            }
        }
        let ch = self.db.read(|c| c.query_row("SELECT id, type, token, status, validated_at, error FROM acme_challenges WHERE id = ?", [id], challenge_row))?;
        Ok(Answer { status: 200, body: Body::Json(ch.json(req)), location: None, up: Some(format!("{}/authz/{authz}", req.base)) })
    }

    /// Prove control of an identifier: the key authorization at `/.well-known/acme-challenge/`
    /// (http-01), its digest in `_acme-challenge`'s TXT records (dns-01) or in the certificate an
    /// `acme-tls/1` handshake presents (tls-alpn-01), through rolld.
    fn validate(&self, typ: &str, kind: &str, value: &str, tok: &str, key_auth: &str) -> Acme<()> {
        let checks = self.acme.checks.get().ok_or_else(|| Problem::new("serverInternal", 500, "this server validates no challenges"))?;
        match typ {
            "http-01" => {
                let host = if kind == "ip" && value.contains(':') { format!("[{value}]") } else { value.to_string() };
                let url = format!("http://{host}/.well-known/acme-challenge/{tok}");
                let body = checks.get(&url, ANSWER_MAX).map_err(|e| Problem::new("connection", 400, format!("fetching {url}: {e}")))?;
                if String::from_utf8_lossy(&body).trim() == key_auth {
                    Ok(())
                } else {
                    Err(Problem::new("incorrectResponse", 403, format!("{url} didn't answer with the key authorization")))
                }
            }
            "dns-01" => {
                use sha2::Digest;
                let name = format!("_acme-challenge.{value}");
                let digest = b64(&sha2::Sha256::digest(key_auth.as_bytes()));
                let records = checks.txt(&name).map_err(|e| Problem::new("dns", 400, format!("looking up TXT {name}: {e}")))?;
                if records.iter().take(TXT_MAX).any(|r| r.trim() == digest) {
                    Ok(())
                } else {
                    Err(Problem::new("incorrectResponse", 403, format!("no TXT record at {name} holds the key authorization's digest")))
                }
            }
            "tls-alpn-01" => {
                use sha2::Digest;
                let digest: [u8; 32] = sha2::Sha256::digest(key_auth.as_bytes()).into();
                checks.tls_alpn(value, &digest).map_err(|e| match e {
                    AlpnFailure::Tls(m) => Problem::new("tls", 400, format!("tls-alpn-01 with {value}:443: {m}")),
                    AlpnFailure::Incorrect(m) => Problem::new("incorrectResponse", 403, format!("tls-alpn-01 with {value}:443: {m}")),
                })
            }
            t => Err(Problem::malformed(format!("roll doesn't validate {t}"))),
        }
    }

    /// The certificate and the CA's (`application/pem-certificate-chain`), for the account that
    /// ordered it.
    fn certificate(&self, req: &Req<'_>, (a, _): &(Account, Live), serial: &str) -> Acme<Answer> {
        let row: Option<(String, String)> = self.db.read(|c| c.query_row("SELECT account_id, chain FROM acme_certs WHERE ca_id = ? AND serial = ?", [req.ca, serial], |r| Ok((r.get(0)?, r.get(1)?))).optional())?;
        match row {
            Some((owner, chain)) if owner == a.id => Ok(Answer { status: 200, body: Body::Pem(chain), location: None, up: None }),
            Some(_) => Err(Problem::unauthorized("a certificate is read by the account that ordered it")),
            None => Err(Problem::not_found("certificate")),
        }
    }

    /// revokeCert (RFC 8555 §7.6): a certificate issued over ACME, by the account that ordered it
    /// or with the certificate's own key. It goes on the CA's CRL (and OCSP) until it expires.
    fn revoke_cert(&self, req: &Req<'_>, jws: &Jws, acting: Option<&(Account, Live)>, key: &Jwk) -> Acme<Answer> {
        let p = jws.json()?;
        let der = unb64(p["certificate"].as_str().ok_or_else(|| Problem::malformed("revokeCert takes {certificate}: its DER, base64url"))?, "certificate")?;
        if let Some(r) = p.get("reason")
            && !r.as_u64().is_some_and(|r| r <= 10 && r != 7)
        {
            return Err(Problem::new("badRevocationReason", 400, "a reason is an RFC 5280 CRLReason code: 0–10, not 7"));
        }
        let (_, cert) = x509_parser::parse_x509_certificate(&der).map_err(|_| Problem::malformed("that isn't a certificate"))?;
        let serial = pki::hex(&cert.raw_serial().iter().skip_while(|b| **b == 0).copied().collect::<Vec<_>>());
        let row: Option<(String, String, String)> =
            self.db.read(|c| c.query_row("SELECT account_id, binding_id, chain FROM acme_certs WHERE ca_id = ? AND serial = ?", [req.ca, serial.as_str()], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?))).optional())?;
        let unknown = || Problem::new("malformed", 404, "this CA issued no certificate over ACME with that serial that's still valid");
        let (owner, binding, chain) = row.ok_or_else(unknown)?;
        if !x509_parser::pem::parse_x509_pem(chain.as_bytes()).is_ok_and(|(_, p)| p.contents == der) {
            return Err(unknown());
        }
        let by = match acting {
            Some((a, _)) if a.id == owner => format!("acme:{binding}"),
            Some(_) => return Err(Problem::unauthorized("a certificate is revoked by the account that ordered it, or with its own key")),
            None if key.is_spki_of(&cert) => "acme:certificate-key".to_string(),
            None => return Err(Problem::unauthorized("the request isn't signed with the certificate's key")),
        };
        let (project, ca_name, _, _) = self.ca_of(None, req.ca)?;
        let caller = Secrets::acme_caller(&by, &req.ip);
        let now = self.db.now_ms()?;
        let done = self.db.write(|c| {
            let revoked: Option<Option<i64>> = c.query_row("SELECT revoked_at FROM ca_certs WHERE ca_id = ? AND serial = ? AND not_after >= ?", params![req.ca, serial, now], |r| r.get(0)).optional()?;
            match revoked {
                None => Ok::<_, Error>(None),
                Some(Some(_)) => Ok(Some(false)),
                Some(None) => {
                    c.execute("UPDATE ca_certs SET revoked_at = ?, revoked_by = ? WHERE ca_id = ? AND serial = ?", params![now, by, req.ca, serial])?;
                    audit::record(c, self.keys()?, &project, &caller, "ca.revoke", None, None, &[], &format!("{ca_name} revoked {serial} over ACME (binding {binding})"), now)?;
                    Ok(Some(true))
                }
            }
        })?;
        match done {
            None => Err(unknown()),
            Some(false) => Err(Problem::new("alreadyRevoked", 400, "that certificate is already revoked")),
            Some(true) => {
                self.crls.lock().expect("crls").revoked(req.ca);
                Ok(Answer { status: 200, body: Body::Empty, location: None, up: None })
            }
        }
    }

    /// keyChange (RFC 8555 §7.3.5): the new key signs `{account, oldKey}` inside the old key's
    /// request; no other account on the CA may have it.
    fn key_change(&self, req: &Req<'_>, jws: &Jws, (a, _): &(Account, Live)) -> Acme<Answer> {
        let inner = Jws::from_json(&jws.json()?)?;
        if inner.header.url != req.url || inner.header.nonce.is_some() || inner.header.kid.is_some() {
            return Err(Problem::malformed("the inner JWS's header is {alg, jwk, url} with this request's url, and no nonce"));
        }
        let new = Jwk::parse(inner.header.jwk.as_ref().ok_or_else(|| Problem::malformed("the inner JWS carries the new key (jwk)"))?)?;
        if !jws::ALGORITHMS.contains(&inner.header.alg.as_str()) {
            return Err(Problem::bad_algorithm(&inner.header.alg));
        }
        new.verify(&inner.header.alg, &inner.signing_input, &inner.signature)?;
        let p = inner.json()?;
        if p["account"].as_str() != Some(req.account_url(&a.id).as_str()) || Jwk::parse(&p["oldKey"]).ok().as_ref() != Some(&a.jwk) {
            return Err(Problem::malformed("the inner payload is {account, oldKey}: this account's URL and current key"));
        }
        let thumbprint = new.thumbprint();
        let taken = self.db.write(|c| {
            let other: Option<String> = c.query_row("SELECT id FROM acme_accounts WHERE ca_id = ? AND thumbprint = ?", [req.ca, thumbprint.as_str()], |r| r.get(0)).optional()?;
            if other.is_none() {
                c.execute("UPDATE acme_accounts SET jwk = ?, thumbprint = ? WHERE id = ? AND thumbprint = ?", params![new.canonical(), thumbprint, a.id, a.thumbprint])?;
            }
            Ok::<_, Error>(other)
        })?;
        if let Some(other) = taken {
            let mut p = Problem::new("malformed", 409, "another account has that key");
            p.location = Some(req.account_url(&other));
            return Err(p);
        }
        Ok(ok(Secrets::account_json(req, &a.id, &a.status, &a.contact)))
    }
}

/// An order's identifier, as stored.
#[derive(Debug, Default, Deserialize, Serialize)]
struct Ident {
    r#type: String,
    value: String,
}

struct OrderRow {
    identifiers: Vec<Ident>,
    stored: String,
    /// What a client sees: `pending` reads as `ready` once every authorization is valid, and as
    /// `invalid` once one fails or the order expires.
    status: String,
    expires_at: i64,
    claimed_at: Option<i64>,
    serial: Option<String>,
    authz: Vec<String>,
}

fn order_state(c: &Conn<'_>, account: &str, id: &str, now: i64) -> Result<Option<OrderRow>> {
    type Row = (String, String, i64, Option<i64>, Option<String>);
    let row: Option<Row> = c
        .query_row("SELECT identifiers, status, expires_at, claimed_at, serial FROM acme_orders WHERE id = ? AND account_id = ?", [id, account], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?)))
        .optional()?;
    let Some((identifiers, stored, expires_at, claimed_at, serial)) = row else { return Ok(None) };
    let authz = c.query("SELECT z.id, z.status FROM acme_order_authz l JOIN acme_authz z ON z.id = l.authz_id WHERE l.order_id = ? ORDER BY z.id", [id], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))?;
    let status = match stored.as_str() {
        "pending" | "processing" if expires_at <= now => "invalid".to_string(),
        // An order lists at least one authorization; one listing none (made before orders listed
        // them) is never ready.
        "pending" if authz.is_empty() || authz.iter().any(|(_, s)| s != "valid" && s != "pending") => "invalid".into(),
        "pending" if authz.iter().all(|(_, s)| s == "valid") => "ready".into(),
        s => s.to_string(),
    };
    Ok(Some(OrderRow { identifiers: crate::from_json(&identifiers), stored, status, expires_at, claimed_at, serial, authz: authz.into_iter().map(|(id, _)| id).collect() }))
}

impl OrderRow {
    fn json(&self, req: &Req<'_>, id: &str) -> Json {
        let mut v = json!({
            "status": self.status,
            "expires": rfc3339(self.expires_at),
            "identifiers": self.identifiers,
            "authorizations": self.authz.iter().map(|a| format!("{}/authz/{a}", req.base)).collect::<Vec<_>>(),
            "finalize": format!("{}/order/{id}/finalize", req.base),
        });
        if let (Some(s), "valid") = (&self.serial, self.status.as_str()) {
            v["certificate"] = json!(format!("{}/cert/{s}", req.base));
        }
        v
    }
}

struct Challenge {
    id: String,
    r#type: String,
    token: String,
    status: String,
    validated_at: Option<i64>,
    error: Option<String>,
}

fn challenge_row(r: &code_sql::Row<'_>) -> code_sql::Result<Challenge> {
    Ok(Challenge { id: r.get(0)?, r#type: r.get(1)?, token: r.get(2)?, status: r.get(3)?, validated_at: r.get(4)?, error: r.get(5)? })
}

impl Challenge {
    fn json(&self, req: &Req<'_>) -> Json {
        let mut v = json!({ "type": self.r#type, "url": format!("{}/chall/{}", req.base, self.id), "token": self.token, "status": self.status });
        if let (Some(t), "valid") = (self.validated_at, self.status.as_str()) {
            v["validated"] = json!(rfc3339(t));
        }
        if let Some(e) = self.error.as_deref().and_then(|e| serde_json::from_str::<Json>(e).ok()) {
            v["error"] = e;
        }
        v
    }
}

/// An account's contacts: up to four `mailto:` URLs.
fn contact_of(p: &Json) -> Acme<Vec<String>> {
    let Some(list) = p.get("contact") else { return Ok(vec![]) };
    let list: Vec<String> = list.as_array().ok_or_else(|| Problem::malformed("contact is a list of mailto: URLs"))?.iter().filter_map(Json::as_str).map(str::to_string).collect();
    if list.len() > 4 || list.iter().any(|c| !c.starts_with("mailto:") || c.len() > 256 || c.contains(',')) {
        return Err(Problem::new("invalidContact", 400, "contact is up to four mailto: URLs"));
    }
    Ok(list)
}

/// DER as PEM's base64 lines.
fn base64_lines(der: &[u8]) -> String {
    use base64::Engine;
    let s = base64::engine::general_purpose::STANDARD.encode(der);
    s.as_bytes().chunks(64).map(|c| std::str::from_utf8(c).expect("base64")).collect::<Vec<_>>().join("\n")
}
