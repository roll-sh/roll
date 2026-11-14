//! roll's own accounts and sign-in (docs/accounts.md): invites, one-time codes by email, GitHub
//! and Google, TOTP with recovery codes, and the sessions rolld's gate resolves on every request.
//!
//! - Every piece of state is a row in one `code_sql` database (SQLite under `--home`, or the
//!   system database's `accounts` schema shared by every node), and every read-then-write is one
//!   [`Db::write`] closure: nodes need no leases, and nothing that matters lives on a node.
//! - Tokens and codes are stored only as hashes (email codes as argon2id, since six digits are
//!   easy to guess offline); TOTP seeds are sealed with `ROLL_SECRET_KEY`. None is ever logged.
//! - [`api::router`] serves `/v1/accounts`. [`Accounts::sessions`] is what rolld's gate
//!   (`roll_auth::check`) resolves session cookies with.

pub mod api;
mod mail;
mod oauth;
mod twofactor;

use code_sql::{Db, OptionalExtension, Target, params};
use serde::Serialize;
use sha2::{Digest, Sha256};
use std::sync::Arc;

pub use api::router;
pub use mail::{Mail, MailConfig, Mailer};
pub use oauth::{OAuthApp, Provider, ProviderConfig};

const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS users (
    id TEXT PRIMARY KEY,
    email TEXT NOT NULL UNIQUE,
    handle TEXT NOT NULL UNIQUE,
    name TEXT NOT NULL,
    admin INTEGER NOT NULL,
    disabled INTEGER NOT NULL,
    code_owner_id TEXT,
    invited_by TEXT NOT NULL,
    created_at INTEGER NOT NULL
) WITHOUT ROWID;
CREATE TABLE IF NOT EXISTS identities (
    provider TEXT NOT NULL,
    subject TEXT NOT NULL,
    user_id TEXT NOT NULL,
    email TEXT NOT NULL,
    login TEXT NOT NULL,
    created_at INTEGER NOT NULL,
    PRIMARY KEY (provider, subject)
) WITHOUT ROWID;
CREATE INDEX IF NOT EXISTS identities_user ON identities (user_id);
CREATE TABLE IF NOT EXISTS invites (
    id TEXT PRIMARY KEY,
    token_hash TEXT NOT NULL UNIQUE,
    email TEXT NOT NULL,
    admin INTEGER NOT NULL,
    handle TEXT,
    created_by TEXT NOT NULL,
    created_at INTEGER NOT NULL,
    expires_at INTEGER NOT NULL,
    used_at INTEGER,
    used_by TEXT
) WITHOUT ROWID;
CREATE TABLE IF NOT EXISTS sessions (
    token_hash TEXT PRIMARY KEY,
    id TEXT NOT NULL UNIQUE,
    user_id TEXT NOT NULL,
    level TEXT NOT NULL,
    created_at INTEGER NOT NULL,
    seen_at INTEGER NOT NULL,
    expires_at INTEGER NOT NULL,
    user_agent TEXT NOT NULL
) WITHOUT ROWID;
CREATE INDEX IF NOT EXISTS sessions_user ON sessions (user_id);
CREATE TABLE IF NOT EXISTS email_codes (
    id TEXT PRIMARY KEY,
    email TEXT NOT NULL,
    user_id TEXT,
    invite_id TEXT,
    code_hash TEXT NOT NULL,
    attempts INTEGER NOT NULL,
    created_at INTEGER NOT NULL,
    expires_at INTEGER NOT NULL
) WITHOUT ROWID;
CREATE TABLE IF NOT EXISTS oauth_states (
    state_hash TEXT PRIMARY KEY,
    provider TEXT NOT NULL,
    intent TEXT NOT NULL,
    user_id TEXT,
    invite_id TEXT,
    verifier TEXT NOT NULL,
    nonce TEXT NOT NULL,
    browser_hash TEXT NOT NULL,
    return_to TEXT NOT NULL,
    created_at INTEGER NOT NULL,
    expires_at INTEGER NOT NULL
) WITHOUT ROWID;
CREATE TABLE IF NOT EXISTS totp (
    user_id TEXT PRIMARY KEY,
    seed TEXT NOT NULL,
    enabled INTEGER NOT NULL,
    last_step INTEGER NOT NULL,
    created_at INTEGER NOT NULL
) WITHOUT ROWID;
CREATE TABLE IF NOT EXISTS recovery_codes (
    user_id TEXT NOT NULL,
    code_hash TEXT NOT NULL,
    PRIMARY KEY (user_id, code_hash)
) WITHOUT ROWID;
CREATE TABLE IF NOT EXISTS passkeys (
    id TEXT PRIMARY KEY,
    user_id TEXT NOT NULL,
    credential_id TEXT NOT NULL UNIQUE,
    public_key TEXT NOT NULL,
    sign_count INTEGER NOT NULL,
    name TEXT NOT NULL,
    created_at INTEGER NOT NULL,
    last_used_at INTEGER
) WITHOUT ROWID;
CREATE TABLE IF NOT EXISTS throttle (
    key TEXT PRIMARY KEY,
    n INTEGER NOT NULL,
    since INTEGER NOT NULL
) WITHOUT ROWID;
";

