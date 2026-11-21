//! roll's digest key (docs/secrets.md, "Tamper evidence"): the key that signs the audit log's
//! hourly digests, and only them. CloudTrail signs its digest files with keys of their own; so does
//! roll, so a leak of the issuer key (clouds' and apps' tokens) forges no digest, and pulling a
//! leaked digest key touches no cloud trust.
//!
//! - Ed25519 (JWS `EdDSA`): only roll's own code checks a digest (the server and `roll secrets
//!   audit verify`), so nothing needs RS256 the way Azure does; keys are 32 bytes and quick to make
//!   inside a request, signatures are deterministic, and a digest key can't pass as an issuer key or
//!   the other way round (`roll_secrets_chain::open` takes each key's algorithm only).
//! - Sealed with the deployment key and bound to its row: the private key is sealed under a data
//!   key of its own (`audit-key:<kid>`) with the kid and the row's `legacy` list as associated data,
//!   and its public half must be the row's `x` ([`open_row`]). A row that fails isn't roll's: it
//!   neither signs, nor checks, nor is published.
//! - Rotated every [`ROTATE_DAYS`] (`ROLL_SECRETS_AUDIT_KEY_ROTATE_DAYS`): the new key signs at
//!   once (no cloud caches it), and the one it replaces retires but stays published, since its
//!   digests stay in the log.
//! - An emergency ([`rotate_now`], the operator's) also pulls a key: its private half is wiped,
//!   its public half moves to the JWKS's `pulled`, and every project gets a digest under the new key
//!   at once, which covers the pulled key's digests (the chain's hash runs through them).
//! - `legacy` names the issuer keys whose RS256 digests, from before the digest key, still count:
//!   roll's issuer keys when the first digest key was made, copied to each key after it, less any
//!   pulled since ([`drop_legacy`]). It's bound into each row's sealing, so a database writer can't
//!   add a key to it or put a pulled one back.

use crate::chain;
use crate::crypto::Keyring;
use crate::{Error, Result, Value, names};
use base64::Engine as _;
use code_sql::{Db, params};
use serde_json::{Value as Json, json};

pub(crate) const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS audit_keys (
    kid TEXT PRIMARY KEY,
    sealed_dek TEXT NOT NULL,
    private BLOB NOT NULL,
    x TEXT NOT NULL,
    legacy TEXT NOT NULL,
    created_at INTEGER NOT NULL,
    retired_at INTEGER,
    pulled_at INTEGER
) WITHOUT ROWID;
";

/// Days between scheduled rotations of the digest key (`ROLL_SECRETS_AUDIT_KEY_ROTATE_DAYS`).
pub const ROTATE_DAYS: u32 = 90;

fn b64(b: &[u8]) -> String {
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(b)
}

fn aad(kid: &str, legacy: &str) -> String {
    format!("roll-secrets:audit-key:{kid}:legacy={legacy}:private")
}

fn dek_owner(kid: &str) -> String {
    format!("audit-key:{kid}")
}

/// One row of `audit_keys`.
struct KeyRow {
    kid: String,
    sealed_dek: String,
    private: Vec<u8>,
    x: String,
    /// Issuer kids whose digests count as legacy, comma-separated and sorted.
    legacy: String,
    created_at: i64,
    retired_at: Option<i64>,
    pulled_at: Option<i64>,
}

impl KeyRow {
    fn legacy(&self) -> impl Iterator<Item = &str> {
        self.legacy.split(',').filter(|k| !k.is_empty())
    }
}

/// Every row, the newest first.
fn read_rows(c: &code_sql::Conn<'_>) -> code_sql::Result<Vec<KeyRow>> {
    c.query("SELECT kid, sealed_dek, private, x, legacy, created_at, retired_at, pulled_at FROM audit_keys ORDER BY created_at DESC, kid", (), |r| {
        Ok(KeyRow { kid: r.get(0)?, sealed_dek: r.get(1)?, private: r.get(2)?, x: r.get(3)?, legacy: r.get(4)?, created_at: r.get(5)?, retired_at: r.get(6)?, pulled_at: r.get(7)? })
    })
}

fn rows(db: &Db) -> Result<Vec<KeyRow>> {
    Ok(db.read(read_rows)?)
}

