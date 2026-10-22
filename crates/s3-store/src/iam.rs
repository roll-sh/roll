//! IAM entities, per account: users, groups, customer-managed policies, virtual MFA devices and
//! OpenID Connect providers. Roles are rows of `roles` (`sts.rs`); access keys rows of `keys`
//! (a user's keys name it in `user_name`).
//!
//! Each entity is a JSON document in `iam (account_id, kind, name, value)`, `name` lowercase:
//! IAM names are unique per account regardless of case. The IAM query API (`s3-server`) does
//! its checks and edits inside one transaction ([`Store::iam_write`]), so they're atomic and
//! durable like every other metadata write. Policy documents are stored as given (minified);
//! `s3-server` parses and evaluates them.

use crate::error::{Result, StoreError};
use crate::model::{AccessKey, Permission};
use crate::store::{KEY_COLS, Store, insert_key, key_row};
use crate::sts::{Role, find_role};
use rusqlite::{Connection, OptionalExtension, params};
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

/// IAM's default quotas.
pub const MAX_USERS: usize = 5000;
pub const MAX_GROUPS: usize = 300;
pub const MAX_ROLES: usize = 1000;
pub const MAX_POLICIES: usize = 1500;
pub const MAX_KEYS_PER_USER: usize = 2;
pub const MAX_GROUPS_PER_USER: usize = 10;
pub const MAX_ATTACHED: usize = 10;
pub const MAX_POLICY_VERSIONS: usize = 5;
pub const MAX_MFA_PER_USER: usize = 8;
pub const MAX_OIDC_PROVIDERS: usize = 100;
/// Document sizes (characters, whitespace not counted for managed policies on AWS; roll counts
/// the minified document).
pub const MAX_MANAGED_POLICY_BYTES: usize = 6144;
/// Inline policies, all of an entity's together.
pub const MAX_USER_INLINE_BYTES: usize = 2048;
pub const MAX_GROUP_INLINE_BYTES: usize = 5120;
pub const MAX_ROLE_INLINE_BYTES: usize = 10_240;

pub mod kind {
    pub const USER: &str = "user";
    pub const GROUP: &str = "group";
    pub const POLICY: &str = "policy";
    pub const MFA: &str = "mfa";
    pub const OIDC: &str = "oidc";
}

