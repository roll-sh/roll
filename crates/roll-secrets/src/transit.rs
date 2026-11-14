//! Encryption as a service (docs/secrets.md, "Encryption keys"), as Vault's Transit engine and
//! Infisical's KMS give: an app sends data and gets ciphertext back under a named key that never
//! leaves roll, so it can keep encrypted fields in its own database without holding a key.
//!
//! - A key is versioned: each version is 32 random bytes, sealed with the project's data key
//!   (bound to the key and version), so customer-managed keys cover it too. `rotate` adds a
//!   version that new encryptions use; older versions keep decrypting until `min_decrypt_version`
//!   retires them, and `rewrap` moves a ciphertext to the newest without showing its plaintext.
//! - A ciphertext is `roll:v<version>:<base64url(nonce ‖ AES-256-GCM)>`, with an optional
//!   `context` bound in (a row's id, say): the same ciphertext under another context won't open.
//! - `datakey` hands out a fresh data key and its ciphertext, for encrypting large data locally.
//! - Encrypting and rewrapping are `write`; decrypting and data keys are `reveal`, and audited.

use crate::access::{Action, Caller, Scope};
use crate::crypto::{open_with, seal_with};
use crate::{Error, Result, Secrets, Value, audit, names};
use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use code_sql::{OptionalExtension, params};
use serde::{Deserialize, Serialize};
use serde_json::{Value as Json, json};

pub(crate) const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS transit_keys (
    id TEXT PRIMARY KEY,
    project_id TEXT NOT NULL,
    name TEXT NOT NULL,
    versions TEXT NOT NULL,
    min_decrypt_version INTEGER NOT NULL,
    created_at INTEGER NOT NULL,
    created_by TEXT NOT NULL,
    UNIQUE (project_id, name)
) WITHOUT ROWID;
";

/// The most of one plaintext, and of one batch.
const ITEM_MAX: usize = 64 * 1024;
const BATCH_MAX: usize = 1000;
const BATCH_BYTES: usize = 4 << 20;

/// One version as stored: its key sealed with the project's data key.
#[derive(Serialize, Deserialize, Clone)]
struct Stored {
    v: u32,
    key: String,
    at: i64,
}

#[derive(Debug, Serialize, PartialEq)]
pub struct KeyVersion {
    pub version: u32,
    pub created_at: i64,
}

#[derive(Debug, Serialize, PartialEq)]
pub struct TransitKey {
    pub id: String,
    pub name: String,
    pub latest_version: u32,
    pub min_decrypt_version: u32,
    pub versions: Vec<KeyVersion>,
    pub created_at: i64,
    pub created_by: String,
}

/// One item to encrypt: base64 plaintext, and the context bound to it.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Plain {
    pub plaintext: String,
    #[serde(default)]
    pub context: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Cipher {
    pub ciphertext: String,
    #[serde(default)]
    pub context: Option<String>,
}

fn version_aad(project: &str, id: &str, v: u32) -> String {
    format!("roll-secrets:transit:{project}:{id}:{v}")
}

/// What a ciphertext is bound to besides its key: the context. Not the key's id, which a restore
/// into another project changes (the key's bytes, random per version, keep keys apart).
fn data_aad(context: Option<&str>) -> String {
    format!("roll-transit:v1:{}", context.unwrap_or(""))
}

fn check_name(n: &str) -> Result<()> {
    if n.is_empty() || n.len() > 64 || !n.chars().all(|c| c.is_ascii_alphanumeric() || "-_".contains(c)) {
        return Err(Error::BadRequest("a key's name is letters, digits, - and _ (64 at most)".into()));
    }
    Ok(())
}

fn check_context(c: Option<&str>) -> Result<()> {
    if c.is_some_and(|c| c.len() > 512) {
        return Err(Error::BadRequest("a context is 512 bytes at most".into()));
    }
    Ok(())
}

/// A ciphertext's version and bytes.
fn parse(ct: &str) -> Result<(u32, Vec<u8>)> {
    let bad = || Error::BadRequest("a ciphertext is roll:v<version>:<base64url>".into());
    let rest = ct.trim().strip_prefix("roll:v").ok_or_else(bad)?;
    let (v, b) = rest.split_once(':').ok_or_else(bad)?;
    Ok((v.parse().map_err(|_| bad())?, URL_SAFE_NO_PAD.decode(b).map_err(|_| bad())?))
}