const MINUTE: i64 = 60_000;
const DAY: i64 = 24 * 60 * MINUTE;
/// A full session's absolute lifetime.
pub const SESSION_TTL_MS: i64 = 30 * DAY;
/// A full session ends after this long unused.
pub const SESSION_IDLE_MS: i64 = 7 * DAY;
/// A session that owes its second factor.
pub const SECOND_FACTOR_TTL_MS: i64 = 10 * MINUTE;
/// `seen_at` is written at most this often.
const SEEN_EVERY_MS: i64 = 5 * MINUTE;
pub const INVITE_TTL_MS: i64 = 7 * DAY;
pub const CODE_TTL_MS: i64 = 10 * MINUTE;
/// Codes mailed to one address per hour.
const SENDS_PER_HOUR: i64 = 5;
/// Wrong email codes one user (or invite) gets per 15 minutes, across codes.
const CODE_FAILS: i64 = 10;
/// Tries one email code gets.
const CODE_TRIES: i64 = 5;

pub type Result<T, E = Error> = std::result::Result<T, E>;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// A 400, with the `code` the dashboard branches on.
    #[error("{0}")]
    BadRequest(String, Option<&'static str>),
    #[error("{0}")]
    Unauthorized(String),
    #[error("{0}")]
    Forbidden(String, &'static str),
    #[error("{0}")]
    NotFound(String),
    #[error("{0}")]
    Conflict(String),
    #[error("{0}")]
    Unavailable(String, &'static str),
    #[error("accounts database: {0}")]
    Db(#[from] code_sql::Error),
    #[error("{0}")]
    Internal(String),
}

impl Error {
    fn bad_code() -> Error {
        Error::BadRequest("the code is wrong or expired".into(), Some("bad_code"))
    }

    fn signed_out() -> Error {
        Error::Unauthorized("sign in first".into())
    }

    /// The `code` a sign-in redirect carries in `?error=`.
    pub(crate) fn redirect_code(&self) -> &'static str {
        match self {
            Error::BadRequest(_, Some(c)) | Error::Forbidden(_, c) => c,
            _ => "provider",
        }
    }
}

/// A secret from the environment. `Debug` never shows it.
#[derive(Clone)]
pub struct Secret(Arc<str>);

impl Secret {
    pub fn new(s: &str) -> Secret {
        Secret(s.into())
    }

    pub fn expose(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Debug for Secret {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Secret(…)")
    }
}

/// roll code's owners, from rolld: every user gets a user owner named after their handle.
pub trait Owners: Send + Sync + 'static {
    /// Whether `handle` could be a new owner's name (valid, not reserved, not taken).
    fn available(&self, handle: &str) -> Result<bool, String>;
    /// A new user owner: its id, or `None` if the name was taken meanwhile.
    fn create(&self, handle: &str, name: &str) -> Result<Option<String>, String>;
    /// Cut owner `owner_id` off (its user was disabled): its tokens and machine links stop now.
    fn suspend(&self, _owner_id: &str) -> Result<(), String> {
        Ok(())
    }
}

#[derive(Clone, Debug)]
pub struct Config {
    /// Where browsers reach the API (`https://api.roll.sh`): OAuth callbacks, and whether the
    /// cookies are `Secure` (`https://`).
    pub api_url: String,
    /// The dashboard (`https://roll.sh`): invite links and where sign-ins land.
    pub web_url: String,
    pub mail: MailConfig,
    pub github: Option<ProviderConfig>,
    pub google: Option<ProviderConfig>,
    /// `ROLL_SECRET_KEY`: seals TOTP seeds. Without it TOTP can't be turned on.
    pub secret_key: Option<Secret>,
    /// The gate checks nothing (a daemon on loopback without `ROLL_ADMIN_TOKEN`): anyone may use
    /// the admin routes, as every other route.
    pub open: bool,
}

impl Config {
    /// The settings from the environment (docs/accounts.md, "Settings").
    pub fn from_env(api_url: &str, web_url: &str, open: bool) -> Config {
        let var = |k: &str| std::env::var(k).ok().map(|v| v.trim().to_string()).filter(|v| !v.is_empty());
        let app = |id: &str, secret: &str| Some(OAuthApp { client_id: var(id)?, client_secret: Secret::new(&var(secret)?) });
        let mail = match (var("ROLL_ACCOUNTS_OUTBOX"), var("ROLL_ACCOUNTS_MAIL_FROM")) {
            (Some(dir), _) => MailConfig::Outbox(dir.into()),
            (None, Some(from)) => MailConfig::Ses { from },
            (None, None) => MailConfig::Off,
        };
        Config {
            api_url: api_url.trim_end_matches('/').to_string(),
            web_url: web_url.trim_end_matches('/').to_string(),
            mail,
            github: app("ROLL_ACCOUNTS_GITHUB_CLIENT_ID", "ROLL_ACCOUNTS_GITHUB_CLIENT_SECRET").map(|a| ProviderConfig::new(Provider::Github, a)),
            google: app("ROLL_ACCOUNTS_GOOGLE_CLIENT_ID", "ROLL_ACCOUNTS_GOOGLE_CLIENT_SECRET").map(|a| ProviderConfig::new(Provider::Google, a)),
            secret_key: var("ROLL_SECRET_KEY").map(|k| Secret::new(&k)),
            open,
        }
    }

    fn secure(&self) -> bool {
        self.api_url.starts_with("https://")
    }

    /// The session cookie's name: `__Host-` (no `Domain`, `Secure`, `Path=/`) over HTTPS.
    pub fn session_cookie(&self) -> &'static str {
        if self.secure() { "__Host-roll_session" } else { "roll_session" }
    }

    pub(crate) fn oauth_cookie(&self) -> &'static str {
        if self.secure() { "__Host-roll_oauth" } else { "roll_oauth" }
    }

    pub(crate) fn provider(&self, p: Provider) -> Option<&ProviderConfig> {
        match p {
            Provider::Github => self.github.as_ref(),
            Provider::Google => self.google.as_ref(),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct Identity {
    pub provider: String,
    pub login: String,
    pub email: String,
}

#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct User {
    pub id: String,
    pub email: String,
    pub handle: String,
    pub name: String,
    pub admin: bool,
    pub disabled: bool,
    pub created_at: i64,
    pub totp: bool,
    pub identities: Vec<Identity>,
}

#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct SessionInfo {
    pub id: String,
    pub created_at: i64,
    pub seen_at: i64,
    pub expires_at: i64,
    pub user_agent: String,
    pub current: bool,
}

#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct Invite {
    pub id: String,
    pub email: String,
    pub admin: bool,
    pub handle: Option<String>,
    pub created_by: String,
    pub created_at: i64,
    pub expires_at: i64,
    pub used_at: Option<i64>,
    pub used_by: Option<String>,
}

