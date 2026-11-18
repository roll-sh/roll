//! roll as an OpenID Connect issuer (docs/secrets.md, "GCP and Azure"): a short-lived token that
//! says "this is roll, acting for project P", which a customer's cloud trusts the way it trusts
//! GitHub Actions' tokens. GCP's workload identity federation and Azure's federated credentials
//! then hand roll an access token for the customer's own service account or app, with no secret of
//! the customer's stored anywhere.
//!
//! - The issuer is `<rolld's public URL>/v1/secrets/oidc`; its discovery document and keys
//!   (`/.well-known/openid-configuration`, `/jwks`) are public, as clouds fetch them anonymously.
//! - Tokens are RS256 (Azure takes nothing else), live five minutes, and carry `sub =
//!   roll-secrets:<project>`, so a cloud's trust names one project and nobody else's.
//! - Other roll products get tokens in process ([`Subject`], `Secrets::issue`): roll apps' instances
//!   carry `sub = roll-apps:<project>:<service>:<env>`, for an hour at most.
//! - Signing keys (RSA 2048) are sealed with the deployment key and shared by every node through
//!   the database; a node that finds none makes one. They rotate ([`rotate`], every 90 days by
//!   default): the next key is published [`PUBLISH_AHEAD_MS`] before it starts signing (clouds
//!   cache JWKS), and the one it replaces leaves the JWKS [`RETIRE_AFTER_MS`] after that, once its
//!   last token has expired. A retired key never signs again, but stays (sealed) to check the
//!   audit log's digests it signed.
//! - The same key signs the audit log's digests (roll-secrets-chain), so the JWKS is served even on a server
//!   without a public URL for the issuer: it's what an export is verified with offline.
//! - Only a row whose sealed private key opens with the deployment key, and whose public half is
//!   that key's, is roll's ([`open_row`]): it alone signs, is published and checks digests. A
//!   database writer without `ROLL_SECRET_KEY` can add a row but can't make one that passes.

use crate::crypto::Keyring;
use crate::{Error, Result, Value, names};
use base64::Engine as _;
use code_sql::{Db, params};
use serde_json::{Value as Json, json};

pub(crate) const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS oidc_keys (
    kid TEXT PRIMARY KEY,
    sealed_dek TEXT NOT NULL,
    private BLOB NOT NULL,
    n TEXT NOT NULL,
    e TEXT NOT NULL,
    created_at INTEGER NOT NULL
) WITHOUT ROWID;
-- Tokens issued to other products (`Secrets::issue`), counted per subject, audience and hour: the
-- first of each is an audit row, the rest only count here.
CREATE TABLE IF NOT EXISTS oidc_issued (
    subject TEXT NOT NULL,
    audience TEXT NOT NULL,
    hour INTEGER NOT NULL,
    project_id TEXT NOT NULL,
    tokens INTEGER NOT NULL,
    PRIMARY KEY (subject, audience, hour)
) WITHOUT ROWID;
";

/// How long a cloud's token lives: long enough for one exchange.
pub const TOKEN_SECONDS: i64 = 300;
/// The longest any token of roll's issuer lives (`Secrets::issue` caps its ttl here).
pub const TOKEN_MAX_SECONDS: i64 = 3600;
/// Days between scheduled rotations of the signing key (`ROLL_SECRETS_OIDC_ROTATE_DAYS`).
pub const ROTATE_DAYS: u32 = 90;
/// How long the next key is in the JWKS before it signs: clouds cache JWKS (Azure for a day).
pub const PUBLISH_AHEAD_MS: i64 = 2 * 86_400_000;
/// How long a replaced key stays in the JWKS after the next one starts signing: its last token's
/// lifetime and an hour's margin (clock skew, verifiers' leeway).
pub const RETIRE_AFTER_MS: i64 = TOKEN_MAX_SECONDS * 1000 + 3_600_000;

/// The subject a cloud's trust names for `project`.
pub fn subject(project: &str) -> String {
    format!("roll-secrets:{project}")
}

/// Who a token roll signs for another roll product stands for.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Subject {
    /// One environment of a roll apps service: `roll-apps:<project>:<service>:<env>`.
    App { project: String, service: String, env: String },
}

const APPS_PREFIX: &str = "roll-apps:";

/// A project's or a service's id: `prj_…`, `svc_…` (lowercase letters, digits, `_` and `-`).
fn id_ok(s: &str) -> bool {
    (1..=64).contains(&s.len()) && s.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_' || b == b'-')
}