/// A key's id, versions (opened) and the oldest version that still decrypts.
struct Opened {
    id: String,
    versions: Vec<(u32, zeroize::Zeroizing<[u8; 32]>)>,
    min: u32,
}

impl Opened {
    fn latest(&self) -> &(u32, zeroize::Zeroizing<[u8; 32]>) {
        self.versions.last().expect("a key has a version")
    }

    fn version(&self, v: u32) -> Result<&zeroize::Zeroizing<[u8; 32]>> {
        if v < self.min {
            return Err(Error::BadRequest(format!("version {v} is retired (the oldest that decrypts is {})", self.min)));
        }
        self.versions.iter().find(|(n, _)| *n == v).map(|(_, k)| k).ok_or_else(|| Error::BadRequest(format!("no version {v}")))
    }

    fn encrypt(&self, plain: &[u8], context: Option<&str>) -> String {
        let (v, key) = self.latest();
        format!("roll:v{v}:{}", URL_SAFE_NO_PAD.encode(seal_with(key, &data_aad(context), plain)))
    }

    fn decrypt(&self, ct: &str, context: Option<&str>) -> Result<zeroize::Zeroizing<Vec<u8>>> {
        let (v, raw) = parse(ct)?;
        open_with(self.version(v)?, &data_aad(context), &raw).map_err(|_| Error::BadRequest("the ciphertext doesn't open under this key and context".into()))
    }
}