/// A session just issued: its cookie's token (the only copy) and whether it owes a second
/// factor. `Debug` never shows the token.
pub struct Issued {
    pub token: String,
    pub id: String,
    pub second_factor: bool,
}

impl std::fmt::Debug for Issued {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Issued").field("id", &self.id).field("second_factor", &self.second_factor).finish_non_exhaustive()
    }
}

/// A new account: the invite it uses and what we know of the person.
pub(crate) struct NewUser<'a> {
    pub invite_id: &'a str,
    pub email: &'a str,
    /// A handle to try first (a GitHub login).
    pub hint: Option<&'a str>,
    pub name: Option<&'a str>,
    /// `(provider, subject, login)` to link in the same write.
    pub identity: Option<(&'a str, &'a str, &'a str)>,
}

pub struct Accounts {
    db: Db,
    config: Config,
    mailer: Mailer,
    http: reqwest::Client,
    owners: Option<Arc<dyn Owners>>,
}

impl Accounts {
    /// Open `target` and create the tables if they're missing (every node may do this at once).
    pub async fn open(target: &Target, pg: code_sql::PgOptions, config: Config, owners: Option<Arc<dyn Owners>>) -> anyhow::Result<Accounts> {
        let db = match target {
            Target::Sqlite(path) => Db::open_sqlite(path, code_sql::SqliteOptions { readers: 2, cache_kib: 2 << 10 })?,
            Target::Postgres { url, schema } => Db::connect(url, schema, pg)?,
        };
        Accounts::new(db, config, owners).await
    }

    pub async fn new(db: Db, config: Config, owners: Option<Arc<dyn Owners>>) -> anyhow::Result<Accounts> {
        db.ensure_schema(SCHEMA)?;
        let mailer = Mailer::new(&config.mail).await?;
        let http = reqwest::Client::builder().redirect(reqwest::redirect::Policy::none()).timeout(std::time::Duration::from_secs(10)).user_agent("roll").build()?;
        Ok(Accounts { db, config, mailer, http, owners })
    }

    pub fn config(&self) -> &Config {
        &self.config
    }

    /// What rolld's gate resolves session cookies with.
    pub fn sessions(self: &Arc<Self>) -> Arc<dyn roll_auth::Sessions> {
        Arc::new(Resolver(self.clone()))
    }

    // ── sessions ──

    /// The session behind a cookie's token: one read joined with its user, and a write of
    /// `seen_at` at most every few minutes. Nothing is cached.
    pub fn resolve(&self, token: &str) -> Result<Option<roll_auth::Session>> {
        if !token.starts_with("rls_") || token.len() > 100 {
            return Ok(None);
        }
        let hash = sha256_hex(token);
        let now = self.db.now_ms()?;
        let row = self.db.read(|c| {
            c.query_row(
                "SELECT s.id, s.user_id, s.level, s.seen_at, s.expires_at, u.handle, u.admin, u.disabled, u.code_owner_id
                 FROM sessions s JOIN users u ON u.id = s.user_id WHERE s.token_hash = ?",
                [hash.as_str()],
                |r| {
                    Ok((
                        (r.get::<_, String>(0)?, r.get::<_, String>(1)?, r.get::<_, String>(2)?),
                        (r.get::<_, i64>(3)?, r.get::<_, i64>(4)?),
                        (r.get::<_, String>(5)?, r.get::<_, bool>(6)?, r.get::<_, bool>(7)?, r.get::<_, Option<String>>(8)?),
                    ))
                },
            )
            .optional()
        })?;
        let Some(((id, user, level), (seen_at, expires_at), (handle, admin, disabled, code_owner))) = row else { return Ok(None) };
        let full = level == "full";
        if disabled || expires_at <= now || (full && seen_at + SESSION_IDLE_MS <= now) {
            return Ok(None);
        }
        if !full {
            return Ok(Some(roll_auth::Session::SecondFactor(roll_auth::SecondFactorOwed { user, session: id })));
        }
        if now - seen_at >= SEEN_EVERY_MS {
            self.db.write(|c| c.execute("UPDATE sessions SET seen_at = ? WHERE token_hash = ?", params![now, hash]))?;
        }
        Ok(Some(roll_auth::Session::Full(roll_auth::SignedIn { user, handle, admin, code_owner, session: id })))
    }

    /// A user as they are now, for a token of theirs (roll-access): `None` once disabled or gone.
    pub fn person(&self, user: &str) -> Result<Option<roll_auth::SignedIn>> {
        let row = self.db.read(|c| {
            c.query_row("SELECT handle, admin, disabled, code_owner_id FROM users WHERE id = ?", [user], |r| {
                Ok((r.get::<_, String>(0)?, r.get::<_, bool>(1)?, r.get::<_, bool>(2)?, r.get::<_, Option<String>>(3)?))
            })
            .optional()
        })?;
        Ok(row.filter(|(_, _, disabled, _)| !disabled).map(|(handle, admin, _, code_owner)| roll_auth::SignedIn { user: user.to_string(), handle, admin, code_owner, session: String::new() }))
    }

