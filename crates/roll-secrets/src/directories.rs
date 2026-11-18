//! LDAP and Active Directory servers roll doesn't run (docs/secrets.md, "Directories (LDAP and
//! Active Directory)"), as Vault's LDAP secrets engine and Infisical's LDAP rotation.
//!
//! A project registers a directory with a bind account (its password sealed with the project's
//! data key) and up to two managed accounts. With two, they take turns (turns.rs) as a database's
//! roles do: each rotation sets a new password on the account not in use and hands it out, while
//! the other keeps working through the grace period. With one, each rotation replaces its
//! password at once (no grace). A directory registered with `user_base_dn` also issues `dynamic`
//! users: each lease gets `cn=roll-<lease>` under it, in `groups`, deleted when the lease ends.
//! The address is the tenant's: rolld connects only to public addresses it resolved itself, and
//! only over TLS ([`Directory`]).

use crate::access::{Action, Caller, Scope};
use crate::crypto::Keyring;
use crate::links::{Field, Issued, Link, LinkProvider, Mode};
use crate::{Error, Result, Secrets, Value, audit, names};
use code_sql::{Db, OptionalExtension, params};
use serde::{Deserialize, Serialize};
use serde_json::{Value as Json, json};
use std::collections::BTreeMap;
use std::sync::Arc;

pub(crate) const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS directories (
    id TEXT PRIMARY KEY,
    project_id TEXT NOT NULL,
    name TEXT NOT NULL,
    url TEXT NOT NULL,
    flavor TEXT NOT NULL,
    bind_dn TEXT NOT NULL,
    bind_password BLOB NOT NULL,
    user_base_dn TEXT,
    accounts TEXT NOT NULL,
    groups TEXT NOT NULL,
    ca TEXT,
    state TEXT NOT NULL,
    created_at INTEGER NOT NULL,
    created_by TEXT NOT NULL,
    UNIQUE (project_id, name)
) WITHOUT ROWID;
";

pub(crate) const KIND: &str = "directory";

/// The directory server's kind: how a password is set and a user made.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Flavor {
    /// `userPassword` (through the Password Modify extended operation where the server has it),
    /// `inetOrgPerson` users, `groupOfNames` groups.
    Openldap,
    /// `unicodePwd` (a quoted UTF-16LE value, over TLS), `user` objects.
    ActiveDirectory,
}

impl Flavor {
    fn as_str(self) -> &'static str {
        match self {
            Flavor::Openldap => "openldap",
            Flavor::ActiveDirectory => "active_directory",
        }
    }

    fn parse(s: &str) -> Flavor {
        if s == "active_directory" { Flavor::ActiveDirectory } else { Flavor::Openldap }
    }
}

/// How rolld reaches a directory: `ldaps://host[:port]` or `ldap://host[:port]` (StartTLS,
/// always), bound as `bind_dn`. No `Debug`: it holds the bind password.
pub struct Bind<'a> {
    pub url: &'a str,
    pub flavor: Flavor,
    pub bind_dn: &'a str,
    pub bind_password: &'a str,
    /// The server's CA certificates (PEM), trusted as well as the system's.
    pub ca: Option<&'a str>,
}

