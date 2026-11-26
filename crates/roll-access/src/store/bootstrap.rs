//! Bootstrap tokens and machine credentials (docs/permissions.md, "Bootstrap tokens"): secret zero
//! for a machine with no platform identity, as Akeyless' Universal Identity and Vault's AppRole
//! `secret_id` are.
//!
//! A bootstrap token (`rlbt_…`) is a workload grant that opens nothing: it's single-use, lives
//! minutes and may be bound to addresses. Exchanged once, it becomes a machine credential
//! (`rlmc_…`), which opens nothing either: it only logs its machine in (roll secrets' identity
//! logins, through `Access::mint_workload_via`) and is exchanged for the next one. Every one of
//! them, and every token their logins got, carries the bootstrap token's id as its `chain`.
//!
//! A spent secret (the bootstrap token once exchanged, a machine credential once replaced) is kept
//! as `revoked_why = 'spent'` until it would have ended. Presenting one again means a copy exists
//! somewhere: the whole chain is revoked, so a stolen credential dies with the machine's own as
//! soon as either side uses a spent one.
//!
//! One exception, for an answer that was lost (a timeout after the commit, a proxy's 502, rolld
//! restarting mid-answer): exchanged again once within [`RETRY_GRACE_MS`], while the credential it
//! was spent for is still unused, a spent secret gets a new credential, and the unanswered one
//! becomes a spent secret itself. So whichever side didn't get the newest one ends the chain as
//! soon as it uses what it holds, and a third exchange of the same secret ends it at once.
//!
//! These grants don't count toward [`super::MAX_WORKLOADS`] or [`super::OWNER_MAX_WORKLOADS`]
//! (they open nothing, and live as long as their machine): roll secrets caps them per identity.

use super::{Store, parse_ip_allow};
use crate::{Error, Result, hash, new_id, new_secret};
use code_sql::{OptionalExtension, params};

pub const BOOTSTRAP_PREFIX: &str = "rlbt_";
pub const MACHINE_PREFIX: &str = "rlmc_";
/// How long a bootstrap token lives, in minutes.
pub const BOOTSTRAP_MINUTES: std::ops::RangeInclusive<u32> = 1..=60;
/// How long a machine credential lives, in hours (its machine replaces it at half that).
pub const MACHINE_HOURS: std::ops::RangeInclusive<u32> = 1..=720;
/// Why a spent secret's grant ended: presenting it again revokes its chain.
const SPENT: &str = "spent";
/// A spent secret exchanged again within the grace, or the credential that exchange replaced
/// unanswered: presenting either again revokes its chain, with no second grace.
const RETRIED: &str = "spent, retried";
/// How long after a spent secret's exchange it may be exchanged once more, its answer lost.
pub const RETRY_GRACE_MS: i64 = 2 * 60 * 1000;

/// Which of the two a presented secret must be.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ChainSecret {
    Bootstrap,
    Machine,
}

impl ChainSecret {
    fn kind(self) -> &'static str {
        match self {
            ChainSecret::Bootstrap => "bootstrap",
            ChainSecret::Machine => "machine",
        }
    }

    fn named(kind: &str) -> &'static str {
        if kind == "bootstrap" { "bootstrap token" } else { "machine credential" }
    }
}

/// A new bootstrap token or machine credential: the secret (shown once) and its grant. No `Debug`:
/// it holds the secret.
pub struct Issued {
    pub chain: String,
    pub grant: String,
    pub secret: String,
    pub issued_at: i64,
    pub expires_at: i64,
    /// It answers a retried exchange (its first answer lost): the one that went unanswered is spent.
    pub retried: bool,
}

/// What presenting a chain's secret came to (a refusal is an `Err`).
#[derive(Debug)]
pub enum Presented<T> {
    Live(T),
    /// It was spent already: the chain is revoked now.
    Reused { chain: String },
}