    /// A new session for `user`: full, or owing a second factor when they have one on.
    pub fn issue(&self, user: &str, user_agent: &str) -> Result<Issued> {
        self.provision_owner(user);
        let now = self.db.now_ms()?;
        let token = new_token("rls");
        let id = new_id("ses");
        let second_factor = self.db.write(|c| {
            let disabled = c.query_row("SELECT disabled FROM users WHERE id = ?", [user], |r| r.get::<_, bool>(0)).optional()?;
            match disabled {
                None => return Err(Error::NotFound("no such user".into())),
                Some(true) => return Err(Error::Forbidden("this account is disabled".into(), "disabled")),
                Some(false) => {}
            }
            let mfa = c.query_row("SELECT 1 FROM totp WHERE user_id = ? AND enabled = 1", [user], |_| Ok(())).optional()?.is_some();
            let (level, ttl) = if mfa { ("mfa", SECOND_FACTOR_TTL_MS) } else { ("full", SESSION_TTL_MS) };
            // The user's sessions that ended go with each new one: no sweeper needed.
            c.execute("DELETE FROM sessions WHERE user_id = ? AND (expires_at <= ? OR (level = 'full' AND seen_at <= ?))", params![user, now, now - SESSION_IDLE_MS])?;
            c.execute(
                "INSERT INTO sessions (token_hash, id, user_id, level, created_at, seen_at, expires_at, user_agent) VALUES (?, ?, ?, ?, ?, ?, ?, ?)",
                params![sha256_hex(&token), id, user, level, now, now, now + ttl, user_agent],
            )?;
            Ok(mfa)
        })?;
        Ok(Issued { token, id, second_factor })
    }

    /// The second factor passed: the owing session is replaced by a new, full one.
    pub(crate) fn complete(&self, owed: &roll_auth::SecondFactorOwed, user_agent: &str) -> Result<Issued> {
        let now = self.db.now_ms()?;
        let token = new_token("rls");
        let id = new_id("ses");
        self.db.write(|c| {
            let n = c.execute("DELETE FROM sessions WHERE id = ? AND user_id = ? AND level = 'mfa'", [owed.session.as_str(), owed.user.as_str()])?;
            if n == 0 {
                return Err(Error::signed_out());
            }
            c.execute(
                "INSERT INTO sessions (token_hash, id, user_id, level, created_at, seen_at, expires_at, user_agent) VALUES (?, ?, ?, 'full', ?, ?, ?, ?)",
                params![sha256_hex(&token), id, owed.user, now, now, now + SESSION_TTL_MS, user_agent],
            )?;
            Ok(())
        })?;
        Ok(Issued { token, id, second_factor: false })
    }

    pub fn sign_out(&self, session: &str) -> Result<()> {
        self.db.write(|c| c.execute("DELETE FROM sessions WHERE id = ?", [session]))?;
        Ok(())
    }

    pub fn sessions_of(&self, user: &str, current: &str) -> Result<Vec<SessionInfo>> {
        let now = self.db.now_ms()?;
        Ok(self.db.read(|c| {
            c.query(
                "SELECT id, created_at, seen_at, expires_at, user_agent FROM sessions WHERE user_id = ? AND level = 'full' AND expires_at > ? ORDER BY created_at DESC, id",
                params![user, now],
                |r| {
                    let id: String = r.get(0)?;
                    Ok(SessionInfo { current: id == current, id, created_at: r.get(1)?, seen_at: r.get(2)?, expires_at: r.get(3)?, user_agent: r.get(4)? })
                },
            )
        })?)
    }

    pub(crate) fn session_info(&self, id: &str) -> Result<SessionInfo> {
        let row = self.db.read(|c| {
            c.query_row("SELECT id, created_at, seen_at, expires_at, user_agent FROM sessions WHERE id = ?", [id], |r| {
                Ok(SessionInfo { id: r.get(0)?, created_at: r.get(1)?, seen_at: r.get(2)?, expires_at: r.get(3)?, user_agent: r.get(4)?, current: true })
            })
            .optional()
        })?;
        row.ok_or_else(Error::signed_out)
    }

    pub fn revoke_session(&self, user: &str, id: &str) -> Result<()> {
        match self.db.write(|c| c.execute("DELETE FROM sessions WHERE id = ? AND user_id = ?", [id, user]))? {
            0 => Err(Error::NotFound(format!("no session {id}"))),
            _ => Ok(()),
        }
    }

    // ── users ──

    pub fn user(&self, id: &str) -> Result<User> {
        self.db.read(|c| load_user(c, id))?.ok_or_else(|| Error::NotFound(format!("no user {id}")))
    }

    pub fn users(&self) -> Result<Vec<User>> {
        let ids = self.db.read(|c| c.query("SELECT id FROM users ORDER BY created_at, id", (), |r| r.get::<_, String>(0)))?;
        ids.iter().map(|id| self.user(id)).collect()
    }

    pub fn rename(&self, user: &str, name: &str) -> Result<User> {
        let name = name.trim();
        if !(1..=100).contains(&name.chars().count()) {
            return Err(Error::BadRequest("a name is 1–100 characters".into(), None));
        }
        self.db.write(|c| c.execute("UPDATE users SET name = ? WHERE id = ?", [name, user]))?;
        self.user(user)
    }

