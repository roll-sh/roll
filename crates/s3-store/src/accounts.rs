//! Accounts: who owns buckets, objects, keys, roles and IAM entities.
//!
//! Every account has a 12-digit id (ARNs, `x-amz-expected-bucket-owner`, `aws:PrincipalAccount`),
//! a 64-hex-char canonical user id (ACLs, `Owner` elements), a display name and an optional
//! email address (ACL grants by email). The deployment's original owner is the **default
//! account**: its canonical id is the canonical owner id (`config.rs`), its account id derived
//! from that as before, so ids from before accounts existed keep meaning the same thing.
//!
//! Bucket names stay unique across the deployment (as S3's are across AWS); a bucket belongs to
//! the account that created it.

use crate::error::{Result, StoreError};
use crate::store::Store;
use rusqlite::{Connection, OptionalExtension, params};
use serde::Serialize;

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct Account {
    /// 12 digits.
    pub id: String,
    /// 64 lowercase hex characters.
    pub canonical: String,
    /// The display name ACLs and listings show.
    pub name: String,
    /// Grants by email address (`x-amz-grant-*: emailAddress=…`) resolve to the account with it.
    pub email: Option<String>,
    pub created_at: i64,
}

const COLS: &str = "id, canonical, name, email, created_at";

fn row(r: &rusqlite::Row) -> rusqlite::Result<Account> {
    Ok(Account { id: r.get(0)?, canonical: r.get(1)?, name: r.get(2)?, email: r.get(3)?, created_at: r.get(4)? })
}

pub(crate) fn find(c: &Connection, id: &str) -> Result<Option<Account>> {
    Ok(c.prepare_cached(&format!("SELECT {COLS} FROM accounts WHERE id = ?1"))?.query_row([id], row).optional()?)
}

fn invalid(why: impl Into<String>) -> StoreError {
    StoreError::InvalidArgument(why.into())
}

/// Display names: 1-64 printable characters.
pub fn validate_name(name: &str) -> Result<()> {
    if name.trim().is_empty() || name.chars().count() > 64 || name.chars().any(char::is_control) {
        return Err(invalid("account names are 1-64 printable characters"));
    }
    Ok(())
}

/// A plausible email address (one `@`, something on both sides, no spaces), at most 254
/// characters. Stored lowercase: grants by email are case-insensitive.
pub fn normalize_email(email: &str) -> Result<String> {
    let e = email.trim().to_ascii_lowercase();
    let ok = e.len() <= 254 && !e.contains(char::is_whitespace) && e.split('@').count() == 2 && e.split('@').all(|p| !p.is_empty());
    if !ok {
        return Err(invalid(format!("{email:?} isn't an email address")));
    }
    Ok(e)
}

fn random_digits(n: usize) -> String {
    // 250 is a multiple of 10: reject the rest to keep the digits uniform.
    let mut out = String::with_capacity(n);
    while out.len() < n {
        let mut x = [0u8; 1];
        getrandom::fill(&mut x).expect("os randomness");
        if x[0] < 250 {
            out.push(char::from(b'0' + x[0] % 10));
        }
    }
    out
}

/// What to change on an account; `None` leaves a field as it is. `email: Some(None)` removes it.
#[derive(Clone, Debug, Default, serde::Deserialize)]
pub struct AccountUpdate {
    pub name: Option<String>,
    #[serde(default, with = "double_option")]
    pub email: Option<Option<String>>,
}

mod double_option {
    use serde::{Deserialize, Deserializer};
    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<Option<Option<String>>, D::Error> {
        Ok(Some(Option::deserialize(d)?))
    }
}

impl Store {
    /// Make sure the default account exists (the canonical owner id must exist already).
    pub(crate) async fn ensure_default_account(&self) -> Result<()> {
        let owner = self.owner_id();
        let id = crate::config::account_id(&owner);
        if self.read(|c| find(c, &id))?.is_some() {
            return Ok(());
        }
        let now = self.now();
        self.write(|c| {
            c.execute("INSERT INTO accounts (id, canonical, name, email, created_at) VALUES (?1, ?2, 'default', NULL, ?3) ON CONFLICT (id) DO NOTHING", params![id, owner, now])?;
            Ok(())
        })
        .await
    }

    /// The default account's id (the deployment's account before there were others).
    pub fn default_account_id(&self) -> String {
        self.account_id()
    }

    pub fn accounts(&self) -> Result<Vec<Account>> {
        self.read(|c| Ok(c.prepare(&format!("SELECT {COLS} FROM accounts ORDER BY created_at, id"))?.query_map([], row)?.collect::<rusqlite::Result<Vec<_>>>()?))
    }

    pub fn account(&self, id: &str) -> Result<Option<Account>> {
        self.read(|c| find(c, id))
    }