/// An IAM-style unique id: a 4-letter prefix (`AIDA` users, `AGPA` groups, `ANPA` policies,
/// `AROA` roles) and 17 random upper base32 characters.
pub fn entity_id(prefix: &str) -> String {
    const A: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZ234567";
    let mut b = [0u8; 17];
    getrandom::fill(&mut b).expect("os randomness");
    format!("{prefix}{}", b.iter().map(|x| A[*x as usize % 32] as char).collect::<String>())
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct User {
    pub name: String,
    pub id: String,
    pub path: String,
    pub created_at: i64,
    pub tags: Vec<(String, String)>,
    /// Inline policies by name.
    pub policies: BTreeMap<String, String>,
    /// Managed policy ARNs.
    pub attached: Vec<String>,
    /// Group names (as the groups spell them).
    pub groups: Vec<String>,
    /// Permissions boundary (a managed policy ARN).
    pub boundary: Option<String>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Group {
    pub name: String,
    pub id: String,
    pub path: String,
    pub created_at: i64,
    pub policies: BTreeMap<String, String>,
    pub attached: Vec<String>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct PolicyVersion {
    /// `v1`, `v2`, …: the number.
    pub id: u32,
    pub document: String,
    pub created_at: i64,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct ManagedPolicy {
    pub name: String,
    pub id: String,
    pub path: String,
    pub description: String,
    pub created_at: i64,
    pub updated_at: i64,
    pub default_version: u32,
    pub versions: Vec<PolicyVersion>,
    /// The next version's number (never reused).
    pub next_version: u32,
    pub tags: Vec<(String, String)>,
}

impl ManagedPolicy {
    /// The default version's document.
    pub fn document(&self) -> Option<&str> {
        self.versions.iter().find(|v| v.id == self.default_version).map(|v| v.document.as_str())
    }
}

/// A virtual MFA device (TOTP, RFC 6238).
#[derive(Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct MfaDevice {
    /// `<path><name>` as in the serial number.
    pub name: String,
    pub serial: String,
    /// The shared secret, Base32 (RFC 4648, no padding). Never logged.
    pub seed: String,
    /// The IAM user it's enabled for …
    pub user: Option<String>,
    /// … or the account's root user.
    pub root: bool,
    /// When it was enabled (Unix ms); `None`: created, not enabled.
    pub enabled_at: Option<i64>,
    /// The last time step a code was accepted for: codes of that step or older are refused
    /// (no replay).
    pub last_step: i64,
    pub created_at: i64,
    pub tags: Vec<(String, String)>,
}

impl std::fmt::Debug for MfaDevice {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MfaDevice").field("serial", &self.serial).field("user", &self.user).field("root", &self.root).field("enabled_at", &self.enabled_at).finish_non_exhaustive()
    }
}

/// An IAM OpenID Connect provider (`CreateOpenIDConnectProvider`): its issuer URL (`https://…`),
/// client ids (audiences) and thumbprints.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct OidcProviderEntry {
    pub url: String,
    pub client_ids: Vec<String>,
    pub thumbprints: Vec<String>,
    pub created_at: i64,
    pub tags: Vec<(String, String)>,
}

impl OidcProviderEntry {
    /// The issuer without `https://` and trailing `/`: the name in its ARN and condition keys.
    pub fn name(&self) -> String {
        crate::sts::provider_name(&self.url)
    }
}

/// The metadata inside an IAM transaction ([`Store::iam_read`], [`Store::iam_write`]).
pub struct Iam<'a> {
    c: &'a Connection,
    now: i64,
}