/// A live machine credential: its grant and chain.
#[derive(Debug)]
pub struct Machine {
    pub grant: String,
    pub chain: String,
}

struct Found {
    grant: String,
    chain: String,
    ip_allow: String,
    created_by: String,
    user: Option<String>,
    name: String,
}

/// What a presented chain secret is, inside a write.
enum Shown {
    Live(Found),
    /// Spent within [`RETRY_GRACE_MS`] and not exchanged again yet: when (its `revoked_at`).
    Spent(Found, i64),
    /// It was spent already: the chain is revoked now.
    Reused(String),
}

impl Store {
    /// A bootstrap token for `minutes`, from `ip_allow` only (empty: anywhere), owned as a workload
    /// grant is (`created_by`, `user`). Its id is the chain every credential from it carries. Not
    /// counted toward the workload caps: its caller caps chains (roll secrets: per identity).
    pub fn create_bootstrap(&self, name: &str, minutes: u32, ip_allow: &[String], created_by: &str, user: Option<&str>) -> Result<Issued> {
        super::check_name(name)?;
        parse_ip_allow(ip_allow)?;
        if !BOOTSTRAP_MINUTES.contains(&minutes) {
            return Err(Error::BadRequest(format!("a bootstrap token lives {} to {} minutes", BOOTSTRAP_MINUTES.start(), BOOTSTRAP_MINUTES.end())));
        }
        let now = code_sql::now_ms();
        let until = now + i64::from(minutes) * 60_000;
        let (id, secret) = (new_id("tok"), new_secret(BOOTSTRAP_PREFIX));
        let ip = serde_json::to_string(ip_allow).map_err(|e| Error::Internal(e.to_string()))?;
        self.db.write(|c| insert(c, &id, name.trim(), until, &ip, created_by, user, &id, now, &secret, ChainSecret::Bootstrap))?;
        Ok(Issued { chain: id.clone(), grant: id, secret, issued_at: now, expires_at: until, retried: false })
    }

    /// The chain a bootstrap token or machine credential belongs to, live or not (`None`: it isn't
    /// one, or it's gone).
    pub fn chain_of(&self, secret: &str) -> Result<Option<String>> {
        let h = hash(secret);
        let chain: Option<Option<String>> = self.db.read(|c| {
            c.query_row("SELECT g.chain FROM grant_secrets s JOIN grants g ON g.id = s.grant_id WHERE s.hash = ? AND s.kind IN ('bootstrap', 'machine')", [&h], |r| r.get(0)).optional()
        })?;
        Ok(chain.flatten())
    }

    /// Spend a live `from` (a bootstrap token, or a machine credential that's being replaced) for a
    /// new machine credential of `hours`, bound to the same addresses. One that was spent already
    /// revokes its chain, unless it's a retry of an exchange whose answer was lost (module docs).
    pub fn redeem(&self, secret: &str, from: ChainSecret, ip: Option<std::net::IpAddr>, hours: u32) -> Result<Presented<Issued>> {
        if !MACHINE_HOURS.contains(&hours) {
            return Err(Error::BadRequest(format!("a machine credential lives {} to {} hours", MACHINE_HOURS.start(), MACHINE_HOURS.end())));
        }
        let now = code_sql::now_ms();
        let until = now + i64::from(hours) * 3_600_000;
        let (id, next) = (new_id("tok"), new_secret(MACHINE_PREFIX));
        self.db.write(|c| {
            let (f, retried) = match present(c, secret, from, ip, now)? {
                Shown::Live(f) => {
                    // Spent once: a request that raced this one to the same secret finds it spent.
                    if c.execute("UPDATE grants SET revoked_at = ?, revoked_why = ? WHERE id = ? AND revoked_at IS NULL", params![now, SPENT, f.grant])? == 0 {
                        revoke_chain(c, &f.chain, now)?;
                        return Ok(Presented::Reused { chain: f.chain });
                    }
                    (f, false)
                }
                // The credential it was spent for (made in that write, at its `revoked_at`) still
                // unused: its answer was lost. That one is spent now, and so is this, for good.
                Shown::Spent(f, at) => {
                    let unanswered = c.execute(
                        "UPDATE grants SET revoked_at = ?, revoked_why = ? WHERE chain = ? AND rules = '[]' AND created_at = ? AND revoked_at IS NULL AND last_used_at IS NULL AND id <> ?",
                        params![now, RETRIED, f.chain, at, f.grant],
                    )?;
                    if unanswered != 1 || c.execute("UPDATE grants SET revoked_why = ? WHERE id = ? AND revoked_why = ?", params![RETRIED, f.grant, SPENT])? != 1 {
                        revoke_chain(c, &f.chain, now)?;
                        return Ok(Presented::Reused { chain: f.chain });
                    }
                    (f, true)
                }
                Shown::Reused(chain) => return Ok(Presented::Reused { chain }),
            };
            insert(c, &id, &f.name, until, &f.ip_allow, &f.created_by, f.user.as_deref(), &f.chain, now, &next, ChainSecret::Machine)?;
            Ok::<_, Error>(Presented::Live(Issued { chain: f.chain.clone(), grant: id.clone(), secret: next.clone(), issued_at: now, expires_at: until, retried }))
        })
    }

