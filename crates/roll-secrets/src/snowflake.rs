//! Snowflake accounts (docs/secrets.md, "Snowflake"), as Vault's Snowflake database plugin.
//!
//! A project registers an account with an admin user roll signs in as (key-pair JWT; its private
//! key sealed with the project's data key), a managed user, the roles dynamic users may get, or
//! both. Managed, key pairs: a Snowflake user has two key slots, `RSA_PUBLIC_KEY` and
//! `RSA_PUBLIC_KEY_2`, and they take turns (turns.rs): each rotation sets a new key in the slot not
//! in use and hands out its private half, while the other keeps working through the grace period;
//! then that slot is cleared. Managed, passwords: each rotation replaces the user's password at
//! once. Dynamic: each lease gets a user of its own (`ROLL_<LEASE>`), its `DEFAULT_ROLE` and the
//! link's roles granted, `DAYS_TO_EXPIRY` as a backstop, dropped when the lease ends.
//!
//! Every statement goes to Snowflake's SQL API (`POST https://<account>.snowflakecomputing.com/
//! api/v2/statements`) through rolld's guarded client ([`crate::Deliver::request`]). The account
//! and every identifier are checked strictly before they're put in a URL or a statement.

use crate::access::{Action, Caller, Scope};
use crate::crypto::Keyring;
use crate::links::{Field, Issued, Link, LinkProvider, Mode};
use crate::{Deliver, Error, Result, Secrets, Value, audit, names};
use base64::Engine as _;
use code_sql::{Db, OptionalExtension, params};
use serde::{Deserialize, Serialize};
use serde_json::{Value as Json, json};
use std::collections::BTreeMap;
use std::sync::Arc;
use zeroize::Zeroizing;

pub(crate) const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS snowflake_accounts (
    id TEXT PRIMARY KEY,
    project_id TEXT NOT NULL,
    name TEXT NOT NULL,
    account TEXT NOT NULL,
    host TEXT NOT NULL,
    admin_user TEXT NOT NULL,
    admin_role TEXT,
    admin_key BLOB NOT NULL,
    admin_key_fp TEXT NOT NULL,
    auth TEXT NOT NULL,
    managed_user TEXT,
    roles TEXT NOT NULL,
    state TEXT NOT NULL,
    created_at INTEGER NOT NULL,
    created_by TEXT NOT NULL,
    UNIQUE (project_id, name)
) WITHOUT ROWID;
";

pub(crate) const KIND: &str = "snowflake";

/// How the users roll hands out sign in.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Auth {
    /// An RSA key pair (PKCS#8): the user's two key slots take turns.
    KeyPair,
    /// A password, for users that can't use keys (`TYPE = LEGACY_SERVICE` or `PERSON`).
    Password,
}

impl Auth {
    fn as_str(self) -> &'static str {
        match self {
            Auth::KeyPair => "key_pair",
            Auth::Password => "password",
        }
    }

    fn parse(s: &str) -> Auth {
        if s == "password" { Auth::Password } else { Auth::KeyPair }
    }
}

fn key_pair() -> Auth {
    Auth::KeyPair
}