impl Secrets {
    fn transit_open(&self, project: &str, name: &str) -> Result<Opened> {
        let dek = self.project(project)?;
        let row: Option<(String, String, i64)> = self.db.read(|c| c.query_row("SELECT id, versions, min_decrypt_version FROM transit_keys WHERE project_id = ? AND name = ?", [project, name], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?))).optional())?;
        let (id, versions, min) = row.ok_or_else(|| Error::NotFound(format!("no key {name}")))?;
        let stored: Vec<Stored> = serde_json::from_str(&versions).map_err(|_| Error::Internal("a key's versions aren't JSON".into()))?;
        let mut out = Vec::with_capacity(stored.len());
        for s in stored {
            let sealed = URL_SAFE_NO_PAD.decode(&s.key).map_err(|_| Error::Internal("a key version is damaged".into()))?;
            let raw = URL_SAFE_NO_PAD.decode(dek.open(&version_aad(project, &id, s.v), &sealed)?.expose()).map_err(|_| Error::Internal("a key version is damaged".into()))?;
            let key: [u8; 32] = raw.as_slice().try_into().map_err(|_| Error::Internal("a key version is damaged".into()))?;
            out.push((s.v, zeroize::Zeroizing::new(key)));
        }
        Ok(Opened { id, versions: out, min: min as u32 })
    }

    /// A new version's key, sealed for the store.
    fn transit_version(&self, project: &str, id: &str, v: u32) -> Result<Stored> {
        let dek = self.project(project)?;
        let mut key = zeroize::Zeroizing::new([0u8; 32]);
        getrandom::fill(key.as_mut()).map_err(|e| Error::Internal(e.to_string()))?;
        let sealed = dek.seal(&version_aad(project, id, v), &Value::new(URL_SAFE_NO_PAD.encode(key.as_ref())));
        Ok(Stored { v, key: URL_SAFE_NO_PAD.encode(sealed), at: self.db.now_ms()? })
    }

    pub fn transit_keys(&self, project: &str, caller: &Caller) -> Result<Vec<TransitKey>> {
        caller.need(Action::Read, &Scope::project(project))?;
        self.project(project)?;
        self.db.read(|c| {
            Ok(c.query("SELECT id, name, versions, min_decrypt_version, created_at, created_by FROM transit_keys WHERE project_id = ? ORDER BY name", [project], |r| {
                Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?, r.get::<_, String>(2)?, r.get::<_, i64>(3)?, r.get(4)?, r.get(5)?))
            })?
            .into_iter()
            .map(|(id, name, versions, min, created_at, created_by)| {
                let versions: Vec<KeyVersion> = serde_json::from_str::<Vec<Stored>>(&versions).unwrap_or_default().into_iter().map(|s| KeyVersion { version: s.v, created_at: s.at }).collect();
                TransitKey { id, name, latest_version: versions.last().map_or(0, |v| v.version), min_decrypt_version: min as u32, versions, created_at, created_by }
            })
            .collect())
        })
    }

    pub fn add_transit_key(&self, project: &str, name: &str, caller: &Caller) -> Result<TransitKey> {
        caller.need(Action::Admin, &Scope::project(project))?;
        check_name(name)?;
        self.project(project)?;
        let id = names::new_id("tk", 16);
        let first = self.transit_version(project, &id, 1)?;
        let now = self.db.now_ms()?;
        self.db.write(|c| {
            if c.query_row("SELECT 1 FROM transit_keys WHERE project_id = ? AND name = ?", [project, name], |_| Ok(())).optional()?.is_some() {
                return Err(Error::Conflict("exists", format!("a key named {name} exists")));
            }
            c.execute(
                "INSERT INTO transit_keys (id, project_id, name, versions, min_decrypt_version, created_at, created_by) VALUES (?, ?, ?, ?, 1, ?, ?)",
                params![id, project, name, serde_json::to_string(&[&first]).expect("JSON"), now, caller.principal],
            )?;
            audit::record(c, project, caller, "transit.add", None, None, &[], &format!("made encryption key {name}"), now)
        })?;
        self.transit_keys(project, caller)?.into_iter().find(|k| k.name == name).ok_or_else(|| Error::Internal("the key vanished".into()))
    }

    /// A new version that encryptions use from now on.
    pub fn rotate_transit_key(&self, project: &str, name: &str, caller: &Caller) -> Result<TransitKey> {
        caller.need(Action::Admin, &Scope::project(project))?;
        let (id, latest) = self.transit_open(project, name).map(|o| (o.id.clone(), o.latest().0))?;
        let next = self.transit_version(project, &id, latest + 1)?;
        let now = self.db.now_ms()?;
        self.db.write(|c| {
            let versions: String = c.query_row("SELECT versions FROM transit_keys WHERE id = ?", [&id], |r| r.get(0))?;
            let mut stored: Vec<Stored> = serde_json::from_str(&versions).map_err(|_| Error::Internal("a key's versions aren't JSON".into()))?;
            // Another rotation got there first: keep its version, not two of the same number.
            if stored.last().map(|s| s.v) != Some(latest) {
                return Err(Error::Conflict("stale", format!("{name} was rotated meanwhile")));
            }
            stored.push(next.clone());
            c.execute("UPDATE transit_keys SET versions = ? WHERE id = ?", params![serde_json::to_string(&stored).expect("JSON"), id])?;
            audit::record(c, project, caller, "transit.rotate", None, None, &[], &format!("rotated encryption key {name} to version {}", next.v), now)
        })?;
        self.transit_keys(project, caller)?.into_iter().find(|k| k.name == name).ok_or_else(|| Error::Internal("the key vanished".into()))
    }

    /// Retire every version older than `min` (it no longer decrypts; ciphertexts under it should
    /// be rewrapped first).
    pub fn set_min_decrypt_version(&self, project: &str, name: &str, min: u32, caller: &Caller) -> Result<TransitKey> {
        caller.need(Action::Admin, &Scope::project(project))?;
        let latest = self.transit_open(project, name)?.latest().0;
        if !(1..=latest).contains(&min) {
            return Err(Error::BadRequest(format!("min_decrypt_version is 1–{latest}")));
        }
        let now = self.db.now_ms()?;
        self.db.write(|c| {
            c.execute("UPDATE transit_keys SET min_decrypt_version = ? WHERE project_id = ? AND name = ?", params![min, project, name])?;
            audit::record(c, project, caller, "transit.config", None, None, &[], &format!("encryption key {name} decrypts from version {min}"), now)
        })?;
        self.transit_keys(project, caller)?.into_iter().find(|k| k.name == name).ok_or_else(|| Error::Internal("the key vanished".into()))
    }

    /// Delete a key for good: what it encrypted can't be decrypted again. `confirm` repeats its name.
    pub fn delete_transit_key(&self, project: &str, name: &str, confirm: &str, caller: &Caller) -> Result<()> {
        caller.need(Action::Admin, &Scope::project(project))?;
        if confirm != name {
            return Err(Error::BadRequest(format!("deleting {name} makes everything it encrypted unreadable for good: confirm with its name")));
        }
        self.project(project)?;
        let now = self.db.now_ms()?;
        self.db.write(|c| {
            if c.execute("DELETE FROM transit_keys WHERE project_id = ? AND name = ?", [project, name])? == 0 {
                return Err(Error::NotFound(format!("no key {name}")));
            }
            audit::record(c, project, caller, "transit.delete", None, None, &[], &format!("deleted encryption key {name}"), now)
        })
    }

    pub fn transit_encrypt(&self, project: &str, name: &str, items: Vec<Plain>, caller: &Caller) -> Result<Vec<String>> {
        caller.need(Action::Write, &Scope::project(project))?;
        batch_limits(items.iter().map(|i| i.plaintext.len()))?;
        let key = self.transit_open(project, name)?;
        items
            .iter()
            .map(|i| {
                check_context(i.context.as_deref())?;
                let plain = zeroize::Zeroizing::new(base64::engine::general_purpose::STANDARD.decode(&i.plaintext).map_err(|_| Error::BadRequest("plaintext is base64".into()))?);
                if plain.len() > ITEM_MAX {
                    return Err(Error::BadRequest(format!("a plaintext is {} KiB at most", ITEM_MAX / 1024)));
                }
                Ok(key.encrypt(&plain, i.context.as_deref()))
            })
            .collect()
    }

    pub fn transit_decrypt(&self, project: &str, name: &str, items: Vec<Cipher>, caller: &Caller) -> Result<Vec<Value>> {
        caller.need(Action::Reveal, &Scope::project(project))?;
        batch_limits(items.iter().map(|i| i.ciphertext.len()))?;
        let key = self.transit_open(project, name)?;
        let out: Result<Vec<Value>> = items
            .iter()
            .map(|i| {
                check_context(i.context.as_deref())?;
                let plain = key.decrypt(&i.ciphertext, i.context.as_deref())?;
                Ok(Value::new(base64::engine::general_purpose::STANDARD.encode(plain.as_slice())))
            })
            .collect();
        // A failure is audited too: probing contexts or ciphertexts leaves a trace.
        let summary = match &out {
            Ok(v) => format!("decrypted {} with {name}", crate::write::plural(v.len(), "ciphertext")),
            Err(_) => format!("failed to decrypt with {name} ({} given)", crate::write::plural(items.len(), "ciphertext")),
        };
        self.audit_now(project, caller, "transit.decrypt", None, None, &[], &summary);
        out
    }

    /// Re-encrypt under the newest version, without the plaintext leaving roll.
    pub fn transit_rewrap(&self, project: &str, name: &str, items: Vec<Cipher>, caller: &Caller) -> Result<Vec<String>> {
        caller.need(Action::Write, &Scope::project(project))?;
        batch_limits(items.iter().map(|i| i.ciphertext.len()))?;
        let key = self.transit_open(project, name)?;
        let out: Result<Vec<String>> = items
            .iter()
            .map(|i| {
                check_context(i.context.as_deref())?;
                let plain = key.decrypt(&i.ciphertext, i.context.as_deref())?;
                Ok(key.encrypt(&plain, i.context.as_deref()))
            })
            .collect();
        if out.is_err() {
            self.audit_now(project, caller, "transit.rewrap", None, None, &[], &format!("failed to rewrap with {name} ({} given)", crate::write::plural(items.len(), "ciphertext")));
        }
        out
    }

    /// A fresh 256-bit data key and its ciphertext under this key, for envelope encryption done
    /// locally: keep the ciphertext, decrypt it when the data must be read.
    pub fn transit_datakey(&self, project: &str, name: &str, context: Option<String>, caller: &Caller) -> Result<Json> {
        caller.need(Action::Reveal, &Scope::project(project))?;
        check_context(context.as_deref())?;
        let key = self.transit_open(project, name)?;
        let mut data = zeroize::Zeroizing::new([0u8; 32]);
        getrandom::fill(data.as_mut()).map_err(|e| Error::Internal(e.to_string()))?;
        let ciphertext = key.encrypt(data.as_ref(), context.as_deref());
        self.audit_now(project, caller, "transit.datakey", None, None, &[], &format!("made a data key with {name}"));
        Ok(json!({ "plaintext": base64::engine::general_purpose::STANDARD.encode(data.as_ref()), "ciphertext": ciphertext }))
    }
}