impl Iam<'_> {
    pub fn now(&self) -> i64 {
        self.now
    }

    pub fn get<T: DeserializeOwned>(&self, account: &str, kind: &str, name: &str) -> Result<Option<T>> {
        let v: Option<String> = self.c.prepare_cached("SELECT value FROM iam WHERE account_id = ?1 AND kind = ?2 AND name = ?3")?.query_row(params![account, kind, name.to_lowercase()], |r| r.get(0)).optional()?;
        v.map(|v| serde_json::from_str(&v).map_err(StoreError::from)).transpose()
    }

    /// Every entity of a kind, in name order.
    pub fn list<T: DeserializeOwned>(&self, account: &str, kind: &str) -> Result<Vec<T>> {
        let rows = self.c.prepare_cached("SELECT value FROM iam WHERE account_id = ?1 AND kind = ?2 ORDER BY name")?.query_map(params![account, kind], |r| r.get::<_, String>(0))?.collect::<rusqlite::Result<Vec<_>>>()?;
        rows.iter().map(|v| serde_json::from_str(v).map_err(StoreError::from)).collect()
    }

    pub fn count(&self, account: &str, kind: &str) -> Result<usize> {
        Ok(self.c.prepare_cached("SELECT count(*) FROM iam WHERE account_id = ?1 AND kind = ?2")?.query_row(params![account, kind], |r| r.get::<_, i64>(0))? as usize)
    }

    pub fn exists(&self, account: &str, kind: &str, name: &str) -> Result<bool> {
        Ok(self.c.prepare_cached("SELECT 1 FROM iam WHERE account_id = ?1 AND kind = ?2 AND name = ?3")?.exists(params![account, kind, name.to_lowercase()])?)
    }

    /// Insert or replace.
    pub fn put<T: Serialize>(&self, account: &str, kind: &str, name: &str, v: &T) -> Result<()> {
        self.c.execute(
            "INSERT INTO iam (account_id, kind, name, value) VALUES (?1, ?2, ?3, ?4) ON CONFLICT (account_id, kind, name) DO UPDATE SET value = excluded.value",
            params![account, kind, name.to_lowercase(), serde_json::to_string(v)?],
        )?;
        Ok(())
    }

    /// Whether there was one.
    pub fn delete(&self, account: &str, kind: &str, name: &str) -> Result<bool> {
        Ok(self.c.execute("DELETE FROM iam WHERE account_id = ?1 AND kind = ?2 AND name = ?3", params![account, kind, name.to_lowercase()])? > 0)
    }

    pub fn account(&self, id: &str) -> Result<Option<crate::accounts::Account>> {
        crate::accounts::find(self.c, id)
    }

    /// A user's access keys.
    pub fn user_keys(&self, account: &str, user: &str) -> Result<Vec<AccessKey>> {
        Ok(self.c.prepare_cached(&format!("SELECT {KEY_COLS} FROM keys WHERE account_id = ?1 AND user_name = ?2 ORDER BY created_at, id"))?.query_map(params![account, user], key_row)?.collect::<rusqlite::Result<Vec<_>>>()?)
    }

    /// The account's keys that belong to no user (the root's and roll permission keys).
    pub fn account_keys(&self, account: &str) -> Result<Vec<AccessKey>> {
        Ok(self.c.prepare_cached(&format!("SELECT {KEY_COLS} FROM keys WHERE account_id = ?1 AND user_name IS NULL ORDER BY created_at, id"))?.query_map([account], key_row)?.collect::<rusqlite::Result<Vec<_>>>()?)
    }

    pub fn key(&self, id: &str) -> Result<Option<AccessKey>> {
        Ok(self.c.prepare_cached(&format!("SELECT {KEY_COLS} FROM keys WHERE id = ?1"))?.query_row([id], key_row).optional()?)
    }

    /// A new access key for an IAM user (its secret is only returned here).
    pub fn create_user_key(&self, account: &str, user: &str) -> Result<AccessKey> {
        let k = AccessKey {
            id: crate::ids::access_key_id(),
            secret: crate::ids::secret_key(),
            name: user.to_string(),
            // A user's key has no roll permission level: an empty scope covers nothing.
            permission: Permission::Read,
            buckets: Some(Vec::new()),
            created_at: self.now,
            last_used_at: None,
            account: account.to_string(),
            user: Some(user.to_string()),
            active: true,
            session: None,
        };
        insert_key(self.c, &k)?;
        Ok(k)
    }

    pub fn set_key_active(&self, id: &str, active: bool) -> Result<()> {
        self.c.execute("UPDATE keys SET status = ?2 WHERE id = ?1", params![id, if active { "Active" } else { "Inactive" }])?;
        Ok(())
    }

    /// A renamed user keeps its keys.
    pub fn rename_user_keys(&self, account: &str, old: &str, new: &str) -> Result<()> {
        self.c.execute("UPDATE keys SET user_name = ?3, name = ?3 WHERE account_id = ?1 AND user_name = ?2", params![account, old, new])?;
        Ok(())
    }

    pub fn delete_key(&self, id: &str) -> Result<bool> {
        Ok(self.c.execute("DELETE FROM keys WHERE id = ?1", [id])? > 0)
    }

    pub fn role(&self, account: &str, name: &str) -> Result<Option<Role>> {
        find_role(self.c, account, name)
    }

    pub fn roles(&self, account: &str) -> Result<Vec<Role>> {
        let names: Vec<String> = self.c.prepare_cached("SELECT name FROM roles WHERE account_id = ?1 ORDER BY name")?.query_map([account], |r| r.get(0))?.collect::<rusqlite::Result<Vec<_>>>()?;
        let mut out = Vec::with_capacity(names.len());
        for n in names {
            out.extend(find_role(self.c, account, &n)?);
        }
        Ok(out)
    }

    /// The MFA devices enabled for a user (`Some`) or the root user (`None`).
    pub fn mfa_devices_of(&self, account: &str, user: Option<&str>) -> Result<Vec<MfaDevice>> {
        Ok(self
            .list::<MfaDevice>(account, kind::MFA)?
            .into_iter()
            .filter(|d| d.enabled_at.is_some() && match user {
                Some(u) => d.user.as_deref().is_some_and(|x| x.eq_ignore_ascii_case(u)),
                None => d.root,
            })
            .collect())
    }
}