impl Subject {
    /// The token's `sub`.
    pub fn sub(&self) -> String {
        match self {
            Subject::App { project, service, env } => format!("{APPS_PREFIX}{project}:{service}:{env}"),
        }
    }

    pub fn project(&self) -> &str {
        match self {
            Subject::App { project, .. } => project,
        }
    }

    /// Each part by the names rules, so no part holds a `:` and a `sub` names exactly one.
    pub fn check(&self) -> Result<()> {
        match self {
            Subject::App { project, service, env } => {
                if !id_ok(project) || !id_ok(service) {
                    return Err(Error::BadRequest("a project and a service are ids: 1–64 characters of a-z, 0-9, _ and -".into()));
                }
                names::slug(env)
            }
        }
    }

    /// The subject a `sub` names, when it's one of roll's products' and well formed.
    pub fn parse(sub: &str) -> Option<Subject> {
        let rest = sub.strip_prefix(APPS_PREFIX)?;
        let mut parts = rest.split(':');
        let (project, service, env) = (parts.next()?, parts.next()?, parts.next()?);
        let s = Subject::App { project: project.into(), service: service.into(), env: env.into() };
        (parts.next().is_none() && s.check().is_ok()).then_some(s)
    }

    /// The claims clouds map attributes from (GCP's `attribute.service`, AWS's tags).
    fn claims(&self) -> Json {
        match self {
            Subject::App { project, service, env } => json!({ "project": project, "service": service, "environment": env }),
        }
    }
}

fn b64(b: &[u8]) -> String {
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(b)
}

fn aad(kid: &str) -> String {
    format!("roll-secrets:oidc:{kid}:private")
}

/// The DEK that seals one signing key is "a project" of its own to the keyring.
fn dek_owner(kid: &str) -> String {
    format!("oidc-key:{kid}")
}

pub(crate) struct Issuer {
    pub url: String,
}

/// One row of `oidc_keys`.
struct KeyRow {
    kid: String,
    sealed_dek: String,
    private: Vec<u8>,
    n: String,
    e: String,
    /// When it signs from (unix ms): its creation for a key made before rotation.
    signs_from: i64,
    /// When it left the JWKS; it never signs again.
    retired_at: Option<i64>,
}

/// Every row, the latest to sign first.
fn read_rows(c: &code_sql::Conn<'_>) -> code_sql::Result<Vec<KeyRow>> {
    c.query(
        "SELECT kid, sealed_dek, private, n, e, COALESCE(signs_from, created_at), retired_at FROM oidc_keys ORDER BY COALESCE(signs_from, created_at) DESC, created_at DESC, kid",
        (),
        |r| Ok(KeyRow { kid: r.get(0)?, sealed_dek: r.get(1)?, private: r.get(2)?, n: r.get(3)?, e: r.get(4)?, signs_from: r.get(5)?, retired_at: r.get(6)? }),
    )
}

fn rows(db: &Db) -> Result<Vec<KeyRow>> {
    Ok(db.read(read_rows)?)
}

/// The row's private key (PKCS#8 DER) when the row is roll's: its sealed key opens with the
/// deployment key and its public half is the row's `n` and `e`. Also its data key resealed with the
/// current deployment key, when it needed the previous one.
fn open_row(keys: &Keyring, r: &KeyRow) -> Option<(zeroize::Zeroizing<Vec<u8>>, Option<String>)> {
    use rsa::pkcs8::DecodePrivateKey;
    use rsa::traits::PublicKeyParts;
    let (dek, resealed) = keys.open_dek(&dek_owner(&r.kid), &r.sealed_dek).ok()?;
    let der = zeroize::Zeroizing::new(base64::engine::general_purpose::URL_SAFE_NO_PAD.decode(dek.open(&aad(&r.kid), &r.private).ok()?.expose()).ok()?);
    let key = rsa::RsaPrivateKey::from_pkcs8_der(&der).ok()?;
    (b64(&key.n().to_bytes_be()) == r.n && b64(&key.e().to_bytes_be()) == r.e).then_some((der, resealed))
}

fn untrusted(kid: &str) {
    tracing::warn!(kid, "an OIDC key row isn't roll's (its private key doesn't open with the deployment key, or isn't the row's public key): it's not trusted, published or used");
}