/// The row's key pair when the row is roll's: its private key opens with the deployment key under
/// the row's kid and legacy list, and its public half is the row's `x`. Also its data key resealed
/// with the current deployment key, when it needed the previous one.
fn open_row(keys: &Keyring, r: &KeyRow) -> Option<(ring::signature::Ed25519KeyPair, Option<String>)> {
    use ring::signature::KeyPair as _;
    let (dek, resealed) = keys.open_dek(&dek_owner(&r.kid), &r.sealed_dek).ok()?;
    let der = zeroize::Zeroizing::new(base64::engine::general_purpose::URL_SAFE_NO_PAD.decode(dek.open(&aad(&r.kid, &r.legacy), &r.private).ok()?.expose()).ok()?);
    let pair = ring::signature::Ed25519KeyPair::from_pkcs8(&der).ok()?;
    (b64(pair.public_key().as_ref()) == r.x).then_some((pair, resealed))
}

fn untrusted(kid: &str) {
    tracing::warn!(kid, "an audit digest key row isn't roll's (its private key doesn't open with the deployment key, or isn't the row's public key): it's not trusted, published or used");
}

/// A new key's row (not stored), signing from `now`, naming `legacy`.
fn new_key(keys: &Keyring, now: i64, legacy: &str) -> Result<KeyRow> {
    use ring::signature::KeyPair as _;
    let der = ring::signature::Ed25519KeyPair::generate_pkcs8(&ring::rand::SystemRandom::new()).map_err(|_| Error::Internal("making the audit log's digest key failed".into()))?;
    let pair = ring::signature::Ed25519KeyPair::from_pkcs8(der.as_ref()).map_err(|_| Error::Internal("making the audit log's digest key failed".into()))?;
    let kid = names::new_id("ak", 16);
    let (dek, sealed_dek) = keys.new_dek(&dek_owner(&kid));
    let private = dek.seal(&aad(&kid, legacy), &Value::new(b64(der.as_ref())));
    Ok(KeyRow { x: b64(pair.public_key().as_ref()), kid, sealed_dek, private, legacy: legacy.into(), created_at: now, retired_at: None, pulled_at: None })
}

fn insert(c: &code_sql::Conn<'_>, r: &KeyRow) -> code_sql::Result<usize> {
    c.execute(
        "INSERT INTO audit_keys (kid, sealed_dek, private, x, legacy, created_at) VALUES (?, ?, ?, ?, ?, ?)",
        params![r.kid, r.sealed_dek, r.private, r.x, r.legacy, r.created_at],
    )
}

/// Rows already checked by [`open_row`], by kid: a hash of the row and whether it's roll's, so the
/// public JWKS route doesn't open every key, nor the log warn, per request.
#[derive(Default)]
pub(crate) struct Checked(std::sync::Mutex<std::collections::HashMap<String, ([u8; 32], bool)>>);

fn row_hash(r: &KeyRow) -> [u8; 32] {
    use sha2::Digest as _;
    let mut h = sha2::Sha256::new();
    for part in [r.kid.as_bytes(), r.sealed_dek.as_bytes(), &r.private, r.x.as_bytes(), r.legacy.as_bytes()] {
        h.update((part.len() as u64).to_be_bytes());
        h.update(part);
    }
    h.finalize().into()
}

/// roll's rows of `rows`, in order: live and retired keys that open ([`open_row`]), and pulled
/// ones (their private half is gone; only their kid and public half are kept, to name them).
fn only_rolls(rows: Vec<KeyRow>, keys: &Keyring, checked: &Checked) -> Vec<KeyRow> {
    let mut seen = checked.0.lock().unwrap_or_else(|p| p.into_inner());
    seen.retain(|kid, _| rows.iter().any(|r| &r.kid == kid));
    rows.into_iter()
        .filter(|r| {
            if r.pulled_at.is_some() {
                return true;
            }
            let h = row_hash(r);
            match seen.get(&r.kid) {
                Some((was, ok)) if *was == h => *ok,
                _ => {
                    let ok = open_row(keys, r).is_some();
                    if !ok {
                        untrusted(&r.kid);
                    }
                    seen.insert(r.kid.clone(), (h, ok));
                    ok
                }
            }
        })
        .collect()
}

/// The legacy list a new key carries: the newest of roll's keys', or, for the first key ever, the
/// issuer keys of roll's there are now (they may have signed digests before this one existed).
fn next_legacy(db: &Db, keys: &Keyring, mine: &[KeyRow], issuer: &crate::issuer::Checked) -> Result<String> {
    if let Some(r) = mine.iter().find(|r| r.pulled_at.is_none()) {
        return Ok(r.legacy.clone());
    }
    if !mine.is_empty() {
        // Every key pulled: nothing to copy from, and the issuer keys were already counted.
        return Ok(String::new());
    }
    // Not one pulled already: the list is bound to the row, a pulled_at isn't.
    let pulled = crate::issuer::pulled_all(db)?;
    let mut kids: Vec<String> = crate::issuer::trusted_jwks(db, keys, issuer)?
        .into_iter()
        .filter_map(|k| k["kid"].as_str().map(String::from))
        .filter(|kid| !pulled.iter().any(|(p, _)| p == kid))
        .collect();
    kids.sort();
    Ok(kids.join(","))
}