    /// A live machine credential, presented to log its machine in (nothing is spent, but it's
    /// marked used: an exchange that made it can't be retried any more). One that was spent
    /// already revokes its chain, within the retry grace too.
    pub fn machine(&self, secret: &str, ip: Option<std::net::IpAddr>) -> Result<Presented<Machine>> {
        let now = code_sql::now_ms();
        self.db.write(|c| {
            Ok::<_, Error>(match present(c, secret, ChainSecret::Machine, ip, now)? {
                Shown::Live(f) => {
                    c.execute("UPDATE grants SET last_used_at = ? WHERE id = ? AND last_used_at IS NULL", params![now, f.grant])?;
                    Presented::Live(Machine { grant: f.grant, chain: f.chain })
                }
                Shown::Spent(f, _) => {
                    revoke_chain(c, &f.chain, now)?;
                    Presented::Reused { chain: f.chain }
                }
                Shown::Reused(chain) => Presented::Reused { chain },
            })
        })
    }

    /// End every live grant of `chain`, saying why. How many.
    pub fn revoke_chain(&self, chain: &str, why: &str) -> Result<usize> {
        let now = code_sql::now_ms();
        let why: String = why.chars().take(200).collect();
        Ok(self.db.write(|c| c.execute("UPDATE grants SET revoked_at = ?, revoked_why = ? WHERE chain = ? AND revoked_at IS NULL", params![now, why, chain]))?)
    }
}

/// A chain's grant and its secret, inside a write. Its rules are none: it opens nothing.
#[allow(clippy::too_many_arguments)]
fn insert(c: &code_sql::Conn<'_>, id: &str, name: &str, until: i64, ip: &str, created_by: &str, user: Option<&str>, chain: &str, now: i64, secret: &str, kind: ChainSecret) -> code_sql::Result<()> {
    c.execute(
        "INSERT INTO grants (id, name, kind, rules, expires_at, ip_allow, created_by, created_at, client_id, user_id, ask_otherwise, hint, chain) VALUES (?, ?, 'workload', '[]', ?, ?, ?, ?, NULL, ?, 0, ?, ?)",
        params![id, name, until, ip, created_by, now, user, super::hint(secret), chain],
    )?;
    c.execute("INSERT INTO grant_secrets (hash, grant_id, kind, expires_at, created_at) VALUES (?, ?, ?, ?, ?)", params![hash(secret), id, kind.kind(), until, now])?;
    Ok(())
}