#[derive(Debug, Serialize, PartialEq)]
pub struct SnowflakeAccount {
    pub id: String,
    pub name: String,
    /// The account identifier as registered (`myorg-myaccount`, `xy12345.us-east-2.aws`).
    pub account: String,
    /// Where roll sends statements: `<account>[.privatelink].snowflakecomputing.com`.
    pub host: String,
    pub admin_user: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub admin_role: Option<String>,
    /// The admin key's public fingerprint (`SHA256:…`, as `DESC USER` shows `RSA_PUBLIC_KEY_FP`).
    pub admin_key_fp: String,
    pub auth: Auth,
    /// The user managed links rotate, if any.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub managed_user: Option<String>,
    /// The roles dynamic users may be granted (the first is their default), if any.
    pub roles: Vec<String>,
    pub created_at: i64,
    pub created_by: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NewSnowflakeAccount {
    pub name: String,
    pub account: String,
    #[serde(default)]
    pub privatelink: bool,
    pub admin_user: String,
    #[serde(default)]
    pub admin_role: Option<String>,
    /// The admin user's RSA private key, PEM (PKCS#8 unencrypted, or PKCS#1).
    pub admin_private_key: Value,
    #[serde(default = "key_pair")]
    pub auth: Auth,
    #[serde(default)]
    pub managed_user: Option<String>,
    #[serde(default)]
    pub roles: Vec<String>,
}

/// Its key slots (or its one user, with passwords) take turns (turns.rs), named `login` in a
/// credential's handle.
const TURNS: crate::turns::Turns = crate::turns::Turns { table: "snowflake_accounts", kind: KIND, key: "login" };

/// A user's two key slots, in turn order.
const SLOTS: [&str; 2] = ["RSA_PUBLIC_KEY", "RSA_PUBLIC_KEY_2"];

/// Roles a lease never gets: an account's administration.
const ADMIN_ROLES: &[&str] = &["ACCOUNTADMIN", "ORGADMIN", "SECURITYADMIN", "GLOBALORGADMIN"];

/// How long past its lease a dynamic user lasts by `DAYS_TO_EXPIRY` (rounded up to whole days).
const SLACK_MS: i64 = 5 * 60_000;
const DAY_MS: i64 = 24 * 60 * 60_000;

/// The most of an answer read from Snowflake.
const ANSWER_MAX: usize = 1024 * 1024;

fn aad(project: &str, id: &str) -> String {
    format!("roll-secrets:snowflake:{project}:{id}:admin_key")
}

/// An identifier as Snowflake reads it unquoted (letters, digits, `_` and `$`, not starting with
/// a digit or `$`; 255 at most), upper-cased: what its quoted form names too.
pub(crate) fn identifier(s: &str) -> Option<String> {
    let b = s.as_bytes();
    let ok = !b.is_empty() && b.len() <= 255 && (b[0].is_ascii_alphabetic() || b[0] == b'_') && b.iter().all(|c| c.is_ascii_alphanumeric() || *c == b'_' || *c == b'$');
    ok.then(|| s.to_ascii_uppercase())
}

/// An identifier quoted for a statement, checked again here: nothing unchecked is ever put in SQL.
fn quoted(name: &str) -> Result<String> {
    match identifier(name) {
        Some(n) if n == name => Ok(format!("\"{n}\"")),
        _ => Err(Error::Internal(format!("{name:?} isn't a checked Snowflake identifier"))),
    }
}

/// An account identifier: letters, digits, `-`, `_` and `.`, dot-separated parts, 255 at most,
/// and not a host (no `snowflakecomputing`, no `privatelink`: that's its own flag).
pub(crate) fn check_account(a: &str) -> bool {
    let parts_ok = a.split('.').all(|p| !p.is_empty() && !p.starts_with('-') && !p.ends_with('-') && p.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_'));
    let lower = a.to_ascii_lowercase();
    !a.is_empty() && a.len() <= 255 && parts_ok && !lower.contains("snowflakecomputing") && !lower.split('.').any(|p| p == "privatelink")
}

/// The host statements go to.
fn host(account: &str, privatelink: bool) -> String {
    let account = account.to_ascii_lowercase();
    if privatelink { format!("{account}.privatelink.snowflakecomputing.com") } else { format!("{account}.snowflakecomputing.com") }
}

/// The account as a key-pair JWT names it: upper-case, without a locator's region (Snowflake's
/// own rule: up to the first `.`, or for a `.global` account up to the first `-`).
fn jwt_account(account: &str) -> String {
    let cut = if account.contains(".global") { account.find('-') } else { account.find('.') };
    account[..cut.unwrap_or(account.len())].to_ascii_uppercase()
}

/// An RSA private key from PEM: PKCS#8 (unencrypted) or PKCS#1, 2048 to 4096 bits.
fn parse_key(pem: &str) -> Result<rsa::RsaPrivateKey> {
    use rsa::pkcs1::DecodeRsaPrivateKey;
    use rsa::pkcs8::DecodePrivateKey;
    use rsa::traits::PublicKeyParts;
    let pem = pem.trim();
    if pem.contains("ENCRYPTED PRIVATE KEY") {
        return Err(Error::BadRequest("admin_private_key is encrypted: give roll the unencrypted PKCS#8 key (openssl pkcs8 -topk8 -nocrypt)".into()));
    }
    // The decoders' errors say nothing of the key, but they aren't passed on either.
    let key = rsa::RsaPrivateKey::from_pkcs8_pem(pem).or_else(|_| rsa::RsaPrivateKey::from_pkcs1_pem(pem)).map_err(|_| Error::BadRequest("admin_private_key is an RSA private key, PEM (PKCS#8 or PKCS#1)".into()))?;
    if !(2048..=4096).contains(&(key.size() * 8)) {
        return Err(Error::BadRequest("admin_private_key is an RSA key of 2048 to 4096 bits".into()));
    }
    Ok(key)
}

/// A public key as `ALTER USER … SET RSA_PUBLIC_KEY` takes it (base64 SPKI DER, no PEM lines),
/// and its fingerprint as Snowflake shows it (`SHA256:` and the base64 SHA-256 of that DER).
fn public_key(key: &rsa::RsaPrivateKey) -> Result<(String, String)> {
    use rsa::pkcs8::EncodePublicKey;
    use sha2::Digest;
    let der = key.to_public_key().to_public_key_der().map_err(|_| Error::Internal("encoding a public key".into()))?;
    let b64 = base64::engine::general_purpose::STANDARD;
    Ok((b64.encode(der.as_bytes()), format!("SHA256:{}", b64.encode(sha2::Sha256::digest(der.as_bytes())))))
}

/// A new key pair: its private half as PKCS#8 PEM, empty if making one failed (setting it then
/// fails: an issue never panics).
fn new_private_key() -> String {
    use rsa::pkcs8::EncodePrivateKey;
    rsa::RsaPrivateKey::new(&mut rand::thread_rng(), 2048).ok().and_then(|k| k.to_pkcs8_pem(rsa::pkcs8::LineEnding::LF).ok()).map(|p| p.to_string()).unwrap_or_default()
}

/// The public half of a key [`new_private_key`] made.
fn public_of(pem: &str) -> Result<String> {
    use rsa::pkcs8::DecodePrivateKey;
    let key = rsa::RsaPrivateKey::from_pkcs8_pem(pem).map_err(|_| Error::Link("making a key pair failed".into()))?;
    Ok(public_key(&key)?.0)
}

/// A password nobody chose, to Snowflake's default policy (upper- and lowercase letters, digits
/// and a symbol; none needs quoting in a SQL string).
fn password() -> String {
    crate::directories::password(crate::directories::Flavor::ActiveDirectory)
}

/// Text for a `COMMENT`: letters, digits and a few marks, nothing that ends a string.
fn comment(s: &str) -> String {
    s.chars().map(|c| if c.is_ascii_alphanumeric() || " _-.:#/@".contains(c) { c } else { '-' }).take(200).collect()
}

/// Whole days from now until `until` plus the slack, at least one: `DAYS_TO_EXPIRY`.
fn days(now: i64, until: i64) -> i64 {
    ((until + SLACK_MS - now) + DAY_MS - 1).div_euclid(DAY_MS).max(1)
}

/// A dynamic user's name: `ROLL_` and its lease's id.
fn user_for(lease: &str) -> Result<String> {
    identifier(&format!("roll_{lease}")).ok_or_else(|| Error::Internal(format!("lease id {lease:?}")))
}

/// One statement's rows, by lower-cased column name (`null` cells left out).
type Rows = Vec<BTreeMap<String, String>>;

/// Roll signed in as an account's admin user: statements through the SQL API.
struct Client<'a> {
    deliver: &'a dyn Deliver,
    host: String,
    account: String,
    user: String,
    role: Option<String>,
    /// The admin key, PKCS#8 DER.
    key: Zeroizing<Vec<u8>>,
    fp: String,
}

impl Client<'_> {
    /// A key-pair JWT for the admin user (Snowflake takes an hour at most; this lives ten minutes).
    fn jwt(&self) -> Result<Zeroizing<String>> {
        let pair = ring::signature::RsaKeyPair::from_pkcs8(&self.key).map_err(|_| Error::Internal("the admin key is damaged".into()))?;
        let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or_default();
        let who = format!("{}.{}", self.account, self.user);
        let b64 = |b: &[u8]| base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(b);
        let claims = json!({ "iss": format!("{who}.{}", self.fp), "sub": who, "iat": now.saturating_sub(60), "exp": now + 540 });
        let signed = format!("{}.{}", b64(json!({ "alg": "RS256", "typ": "JWT" }).to_string().as_bytes()), b64(claims.to_string().as_bytes()));
        let mut sig = vec![0u8; pair.public().modulus_len()];
        pair.sign(&ring::signature::RSA_PKCS1_SHA256, &ring::rand::SystemRandom::new(), signed.as_bytes(), &mut sig).map_err(|_| Error::Internal("signing failed".into()))?;
        Ok(Zeroizing::new(format!("{signed}.{}", b64(&sig))))
    }