/// The key that signs now: the newest of roll's live keys, made (signing at once) when there's
/// none. Its data key is resealed with the current deployment key when it needed the previous one.
fn signing_key(db: &Db, keys: &Keyring, checked: &Checked, issuer: &crate::issuer::Checked) -> Result<(String, ring::signature::Ed25519KeyPair)> {
    let all = rows(db)?;
    for r in all.iter().filter(|r| r.retired_at.is_none() && r.pulled_at.is_none()) {
        let Some((pair, resealed)) = open_row(keys, r) else {
            untrusted(&r.kid);
            continue;
        };
        if let Some(new) = resealed {
            db.write(|c| c.execute("UPDATE audit_keys SET sealed_dek = ? WHERE kid = ? AND sealed_dek = ?", [new.as_str(), r.kid.as_str(), r.sealed_dek.as_str()]))?;
        }
        return Ok((r.kid.clone(), pair));
    }
    let mine = only_rolls(all, keys, checked);
    let row = new_key(keys, db.now_ms()?, &next_legacy(db, keys, &mine, issuer)?)?;
    db.write(|c| insert(c, &row))?;
    tracing::info!(kid = %row.kid, "the audit log's digest key is made");
    let pair = open_row(keys, &row).map(|(p, _)| p).ok_or_else(|| Error::Internal("the audit log's new digest key doesn't open".into()))?;
    Ok((row.kid, pair))
}

/// Make the digest key if there's none of roll's (at startup, before the issuer's: a deployment's
/// first issuer key is then never a legacy digest signer).
pub(crate) fn ensure_key(db: &Db, keys: &Keyring, checked: &Checked, issuer: &crate::issuer::Checked) -> Result<()> {
    signing_key(db, keys, checked, issuer).map(drop)
}

/// The key that signs digests now: (kid, key pair).
pub(crate) fn signer(db: &Db, keys: &Keyring, checked: &Checked, issuer: &crate::issuer::Checked) -> Result<(String, ring::signature::Ed25519KeyPair)> {
    signing_key(db, keys, checked, issuer)
}

/// The digest JWKS (`GET /v1/secrets/audit/jwks`): `keys`, roll's digest keys that check digests
/// (live and retired, never pulled); `legacy`, the issuer keys whose digests from before the
/// digest key count (RSA JWKs, never pulled); `pulled`, the kids pulled in an emergency, either
/// kind, with when.
pub(crate) fn jwks(db: &Db, keys: &Keyring, checked: &Checked, issuer: &crate::issuer::Checked) -> Result<Json> {
    let mine = only_rolls(rows(db)?, keys, checked);
    let mut pulled: Vec<(String, i64)> = mine.iter().filter_map(|r| Some((r.kid.clone(), r.pulled_at?))).collect();
    // The legacy list is the newest live key's: a pulled issuer key has left it.
    let named: Vec<&str> = mine.iter().find(|r| r.pulled_at.is_none()).map(|r| r.legacy().collect()).unwrap_or_default();
    let issuer_pulled = crate::issuer::pulled_all(db)?;
    let legacy: Vec<Json> = crate::issuer::trusted_jwks(db, keys, issuer)?
        .into_iter()
        .filter(|k| k["kid"].as_str().is_some_and(|kid| named.contains(&kid) && !issuer_pulled.iter().any(|(p, _)| p == kid)))
        .collect();
    // Every pulled issuer key: a digest on one vouches for nothing, whether it ever signed one.
    pulled.extend(issuer_pulled);
    let published: Vec<Json> = mine.iter().filter(|r| r.pulled_at.is_none()).map(|r| json!({ "kty": "OKP", "crv": "Ed25519", "use": "sig", "alg": "EdDSA", "kid": r.kid, "x": r.x })).collect();
    Ok(json!({
        "keys": published,
        "legacy": legacy,
        "pulled": pulled.into_iter().map(|(kid, at)| json!({ "kid": kid, "pulled_at": at })).collect::<Vec<_>>(),
    }))
}