    /// An admin's change to a user. Disabling one signs them out everywhere, in the same write.
    pub fn update_user(&self, id: &str, admin: Option<bool>, disabled: Option<bool>) -> Result<User> {
        self.db.write(|c| {
            if c.query_row("SELECT 1 FROM users WHERE id = ?", [id], |_| Ok(())).optional()?.is_none() {
                return Err(Error::NotFound(format!("no user {id}")));
            }
            if let Some(a) = admin {
                c.execute("UPDATE users SET admin = ? WHERE id = ?", params![a, id])?;
            }
            if let Some(d) = disabled {
                c.execute("UPDATE users SET disabled = ? WHERE id = ?", params![d, id])?;
                if d {
                    c.execute("DELETE FROM sessions WHERE user_id = ?", [id])?;
                }
            }
            Ok(())
        })?;
        // A session isn't the only way in: the roll code owner's own tokens and machine links
        // go too (another database, so after the write; an admin who sees the error retries).
        if disabled == Some(true)
            && let Some(owners) = &self.owners
            && let Some(owner) = self.db.read(|c| c.query_row("SELECT code_owner_id FROM users WHERE id = ?", [id], |r| r.get::<_, Option<String>>(0)))?
        {
            owners.suspend(&owner).map_err(|e| Error::Internal(format!("the account is disabled, but cutting off its roll code owner failed: {e}")))?;
        }
        self.user(id)
    }

    pub fn revoke_sessions(&self, user: &str) -> Result<()> {
        self.db.write(|c| {
            if c.query_row("SELECT 1 FROM users WHERE id = ?", [user], |_| Ok(())).optional()?.is_none() {
                return Err(Error::NotFound(format!("no user {user}")));
            }
            c.execute("DELETE FROM sessions WHERE user_id = ?", [user])?;
            Ok(())
        })
    }

    pub fn unlink(&self, user: &str, provider: &str) -> Result<()> {
        match self.db.write(|c| c.execute("DELETE FROM identities WHERE user_id = ? AND provider = ?", [user, provider]))? {
            0 => Err(Error::NotFound(format!("no {provider} identity is linked"))),
            _ => Ok(()),
        }
    }