    /// Run one statement. With `secret`, the statement holds one (a password): a refusal then
    /// says only Snowflake's code, never its message (which may quote the statement).
    fn exec(&self, statement: &str, secret: bool) -> Result<Rows> {
        let jwt = self.jwt()?;
        let auth = Zeroizing::new(format!("Bearer {}", jwt.as_str()));
        let headers = [("authorization", auth.as_str()), ("x-snowflake-authorization-token-type", "KEYPAIR_JWT"), ("accept", "application/json")];
        let mut body = json!({ "statement": statement, "timeout": 30 });
        if let Some(role) = &self.role {
            body["role"] = json!(role);
        }
        let url = format!("https://{}/api/v2/statements", self.host);
        let (mut status, mut answer) = self.deliver.request("POST", &url, &headers, Some(&body), ANSWER_MAX)?;
        // A statement still running (202) is asked after, a second apart, for up to 30 seconds.
        let mut polls = 0;
        while status == 202 && polls < 30 {
            let v: Json = serde_json::from_slice(&answer).unwrap_or_default();
            let next = v["statementStatusUrl"].as_str().filter(|p| p.starts_with("/api/v2/statements/") && p.len() < 256 && p.bytes().all(|b| b.is_ascii_alphanumeric() || b"/-_?=&".contains(&b)));
            let Some(next) = next else { break };
            std::thread::sleep(std::time::Duration::from_secs(1));
            (status, answer) = self.deliver.request("GET", &format!("https://{}{next}", self.host), &headers, None, ANSWER_MAX)?;
            polls += 1;
        }
        let v: Json = serde_json::from_slice(&answer).unwrap_or_default();
        if status != 200 {
            let code = v["code"].as_str().unwrap_or("?");
            let state = v["sqlState"].as_str().map(|s| format!(", SQLSTATE {s}")).unwrap_or_default();
            let message = v["message"].as_str().filter(|_| !secret).map(|m| format!(": {}", m.chars().take(300).collect::<String>())).unwrap_or_default();
            let what = match status {
                202 => "didn't finish in 30 seconds".to_string(),
                401 | 403 => format!("refused roll's sign-in as {} ({status}; is the key's public half the user's RSA_PUBLIC_KEY?)", self.user),
                _ => format!("answered {status}"),
            };
            return Err(Error::Link(format!("Snowflake {what} (code {code}{state}){message}")));
        }
        let columns: Vec<String> = v["resultSetMetaData"]["rowType"].as_array().into_iter().flatten().map(|c| c["name"].as_str().unwrap_or_default().to_ascii_lowercase()).collect();
        Ok(v["data"]
            .as_array()
            .into_iter()
            .flatten()
            .map(|row| columns.iter().zip(row.as_array().into_iter().flatten()).filter_map(|(c, cell)| cell.as_str().map(|s| (c.clone(), s.to_string()))).collect())
            .collect())
    }