/// What `audit verify` checks digests with: the digest JWKS, as `audit verify --file` reads it.
pub(crate) fn verify_keys(db: &Db, keys: &Keyring, checked: &Checked, issuer: &crate::issuer::Checked) -> Result<chain::Keys> {
    Ok(chain::Keys::from_jwks(&[jwks(db, keys, checked, issuer)?]))
}

/// Reseal every digest key's data key with the current deployment key; how many still need the
/// previous one (health's `resealed`).
pub(crate) fn reseal(db: &Db, keys: &Keyring) -> Result<usize> {
    let rows: Vec<(String, String)> = db.read(|c| c.query("SELECT kid, sealed_dek FROM audit_keys WHERE pulled_at IS NULL", (), |r| Ok((r.get(0)?, r.get(1)?))))?;
    let mut old = 0;
    for (kid, sealed) in rows {
        match keys.open_dek(&dek_owner(&kid), &sealed) {
            Ok((_, Some(new))) => {
                db.write(|c| c.execute("UPDATE audit_keys SET sealed_dek = ? WHERE kid = ? AND sealed_dek = ?", [new.as_str(), kid.as_str(), sealed.as_str()]))?;
            }
            Ok((_, None)) => {}
            Err(_) => old += 1,
        }
    }
    Ok(old)
}

/// In the caller's write: a new key that signs at once, every other live key retired, and with
/// `pull` (one of roll's keys, live or retired, not pulled yet) its private half wiped and
/// `pulled_at` set. Every project's head is then made due, so the sweep that follows signs each
/// chain under the new key.
fn replace(c: &code_sql::Conn<'_>, keys: &Keyring, checked: &Checked, row: &KeyRow, pull: Option<&str>, now: i64) -> Result<()> {
    let mine = only_rolls(read_rows(c)?, keys, checked);
    if let Some(pull) = pull
        && !mine.iter().any(|r| r.kid == pull && r.pulled_at.is_none())
    {
        return Err(Error::NotFound(format!("{pull} isn't one of roll's digest keys (or it was pulled already)")));
    }
    insert(c, row)?;
    c.execute("UPDATE audit_keys SET retired_at = ? WHERE retired_at IS NULL AND kid <> ?", params![now, row.kid])?;
    if let Some(pull) = pull {
        c.execute("UPDATE audit_keys SET private = ?, sealed_dek = '', pulled_at = ?, retired_at = COALESCE(retired_at, ?) WHERE kid = ?", params![Vec::<u8>::new(), now, now, pull])?;
        c.execute("UPDATE audit_heads SET digest_at = 0", ())?;
    }
    Ok(())
}

/// A scheduled rotation, when the key signing now is `every_ms` old: a new key signs at once and
/// the old one retires (it stays published: its digests stay in the log). The write is the claim:
/// it goes ahead only while the key it replaces is still the newest live one. Answers the new kid.
pub(crate) fn rotate(db: &Db, keys: &Keyring, checked: &Checked, every_ms: i64) -> Result<Option<String>> {
    let now = db.now_ms()?;
    let mine = only_rolls(rows(db)?, keys, checked);
    let Some(current) = mine.iter().find(|r| r.retired_at.is_none() && r.pulled_at.is_none()) else { return Ok(None) };
    if current.created_at + every_ms > now {
        return Ok(None);
    }
    let row = new_key(keys, now, &current.legacy)?;
    let current = current.kid.clone();
    let done = db.write(|c| {
        let newest = only_rolls(read_rows(c)?, keys, checked).into_iter().find(|r| r.retired_at.is_none() && r.pulled_at.is_none()).map(|r| r.kid);
        if newest.as_deref() != Some(current.as_str()) {
            return Ok::<_, Error>(false);
        }
        replace(c, keys, checked, &row, None, now)?;
        Ok(true)
    })?;
    if done {
        tracing::info!(kid = %row.kid, retired = %current, "the audit log's digest key rotated");
    }
    Ok(done.then_some(row.kid))
}