    /// A user owner in roll code for `user`, if they don't have one yet and roll code runs here.
    /// Failures are logged: the next sign-in tries again.
    fn provision_owner(&self, user: &str) {
        let Some(owners) = &self.owners else { return };
        let row = self.db.read(|c| {
            c.query_row("SELECT handle, name, code_owner_id FROM users WHERE id = ?", [user], |r| {
                Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?, r.get::<_, Option<String>>(2)?))
            })
            .optional()
        });
        let (handle, name) = match row {
            Ok(Some((h, n, None))) => (h, n),
            Ok(_) => return,
            Err(e) => return tracing::warn!(user, error = %e, "accounts: reading a user for their roll code owner"),
        };
        match owners.create(&handle, &name) {
            Ok(Some(owner)) => {
                if let Err(e) = self.db.write(|c| c.execute("UPDATE users SET code_owner_id = ? WHERE id = ? AND code_owner_id IS NULL", [owner.as_str(), user])) {
                    tracing::warn!(user, error = %e, "accounts: recording the roll code owner");
                }
            }
            Ok(None) => tracing::warn!(user, handle, "accounts: the user's handle is taken in roll code; no owner made"),
            Err(e) => tracing::warn!(user, error = %e, "accounts: making the user's roll code owner"),
        }
    }

    /// The first free handle from `hint` (else the email's local part): an owner name, unique
    /// among users and roll code's owners.
    fn pick_handle(&self, hint: Option<&str>, email: &str) -> Result<String> {
        let base = hint.map(slug).filter(|s| !s.is_empty()).unwrap_or_else(|| slug(email.split('@').next().unwrap_or_default()));
        let base = if base.is_empty() { "user".to_string() } else { base };
        for n in 1..=50 {
            let suffix = if n == 1 { String::new() } else { format!("-{n}") };
            let candidate = format!("{}{suffix}", base[..base.len().min(39 - suffix.len())].trim_end_matches('-'));
            let taken = self.db.read(|c| c.query_row("SELECT 1 FROM users WHERE handle = ?", [candidate.as_str()], |_| Ok(())).optional())?.is_some();
            let free = match &self.owners {
                Some(o) => o.available(&candidate).map_err(Error::Internal)?,
                None => true,
            };
            if !taken && free {
                return Ok(candidate);
            }
        }
        Err(Error::Conflict(format!("no free handle like {base}")))
    }

    /// A new user from a valid invite, in one write that uses the invite up.
    pub(crate) fn create_user(&self, new: NewUser<'_>) -> Result<String> {
        let invite_handle = self.db.read(|c| c.query_row("SELECT handle FROM invites WHERE id = ?", [new.invite_id], |r| r.get::<_, Option<String>>(0)).optional())?.flatten();
        let handle = self.pick_handle(invite_handle.as_deref().or(new.hint), new.email)?;
        let id = new_id("usr");
        let now = self.db.now_ms()?;
        let name: String = new.name.map(str::trim).filter(|n| !n.is_empty()).unwrap_or(&handle).chars().take(100).collect();
        self.db.write(|c| {
            let invite = c
                .query_row("SELECT email, admin, created_by, used_at, expires_at FROM invites WHERE id = ?", [new.invite_id], |r| {
                    Ok((r.get::<_, String>(0)?, r.get::<_, bool>(1)?, r.get::<_, String>(2)?, r.get::<_, Option<i64>>(3)?, r.get::<_, i64>(4)?))
                })
                .optional()?;
            let Some((email, admin, by, None, _)) = invite.filter(|i| i.4 > now) else {
                return Err(Error::BadRequest("the invite is used, revoked or expired".into(), Some("invite_invalid")));
            };
            if email != new.email {
                return Err(Error::BadRequest("the invite is for another email address".into(), Some("invite_email")));
            }
            if c.query_row("SELECT 1 FROM users WHERE email = ? OR handle = ?", [email.as_str(), handle.as_str()], |_| Ok(())).optional()?.is_some() {
                return Err(Error::Conflict("an account has this email or handle; try again".into()));
            }
            c.execute(
                "INSERT INTO users (id, email, handle, name, admin, disabled, code_owner_id, invited_by, created_at) VALUES (?, ?, ?, ?, ?, 0, NULL, ?, ?)",
                params![id, email, handle, name, admin, by, now],
            )?;
            c.execute("UPDATE invites SET used_at = ?, used_by = ? WHERE id = ?", params![now, id, new.invite_id])?;
            if let Some((provider, subject, login)) = new.identity {
                c.execute(
                    "INSERT INTO identities (provider, subject, user_id, email, login, created_at) VALUES (?, ?, ?, ?, ?, ?)",
                    params![provider, subject, id, email, login, now],
                )?;
            }
            Ok(())
        })?;
        tracing::info!(user = %id, "accounts: user created from an invite");
        Ok(id)
    }

    // ── invites ──

    /// A new invite for `email`: it, its token (the only copy) and its link.
    pub fn invite(&self, by: &str, email: &str, admin: bool, handle: Option<&str>) -> Result<(Invite, String, String)> {
        let email = normalize_email(email)?;
        let handle = match handle.map(str::trim).filter(|h| !h.is_empty()) {
            Some(h) if slug(h) == h => Some(h.to_string()),
            Some(_) => return Err(Error::BadRequest("a handle is 1–39 characters of a-z, 0-9 and single hyphens".into(), None)),
            None => None,
        };
        let token = new_token("rli");
        let now = self.db.now_ms()?;
        let invite = Invite {
            id: new_id("inv"),
            email: email.clone(),
            admin,
            handle,
            created_by: by.to_string(),
            created_at: now,
            expires_at: now + INVITE_TTL_MS,
            used_at: None,
            used_by: None,
        };
        self.db.write(|c| {
            if c.query_row("SELECT 1 FROM users WHERE email = ?", [email.as_str()], |_| Ok(())).optional()?.is_some() {
                return Err(Error::Conflict(format!("an account has {email}")));
            }
            c.execute(
                "INSERT INTO invites (id, token_hash, email, admin, handle, created_by, created_at, expires_at) VALUES (?, ?, ?, ?, ?, ?, ?, ?)",
                params![invite.id, sha256_hex(&token), invite.email, invite.admin, invite.handle, invite.created_by, invite.created_at, invite.expires_at],
            )?;
            Ok(())
        })?;
        let url = format!("{}/signin?invite={token}", self.config.web_url);
        Ok((invite, token, url))
    }

    pub fn invites(&self) -> Result<Vec<Invite>> {
        Ok(self.db.read(|c| c.query("SELECT id, email, admin, handle, created_by, created_at, expires_at, used_at, used_by FROM invites ORDER BY created_at DESC, id", (), invite_row))?)
    }

    pub fn revoke_invite(&self, id: &str) -> Result<()> {
        self.db.write(|c| {
            let used = c.query_row("SELECT used_at FROM invites WHERE id = ?", [id], |r| r.get::<_, Option<i64>>(0)).optional()?;
            match used {
                None => Err(Error::NotFound(format!("no invite {id}"))),
                Some(Some(_)) => Err(Error::Conflict("the invite is used".into())),
                Some(None) => {
                    c.execute("DELETE FROM invites WHERE id = ?", [id])?;
                    Ok(())
                }
            }
        })
    }

    /// The valid (unused, unexpired) invite a token stands for: its id, email and expiry.
    pub(crate) fn valid_invite(&self, token: &str) -> Result<Option<(String, String, i64)>> {
        let now = self.db.now_ms()?;
        let row = self.db.read(|c| {
            c.query_row("SELECT id, email, expires_at, used_at FROM invites WHERE token_hash = ?", [sha256_hex(token.trim()).as_str()], |r| {
                Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?, r.get::<_, i64>(2)?, r.get::<_, Option<i64>>(3)?))
            })
            .optional()
        })?;
        Ok(row.filter(|r| r.3.is_none() && r.2 > now).map(|(id, email, expires, _)| (id, email, expires)))
    }

    pub fn peek_invite(&self, token: &str) -> Result<(String, i64)> {
        self.valid_invite(token)?.map(|(_, email, expires)| (email, expires)).ok_or_else(|| Error::NotFound("no such invite (used, revoked or expired?)".into()))
    }

    // ── throttles: durable, and only for things that exist ──

    /// Count one attempt at `key` and say whether it's within `max` per `window_ms`. Counting
    /// and checking are one write, so attempts made in parallel (on any node) can't all slip in
    /// under the limit before any of them is counted.
    pub(crate) fn attempt(&self, key: &str, max: i64, window_ms: i64) -> Result<bool> {
        let now = self.db.now_ms()?;
        self.db.write(|c| {
            let row = c.query_row("SELECT n, since FROM throttle WHERE key = ?", [key], |r| Ok((r.get::<_, i64>(0)?, r.get::<_, i64>(1)?))).optional()?;
            let (n, since) = match row {
                Some((n, since)) if now - since < window_ms => (n + 1, since),
                _ => (1, now),
            };
            c.execute("INSERT INTO throttle (key, n, since) VALUES (?, ?, ?) ON CONFLICT (key) DO UPDATE SET n = excluded.n, since = excluded.since", params![key, n, since])?;
            Ok(n <= max)
        })
    }

    pub(crate) fn forget(&self, key: &str) -> Result<()> {
        if self.db.read(|c| c.query_row("SELECT 1 FROM throttle WHERE key = ?", [key], |_| Ok(())).optional())?.is_some() {
            self.db.write(|c| c.execute("DELETE FROM throttle WHERE key = ?", [key]))?;
        }
        Ok(())
    }

    // ── email codes ──

    /// Start email sign-in `challenge`: the mail to send if the address has an account (or
    /// `invite` is a valid invite for it) and hasn't had too many codes lately. The caller answers
    /// before running this, so neither the answer nor its timing says which it was.
    pub fn email_start(&self, challenge: &str, email: &str, invite: Option<&str>) -> Result<Option<Mail>> {
        let email = normalize_email(email)?;
        let user = self.db.read(|c| c.query_row("SELECT id, disabled FROM users WHERE email = ?", [email.as_str()], |r| Ok((r.get::<_, String>(0)?, r.get::<_, bool>(1)?))).optional())?;
        let (user_id, invite_id) = match (user, invite) {
            (Some((_, true)), _) => return Ok(None),
            (Some((u, false)), _) => (Some(u), None),
            (None, Some(token)) => match self.valid_invite(token)? {
                Some((inv, inv_email, _)) if inv_email == email => (None, Some(inv)),
                _ => return Ok(None),
            },
            (None, None) => return Ok(None),
        };
        if !self.attempt(&format!("send:{email}"), SENDS_PER_HOUR, 60 * MINUTE)? {
            return Ok(None);
        }
        let code = zeroize::Zeroizing::new(six_digits());
        let hash = twofactor::hash_code(&code)?;
        let now = self.db.now_ms()?;
        self.db.write(|c| {
            // Codes that expired go with each new one.
            c.execute("DELETE FROM email_codes WHERE expires_at <= ?", [now])?;
            c.execute(
                "INSERT INTO email_codes (id, email, user_id, invite_id, code_hash, attempts, created_at, expires_at) VALUES (?, ?, ?, ?, ?, 0, ?, ?)",
                params![challenge, email, user_id, invite_id, hash, now, now + CODE_TTL_MS],
            )
        })?;
        let mail = Mail {
            to: email,
            subject: format!("{} is your roll sign-in code", *code),
            text: format!("Your roll sign-in code is {}.\n\nIt works once, for the next 10 minutes. If you didn't ask for it, ignore this mail.\n", *code),
        };
        Ok(Some(mail))
    }

    /// Check a challenge's code: the user it signs in (made from the invite on first sign-in).
    pub fn email_verify(&self, challenge: &str, code: &str) -> Result<String> {
        let now = self.db.now_ms()?;
        // A try is used up before the code is checked, in one write: guesses sent in parallel
        // can't share one.
        let row = self.db.write(|c| {
            if c.execute("UPDATE email_codes SET attempts = attempts + 1 WHERE id = ? AND attempts < ? AND expires_at > ?", params![challenge, CODE_TRIES, now])? == 0 {
                return Ok(None);
            }
            c.query_row("SELECT email, user_id, invite_id, code_hash FROM email_codes WHERE id = ?", [challenge], |r| {
                Ok((r.get::<_, String>(0)?, r.get::<_, Option<String>>(1)?, r.get::<_, Option<String>>(2)?, r.get::<_, String>(3)?))
            })
            .optional()
        })?;
        let Some((email, user, invite, hash)) = row else { return Err(Error::bad_code()) };
        let key = format!("code:{}", user.as_deref().or(invite.as_deref()).unwrap_or_default());
        if !self.attempt(&key, CODE_FAILS, 15 * MINUTE)? || !twofactor::verify_code(code.trim(), &hash) {
            return Err(Error::bad_code());
        }
        // Single use: whoever deletes it signs in.
        if self.db.write(|c| c.execute("DELETE FROM email_codes WHERE id = ?", [challenge]))? == 0 {
            return Err(Error::bad_code());
        }
        self.forget(&key)?;
        match (user, invite) {
            (Some(u), _) => Ok(u),
            (None, Some(inv)) => self.create_user(NewUser { invite_id: &inv, email: &email, hint: None, name: None, identity: None }),
            (None, None) => Err(Error::bad_code()),
        }
    }
}