    /// Which of `user`'s key slots hold a key.
    fn slots(&self, user: &str) -> Result<[bool; 2]> {
        let rows = self.exec(&format!("DESCRIBE USER {}", quoted(user)?), false)?;
        let set = |slot: &str| rows.iter().any(|r| r.get("property").is_some_and(|p| p.eq_ignore_ascii_case(&format!("{slot}_FP"))) && r.get("value").is_some_and(|v| !v.is_empty() && v != "null"));
        Ok([set(SLOTS[0]), set(SLOTS[1])])
    }

    fn role_exists(&self, role: &str) -> Result<bool> {
        // A checked identifier has no quote; `_` matches any one character, so the names are compared.
        let rows = self.exec(&format!("SHOW ROLES LIKE '{}'", identifier(role).filter(|r| r == role).ok_or_else(|| Error::Internal("unchecked role".into()))?), false)?;
        Ok(rows.iter().any(|r| r.get("name").is_some_and(|n| n == role)))
    }

    fn set_key(&self, user: &str, slot: &str, private_pem: &str) -> Result<()> {
        let slot = SLOTS.iter().find(|s| **s == slot).ok_or_else(|| Error::Internal(format!("slot {slot}")))?;
        self.exec(&format!("ALTER USER {} SET {slot} = '{}'", quoted(user)?, public_of(private_pem)?), false).map(|_| ())
    }

    fn unset_key(&self, user: &str, slot: &str) -> Result<()> {
        let slot = SLOTS.iter().find(|s| **s == slot).ok_or_else(|| Error::Internal(format!("slot {slot}")))?;
        self.exec(&format!("ALTER USER {} UNSET {slot}", quoted(user)?), false).map(|_| ())
    }

    fn set_password(&self, user: &str, pw: &str) -> Result<()> {
        if pw.contains(['\'', '\\']) {
            return Err(Error::Internal("a password that needs quoting".into()));
        }
        let statement = Zeroizing::new(format!("ALTER USER {} SET PASSWORD = '{pw}' MUST_CHANGE_PASSWORD = FALSE", quoted(user)?));
        self.exec(&statement, true).map(|_| ())
    }

    fn drop_user(&self, user: &str) -> Result<()> {
        self.exec(&format!("DROP USER IF EXISTS {}", quoted(user)?), false).map(|_| ())
    }
}

/// What the provider needs to reach one account.
struct Opened {
    account: String,
    host: String,
    admin_user: String,
    admin_role: Option<String>,
    admin_key: Zeroizing<Vec<u8>>,
    fp: String,
    auth: Auth,
    managed_user: Option<String>,
    roles: Vec<String>,
}

impl Opened {
    fn client<'a>(&self, deliver: &'a dyn Deliver) -> Client<'a> {
        Client { deliver, host: self.host.clone(), account: jwt_account(&self.account), user: self.admin_user.clone(), role: self.admin_role.clone(), key: self.admin_key.clone(), fp: self.fp.clone() }
    }

    /// The account as a client is given it (with `.privatelink` when that's how it's reached).
    fn account_field(&self) -> String {
        if self.host.contains(".privatelink.") { format!("{}.privatelink", self.account) } else { self.account.clone() }
    }

    fn logins(&self) -> Vec<String> {
        match (self.auth, &self.managed_user) {
            (Auth::KeyPair, _) => SLOTS.iter().map(|s| s.to_string()).collect(),
            (Auth::Password, Some(u)) => vec![u.clone()],
            (Auth::Password, None) => vec![],
        }
    }
}

const FIELDS: &[Field] = &[
    Field { field: "user", label: "User", modes: &[Mode::Managed, Mode::Dynamic], secret: false, suggest: "SNOWFLAKE_USER", credential: Some("login") },
    Field { field: "account", label: "Account", modes: &[Mode::Managed, Mode::Dynamic], secret: false, suggest: "SNOWFLAKE_ACCOUNT", credential: Some("login") },
    Field { field: "private_key", label: "Private key (PKCS#8 PEM)", modes: &[Mode::Managed, Mode::Dynamic], secret: true, suggest: "SNOWFLAKE_PRIVATE_KEY", credential: Some("login") },
    Field { field: "password", label: "Password", modes: &[Mode::Managed, Mode::Dynamic], secret: true, suggest: "SNOWFLAKE_PASSWORD", credential: Some("login") },
];

/// The credential's secret field for each way of signing in.
fn secret_field(auth: Auth) -> &'static str {
    match auth {
        Auth::KeyPair => "private_key",
        Auth::Password => "password",
    }
}

/// A dynamic link's roles: its `roles` option (a subset of the account's), else all of them.
fn link_roles(link: &Link, allowed: &[String]) -> Result<Vec<String>> {
    let Some(asked) = link.options.get("roles") else { return Ok(allowed.to_vec()) };
    let asked: Vec<String> = asked.as_array().map(|a| a.iter().filter_map(|r| r.as_str().and_then(identifier)).collect()).filter(|v: &Vec<String>| v.len() == asked.as_array().map_or(0, Vec::len) && !v.is_empty()).ok_or_else(|| Error::BadRequest("roles is a list of role names".into()))?;
    if let Some(r) = asked.iter().find(|r| !allowed.contains(r)) {
        return Err(Error::BadRequest(format!("{r} isn't one of the roles this Snowflake account gives dynamic users ({})", allowed.join(", "))));
    }
    Ok(asked)
}