    /// The account an id names: `NotFound` if there's none.
    pub fn require_account(&self, id: &str) -> Result<Account> {
        self.account(id)?.ok_or_else(|| StoreError::NotFound(format!("no account {id}")))
    }

    /// The account with this canonical user id.
    pub fn account_by_canonical(&self, canonical: &str) -> Result<Option<Account>> {
        let canonical = canonical.trim().to_ascii_lowercase();
        self.read(|c| Ok(c.prepare_cached(&format!("SELECT {COLS} FROM accounts WHERE canonical = ?1"))?.query_row([canonical], row).optional()?))
    }

    /// The account with this email address (case-insensitive).
    pub fn account_by_email(&self, email: &str) -> Result<Option<Account>> {
        let Ok(email) = normalize_email(email) else { return Ok(None) };
        self.read(|c| Ok(c.prepare_cached(&format!("SELECT {COLS} FROM accounts WHERE email = ?1"))?.query_row([email], row).optional()?))
    }

    /// A new account with a random 12-digit id and canonical user id.
    pub async fn create_account(&self, name: &str, email: Option<&str>) -> Result<Account> {
        validate_name(name)?;
        let email = email.map(normalize_email).transpose()?;
        let now = self.now();
        let mut seed = [0u8; 32];
        getrandom::fill(&mut seed).map_err(|e| StoreError::internal(format!("randomness: {e}")))?;
        let canonical = hex::encode(seed);
        let name = name.trim().to_string();
        let id = self
            .write(|c| {
                if let Some(e) = &email
                    && c.prepare_cached("SELECT 1 FROM accounts WHERE email = ?1")?.exists([e])?
                {
                    return Err(StoreError::Conflict(format!("an account with email {e} already exists")));
                }
                // Ids don't start with 0 and never collide.
                for _ in 0..64 {
                    let first = random_digits(12).bytes().find(|b| *b != b'0').map_or('1', char::from);
                    let id = format!("{first}{}", random_digits(11));
                    if find(c, &id)?.is_none() {
                        c.execute("INSERT INTO accounts (id, canonical, name, email, created_at) VALUES (?1, ?2, ?3, ?4, ?5)", params![id, canonical, name, email, now])?;
                        return Ok(id);
                    }
                }
                Err(StoreError::internal("couldn't pick an unused account id"))
            })
            .await?;
        self.require_account(&id)
    }

    pub async fn update_account(&self, id: &str, u: AccountUpdate) -> Result<Account> {
        if let Some(n) = &u.name {
            validate_name(n)?;
        }
        let email = match &u.email {
            Some(Some(e)) => Some(Some(normalize_email(e)?)),
            Some(None) => Some(None),
            None => None,
        };
        self.write(|c| {
            find(c, id)?.ok_or_else(|| StoreError::NotFound(format!("no account {id}")))?;
            if let Some(n) = &u.name {
                c.execute("UPDATE accounts SET name = ?2 WHERE id = ?1", params![id, n.trim()])?;
            }
            if let Some(e) = &email {
                if let Some(e) = e
                    && c.prepare_cached("SELECT 1 FROM accounts WHERE email = ?1 AND id != ?2")?.exists(params![e, id])?
                {
                    return Err(StoreError::Conflict(format!("an account with email {e} already exists")));
                }
                c.execute("UPDATE accounts SET email = ?2 WHERE id = ?1", params![id, e])?;
            }
            Ok(())
        })
        .await?;
        self.require_account(id)
    }

    /// Delete an account that owns no buckets, with its keys, roles and IAM entities. The
    /// default account can't be deleted.
    pub async fn delete_account(&self, id: &str) -> Result<()> {
        if id == self.default_account_id() {
            return Err(StoreError::InvalidRequest("the default account can't be deleted".into()));
        }
        self.write(|c| {
            find(c, id)?.ok_or_else(|| StoreError::NotFound(format!("no account {id}")))?;
            if c.prepare_cached("SELECT 1 FROM buckets WHERE account_id = ?1 LIMIT 1")?.exists([id])? {
                return Err(StoreError::DeleteConflict(format!("account {id} still owns buckets; delete them first")));
            }
            for table in ["keys", "roles", "iam"] {
                c.execute(&format!("DELETE FROM {table} WHERE account_id = ?1"), [id])?;
            }
            c.execute("DELETE FROM accounts WHERE id = ?1", [id])?;
            Ok(())
        })
        .await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn emails_and_names() {
        assert_eq!(normalize_email(" Alt@Example.COM ").unwrap(), "alt@example.com");
        for bad in ["", "a", "a@", "@b", "a@b@c", "a b@c"] {
            assert!(normalize_email(bad).is_err(), "{bad}");
        }
        assert!(validate_name("main").is_ok());
        assert!(validate_name(" ").is_err());
        assert!(validate_name(&"x".repeat(65)).is_err());
        let d = random_digits(11);
        assert!(d.len() == 11 && d.bytes().all(|b| b.is_ascii_digit()));
    }
}