/// A new signing key's row (not stored) and its PKCS#8 DER. Slow (an RSA key): made outside any
/// write.
fn new_key(keys: &Keyring, signs_from: i64) -> Result<(KeyRow, zeroize::Zeroizing<Vec<u8>>)> {
    use rsa::pkcs8::EncodePrivateKey;
    use rsa::traits::PublicKeyParts;
    let key = rsa::RsaPrivateKey::new(&mut rand::thread_rng(), 2048).map_err(|e| Error::Internal(format!("making the issuer's key: {e}")))?;
    let der = key.to_pkcs8_der().map_err(|e| Error::Internal(format!("encoding the issuer's key: {e}")))?;
    let kid = names::new_id("k", 16);
    let (dek, sealed_dek) = keys.new_dek(&dek_owner(&kid));
    let private = dek.seal(&aad(&kid), &Value::new(b64(der.as_bytes())));
    let row = KeyRow { n: b64(&key.n().to_bytes_be()), e: b64(&key.e().to_bytes_be()), kid, sealed_dek, private, signs_from, retired_at: None };
    Ok((row, zeroize::Zeroizing::new(der.as_bytes().to_vec())))
}

fn insert(c: &code_sql::Conn<'_>, r: &KeyRow, now: i64) -> code_sql::Result<usize> {
    c.execute(
        "INSERT INTO oidc_keys (kid, sealed_dek, private, n, e, created_at, signs_from) VALUES (?, ?, ?, ?, ?, ?, ?)",
        params![r.kid, r.sealed_dek, r.private, r.n, r.e, now, r.signs_from],
    )
}

/// Make the signing key if there's none of roll's (at startup: the public keys are there before
/// the first token, and no anonymous request ever makes one). Two nodes may make one each: both
/// are published, the latest signs, and rotation retires the other.
pub(crate) fn ensure_key(db: &Db, keys: &Keyring) -> Result<()> {
    signing_key(db, keys).map(drop)
}

/// The key that signs now: the latest of roll's live keys whose time has come, made (signing at
/// once) when there's none. (kid, PKCS#8 DER). Its data key is resealed with the current deployment
/// key when it was sealed with a previous one, as projects' are.
fn signing_key(db: &Db, keys: &Keyring) -> Result<(String, zeroize::Zeroizing<Vec<u8>>)> {
    let now = db.now_ms()?;
    for r in rows(db)?.into_iter().filter(|r| r.retired_at.is_none() && r.signs_from <= now) {
        let Some((der, resealed)) = open_row(keys, &r) else {
            untrusted(&r.kid);
            continue;
        };
        if let Some(new) = resealed {
            db.write(|c| c.execute("UPDATE oidc_keys SET sealed_dek = ? WHERE kid = ? AND sealed_dek = ?", [new.as_str(), r.kid.as_str(), r.sealed_dek.as_str()]))?;
        }
        return Ok((r.kid, der));
    }
    let (row, der) = new_key(keys, now)?;
    db.write(|c| insert(c, &row, now))?;
    Ok((row.kid, der))
}

/// The key that signs now, to sign the audit log's digests with: (kid, key).
pub(crate) fn signer(db: &Db, keys: &Keyring) -> Result<(String, ring::signature::RsaKeyPair)> {
    let (kid, der) = signing_key(db, keys)?;
    let pair = ring::signature::RsaKeyPair::from_pkcs8(&der).map_err(|e| Error::Internal(format!("the issuer's key: {e}")))?;
    Ok((kid, pair))
}

/// Rows already checked by [`open_row`], by kid: a hash of the row and whether it's roll's. A
/// changed row is checked again. It spares the public JWKS route an RSA key parse, and the log a
/// warning, per request.
#[derive(Default)]
pub(crate) struct Checked(std::sync::Mutex<std::collections::HashMap<String, ([u8; 32], bool)>>);

fn row_hash(r: &KeyRow) -> [u8; 32] {
    use sha2::Digest as _;
    let mut h = sha2::Sha256::new();
    for part in [r.kid.as_bytes(), r.sealed_dek.as_bytes(), &r.private, r.n.as_bytes(), r.e.as_bytes()] {
        h.update((part.len() as u64).to_be_bytes());
        h.update(part);
    }
    h.finalize().into()
}