/// The live grant `secret` opens as a `want`, inside a write; a spent one revokes its chain
/// (committed: `Reused` isn't an error), unless it's within its retry grace (`Spent`: the caller
/// decides). Refusals change nothing.
fn present(c: &code_sql::Conn<'_>, secret: &str, want: ChainSecret, ip: Option<std::net::IpAddr>, now: i64) -> Result<Shown> {
    type Row = (String, Option<String>, String, Option<i64>, Option<String>, Option<i64>, String, String, Option<String>, String);
    let row: Option<Row> = c
        .query_row(
            "SELECT g.id, g.chain, s.kind, g.revoked_at, g.revoked_why, g.expires_at, g.ip_allow, g.created_by, g.user_id, g.name FROM grant_secrets s JOIN grants g ON g.id = s.grant_id WHERE s.hash = ?",
            [hash(secret)],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?, r.get(5)?, r.get(6)?, r.get(7)?, r.get(8)?, r.get(9)?)),
        )
        .optional()?;
    let unknown = || Error::Unauthorized(format!("that isn't a live {}", ChainSecret::named(want.kind())));
    let Some((grant, chain, kind, revoked_at, why, expires_at, ip_allow, created_by, user, name)) = row else { return Err(unknown()) };
    let Some(chain) = chain.filter(|_| kind == "bootstrap" || kind == "machine") else { return Err(unknown()) };
    // Spent within its grace, not retried yet: checked as a live one is before the caller decides.
    let mut spent_at = None;
    if let Some(at) = revoked_at {
        match why.as_deref() {
            Some(SPENT) if now - at < RETRY_GRACE_MS => spent_at = Some(at),
            Some(SPENT | RETRIED) => {
                revoke_chain(c, &chain, now)?;
                return Ok(Shown::Reused(chain));
            }
            _ => return Err(Error::Unauthorized(format!("this {} was revoked", ChainSecret::named(&kind)))),
        }
    }
    if kind != want.kind() {
        return Err(unknown());
    }
    if expires_at.is_none_or(|e| e <= now) {
        return Err(Error::Unauthorized(format!("this {} has ended: the machine needs a new bootstrap token", ChainSecret::named(&kind))));
    }
    // An address list that can't be read works from nowhere, never from anywhere.
    let unreadable = || Error::Forbidden(format!("this {}'s address list can't be read, so it doesn't work from any address: make a new one", ChainSecret::named(&kind)));
    let list: Vec<String> = serde_json::from_str(&ip_allow).map_err(|_| unreadable())?;
    let nets = parse_ip_allow(&list).map_err(|_| unreadable())?;
    if !nets.is_empty() && !ip.is_some_and(|ip| nets.iter().any(|n| n.contains(&ip))) {
        return Err(Error::Forbidden(format!("this {} doesn't work from this address", ChainSecret::named(&kind))));
    }
    let found = Found { grant, chain, ip_allow, created_by, user, name };
    Ok(match spent_at {
        Some(at) => Shown::Spent(found, at),
        None => Shown::Live(found),
    })
}

/// Every live grant of `chain` ended, inside a write: a spent secret came back.
fn revoke_chain(c: &code_sql::Conn<'_>, chain: &str, now: i64) -> code_sql::Result<usize> {
    c.execute("UPDATE grants SET revoked_at = ?, revoked_why = 'a spent bootstrap token or machine credential was used again' WHERE chain = ? AND revoked_at IS NULL", params![now, chain])
}

/// The chain a workload grant joins when a machine credential's login made it (`via`: that
/// credential's grant, live), inside the write that makes it.
pub(super) fn chain_via(c: &code_sql::Conn<'_>, via: &str, now: i64) -> Result<String> {
    let chain: Option<Option<String>> = c.query_row("SELECT chain FROM grants WHERE id = ? AND revoked_at IS NULL AND expires_at > ?", params![via, now], |r| r.get(0)).optional()?;
    chain.flatten().ok_or_else(|| Error::Unauthorized("the machine credential has ended".into()))
}