/// A key in a backup (backup.rs): its versions' bytes, in an archive sealed to its taker's key, so
/// a restore opens what it encrypted.
#[derive(Debug, Serialize, Deserialize)]
pub struct ArchivedKey {
    pub name: String,
    pub min_decrypt_version: u32,
    /// (version, key as base64url, made at)
    pub versions: Vec<(u32, String, i64)>,
}

impl Secrets {
    pub(crate) fn transit_export(&self, project: &str) -> Result<Vec<ArchivedKey>> {
        let names: Vec<String> = self.db.read(|c| c.query("SELECT name FROM transit_keys WHERE project_id = ? ORDER BY name", [project], |r| r.get(0)))?;
        let mut out = vec![];
        for name in names {
            let o = self.transit_open(project, &name)?;
            let at: Vec<i64> = {
                let s: String = self.db.read(|c| c.query_row("SELECT versions FROM transit_keys WHERE id = ?", [&o.id], |r| r.get(0)))?;
                serde_json::from_str::<Vec<Stored>>(&s).unwrap_or_default().into_iter().map(|s| s.at).collect()
            };
            out.push(ArchivedKey { name, min_decrypt_version: o.min, versions: o.versions.iter().zip(at).map(|((v, k), at)| (*v, URL_SAFE_NO_PAD.encode(k.as_ref()), at)).collect() });
        }
        Ok(out)
    }