/// roll's rows of `rows` (see [`open_row`]), in order. A row that isn't roll's is left out with a
/// warning naming its kid.
fn only_rolls(rows: Vec<KeyRow>, keys: &Keyring, checked: &Checked) -> Vec<KeyRow> {
    let mut seen = checked.0.lock().unwrap_or_else(|p| p.into_inner());
    seen.retain(|kid, _| rows.iter().any(|r| &r.kid == kid));
    rows.into_iter()
        .filter(|r| {
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

/// roll's rows, the latest to sign first.
fn trusted(db: &Db, keys: &Keyring, checked: &Checked) -> Result<Vec<KeyRow>> {
    Ok(only_rolls(rows(db)?, keys, checked))
}

fn jwk(r: &KeyRow) -> Json {
    json!({ "kty": "RSA", "use": "sig", "alg": "RS256", "kid": r.kid, "n": r.n, "e": r.e })
}

/// roll's keys' public halves by kid, to check digests with: retired keys too, as digests they
/// signed stay in the log.
pub(crate) fn public_keys(db: &Db, keys: &Keyring, checked: &Checked) -> Result<std::collections::HashMap<String, crate::chain::PublicKey>> {
    let all = json!({ "keys": trusted(db, keys, checked)?.iter().map(jwk).collect::<Vec<_>>() });
    Ok(crate::chain::PublicKey::from_jwks(&all).into_iter().collect())
}

/// roll's live keys' public halves (the one signing, the next before it signs, the one it replaced
/// until that one's tokens have expired), so a token or a digest from any node verifies.
pub(crate) fn jwks(db: &Db, keys: &Keyring, checked: &Checked) -> Result<Json> {
    let live: Vec<Json> = trusted(db, keys, checked)?.iter().filter(|r| r.retired_at.is_none()).map(jwk).collect();
    Ok(json!({ "keys": live }))
}

/// Reseal every signing key's data key with the current deployment key; how many still need the
/// previous one (health's `resealed`).
pub(crate) fn reseal(db: &Db, keys: &Keyring) -> Result<usize> {
    let rows: Vec<(String, String)> = db.read(|c| c.query("SELECT kid, sealed_dek FROM oidc_keys", (), |r| Ok((r.get(0)?, r.get(1)?))))?;
    let mut old = 0;
    for (kid, sealed) in rows {
        match keys.open_dek(&dek_owner(&kid), &sealed) {
            Ok((_, Some(new))) => {
                db.write(|c| c.execute("UPDATE oidc_keys SET sealed_dek = ? WHERE kid = ? AND sealed_dek = ?", [new.as_str(), kid.as_str(), sealed.as_str()]))?;
            }
            Ok((_, None)) => {}
            Err(_) => old += 1,
        }
    }
    Ok(old)
}

/// A key waiting to sign (kid, signs_from), or whether the one signing is due to be replaced
/// (`every_ms` after it started, less [`PUBLISH_AHEAD_MS`]; at once when `now_due`).
enum Next {
    Waiting(String, i64),
    Due,
    NotYet,
}

fn next(live: &[KeyRow], now: i64, every_ms: i64, now_due: bool) -> Next {
    if let Some(w) = live.iter().find(|r| r.signs_from > now) {
        return Next::Waiting(w.kid.clone(), w.signs_from);
    }
    match live.iter().find(|r| r.signs_from <= now) {
        Some(current) if now_due || current.signs_from + every_ms - PUBLISH_AHEAD_MS <= now => Next::Due,
        _ => Next::NotYet,
    }
}

/// Start a rotation when it's due (or `now_due`, the operator's): the next key, published now,
/// signing from [`PUBLISH_AHEAD_MS`] later. The insert is the claim, in a write that finds no key of
/// roll's waiting: a node that comes second keeps its own out. Answers the key waiting, if any:
/// (kid, signs_from).
pub(crate) fn rotate(db: &Db, keys: &Keyring, checked: &Checked, every_ms: i64, now_due: bool) -> Result<Option<(String, i64)>> {
    let live = |rows: Vec<KeyRow>| -> Vec<KeyRow> { only_rolls(rows, keys, checked).into_iter().filter(|r| r.retired_at.is_none()).collect() };
    let now = db.now_ms()?;
    match next(&live(rows(db)?), now, every_ms, now_due) {
        Next::Waiting(kid, at) => return Ok(Some((kid, at))),
        Next::NotYet => return Ok(None),
        Next::Due => {}
    }
    let (row, _) = new_key(keys, now + PUBLISH_AHEAD_MS)?;
    let waiting = db.write(|c| {
        // A waiting row that isn't roll's doesn't hold a rotation back.
        if let Some(w) = live(read_rows(c)?).into_iter().find(|r| r.signs_from > now) {
            return Ok::<_, Error>((w.kid, w.signs_from));
        }
        insert(c, &row, now)?;
        Ok((row.kid.clone(), row.signs_from))
    })?;
    if waiting.0 == row.kid {
        tracing::info!(kid = %row.kid, signs_from = row.signs_from, "roll's OIDC issuer: the next signing key is published");
    }
    Ok(Some(waiting))
}

/// Retire every live key a later key replaced at least [`RETIRE_AFTER_MS`] ago: its tokens have all
/// expired, so it leaves the JWKS. How many.
pub(crate) fn retire(db: &Db, keys: &Keyring, checked: &Checked) -> Result<usize> {
    let now = db.now_ms()?;
    let live: Vec<KeyRow> = trusted(db, keys, checked)?.into_iter().filter(|r| r.retired_at.is_none()).collect();
    // The latest key that has signed long enough for every older one's tokens to have expired.
    let Some(newest) = live.iter().find(|r| r.signs_from + RETIRE_AFTER_MS <= now) else { return Ok(0) };
    let old: Vec<&str> = live.iter().filter(|r| r.signs_from < newest.signs_from).map(|r| r.kid.as_str()).collect();
    let mut n = 0;
    for kid in old {
        n += db.write(|c| c.execute("UPDATE oidc_keys SET retired_at = ? WHERE kid = ? AND retired_at IS NULL", params![now, kid]))?;
        tracing::info!(kid, "roll's OIDC issuer: a replaced signing key left the JWKS");
    }
    Ok(n)
}

impl Issuer {
    /// A token for `project`, for `audience`: what a cloud exchanges for its own access token.
    pub(crate) fn sign(&self, db: &Db, keys: &Keyring, project: &str, audience: &str) -> Result<Value> {
        self.sign_for(db, keys, &subject(project), audience, TOKEN_SECONDS, json!({ "project": project }))
    }

    /// A token with `sub`, for `audience`, living `ttl_s` seconds, with `extra` claims.
    pub(crate) fn sign_for(&self, db: &Db, keys: &Keyring, sub: &str, audience: &str, ttl_s: i64, extra: Json) -> Result<Value> {
        let (kid, pair) = signer(db, keys)?;
        let now = db.now_ms()? / 1000;
        let mut jti = [0u8; 16];
        getrandom::fill(&mut jti).map_err(|e| Error::Internal(e.to_string()))?;
        let header = json!({ "alg": "RS256", "typ": "JWT", "kid": kid });
        let mut claims = json!({ "iss": self.url, "sub": sub, "aud": audience, "iat": now, "nbf": now, "exp": now + ttl_s, "jti": b64(&jti) });
        if let (Some(c), Json::Object(extra)) = (claims.as_object_mut(), extra) {
            c.extend(extra);
        }
        let signed = format!("{}.{}", b64(header.to_string().as_bytes()), b64(claims.to_string().as_bytes()));
        let mut sig = vec![0u8; pair.public().modulus_len()];
        pair.sign(&ring::signature::RSA_PKCS1_SHA256, &ring::rand::SystemRandom::new(), signed.as_bytes(), &mut sig).map_err(|_| Error::Internal("signing failed".into()))?;
        Ok(Value::new(format!("{signed}.{}", b64(&sig))))
    }

    /// A token for another roll product's `subject` (checked), for `aud`, for `ttl_s` seconds.
    pub(crate) fn sign_subject(&self, db: &Db, keys: &Keyring, subject: &Subject, aud: &str, ttl_s: i64) -> Result<Value> {
        self.sign_for(db, keys, &subject.sub(), aud, ttl_s, subject.claims())
    }

    pub(crate) fn discovery(&self) -> Json {
        json!({
            "issuer": self.url,
            "jwks_uri": format!("{}/jwks", self.url),
            "response_types_supported": ["id_token"],
            "subject_types_supported": ["public"],
            "id_token_signing_alg_values_supported": ["RS256"],
            "claims_supported": ["sub", "aud", "exp", "iat", "iss", "jti", "nbf", "project", "service", "environment"],
        })
    }
}

/// What a person reads about issuing in the audit log: one row per subject, audience and hour.
const HOUR_MS: i64 = 3_600_000;

impl crate::Secrets {
    /// A token of roll's issuer for another roll product's `subject`, for `aud`, living `ttl` (an
    /// hour at most), in process only: roll apps gives its instances theirs (Vercel's OIDC
    /// federation), which clouds and roll secrets' own workload logins trust. Never logged. The
    /// first token per subject, audience and hour is an audit row in the subject's project; the
    /// rest only count (`oidc_issued`), so an app that refreshes often doesn't flood the log.
    pub fn issue(&self, subject: Subject, aud: &str, ttl: std::time::Duration) -> Result<String> {
        subject.check()?;
        if aud.is_empty() || aud.len() > 256 || aud.chars().any(char::is_control) {
            return Err(Error::BadRequest("an audience is 1–256 characters".into()));
        }
        let ttl_s = ttl.as_secs().min(TOKEN_MAX_SECONDS as u64) as i64;
        if ttl_s < 1 {
            return Err(Error::BadRequest("a token lives at least a second".into()));
        }
        let url = self.clouds.as_ref().map(|c| c.issuer.clone()).ok_or_else(|| Error::NotFound("roll's issuer isn't on this server (rolld needs its public URL)".into()))?;
        let keys = self.keys()?;
        let project = subject.project().to_string();
        self.project(&project)?;
        let token = Issuer { url }.sign_subject(&self.db, keys, &subject, aud, ttl_s)?;
        let (sub, now) = (subject.sub(), self.db.now_ms()?);
        let Subject::App { service, env, .. } = &subject;
        let caller = crate::Caller::consumer(&format!("app:{service}"));
        self.db.write(|c| {
            let first = c.execute(
                "INSERT INTO oidc_issued (subject, audience, hour, project_id, tokens) VALUES (?, ?, ?, ?, 1) ON CONFLICT (subject, audience, hour) DO NOTHING",
                params![sub, aud, now / HOUR_MS, project],
            )? == 1;
            if !first {
                c.execute("UPDATE oidc_issued SET tokens = tokens + 1 WHERE subject = ? AND audience = ? AND hour = ?", params![sub, aud, now / HOUR_MS])?;
                return Ok::<_, Error>(());
            }
            crate::audit::record(c, keys, &project, &caller, "oidc.issue", Some(env), None, &[], &format!("roll's issuer signed tokens for {sub} (audience {aud}, {ttl_s} s; the first this hour)"), now)
        })?;
        Ok(token.expose().to_string())
    }

    /// Whether roll's issuer is on this server (rolld has a public URL for it): [`crate::Secrets::issue`]
    /// signs only then.
    pub fn issues(&self) -> bool {
        self.clouds.is_some()
    }

    /// Days between scheduled rotations of the issuer's key (`ROLL_SECRETS_OIDC_ROTATE_DAYS`;
    /// [`ROTATE_DAYS`] when unset). At least 3: a key waits [`PUBLISH_AHEAD_MS`] to sign.
    pub fn set_issuer_rotation_days(&self, days: u32) {
        self.issuer_rotate_days.store(days.max(3), std::sync::atomic::Ordering::Relaxed);
    }

    fn issuer_rotate_ms(&self) -> i64 {
        self.issuer_rotate_days.load(std::sync::atomic::Ordering::Relaxed) as i64 * 86_400_000
    }

    /// `POST /v1/secrets/issuer/rotate` (the operator's): start a rotation now. The next key is
    /// published at once and signs from [`PUBLISH_AHEAD_MS`] later, as a scheduled one; a key
    /// already waiting is answered instead.
    pub fn rotate_issuer(&self, caller: &crate::Caller) -> Result<Json> {
        if !caller.grants.everything() || caller.principal != "operator" {
            return Err(Error::Forbidden("roll's issuer key is the operator's".into()));
        }
        let (kid, signs_from) = rotate(&self.db, self.keys()?, &self.issuer_keys, self.issuer_rotate_ms(), true)?.ok_or_else(|| Error::Internal("no key to rotate".into()))?;
        Ok(json!({ "kid": kid, "signs_from": signs_from }))
    }

    /// The sweeper: a rotation when one is due, replaced keys out of the JWKS once their tokens
    /// have expired, and issue counts after two days.
    pub(crate) fn sweep_issuer(&self) -> Result<()> {
        let Ok(keys) = self.keys() else { return Ok(()) };
        rotate(&self.db, keys, &self.issuer_keys, self.issuer_rotate_ms(), false)?;
        retire(&self.db, keys, &self.issuer_keys)?;
        let old = self.db.now_ms()? / HOUR_MS - 48;
        self.db.write(|c| c.execute("DELETE FROM oidc_issued WHERE hour < ?", [old]))?;
        Ok(())
    }
}