/// The operator's rotation: a new key that signs at once; with `pull`, an emergency, that key
/// pulled in the same write. One write with `audit(c, new kid, now)`, so the rotation isn't without
/// its audit rows. Answers the new kid.
pub(crate) fn rotate_now(db: &Db, keys: &Keyring, checked: &Checked, issuer: &crate::issuer::Checked, pull: Option<&str>, audit: impl Fn(&code_sql::Conn<'_>, &str, i64) -> Result<()>) -> Result<String> {
    let now = db.now_ms()?;
    let mine = only_rolls(rows(db)?, keys, checked);
    // Pulling a digest key changes nothing about which issuer keys signed digests before it.
    let row = new_key(keys, now, &next_legacy(db, keys, &mine, issuer)?)?;
    db.write(|c| {
        replace(c, keys, checked, &row, pull, now)?;
        audit(c, &row.kid, now)
    })?;
    if let Some(pull) = pull {
        tracing::warn!(kid = %row.kid, pulled = pull, "the audit log's digest key: an emergency rotation: a new key signs now, and the pulled key vouches for nothing again");
    }
    Ok(row.kid)
}

/// In the caller's write (the issuer's emergency rotation): take `kid` off every digest key's
/// legacy list, resealing each key under its new list, so its RS256 digests count no more and a
/// database writer can't put it back.
pub(crate) fn drop_legacy(c: &code_sql::Conn<'_>, keys: &Keyring, kid: &str) -> Result<()> {
    for r in read_rows(c)?.into_iter().filter(|r| r.pulled_at.is_none() && r.legacy().any(|k| k == kid)) {
        let Ok((dek, _)) = keys.open_dek(&dek_owner(&r.kid), &r.sealed_dek) else {
            untrusted(&r.kid);
            continue;
        };
        let Ok(private) = dek.open(&aad(&r.kid, &r.legacy), &r.private) else {
            untrusted(&r.kid);
            continue;
        };
        let legacy = r.legacy().filter(|k| *k != kid).collect::<Vec<_>>().join(",");
        let sealed = dek.seal(&aad(&r.kid, &legacy), &private);
        c.execute("UPDATE audit_keys SET legacy = ?, private = ? WHERE kid = ?", params![legacy, sealed, r.kid])?;
    }
    Ok(())
}

impl crate::Secrets {
    /// `GET /v1/secrets/audit/jwks` (public): what digests are checked with ([`jwks`]). Save it
    /// for `audit verify --file`, apart from roll.
    pub fn audit_jwks(&self) -> Result<Json> {
        jwks(&self.db, self.keys()?, &self.audit_keys, &self.issuer_keys)
    }

    /// Days between scheduled rotations of the digest key (`ROLL_SECRETS_AUDIT_KEY_ROTATE_DAYS`;
    /// [`ROTATE_DAYS`] when unset). At least 1.
    pub fn set_audit_key_rotation_days(&self, days: u32) {
        self.audit_rotate_days.store(days.max(1), std::sync::atomic::Ordering::Relaxed);
    }

    /// `POST /v1/secrets/audit/key/rotate` (the operator's): a new digest key signs now. With
    /// `pull` (`{"now": true, "retire": kid}`), an emergency: `kid` is pulled, and every project's
    /// chain is signed under the new key before this answers, so the pulled key's digests are
    /// covered at once. Either is an `audit.key.rotate` row in every project's log.
    pub fn rotate_audit_key(&self, pull: Option<&str>, caller: &crate::Caller) -> Result<Json> {
        if !caller.grants.everything() || caller.principal != "operator" {
            return Err(Error::Forbidden("roll's audit digest key is the operator's".into()));
        }
        let keys = self.keys()?;
        let kid = rotate_now(&self.db, keys, &self.audit_keys, &self.issuer_keys, pull, |c, kid, now| {
            let summary = match pull {
                Some(pull) => format!("an emergency rotation of the audit log's digest key: {kid} signs now, and {pull} was pulled (its digests vouch for nothing; the next digest covers them)"),
                None => format!("a rotation of the audit log's digest key: {kid} signs now"),
            };
            // A deleted project's chain is signed until its closing digest (deleted.rs).
            for project in c.query("SELECT project_id FROM projects WHERE closed_at IS NULL", (), |r| r.get::<_, String>(0))? {
                crate::audit::record(c, keys, &project, caller, "audit.key.rotate", None, None, &[], &summary, now)?;
            }
            Ok(())
        })?;
        let signed = if pull.is_some() { self.sign_due_digests()? } else { 0 };
        Ok(json!({ "kid": kid, "pulled": pull, "signed": signed }))
    }

    /// The sweeper: a scheduled rotation of the digest key when one is due.
    pub(crate) fn sweep_audit_key(&self) -> Result<()> {
        let Ok(keys) = self.keys() else { return Ok(()) };
        let every = self.audit_rotate_days.load(std::sync::atomic::Ordering::Relaxed) as i64 * 86_400_000;
        rotate(&self.db, keys, &self.audit_keys, every)?;
        Ok(())
    }
}