    /// Load archived keys; a name the project already has is a warning, not a second key.
    pub(crate) fn transit_import(&self, project: &str, keys: Vec<ArchivedKey>, caller: &Caller) -> Result<Vec<String>> {
        let dek = self.project(project)?;
        let now = self.db.now_ms()?;
        let mut warnings = vec![];
        for k in keys {
            let id = names::new_id("tk", 16);
            let mut stored = vec![];
            for (v, key, at) in &k.versions {
                let sealed = dek.seal(&version_aad(project, &id, *v), &Value::new(key.clone()));
                stored.push(Stored { v: *v, key: URL_SAFE_NO_PAD.encode(sealed), at: *at });
            }
            if stored.is_empty() {
                continue;
            }
            let done = self.db.write(|c| {
                if c.query_row("SELECT 1 FROM transit_keys WHERE project_id = ? AND name = ?", [project, k.name.as_str()], |_| Ok(())).optional()?.is_some() {
                    return Ok::<_, Error>(false);
                }
                c.execute(
                    "INSERT INTO transit_keys (id, project_id, name, versions, min_decrypt_version, created_at, created_by) VALUES (?, ?, ?, ?, ?, ?, ?)",
                    params![id, project, k.name, serde_json::to_string(&stored).expect("JSON"), k.min_decrypt_version, now, caller.principal],
                )?;
                Ok(true)
            })?;
            if !done {
                warnings.push(format!("encryption key {}: the project has one by that name", k.name));
            }
        }
        Ok(warnings)
    }
}

fn batch_limits(sizes: impl Iterator<Item = usize>) -> Result<()> {
    let sizes: Vec<usize> = sizes.collect();
    if sizes.is_empty() || sizes.len() > BATCH_MAX || sizes.iter().sum::<usize>() > BATCH_BYTES {
        return Err(Error::BadRequest(format!("1–{BATCH_MAX} items, {} MiB at most in all", BATCH_BYTES >> 20)));
    }
    Ok(())
}