pub(crate) struct Provider {
    pub db: Db,
    pub keys: Keyring,
    pub unwrap: crate::kms::Unwrapper,
    pub deliver: Arc<dyn Deliver>,
}

impl Provider {
    fn open(&self, id: &str) -> Result<Opened> {
        type Row = (String, String, String, String, Option<String>, Vec<u8>, String, String, Option<String>, String, String);
        let row: Option<Row> = self.db.read(|c| {
            Ok::<_, Error>(
                c.query_row(
                    "SELECT s.project_id, s.account, s.host, s.admin_user, s.admin_role, s.admin_key, s.admin_key_fp, s.auth, s.managed_user, s.roles, p.dek FROM snowflake_accounts s JOIN projects p ON p.project_id = s.project_id WHERE s.id = ?",
                    [id],
                    |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?, r.get(5)?, r.get(6)?, r.get(7)?, r.get(8)?, r.get(9)?, r.get(10)?)),
                )
                .optional()?,
            )
        })?;
        let (project, account, host, admin_user, admin_role, sealed, fp, auth, managed_user, roles, dek) = row.ok_or_else(|| Error::Link(format!("Snowflake account {id} doesn't exist any more")))?;
        let inner = self.unwrap.inner(&project, &dek)?;
        let (dek, _) = self.keys.open_dek(&project, &inner)?;
        let der = base64::engine::general_purpose::STANDARD.decode(dek.open(&aad(&project, id), &sealed)?.expose()).map_err(|_| Error::Internal("the admin key is damaged".into()))?;
        Ok(Opened { account, host, admin_user, admin_role, admin_key: Zeroizing::new(der), fp, auth: Auth::parse(&auth), managed_user, roles: serde_json::from_str(&roles).unwrap_or_default() })
    }

    /// A user of the lease's own with the link's roles; whatever a failed issue made is dropped.
    fn issue_dynamic(&self, r#ref: &str, link: &Link, label: &str, ttl_minutes: u64, lease: &str) -> Result<Issued> {
        let o = self.open(r#ref)?;
        let roles = link_roles(link, &o.roles)?;
        let Some(default_role) = roles.first() else {
            return Err(Error::BadRequest(format!("Snowflake account {ref} gives dynamic users no roles: register it with some", ref = r#ref)));
        };
        let client = o.client(self.deliver.as_ref());
        let user = user_for(lease)?;
        let now = self.db.now_ms()?;
        let days = days(now, now + ttl_minutes as i64 * 60_000);
        let note = comment(&format!("roll secrets lease {lease} ({label})"));
        let (secret, statement) = match o.auth {
            Auth::KeyPair => {
                let pem = new_private_key();
                let public = public_of(&pem)?;
                (Value::new(pem), format!("CREATE USER {} TYPE = SERVICE RSA_PUBLIC_KEY = '{public}' DEFAULT_ROLE = {} DAYS_TO_EXPIRY = {days} COMMENT = '{note}'", quoted(&user)?, quoted(default_role)?))
            }
            Auth::Password => {
                let pw = password();
                let s = format!("CREATE USER {} TYPE = LEGACY_SERVICE PASSWORD = '{pw}' MUST_CHANGE_PASSWORD = FALSE DEFAULT_ROLE = {} DAYS_TO_EXPIRY = {days} COMMENT = '{note}'", quoted(&user)?, quoted(default_role)?);
                (Value::new(pw), s)
            }
        };
        let statement = Zeroizing::new(statement);
        let made = client.exec(&statement, o.auth == Auth::Password).and_then(|_| roles.iter().try_for_each(|r| client.exec(&format!("GRANT ROLE {} TO USER {}", quoted(r)?, quoted(&user)?), false).map(|_| ())));
        if let Err(why) = made {
            let _ = client.drop_user(&user);
            return Err(Error::Link(format!("creating {user}: {why}")));
        }
        let parts = BTreeMap::from([("user".to_string(), Value::new(user.clone())), ("account".to_string(), Value::new(o.account_field())), (secret_field(o.auth).to_string(), secret)]);
        Ok(Issued { parts, handle: json!({ "user": user }) })
    }
}

impl LinkProvider for Provider {
    fn kind(&self) -> &str {
        KIND
    }

    fn fields(&self) -> &[Field] {
        FIELDS
    }

    /// Only the secret field the account's users sign in with.
    fn fields_for(&self, r#ref: &str) -> Vec<Field> {
        let auth = self.db.read(|c| c.query_row("SELECT auth FROM snowflake_accounts WHERE id = ?", [r#ref], |r| r.get::<_, String>(0)).optional()).ok().flatten().map(|a| Auth::parse(&a));
        let other = secret_field(if auth == Some(Auth::Password) { Auth::KeyPair } else { Auth::Password });
        FIELDS.iter().filter(|f| f.field != other).cloned().collect()
    }

    fn suggested(&self) -> &[(&'static str, Mode)] {
        &[("user", Mode::Managed), ("account", Mode::Managed), ("private_key", Mode::Managed)]
    }

    fn suggested_for(&self, r#ref: &str) -> Vec<(&'static str, Mode)> {
        self.fields_for(r#ref).iter().map(|f| (f.field, Mode::Managed)).collect()
    }

    fn name(&self, r#ref: &str) -> Option<String> {
        self.db.read(|c| c.query_row("SELECT name FROM snowflake_accounts WHERE id = ?", [r#ref], |r| r.get::<_, String>(0)).optional()).ok().flatten()
    }

    fn read(&self, _: &str, _: &str, _: &Link) -> Result<Value> {
        Err(Error::BadRequest("a Snowflake credential is managed or dynamic: roll keeps what it set".into()))
    }

    /// The field fits how the account's users sign in, a managed link has a user to rotate, and a
    /// dynamic one's roles are the account's.
    fn check(&self, link: &Link) -> Result<()> {
        let o = self.open(&link.r#ref)?;
        let wrong = secret_field(if o.auth == Auth::KeyPair { Auth::Password } else { Auth::KeyPair });
        if link.field == wrong {
            return Err(Error::BadRequest(format!("this Snowflake account's users sign in with {}: link {} instead", o.auth.as_str().replace('_', " "), secret_field(o.auth))));
        }
        match link.mode {
            Mode::Managed if o.managed_user.is_none() => Err(Error::BadRequest("this Snowflake account has no managed user: register it with one, or link it dynamic".into())),
            Mode::Managed if link.options.contains_key("roles") => Err(Error::BadRequest("roles are for dynamic links".into())),
            Mode::Dynamic if o.roles.is_empty() => Err(Error::BadRequest("this Snowflake account gives dynamic users no roles: register it with some".into())),
            Mode::Dynamic => link_roles(link, &o.roles).map(|_| ()),
            _ => Ok(()),
        }
    }

    /// Managed: the key slot not in use gets a new key and its private half is handed out (with
    /// passwords, the user's password is replaced). Dynamic: through [`LinkProvider::issue_lease`].
    fn issue(&self, r#ref: &str, _credential: &str, link: &Link, label: &str, ttl: Option<u64>) -> Result<Issued> {
        if link.mode == Mode::Dynamic {
            // Never reached through a resolve (issue_lease names the user after its lease).
            return self.issue_dynamic(r#ref, link, label, ttl.unwrap_or(60), &names::new_id("lse", 16));
        }
        let o = self.open(r#ref)?;
        let user = o.managed_user.clone().ok_or_else(|| Error::Link(format!("Snowflake account {ref} has no managed user", ref = r#ref)))?;
        let client = o.client(self.deliver.as_ref());
        let why = |e: Error| e.to_string();
        let (_, secret, handle) = match o.auth {
            Auth::KeyPair => TURNS.issue(&self.db, r#ref, &o.logins(), new_private_key, |slot, pem| client.set_key(&user, slot, pem).map_err(why))?,
            Auth::Password => TURNS.issue(&self.db, r#ref, &o.logins(), password, |u, pw| client.set_password(u, pw).map_err(why))?,
        };
        let parts = BTreeMap::from([("user".to_string(), Value::new(user)), ("account".to_string(), Value::new(o.account_field())), (secret_field(o.auth).to_string(), secret)]);
        Ok(Issued { parts, handle })
    }

    fn issue_lease(&self, r#ref: &str, credential: &str, link: &Link, label: &str, ttl_minutes: u64, lease: &str) -> Result<Issued> {
        if link.mode != Mode::Dynamic {
            return self.issue(r#ref, credential, link, label, Some(ttl_minutes));
        }
        self.issue_dynamic(r#ref, link, label, ttl_minutes, lease)
    }

    /// A dynamic user is dropped; after its grace a managed key slot is cleared (a password is
    /// replaced by one nobody knows), unless it was handed out again since.
    fn revoke(&self, r#ref: &str, handle: &Json) -> Result<()> {
        let o = self.open(r#ref)?;
        let client = o.client(self.deliver.as_ref());
        if let Some(user) = handle["user"].as_str() {
            let user = identifier(user).filter(|u| u == user && u.starts_with("ROLL_")).ok_or_else(|| Error::Internal(format!("lease handle names {user:?}")))?;
            return client.drop_user(&user).map_err(|why| Error::Link(format!("dropping {user}: {why}")));
        }
        let why = |e: Error| e.to_string();
        match (o.auth, &o.managed_user) {
            (Auth::KeyPair, Some(user)) => TURNS.retire(&self.db, r#ref, &o.logins(), handle, String::new, |slot, _| client.unset_key(user, slot).map_err(why)),
            (Auth::Password, Some(_)) => TURNS.retire(&self.db, r#ref, &o.logins(), handle, password, |u, pw| client.set_password(u, pw).map_err(why)),
            _ => Ok(()),
        }
    }

    /// A dynamic user's `DAYS_TO_EXPIRY` follows its renewed lease.
    fn renew(&self, r#ref: &str, handle: &Json, until: i64) -> Result<()> {
        let Some(user) = handle["user"].as_str() else { return Ok(()) };
        let o = self.open(r#ref)?;
        let days = days(self.db.now_ms()?, until);
        o.client(self.deliver.as_ref()).exec(&format!("ALTER USER {} SET DAYS_TO_EXPIRY = {days}", quoted(user)?), false).map(|_| ()).map_err(|why| Error::Link(format!("extending {user}: {why}")))
    }

    /// One account's managed credential at a time: two environments would hand out each other's
    /// key slots. (Dynamic users are each their own.)
    fn exclusive(&self) -> bool {
        true
    }
}

impl Secrets {
    pub fn snowflake_accounts(&self, project: &str, caller: &Caller) -> Result<Vec<SnowflakeAccount>> {
        caller.need(Action::Read, &Scope::project(project))?;
        self.project(project)?;
        self.db.read(|c| {
            Ok(c.query("SELECT id, name, account, host, admin_user, admin_role, admin_key_fp, auth, managed_user, roles, created_at, created_by FROM snowflake_accounts WHERE project_id = ? ORDER BY name", [project], |r| {
                Ok(SnowflakeAccount {
                    id: r.get(0)?,
                    name: r.get(1)?,
                    account: r.get(2)?,
                    host: r.get(3)?,
                    admin_user: r.get(4)?,
                    admin_role: r.get(5)?,
                    admin_key_fp: r.get(6)?,
                    auth: Auth::parse(&r.get::<_, String>(7)?),
                    managed_user: r.get(8)?,
                    roles: serde_json::from_str(&r.get::<_, String>(9)?).unwrap_or_default(),
                    created_at: r.get(10)?,
                    created_by: r.get(11)?,
                })
            })?)
        })
    }

    /// Register an account, after signing in as its admin user and finding the managed user and
    /// the roles.
    pub fn add_snowflake_account(&self, project: &str, n: NewSnowflakeAccount, caller: &Caller) -> Result<SnowflakeAccount> {
        caller.need(Action::Admin, &Scope::project(project))?;
        let deliver = self.deliver.clone().ok_or_else(|| Error::BadRequest("Snowflake accounts aren't on this server".into()))?;
        if n.name.is_empty() || n.name.len() > 64 || !n.name.chars().all(|c| c.is_ascii_alphanumeric() || "-_".contains(c)) {
            return Err(Error::BadRequest("an account's name is letters, digits, - and _ (64 at most)".into()));
        }
        let account = n.account.trim().to_string();
        if !check_account(&account) {
            return Err(Error::BadRequest("account is the account identifier (myorg-myaccount, or a locator such as xy12345.us-east-2.aws): letters, digits, -, _ and ., without .snowflakecomputing.com (set privatelink for PrivateLink)".into()));
        }
        let ident = |what: &str, s: &str| identifier(s.trim()).ok_or_else(|| Error::BadRequest(format!("{what} is a name Snowflake reads unquoted: letters, digits, _ and $, not starting with a digit ({s:?})")));
        let admin_user = ident("admin_user", &n.admin_user)?;
        let admin_role = n.admin_role.as_deref().map(str::trim).filter(|r| !r.is_empty()).map(|r| ident("admin_role", r)).transpose()?;
        let managed_user = n.managed_user.as_deref().map(str::trim).filter(|u| !u.is_empty()).map(|u| ident("managed_user", u)).transpose()?;
        let mut roles = Vec::new();
        for r in &n.roles {
            let r = ident("each role", r)?;
            if ADMIN_ROLES.contains(&r.as_str()) {
                return Err(Error::BadRequest(format!("{r} administers the account: a lease never gets it")));
            }
            if !roles.contains(&r) {
                roles.push(r);
            }
        }
        if roles.len() > 16 {
            return Err(Error::BadRequest("16 roles at most".into()));
        }
        if managed_user.is_none() && roles.is_empty() {
            return Err(Error::BadRequest("managed_user is the user managed links rotate, roles what dynamic users are granted; one or both".into()));
        }
        if managed_user.as_ref() == Some(&admin_user) {
            return Err(Error::BadRequest("the admin user isn't one roll rotates: rotating it would lock roll out".into()));
        }
        if n.admin_private_key.expose().len() > 16 * 1024 {
            return Err(Error::BadRequest("admin_private_key is an RSA private key, PEM".into()));
        }
        let key = parse_key(n.admin_private_key.expose())?;
        let (_, fp) = public_key(&key)?;
        let der = {
            use rsa::pkcs8::EncodePrivateKey;
            key.to_pkcs8_der().map_err(|_| Error::Internal("encoding the admin key".into()))?
        };
        drop(key);
        let host = host(&account, n.privatelink);
        // Signing in proves the key; the managed user and the roles must exist (outside any
        // transaction).
        let client = Client { deliver: deliver.as_ref(), host: host.clone(), account: jwt_account(&account), user: admin_user.clone(), role: admin_role.clone(), key: Zeroizing::new(der.as_bytes().to_vec()), fp: fp.clone() };
        let refused = |e: Error| Error::BadRequest(format!("roll couldn't use the account: {e}"));
        let mut first = 0;
        if let Some(user) = &managed_user {
            let slots = client.slots(user).map_err(refused)?;
            // A key set by hand in the first slot keeps working until the second rotation.
            if n.auth == Auth::KeyPair && slots == [true, false] {
                first = 1;
            }
        }
        for r in &roles {
            if !client.role_exists(r).map_err(refused)? {
                return Err(Error::BadRequest(format!("roll couldn't use the account: no role {r} (or the admin role can't see it)")));
            }
        }
        let dek = self.project(project)?;
        let id = names::new_id("sfa", 16);
        let sealed = dek.seal(&aad(project, &id), &Value::new(base64::engine::general_purpose::STANDARD.encode(der.as_bytes())));
        let now = self.db.now_ms()?;
        let a = SnowflakeAccount { id: id.clone(), name: n.name, account, host, admin_user, admin_role, admin_key_fp: fp, auth: n.auth, managed_user, roles, created_at: now, created_by: caller.principal.clone() };
        self.db.write(|c| {
            if c.query_row("SELECT 1 FROM snowflake_accounts WHERE project_id = ? AND name = ?", [project, &a.name], |_| Ok(())).optional()?.is_some() {
                return Err(Error::Conflict("exists", format!("a Snowflake account named {} exists", a.name)));
            }
            // Two registrations rotating one user would undo each other's keys.
            if let Some(user) = &a.managed_user {
                let other: Option<String> = c.query_row("SELECT name FROM snowflake_accounts WHERE project_id = ? AND host = ? AND managed_user = ?", [project, &a.host, user], |r| r.get(0)).optional()?;
                if let Some(other) = other {
                    return Err(Error::Conflict("exists", format!("{other} already rotates {user} at {}", a.host)));
                }
            }
            c.execute(
                "INSERT INTO snowflake_accounts (id, project_id, name, account, host, admin_user, admin_role, admin_key, admin_key_fp, auth, managed_user, roles, state, created_at, created_by) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
                params![
                    id,
                    project,
                    a.name,
                    a.account,
                    a.host,
                    a.admin_user,
                    a.admin_role,
                    sealed,
                    a.admin_key_fp,
                    a.auth.as_str(),
                    a.managed_user,
                    serde_json::to_string(&a.roles).expect("roles"),
                    crate::turns::Turns::initial_at(first),
                    now,
                    a.created_by
                ],
            )?;
            audit::record(c, project, caller, "snowflake.add", None, None, &[], &format!("added Snowflake account {} ({}, {})", a.name, a.host, a.auth.as_str()), now)
        })?;
        Ok(a)
    }

    /// Delete an account no key links to and no lease still needs.
    pub fn delete_snowflake_account(&self, project: &str, id: &str, caller: &Caller) -> Result<()> {
        caller.need(Action::Admin, &Scope::project(project))?;
        self.project(project)?;
        let now = self.db.now_ms()?;
        let like = (format!("%\"kind\":\"{KIND}\"%"), format!("%\"ref\":\"{id}\"%"));
        self.db.write(|c| {
            let name: String = c.query_row("SELECT name FROM snowflake_accounts WHERE id = ? AND project_id = ?", [id, project], |r| r.get(0)).optional()?.ok_or_else(|| Error::NotFound(format!("no Snowflake account {id}")))?;
            let linked: Vec<String> = c.query("SELECT key FROM secrets WHERE project_id = ? AND link LIKE ? AND link LIKE ? ORDER BY key LIMIT 5", params![project, like.0, like.1], |r| r.get(0))?;
            if !linked.is_empty() {
                return Err(Error::Conflict("in_use", format!("{name} still issues {}: unlink them first", linked.join(", "))));
            }
            let waiting: i64 = c.query_row("SELECT COUNT(*) FROM leases WHERE project_id = ? AND revoked_at IS NULL AND link LIKE ? AND link LIKE ?", params![project, like.0, like.1], |r| r.get(0))?;
            if waiting > 0 {
                return Err(Error::Conflict("in_use", format!("{name} has {waiting} key(s) or user(s) still to retire (a grace period, or a dynamic lease): delete it once they're done")));
            }
            c.execute("DELETE FROM snowflake_accounts WHERE id = ?", [id])?;
            audit::record(c, project, caller, "snowflake.delete", None, None, &[], &format!("deleted Snowflake account {name}"), now)
        })
    }

    pub(crate) fn snowflake_refs(&self, project: &str) -> Result<Vec<(String, String)>> {
        self.db.read(|c| Ok(c.query("SELECT id, name FROM snowflake_accounts WHERE project_id = ? ORDER BY name", [project], |r| Ok((r.get(0)?, r.get(1)?)))?))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accounts_and_identifiers_are_checked_strictly() {
        for ok in ["myorg-myaccount", "xy12345.us-east-2.aws", "my_org-acct"] {
            assert!(check_account(ok), "{ok}");
        }
        for bad in ["", "evil.com/x", "a..b", "acct.snowflakecomputing.com", "acct.privatelink", "a b", "a@b", ".a", "a-", "x\ny"] {
            assert!(!check_account(bad), "{bad}");
        }
        assert_eq!(host("MyOrg-Acct", true), "myorg-acct.privatelink.snowflakecomputing.com");
        assert_eq!((jwt_account("xy12345.us-east-2.aws"), jwt_account("myorg-acct")), ("XY12345".into(), "MYORG-ACCT".into()));
        assert_eq!(identifier("svc_app$1").as_deref(), Some("SVC_APP$1"));
        for bad in ["", "1abc", "$x", "a\"b", "a b", "a;DROP", "é"] {
            assert!(identifier(bad).is_none(), "{bad}");
        }
        assert!(quoted("lower").is_err(), "only checked, upper-cased names are quoted");
        assert_eq!(user_for("lse_abc234").unwrap(), "ROLL_LSE_ABC234");
    }

    #[test]
    fn expiry_rounds_up_to_whole_days() {
        assert_eq!(days(0, 15 * 60_000), 1);
        assert_eq!(days(0, DAY_MS - SLACK_MS), 1);
        assert_eq!(days(0, DAY_MS), 2);
    }
}
