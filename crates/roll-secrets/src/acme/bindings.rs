//! External Account Bindings (the project's routes, `…/cas/{id}/acme`): the key id and HMAC key an
//! ACME account is made with, what it may order, revoking it; the CA's pre-authorization; and
//! ACME's sweep.

use super::jws::b64;
use super::{BINDING_NAMES_MAX, BINDINGS_MAX, Checks, KEEP_MS};
use crate::access::{Action, Caller, Scope};
use crate::pki::Policy;
use crate::{Error, Result, Secrets, Value, audit, names};
use code_sql::{Conn, OptionalExtension, params};
use serde::{Deserialize, Serialize};
use std::sync::Arc;

/// An External Account Binding: what an account made with it may do. Its key is shown once.
#[derive(Debug, Serialize)]
pub struct Binding {
    pub id: String,
    pub label: String,
    /// The names its accounts may order (each takes itself and every name under it); none: the
    /// CA's whole policy.
    pub names: Option<Vec<String>>,
    pub expires_at: Option<i64>,
    pub created_at: i64,
    pub created_by: String,
    pub revoked_at: Option<i64>,
    pub revoked_by: Option<String>,
    pub used_at: Option<i64>,
    /// Accounts made with it.
    pub accounts: i64,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NewBinding {
    #[serde(default)]
    pub label: Option<String>,
    #[serde(default)]
    pub names: Option<Vec<String>>,
    /// Days until it stops working (none: until revoked).
    #[serde(default)]
    pub days: Option<u32>,
}

/// A new binding and its key, shown this once: what an ACME client's EAB settings take.
#[derive(Debug, Serialize)]
pub struct NewKey {
    #[serde(flatten)]
    pub binding: Binding,
    pub key_id: String,
    /// The HMAC key, base64url (cert-manager's `keySecretRef`, certbot's `--eab-hmac-key`).
    pub hmac_key: String,
    pub directory: String,
}

#[derive(Debug, Serialize)]
pub struct Settings {
    pub directory: String,
    /// Accounts' orders are valid for their binding's names without a challenge.
    pub pre_authorize: bool,
    /// The challenges this server validates (none without rolld's checks).
    pub challenges: Vec<&'static str>,
    pub bindings: Vec<Binding>,
}

pub(super) fn eab_aad(project: &str, id: &str) -> String {
    format!("roll-secrets:acme-eab:{project}:{id}")
}

const BINDING_COLUMNS: &str = "b.id, b.label, b.names, b.expires_at, b.created_at, b.created_by, b.revoked_at, b.revoked_by, b.used_at, (SELECT COUNT(*) FROM acme_accounts a WHERE a.binding_id = b.id)";

fn binding_row(r: &code_sql::Row<'_>) -> code_sql::Result<Binding> {
    Ok(Binding {
        id: r.get(0)?,
        label: r.get(1)?,
        names: r.get::<_, Option<String>>(2)?.and_then(|n| serde_json::from_str(&n).ok()),
        expires_at: r.get(3)?,
        created_at: r.get(4)?,
        created_by: r.get(5)?,
        revoked_at: r.get(6)?,
        revoked_by: r.get(7)?,
        used_at: r.get(8)?,
        accounts: r.get(9)?,
    })
}

/// Whether a binding's `names` take `name`: an IP address exactly, a DNS name (or a wildcard's
/// base) if it is one of them or under one.
pub(super) fn binding_allows(names: &[String], name: &str) -> bool {
    if name.parse::<std::net::IpAddr>().is_ok() {
        return names.iter().any(|n| n == name);
    }
    let bare = name.strip_prefix("*.").unwrap_or(name);
    names.iter().any(|n| bare == n || bare.ends_with(&format!(".{n}")))
}

impl Secrets {
    /// The CA's project, name, policy and whether it pre-authorizes; 404 unless it's `project`'s.
    pub(super) fn ca_of(&self, project: Option<&str>, ca: &str) -> Result<(String, String, Policy, bool)> {
        let row: Option<(String, String, String, Option<i64>)> =
            self.db.read(|c| c.query_row("SELECT project_id, name, policy, acme_preauthorize FROM cas WHERE id = ?", [ca], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?))).optional())?;
        match row {
            Some((p, name, policy, pre)) if project.is_none_or(|x| x == p) => {
                let policy = serde_json::from_str(&policy).map_err(|_| Error::Internal("a CA's policy isn't JSON".into()))?;
                Ok((p, name, policy, pre.unwrap_or(0) != 0))
            }
            _ => Err(Error::NotFound(format!("no CA {ca}"))),
        }
    }

    /// Where ACME clients find a CA, under rolld's public address.
    pub(crate) fn acme_base(&self, ca: &str) -> Result<String> {
        let api = self.public_api.get().ok_or_else(|| Error::Unavailable("ACME needs rolld's public address (ROLL_CODE_PUBLIC_URL)".into()))?;
        Ok(format!("{}/v1/secrets/acme/{ca}", api.trim_end_matches('/')))
    }

    /// rolld's challenge checks: http-01, dns-01 and tls-alpn-01 work once it's set. Set once, at
    /// startup.
    pub fn set_acme_checks(&self, checks: Arc<dyn Checks>) {
        let _ = self.acme.checks.set(checks);
    }

    pub fn acme_settings(&self, project: &str, ca: &str, caller: &Caller) -> Result<Settings> {
        caller.need(Action::Read, &Scope::project(project))?;
        self.project(project)?;
        let (_, _, _, pre_authorize) = self.ca_of(Some(project), ca)?;
        let bindings = self.db.read(|c| c.query(&format!("SELECT {BINDING_COLUMNS} FROM acme_bindings b WHERE b.ca_id = ? ORDER BY b.created_at DESC"), [ca], binding_row))?;
        let challenges = if self.acme.checks.get().is_some() { vec!["http-01", "dns-01", "tls-alpn-01"] } else { vec![] };
        Ok(Settings { directory: format!("{}/directory", self.acme_base(ca)?), pre_authorize, challenges, bindings })
    }

    /// Turn pre-authorized names on or off (admin, audited).
    pub fn set_acme(&self, project: &str, ca: &str, pre_authorize: bool, caller: &Caller) -> Result<Settings> {
        caller.need(Action::Admin, &Scope::project(project))?;
        self.project(project)?;
        let (_, name, _, _) = self.ca_of(Some(project), ca)?;
        let now = self.db.now_ms()?;
        self.db.write(|c| {
            c.execute("UPDATE cas SET acme_preauthorize = ? WHERE id = ?", params![i64::from(pre_authorize), ca])?;
            let what = if pre_authorize { "turned on pre-authorized ACME names (a binding's key is the whole proof)" } else { "turned off pre-authorized ACME names" };
            audit::record(c, project, caller, "ca.acme", None, None, &[], &format!("{name}: {what}"), now)
        })?;
        self.acme_settings(project, ca, caller)
    }

    /// A new binding for `ca` (admin, audited): its key id and HMAC key, shown once; the key is
    /// kept sealed with the project's data key, bound to the binding.
    pub fn add_acme_binding(&self, project: &str, ca: &str, n: NewBinding, caller: &Caller) -> Result<NewKey> {
        caller.need(Action::Admin, &Scope::project(project))?;
        let dek = self.project(project)?;
        let (_, ca_name, policy, _) = self.ca_of(Some(project), ca)?;
        let label = n.label.unwrap_or_default().trim().to_string();
        if label.chars().count() > 64 || label.chars().any(char::is_control) {
            return Err(Error::BadRequest("a binding's label is 64 characters at most".into()));
        }
        let names = match n.names {
            None => None,
            Some(list) => {
                let list: Vec<String> = list.iter().map(|s| s.trim().trim_end_matches('.').to_ascii_lowercase()).filter(|s| !s.is_empty()).collect();
                if list.is_empty() || list.len() > BINDING_NAMES_MAX {
                    return Err(Error::BadRequest(format!("a binding's names are 1–{BINDING_NAMES_MAX} DNS names or IP addresses (leave them out for the CA's whole policy)")));
                }
                if let Some(bad) = list.iter().find(|s| s.starts_with("*.") || !policy.allows(s)) {
                    return Err(Error::BadRequest(format!("{ca_name} doesn't sign for {bad}: a binding's names are within {} (each takes itself and every name under it)", policy.domains.join(", "))));
                }
                Some(list)
            }
        };
        let now = self.db.now_ms()?;
        let expires_at = match n.days {
            Some(d) if !(1..=3650).contains(&d) => return Err(Error::BadRequest("days is 1–3650 (leave it out for a binding that lasts until it's revoked)".into())),
            Some(d) => Some(now + i64::from(d) * 86_400_000),
            None => None,
        };
        let id = names::new_id("eab", 20);
        let mut raw = [0u8; 32];
        getrandom::fill(&mut raw).expect("system randomness");
        let key = Value::new(b64(&raw));
        let sealed = dek.seal(&eab_aad(project, &id), &key);
        let names_json = names.as_ref().map(crate::json);
        self.db.write(|c| {
            let live: i64 = c.query_row("SELECT COUNT(*) FROM acme_bindings WHERE ca_id = ? AND revoked_at IS NULL", [ca], |r| r.get(0))?;
            if live >= BINDINGS_MAX {
                return Err(Error::Conflict("limit", format!("{ca_name} has {BINDINGS_MAX} bindings: revoke one first")));
            }
            c.execute(
                "INSERT INTO acme_bindings (id, ca_id, project_id, hmac, label, names, expires_at, created_at, created_by) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?)",
                params![id, ca, project, sealed, label, names_json, expires_at, now, caller.principal],
            )?;
            let scope = names.as_ref().map_or_else(|| "the CA's whole policy".to_string(), |n| n.join(", "));
            let label = if label.is_empty() { String::new() } else { format!(" ({label})") };
            audit::record(c, project, caller, "ca.acme.eab.add", None, None, &[], &format!("{ca_name}: made ACME binding {id}{label} for {scope}"), now)
        })?;
        let binding = Binding { id: id.clone(), label, names, expires_at, created_at: now, created_by: caller.principal.clone(), revoked_at: None, revoked_by: None, used_at: None, accounts: 0 };
        Ok(NewKey { binding, key_id: id, hmac_key: key.expose().to_string(), directory: format!("{}/directory", self.acme_base(ca)?) })
    }

    /// Revoke a binding (admin, audited): every account made with it stops at once.
    pub fn revoke_acme_binding(&self, project: &str, ca: &str, id: &str, caller: &Caller) -> Result<()> {
        caller.need(Action::Admin, &Scope::project(project))?;
        self.project(project)?;
        let (_, ca_name, _, _) = self.ca_of(Some(project), ca)?;
        let now = self.db.now_ms()?;
        self.db.write(|c| {
            let revoked: Option<Option<i64>> = c.query_row("SELECT revoked_at FROM acme_bindings WHERE id = ? AND ca_id = ?", [id, ca], |r| r.get(0)).optional()?;
            match revoked {
                None => Err(Error::NotFound(format!("{ca_name} has no ACME binding {id}"))),
                Some(Some(_)) => Ok(()),
                Some(None) => {
                    c.execute("UPDATE acme_bindings SET revoked_at = ?, revoked_by = ? WHERE id = ?", params![now, caller.principal, id])?;
                    audit::record(c, project, caller, "ca.acme.eab.revoke", None, None, &[], &format!("{ca_name}: revoked ACME binding {id}; its accounts stop"), now)
                }
            }
        })
    }

    /// Expired nonces, orders and authorizations a day past their end (an authorization once no
    /// order lists it: a reused one outlives the order that made it, never the orders reusing it),
    /// and certificates past theirs.
    pub(crate) fn sweep_acme(&self) -> Result<()> {
        let now = self.db.now_ms()?;
        self.db.write(|c| {
            c.execute("DELETE FROM acme_nonces WHERE expires_at < ?", [code_sql::now_ms()])?;
            let old = c.query("SELECT id FROM acme_orders WHERE expires_at < ? LIMIT 500", [now - KEEP_MS], |r| r.get::<_, String>(0))?;
            for o in &old {
                c.execute("DELETE FROM acme_order_authz WHERE order_id = ?", [o])?;
                c.execute("DELETE FROM acme_orders WHERE id = ?", [o])?;
            }
            let gone = c.query(
                "SELECT z.id FROM acme_authz z WHERE z.expires_at < ? AND NOT EXISTS (SELECT 1 FROM acme_order_authz l WHERE l.authz_id = z.id) LIMIT 500",
                [now - KEEP_MS],
                |r| r.get::<_, String>(0),
            )?;
            for z in &gone {
                c.execute("DELETE FROM acme_challenges WHERE authz_id = ?", [z])?;
                c.execute("DELETE FROM acme_authz WHERE id = ?", [z])?;
            }
            c.execute("DELETE FROM acme_certs WHERE not_after < ?", [now])?;
            Ok(())
        })
    }
}

/// ACME's rows for a CA, when the CA goes.
pub(crate) fn forget_ca(c: &Conn<'_>, ca: &str) -> Result<()> {
    c.execute("DELETE FROM acme_order_authz WHERE order_id IN (SELECT id FROM acme_orders WHERE ca_id = ?)", [ca])?;
    for t in ["acme_bindings", "acme_accounts", "acme_orders", "acme_authz", "acme_challenges", "acme_certs"] {
        c.execute(&format!("DELETE FROM {t} WHERE ca_id = ?"), [ca])?;
    }
    Ok(())
}