impl Store {
    /// Read IAM entities.
    pub fn iam_read<T>(&self, f: impl FnOnce(&Iam) -> Result<T>) -> Result<T> {
        let now = self.now();
        self.read(|c| f(&Iam { c, now }))
    }

    /// Change IAM entities in one durable transaction: an `Err` from `f` changes nothing.
    pub async fn iam_write<T>(&self, f: impl FnOnce(&Iam) -> Result<T>) -> Result<T> {
        let now = self.now();
        self.write(|c| f(&Iam { c, now })).await
    }

    /// An IAM user.
    pub fn iam_user(&self, account: &str, name: &str) -> Result<Option<User>> {
        self.iam_read(|t| t.get(account, kind::USER, name))
    }

    pub fn iam_group(&self, account: &str, name: &str) -> Result<Option<Group>> {
        self.iam_read(|t| t.get(account, kind::GROUP, name))
    }

    /// A customer-managed policy by name.
    pub fn iam_policy(&self, account: &str, name: &str) -> Result<Option<ManagedPolicy>> {
        self.iam_read(|t| t.get(account, kind::POLICY, name))
    }

    pub fn iam_oidc_provider(&self, account: &str, name: &str) -> Result<Option<OidcProviderEntry>> {
        self.iam_read(|t| t.get(account, kind::OIDC, name))
    }

    /// The enabled MFA device with this serial number, if it belongs to `account`.
    pub fn mfa_device(&self, account: &str, serial: &str) -> Result<Option<MfaDevice>> {
        let Some(name) = mfa_name_of(account, serial) else { return Ok(None) };
        self.iam_read(|t| t.get(account, kind::MFA, &name))
    }

    /// Accept a TOTP time step for a device once: `Ok(false)` if `step` isn't newer than the
    /// last one accepted (a replayed or older code). Durable before it returns.
    pub async fn use_mfa_step(&self, account: &str, serial: &str, step: i64) -> Result<bool> {
        let Some(name) = mfa_name_of(account, serial) else { return Ok(false) };
        self.iam_write(|t| {
            let Some(mut d) = t.get::<MfaDevice>(account, kind::MFA, &name)? else { return Ok(false) };
            if d.enabled_at.is_none() || step <= d.last_step {
                return Ok(false);
            }
            d.last_step = step;
            t.put(account, kind::MFA, &name, &d)?;
            Ok(true)
        })
        .await
    }
}

/// `arn:aws:iam::<account>:mfa/<path+name>` → the device's key (`path+name` without the
/// leading `/`), if the serial is one of `account`'s.
pub fn mfa_name_of(account: &str, serial: &str) -> Option<String> {
    let rest = serial.trim().strip_prefix("arn:aws:iam::")?.strip_prefix(account)?.strip_prefix(":mfa/")?;
    (!rest.is_empty() && rest.len() <= 512).then(|| rest.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ids_and_serials() {
        let id = entity_id("AIDA");
        assert_eq!(id.len(), 21);
        assert!(id.starts_with("AIDA") && id.bytes().all(|b| b.is_ascii_uppercase() || b.is_ascii_digit()));
        assert_eq!(mfa_name_of("123456789012", "arn:aws:iam::123456789012:mfa/alice"), Some("alice".into()));
        assert_eq!(mfa_name_of("123456789012", "arn:aws:iam::123456789012:mfa/team/alice"), Some("team/alice".into()));
        assert_eq!(mfa_name_of("123456789012", "arn:aws:iam::999999999999:mfa/alice"), None);
        assert_eq!(mfa_name_of("123456789012", "arn:aws:iam::123456789012:mfa/"), None);
        let d = MfaDevice { seed: "SECRETSEED".into(), ..Default::default() };
        assert!(!format!("{d:?}").contains("SECRETSEED"));
    }
}