/// An LDAP or Active Directory server outside roll (rolld: TLS to a public address it resolved
/// itself). Calls block; errors never carry a password.
pub trait Directory: Send + Sync {
    /// Bind, then check the managed accounts, the groups and the base for users exist.
    fn check(&self, bind: &Bind<'_>, accounts: &[String], groups: &[String], user_base_dn: Option<&str>) -> std::result::Result<(), String>;
    /// Replace `dn`'s password (as an administrator: no old password needed).
    fn set_password(&self, bind: &Bind<'_>, dn: &str, password: &str) -> std::result::Result<(), String>;
    /// Make the user `dn` with `password`, in `groups`, described as `description`; on Active
    /// Directory its account expires at `until` (unix ms). May leave part of it made on failure:
    /// the caller deletes it.
    fn create_user(&self, bind: &Bind<'_>, dn: &str, password: &str, groups: &[String], description: &str, until: i64) -> std::result::Result<(), String>;
    /// Move a user's expiry (Active Directory's `accountExpires`; nothing elsewhere).
    fn extend_user(&self, bind: &Bind<'_>, dn: &str, until: i64) -> std::result::Result<(), String>;
    /// Take `dn` out of `groups` and delete it. A user that's gone already is fine.
    fn delete_user(&self, bind: &Bind<'_>, dn: &str, groups: &[String]) -> std::result::Result<(), String>;
    /// Active Directory accounts' other names, in the order of `dns`.
    fn account_names(&self, bind: &Bind<'_>, dns: &[String]) -> std::result::Result<Vec<AccountNames>, String>;
}

/// The names an Active Directory account logs in with besides its DN.
#[derive(Clone, Debug, PartialEq)]
pub struct AccountNames {
    /// `sAMAccountName` (`DOMAIN\name` logins).
    pub account_name: String,
    /// `userPrincipalName`, else the implicit `account_name@domain` every account has.
    pub upn: String,
}

impl AccountNames {
    /// A dynamic user's names (`cn=roll-<lease>,…`): `roll-` and 15 characters of the lease id
    /// (sAMAccountName is 20 characters at most), at the DN's domain.
    pub fn dynamic(dn: &str) -> AccountNames {
        let cn = dn.split(',').next().unwrap_or_default().split_once('=').map_or("", |(_, v)| v);
        let account_name = format!("roll-{}", cn.rsplit('_').next().unwrap_or(cn).chars().take(15).collect::<String>());
        AccountNames { upn: format!("{account_name}@{}", domain(dn)), account_name }
    }
}

/// The DNS domain a DN is in: its `DC=` parts, joined with dots.
pub fn domain(dn: &str) -> String {
    dn.split(',').filter_map(|rdn| rdn.trim().split_once('=')).filter(|(k, _)| k.trim().eq_ignore_ascii_case("dc")).map(|(_, v)| v.trim()).collect::<Vec<_>>().join(".")
}

#[derive(Debug, Serialize, PartialEq)]
pub struct RegisteredDirectory {
    pub id: String,
    pub name: String,
    pub url: String,
    pub flavor: Flavor,
    pub bind_dn: String,
    /// Where dynamic users are made, if it issues them.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub user_base_dn: Option<String>,
    pub managed_accounts: Vec<String>,
    pub groups: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ca: Option<String>,
    pub created_at: i64,
    pub created_by: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NewDirectory {
    pub name: String,
    pub url: String,
    pub bind_dn: String,
    pub bind_password: Value,
    pub flavor: Flavor,
    #[serde(default)]
    pub user_base_dn: Option<String>,
    #[serde(default)]
    pub managed_accounts: Vec<String>,
    #[serde(default)]
    pub groups: Vec<String>,
    #[serde(default)]
    pub ca: Option<String>,
}

/// What the provider needs to reach one directory.
struct Opened {
    url: String,
    flavor: Flavor,
    bind_dn: String,
    bind_password: Value,
    ca: Option<String>,
    user_base_dn: Option<String>,
    accounts: Vec<String>,
    groups: Vec<String>,
}

impl Opened {
    fn bind(&self) -> Bind<'_> {
        Bind { url: &self.url, flavor: self.flavor, bind_dn: &self.bind_dn, bind_password: self.bind_password.expose(), ca: self.ca.as_deref() }
    }
}

/// A login's parts: its DN and password, and on Active Directory its other names.
fn parts(dn: String, password: Value, names: Option<AccountNames>) -> BTreeMap<String, Value> {
    let mut parts = BTreeMap::from([("username".to_string(), Value::new(dn)), ("password".to_string(), password)]);
    if let Some(n) = names {
        parts.insert("upn".into(), Value::new(n.upn));
        parts.insert("account_name".into(), Value::new(n.account_name));
    }
    parts
}

/// Its managed accounts take turns (turns.rs), named `dn` in a credential's handle.
const TURNS: crate::turns::Turns = crate::turns::Turns { table: "directories", kind: KIND, key: "dn" };

fn aad(project: &str, id: &str) -> String {
    format!("roll-secrets:directory:{project}:{id}:bind_password")
}

/// A distinguished name as given: `attr=value` pairs, no control characters, 512 bytes at most.
fn check_dn(dn: &str) -> bool {
    !dn.is_empty() && dn.len() <= 512 && dn.contains('=') && !dn.chars().any(char::is_control) && dn.trim() == dn
}

/// `ldaps://host[:port]` or `ldap://host[:port]`, nothing else (no base DN, scope or filter in it).
fn check_url(url: &str) -> Option<&str> {
    let rest = url.strip_prefix("ldaps://").or_else(|| url.strip_prefix("ldap://"))?.trim_end_matches('/');
    let ok = !rest.is_empty() && rest.len() <= 255 && rest.bytes().all(|b| b.is_ascii_alphanumeric() || b".-:[]".contains(&b));
    ok.then_some(rest)
}

/// A password nobody chose, to the flavor's rules: 32 characters with upper- and lowercase
/// letters and digits, and on Active Directory a symbol too (its complexity policy). The symbols
/// need no quoting in LDIF, a shell or a URL's query.
pub(crate) fn password(flavor: Flavor) -> String {
    const ALNUM: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789";
    const SYMBOLS: &[u8] = b"!#%+-.:=?@^_~";
    let alphabet: Vec<u8> = match flavor {
        Flavor::Openldap => ALNUM.to_vec(),
        Flavor::ActiveDirectory => [ALNUM, SYMBOLS].concat(),
    };
    let n = alphabet.len();
    // The largest multiple of the alphabet's size under 256: no bias toward the first letters.
    let limit = (256 / n * n) as u8;
    loop {
        let mut out = String::with_capacity(32);
        while out.len() < 32 {
            let mut b = [0u8; 64];
            getrandom::fill(&mut b).expect("system randomness");
            out.extend(b.iter().filter(|x| **x < limit).map(|x| alphabet[*x as usize % n] as char).take(32 - out.len()));
        }
        let symbol = flavor == Flavor::Openldap || out.bytes().any(|b| SYMBOLS.contains(&b));
        if symbol && out.bytes().any(|b| b.is_ascii_uppercase()) && out.bytes().any(|b| b.is_ascii_lowercase()) && out.bytes().any(|b| b.is_ascii_digit()) {
            return out;
        }
    }
}

const FIELDS: &[Field] = &[
    Field { field: "username", label: "Username (DN)", modes: &[Mode::Managed, Mode::Dynamic], secret: false, suggest: "LDAP_BIND_DN", credential: Some("login") },
    Field { field: "password", label: "Password", modes: &[Mode::Managed, Mode::Dynamic], secret: true, suggest: "LDAP_PASSWORD", credential: Some("login") },
    Field { field: "upn", label: "UPN", modes: &[Mode::Managed, Mode::Dynamic], secret: false, suggest: "LDAP_UPN", credential: Some("login") },
    Field { field: "account_name", label: "sAMAccountName", modes: &[Mode::Managed, Mode::Dynamic], secret: false, suggest: "LDAP_USERNAME", credential: Some("login") },
];

/// What only Active Directory issues.
const AD_ONLY: &[&str] = &["upn", "account_name"];

/// How long past its lease a dynamic Active Directory user's account lasts (the lease's end deletes it).
const SLACK_MS: i64 = 5 * 60_000;

pub(crate) struct Provider {
    pub db: Db,
    pub keys: Keyring,
    pub unwrap: crate::kms::Unwrapper,
    pub directory: Arc<dyn Directory>,
}

impl Provider {
    fn flavor(&self, id: &str) -> Option<Flavor> {
        self.db.read(|c| c.query_row("SELECT flavor FROM directories WHERE id = ?", [id], |r| r.get::<_, String>(0)).optional()).ok().flatten().map(|f| Flavor::parse(&f))
    }

    fn open(&self, id: &str) -> Result<Opened> {
        type Row = (String, String, String, String, Vec<u8>, Option<String>, String, String, Option<String>, String);
        let row: Option<Row> = self.db.read(|c| {
            Ok::<_, Error>(
                c.query_row(
                    "SELECT d.project_id, d.url, d.flavor, d.bind_dn, d.bind_password, d.user_base_dn, d.accounts, d.groups, d.ca, p.dek FROM directories d JOIN projects p ON p.project_id = d.project_id WHERE d.id = ?",
                    [id],
                    |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?, r.get(5)?, r.get(6)?, r.get(7)?, r.get(8)?, r.get(9)?)),
                )
                .optional()?,
            )
        })?;
        let (project, url, flavor, bind_dn, sealed, user_base_dn, accounts, groups, ca, dek) = row.ok_or_else(|| Error::Link(format!("directory {id} doesn't exist any more")))?;
        let inner = self.unwrap.inner(&project, &dek)?;
        let (dek, _) = self.keys.open_dek(&project, &inner)?;
        Ok(Opened {
            url,
            flavor: Flavor::parse(&flavor),
            bind_dn,
            bind_password: dek.open(&aad(&project, id), &sealed)?,
            ca,
            user_base_dn,
            accounts: serde_json::from_str(&accounts).unwrap_or_default(),
            groups: serde_json::from_str(&groups).unwrap_or_default(),
        })
    }

    /// A user of the lease's own under the base, in the groups; whatever a failed issue made is
    /// deleted again.
    fn issue_dynamic(&self, r#ref: &str, label: &str, ttl_minutes: u64, lease: &str) -> Result<Issued> {
        let o = self.open(r#ref)?;
        let base = o.user_base_dn.as_deref().ok_or_else(|| Error::BadRequest(format!("directory {ref} has no user_base_dn for dynamic users: register it with one", ref = r#ref)))?;
        let dn = format!("cn=roll-{lease},{base}");
        let pw = Value::new(password(o.flavor));
        let until = self.db.now_ms()? + ttl_minutes as i64 * 60_000 + SLACK_MS;
        let description = format!("roll secrets lease {lease} ({label})");
        if let Err(why) = self.directory.create_user(&o.bind(), &dn, pw.expose(), &o.groups, &description, until) {
            let _ = self.directory.delete_user(&o.bind(), &dn, &o.groups);
            return Err(Error::Link(format!("creating {dn}: {why}")));
        }
        let names = (o.flavor == Flavor::ActiveDirectory).then(|| AccountNames::dynamic(&dn));
        Ok(Issued { parts: parts(dn.clone(), pw, names), handle: json!({ "user": dn }) })
    }
}

impl LinkProvider for Provider {
    fn kind(&self) -> &str {
        KIND
    }

    fn fields(&self) -> &[Field] {
        FIELDS
    }

    fn suggested(&self) -> &[(&'static str, Mode)] {
        &[("username", Mode::Managed), ("password", Mode::Managed)]
    }

    fn fields_for(&self, r#ref: &str) -> Vec<Field> {
        let ad = self.flavor(r#ref) == Some(Flavor::ActiveDirectory);
        FIELDS.iter().filter(|f| ad || !AD_ONLY.contains(&f.field)).cloned().collect()
    }

    fn check(&self, link: &Link) -> Result<()> {
        if AD_ONLY.contains(&link.field.as_str()) && self.flavor(&link.r#ref).is_some_and(|f| f != Flavor::ActiveDirectory) {
            return Err(Error::BadRequest(format!("only Active Directory has {}: link username (the DN) to this directory", link.field)));
        }
        Ok(())
    }

    fn name(&self, r#ref: &str) -> Option<String> {
        self.db.read(|c| c.query_row("SELECT name FROM directories WHERE id = ?", [r#ref], |r| r.get::<_, String>(0)).optional()).ok().flatten()
    }

    fn read(&self, _: &str, _: &str, _: &Link) -> Result<Value> {
        Err(Error::BadRequest("a directory's password is managed: roll keeps what it set".into()))
    }

    /// Managed: the account not in use gets a new password and is handed out (a lone account's
    /// is replaced). Dynamic: through [`LinkProvider::issue_lease`].
    fn issue(&self, r#ref: &str, _credential: &str, link: &Link, label: &str, ttl: Option<u64>) -> Result<Issued> {
        if link.mode == Mode::Dynamic {
            // Never reached through a resolve (issue_lease names the user after its lease).
            return self.issue_dynamic(r#ref, label, ttl.unwrap_or(60), &names::new_id("lse", 16));
        }
        let o = self.open(r#ref)?;
        if o.accounts.is_empty() {
            return Err(Error::Link(format!("directory {ref} has no managed accounts: register it with one or two", ref = r#ref)));
        }
        // Active Directory's names are read first: a failure changes no password.
        let names = match o.flavor {
            Flavor::ActiveDirectory => self.directory.account_names(&o.bind(), &o.accounts).map_err(|why| Error::Link(format!("reading {}: {why}", o.accounts.join(", "))))?,
            Flavor::Openldap => vec![],
        };
        let (dn, pw, handle) = TURNS.issue(&self.db, r#ref, &o.accounts, || password(o.flavor), |dn, pw| self.directory.set_password(&o.bind(), dn, pw))?;
        let names = o.accounts.iter().position(|a| *a == dn).and_then(|i| names.get(i).cloned());
        Ok(Issued { parts: parts(dn, pw, names), handle })
    }

    fn issue_lease(&self, r#ref: &str, credential: &str, link: &Link, label: &str, ttl_minutes: u64, lease: &str) -> Result<Issued> {
        if link.mode != Mode::Dynamic {
            return self.issue(r#ref, credential, link, label, Some(ttl_minutes));
        }
        self.issue_dynamic(r#ref, label, ttl_minutes, lease)
    }

    /// A dynamic user is deleted; a managed account's password is replaced by one nobody knows
    /// after its grace, unless it was handed out again since.
    fn revoke(&self, r#ref: &str, handle: &Json) -> Result<()> {
        let o = self.open(r#ref)?;
        if let Some(dn) = handle["user"].as_str() {
            return self.directory.delete_user(&o.bind(), dn, &o.groups).map_err(|why| Error::Link(format!("deleting {dn}: {why}")));
        }
        TURNS.retire(&self.db, r#ref, &o.accounts, handle, || password(o.flavor), |dn, pw| self.directory.set_password(&o.bind(), dn, pw))
    }

    /// An Active Directory user's account expires with its renewed lease.
    fn renew(&self, r#ref: &str, handle: &Json, until: i64) -> Result<()> {
        let Some(dn) = handle["user"].as_str() else { return Ok(()) };
        let o = self.open(r#ref)?;
        self.directory.extend_user(&o.bind(), dn, until + SLACK_MS).map_err(|why| Error::Link(format!("extending {dn}: {why}")))
    }

    /// One directory's managed credential at a time: two environments would hand out each
    /// other's accounts. (Dynamic users are each their own.)
    fn exclusive(&self) -> bool {
        true
    }
}

impl Secrets {
    pub fn directories(&self, project: &str, caller: &Caller) -> Result<Vec<RegisteredDirectory>> {
        caller.need(Action::Read, &Scope::project(project))?;
        self.project(project)?;
        self.db.read(|c| {
            Ok(c.query("SELECT id, name, url, flavor, bind_dn, user_base_dn, accounts, groups, ca, created_at, created_by FROM directories WHERE project_id = ? ORDER BY name", [project], |r| {
                Ok(RegisteredDirectory {
                    id: r.get(0)?,
                    name: r.get(1)?,
                    url: r.get(2)?,
                    flavor: Flavor::parse(&r.get::<_, String>(3)?),
                    bind_dn: r.get(4)?,
                    user_base_dn: r.get(5)?,
                    managed_accounts: serde_json::from_str(&r.get::<_, String>(6)?).unwrap_or_default(),
                    groups: serde_json::from_str(&r.get::<_, String>(7)?).unwrap_or_default(),
                    ca: r.get(8)?,
                    created_at: r.get(9)?,
                    created_by: r.get(10)?,
                })
            })?)
        })
    }

    /// Register a directory, after binding to it and finding its accounts, groups and base.
    pub fn add_directory(&self, project: &str, n: NewDirectory, caller: &Caller) -> Result<RegisteredDirectory> {
        caller.need(Action::Admin, &Scope::project(project))?;
        let directory = self.directory.clone().ok_or_else(|| Error::BadRequest("directories aren't on this server".into()))?;
        if n.name.is_empty() || n.name.len() > 64 || !n.name.chars().all(|c| c.is_ascii_alphanumeric() || "-_".contains(c)) {
            return Err(Error::BadRequest("a directory's name is letters, digits, - and _ (64 at most)".into()));
        }
        let url = n.url.trim().to_string();
        let host = check_url(&url).ok_or_else(|| Error::BadRequest("the URL is ldaps://host[:636] or ldap://host[:389] (StartTLS, always), with nothing after the host".into()))?.to_string();
        let user_base_dn = n.user_base_dn.map(|d| d.trim().to_string()).filter(|d| !d.is_empty());
        let dns_ok = check_dn(&n.bind_dn) && n.managed_accounts.iter().all(|d| check_dn(d)) && n.groups.iter().all(|d| check_dn(d)) && user_base_dn.as_deref().is_none_or(check_dn);
        if !dns_ok || n.groups.len() > 16 {
            return Err(Error::BadRequest("bind_dn, managed_accounts, groups and user_base_dn are distinguished names (cn=…,dc=example,dc=com; 512 bytes at most, 16 groups at most)".into()));
        }
        let distinct = n.managed_accounts.len() < 2 || !n.managed_accounts[0].eq_ignore_ascii_case(&n.managed_accounts[1]);
        if n.managed_accounts.len() > 2 || !distinct || (n.managed_accounts.is_empty() && user_base_dn.is_none()) {
            return Err(Error::BadRequest("managed_accounts are one or two different accounts (two take turns), user_base_dn where dynamic users are made; one or both".into()));
        }
        if !n.groups.is_empty() && user_base_dn.is_none() {
            return Err(Error::BadRequest("groups are what dynamic users join: set user_base_dn too".into()));
        }
        if n.managed_accounts.iter().any(|a| a.eq_ignore_ascii_case(&n.bind_dn)) {
            return Err(Error::BadRequest("the bind account isn't one roll rotates: rotating it would lock roll out".into()));
        }
        if n.bind_password.expose().is_empty() || n.bind_password.expose().len() > 1024 {
            return Err(Error::BadRequest("bind_password is the bind account's password".into()));
        }
        let ca = n.ca.map(|c| c.trim().to_string()).filter(|c| !c.is_empty());
        if ca.as_ref().is_some_and(|c| c.len() > 64 * 1024 || !c.contains("-----BEGIN CERTIFICATE-----")) {
            return Err(Error::BadRequest("ca is the server's CA certificates, PEM (64 KiB at most)".into()));
        }
        // One bind proves the credentials (outside any transaction).
        let bind = Bind { url: &url, flavor: n.flavor, bind_dn: &n.bind_dn, bind_password: n.bind_password.expose(), ca: ca.as_deref() };
        directory.check(&bind, &n.managed_accounts, &n.groups, user_base_dn.as_deref()).map_err(|why| Error::BadRequest(format!("roll couldn't use the directory: {why}")))?;
        let dek = self.project(project)?;
        let id = names::new_id("dir", 16);
        let sealed = dek.seal(&aad(project, &id), &n.bind_password);
        let now = self.db.now_ms()?;
        let d = RegisteredDirectory { id: id.clone(), name: n.name, url, flavor: n.flavor, bind_dn: n.bind_dn, user_base_dn, managed_accounts: n.managed_accounts, groups: n.groups, ca, created_at: now, created_by: caller.principal.clone() };
        self.db.write(|c| {
            if c.query_row("SELECT 1 FROM directories WHERE project_id = ? AND name = ?", [project, &d.name], |_| Ok(())).optional()?.is_some() {
                return Err(Error::Conflict("exists", format!("a directory named {} exists", d.name)));
            }
            // Two registrations taking turns on one account would undo each other's passwords.
            let same: Vec<(String, String, String)> = c.query("SELECT name, url, accounts FROM directories WHERE project_id = ?", [project], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?;
            let clash = same.iter().find(|(_, u, accounts)| {
                check_url(u).is_some_and(|h| h.eq_ignore_ascii_case(&host)) && serde_json::from_str::<Vec<String>>(accounts).unwrap_or_default().iter().any(|a| d.managed_accounts.iter().any(|b| a.eq_ignore_ascii_case(b)))
            });
            if let Some((other, ..)) = clash {
                return Err(Error::Conflict("exists", format!("{other} already rotates these accounts at {host}")));
            }
            c.execute(
                "INSERT INTO directories (id, project_id, name, url, flavor, bind_dn, bind_password, user_base_dn, accounts, groups, ca, state, created_at, created_by) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
                params![
                    id,
                    project,
                    d.name,
                    d.url,
                    d.flavor.as_str(),
                    d.bind_dn,
                    sealed,
                    d.user_base_dn,
                    serde_json::to_string(&d.managed_accounts).expect("accounts"),
                    serde_json::to_string(&d.groups).expect("groups"),
                    d.ca,
                    crate::turns::Turns::initial(),
                    now,
                    d.created_by
                ],
            )?;
            audit::record(c, project, caller, "directory.add", None, None, &[], &format!("added directory {} ({host}, {})", d.name, d.flavor.as_str()), now)
        })?;
        Ok(d)
    }

    /// Delete a directory no key links to and no lease still needs.
    pub fn delete_directory(&self, project: &str, id: &str, caller: &Caller) -> Result<()> {
        caller.need(Action::Admin, &Scope::project(project))?;
        self.project(project)?;
        let now = self.db.now_ms()?;
        let like = (format!("%\"kind\":\"{KIND}\"%"), format!("%\"ref\":\"{id}\"%"));
        self.db.write(|c| {
            let name: String = c.query_row("SELECT name FROM directories WHERE id = ? AND project_id = ?", [id, project], |r| r.get(0)).optional()?.ok_or_else(|| Error::NotFound(format!("no directory {id}")))?;
            let linked: Vec<String> = c.query("SELECT key FROM secrets WHERE project_id = ? AND link LIKE ? AND link LIKE ? ORDER BY key LIMIT 5", params![project, like.0, like.1], |r| r.get(0))?;
            if !linked.is_empty() {
                return Err(Error::Conflict("in_use", format!("{name} still issues {}: unlink them first", linked.join(", "))));
            }
            let waiting: i64 = c.query_row("SELECT COUNT(*) FROM leases WHERE project_id = ? AND revoked_at IS NULL AND link LIKE ? AND link LIKE ?", params![project, like.0, like.1], |r| r.get(0))?;
            if waiting > 0 {
                return Err(Error::Conflict("in_use", format!("{name} has {waiting} password(s) or user(s) still to retire (a grace period, or a dynamic lease): delete it once they're done")));
            }
            c.execute("DELETE FROM directories WHERE id = ?", [id])?;
            audit::record(c, project, caller, "directory.delete", None, None, &[], &format!("deleted directory {name}"), now)
        })
    }

    pub(crate) fn directory_refs(&self, project: &str) -> Result<Vec<(String, String)>> {
        self.db.read(|c| Ok(c.query("SELECT id, name FROM directories WHERE project_id = ? ORDER BY name", [project], |r| Ok((r.get(0)?, r.get(1)?)))?))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn passwords_follow_the_flavors_rules() {
        for _ in 0..200 {
            let ad = password(Flavor::ActiveDirectory);
            assert!(ad.len() == 32 && ad.bytes().any(|b| b"!#%+-.:=?@^_~".contains(&b)) && ad.bytes().any(|b| b.is_ascii_uppercase()) && ad.bytes().any(|b| b.is_ascii_lowercase()) && ad.bytes().any(|b| b.is_ascii_digit()));
            assert!(!ad.contains('"'), "a quote would end unicodePwd's value");
            let ol = password(Flavor::Openldap);
            assert!(ol.len() == 32 && ol.bytes().all(|b| b.is_ascii_alphanumeric()));
        }
    }

    #[test]
    fn a_url_is_only_a_scheme_and_a_host() {
        assert_eq!(check_url("ldaps://dc1.example.com:636"), Some("dc1.example.com:636"));
        assert_eq!(check_url("ldap://ldap.example.com/"), Some("ldap.example.com"));
        assert_eq!(check_url("ldaps://ldap.example.com/dc=example,dc=com"), None);
        assert_eq!(check_url("ldapi:///var/run/slapd"), None);
        assert_eq!(check_url("ldaps://user:pw@ldap.example.com"), None);
    }
}