/// The session resolver rolld's gate holds.
struct Resolver(Arc<Accounts>);

impl roll_auth::Sessions for Resolver {
    fn cookie(&self) -> &str {
        self.0.config.session_cookie()
    }

    fn resolve<'a>(&'a self, token: &'a str) -> roll_auth::BoxFuture<'a, Result<Option<roll_auth::Session>, String>> {
        let accounts = self.0.clone();
        let token = zeroize::Zeroizing::new(token.to_string());
        Box::pin(async move { tokio::task::spawn_blocking(move || accounts.resolve(&token).map_err(|e| e.to_string())).await.map_err(|e| e.to_string())? })
    }

    fn person<'a>(&'a self, user: &'a str) -> roll_auth::BoxFuture<'a, Result<Option<roll_auth::SignedIn>, String>> {
        let (accounts, user) = (self.0.clone(), user.to_string());
        Box::pin(async move { tokio::task::spawn_blocking(move || accounts.person(&user).map_err(|e| e.to_string())).await.map_err(|e| e.to_string())? })
    }

    fn person_now(&self, user: &str) -> Result<Option<roll_auth::SignedIn>, String> {
        self.0.person(user).map_err(|e| e.to_string())
    }

    fn tell<'a>(&'a self, user: &'a str, subject: &'a str, text: &'a str) -> roll_auth::BoxFuture<'a, Result<(), String>> {
        let (accounts, id) = (self.0.clone(), user.to_string());
        Box::pin(async move {
            if !accounts.mail_on() {
                return Ok(());
            }
            let found = tokio::task::spawn_blocking({
                let accounts = accounts.clone();
                move || accounts.db.read(|c| load_user(c, &id)).map_err(|e| e.to_string())
            })
            .await
            .map_err(|e| e.to_string())??;
            let Some(user) = found.filter(|u| !u.disabled) else { return Ok(()) };
            accounts.mailer.send(&Mail { to: user.email, subject: subject.to_string(), text: text.to_string() }).await.map_err(|e| e.to_string())
        })
    }

    fn admins(&self) -> roll_auth::BoxFuture<'_, Result<Vec<String>, String>> {
        let accounts = self.0.clone();
        Box::pin(async move {
            tokio::task::spawn_blocking(move || accounts.db.read(|c| c.query("SELECT id FROM users WHERE admin = 1 AND disabled = 0 ORDER BY created_at LIMIT 50", (), |r| r.get::<_, String>(0))).map_err(|e| e.to_string()))
                .await
                .map_err(|e| e.to_string())?
        })
    }
}

fn load_user(c: &code_sql::Conn<'_>, id: &str) -> code_sql::Result<Option<User>> {
    let user = c
        .query_row("SELECT id, email, handle, name, admin, disabled, created_at FROM users WHERE id = ?", [id], |r| {
            Ok(User {
                id: r.get(0)?,
                email: r.get(1)?,
                handle: r.get(2)?,
                name: r.get(3)?,
                admin: r.get(4)?,
                disabled: r.get(5)?,
                created_at: r.get(6)?,
                totp: false,
                identities: Vec::new(),
            })
        })
        .optional()?;
    let Some(mut user) = user else { return Ok(None) };
    user.totp = c.query_row("SELECT 1 FROM totp WHERE user_id = ? AND enabled = 1", [id], |_| Ok(())).optional()?.is_some();
    user.identities = c.query("SELECT provider, login, email FROM identities WHERE user_id = ? ORDER BY provider", [id], |r| {
        Ok(Identity { provider: r.get(0)?, login: r.get(1)?, email: r.get(2)? })
    })?;
    Ok(Some(user))
}

fn invite_row(r: &code_sql::Row<'_>) -> code_sql::Result<Invite> {
    Ok(Invite {
        id: r.get(0)?,
        email: r.get(1)?,
        admin: r.get(2)?,
        handle: r.get(3)?,
        created_by: r.get(4)?,
        created_at: r.get(5)?,
        expires_at: r.get(6)?,
        used_at: r.get(7)?,
        used_by: r.get(8)?,
    })
}

/// An email address as we keep it: trimmed and lowercased, one `@`, at most 254 characters.
pub fn normalize_email(email: &str) -> Result<String> {
    let e = email.trim().to_ascii_lowercase();
    let ok = e.len() <= 254
        && e.split('@').count() == 2
        && e.split_once('@').is_some_and(|(local, domain)| !local.is_empty() && domain.contains('.') && !domain.starts_with('.') && !domain.ends_with('.'))
        && !e.bytes().any(|b| b.is_ascii_whitespace() || b.is_ascii_control() || b"<>,;\"()[]\\".contains(&b));
    if ok { Ok(e) } else { Err(Error::BadRequest("that isn't an email address".into(), None)) }
}

/// `text` as an owner name: lowercase `a-z0-9`, single hyphens, at most 39 characters.
fn slug(text: &str) -> String {
    let mut out = String::new();
    for ch in text.trim().chars() {
        let c = ch.to_ascii_lowercase();
        if c.is_ascii_lowercase() || c.is_ascii_digit() {
            out.push(c);
        } else if !out.is_empty() && !out.ends_with('-') {
            out.push('-');
        }
        if out.len() >= 39 {
            break;
        }
    }
    out.trim_matches('-').to_string()
}

pub(crate) fn sha256_hex(s: &str) -> String {
    hex::encode(Sha256::digest(s.as_bytes()))
}

pub(crate) fn random<const N: usize>() -> [u8; N] {
    let mut b = [0u8; N];
    getrandom::fill(&mut b).expect("system randomness");
    b
}

/// `prefix_` and 14 random characters of `[0-9a-z]`.
pub(crate) fn new_id(prefix: &str) -> String {
    const ALPHABET: &[u8] = b"0123456789abcdefghijklmnopqrstuvwxyz";
    let s: String = random::<14>().iter().map(|x| ALPHABET[(*x as usize) % ALPHABET.len()] as char).collect();
    format!("{prefix}_{s}")
}

/// `prefix_` and 32 random bytes in hex.
pub(crate) fn new_token(prefix: &str) -> String {
    format!("{prefix}_{}", hex::encode(random::<32>()))
}

/// Six uniformly random digits.
fn six_digits() -> String {
    loop {
        let n = u32::from_le_bytes(random::<4>());
        // Reject the tail that would bias the modulo.
        if n < u32::MAX - (u32::MAX % 1_000_000) {
            return format!("{:06}", n % 1_000_000);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn emails_handles_and_codes() {
        assert_eq!(normalize_email(" Ada@Example.COM ").unwrap(), "ada@example.com");
        for bad in ["", "ada", "a@b", "a@@b.c", "a b@c.d", "a@.c", "<a@b.c>"] {
            assert!(normalize_email(bad).is_err(), "{bad}");
        }
        assert_eq!(slug("Ada.Lovelace_99"), "ada-lovelace-99");
        assert_eq!(slug("--x--"), "x");
        assert_eq!(slug(&"a".repeat(60)).len(), 39);
        let c = six_digits();
        assert!(c.len() == 6 && c.bytes().all(|b| b.is_ascii_digit()));
        assert!(new_token("rls").starts_with("rls_") && new_id("usr").len() == 18);
        assert!(!format!("{:?}", Secret::new("hunter2")).contains("hunter2"));
    }
}
