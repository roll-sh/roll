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
//! - The signing key is made once (RSA 2048), sealed with the deployment key, and shared by every
//!   node through the database; a node that finds none makes one, and all published keys verify.
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
";

/// How long a token lives: long enough for one exchange.
pub const TOKEN_SECONDS: i64 = 300;

/// The subject a cloud's trust names for `project`.
pub fn subject(project: &str) -> String {
    format!("roll-secrets:{project}")
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
}

/// Every row, newest first.
fn rows(db: &Db) -> Result<Vec<KeyRow>> {
    Ok(db.read(|c| {
        c.query("SELECT kid, sealed_dek, private, n, e FROM oidc_keys ORDER BY created_at DESC, kid", (), |r| Ok(KeyRow { kid: r.get(0)?, sealed_dek: r.get(1)?, private: r.get(2)?, n: r.get(3)?, e: r.get(4)? }))
    })?)
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

/// Make a signing key: (kid, PKCS#8 DER).
fn make_key(db: &Db, keys: &Keyring) -> Result<(String, zeroize::Zeroizing<Vec<u8>>)> {
    use rsa::pkcs8::EncodePrivateKey;
    use rsa::traits::PublicKeyParts;
    let key = rsa::RsaPrivateKey::new(&mut rand::thread_rng(), 2048).map_err(|e| Error::Internal(format!("making the issuer's key: {e}")))?;
    let der = key.to_pkcs8_der().map_err(|e| Error::Internal(format!("encoding the issuer's key: {e}")))?;
    let kid = names::new_id("k", 16);
    let (dek, sealed_dek) = keys.new_dek(&dek_owner(&kid));
    let private = dek.seal(&aad(&kid), &Value::new(b64(der.as_bytes())));
    let (n, e) = (b64(&key.n().to_bytes_be()), b64(&key.e().to_bytes_be()));
    let now = db.now_ms()?;
    db.write(|c| c.execute("INSERT INTO oidc_keys (kid, sealed_dek, private, n, e, created_at) VALUES (?, ?, ?, ?, ?, ?)", params![kid, sealed_dek, private, n, e, now]))?;
    Ok((kid, zeroize::Zeroizing::new(der.as_bytes().to_vec())))
}

/// Make the signing key if there's none of roll's (at startup: the public keys are there before
/// the first token, and no anonymous request ever makes one). Two nodes may make one each: both
/// are published, and the newest signs.
pub(crate) fn ensure_key(db: &Db, keys: &Keyring) -> Result<()> {
    signing_key(db, keys).map(drop)
}

/// The newest of roll's signing keys, made when there's none: (kid, PKCS#8 DER). Its data key is
/// resealed with the current deployment key when it was sealed with a previous one, as projects'
/// are.
fn signing_key(db: &Db, keys: &Keyring) -> Result<(String, zeroize::Zeroizing<Vec<u8>>)> {
    for r in rows(db)? {
        let Some((der, resealed)) = open_row(keys, &r) else {
            untrusted(&r.kid);
            continue;
        };
        if let Some(new) = resealed {
            db.write(|c| c.execute("UPDATE oidc_keys SET sealed_dek = ? WHERE kid = ? AND sealed_dek = ?", [new.as_str(), r.kid.as_str(), r.sealed_dek.as_str()]))?;
        }
        return Ok((r.kid, der));
    }
    make_key(db, keys)
}

/// The newest key, to sign the audit log's digests (roll-secrets-chain) with: (kid, key).
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

/// The public halves of roll's keys (see [`open_row`]), newest first: (kid, n, e). A row that
/// isn't roll's is left out with a warning naming its kid.
fn trusted(db: &Db, keys: &Keyring, checked: &Checked) -> Result<Vec<(String, String, String)>> {
    let rows = rows(db)?;
    let mut seen = checked.0.lock().unwrap_or_else(|p| p.into_inner());
    seen.retain(|kid, _| rows.iter().any(|r| &r.kid == kid));
    let mut out = Vec::with_capacity(rows.len());
    for r in rows {
        let h = row_hash(&r);
        let ok = match seen.get(&r.kid) {
            Some((was, ok)) if *was == h => *ok,
            _ => {
                let ok = open_row(keys, &r).is_some();
                if !ok {
                    untrusted(&r.kid);
                }
                seen.insert(r.kid.clone(), (h, ok));
                ok
            }
        };
        if ok {
            out.push((r.kid, r.n, r.e));
        }
    }
    Ok(out)
}

/// roll's keys' public halves by kid, to check digests with.
pub(crate) fn public_keys(db: &Db, keys: &Keyring, checked: &Checked) -> Result<std::collections::HashMap<String, crate::chain::PublicKey>> {
    Ok(crate::chain::PublicKey::from_jwks(&jwks(db, keys, checked)?).into_iter().collect())
}

/// roll's keys' public halves, so a token or a digest from any node verifies.
pub(crate) fn jwks(db: &Db, keys: &Keyring, checked: &Checked) -> Result<Json> {
    let rows = trusted(db, keys, checked)?;
    Ok(json!({ "keys": rows.into_iter().map(|(kid, n, e)| json!({ "kty": "RSA", "use": "sig", "alg": "RS256", "kid": kid, "n": n, "e": e })).collect::<Vec<_>>() }))
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

impl Issuer {
    /// A token for `project`, for `audience`: what a cloud exchanges for its own access token.
    pub(crate) fn sign(&self, db: &Db, keys: &Keyring, project: &str, audience: &str) -> Result<Value> {
        let (kid, pair) = signer(db, keys)?;
        let now = db.now_ms()? / 1000;
        let mut jti = [0u8; 16];
        getrandom::fill(&mut jti).map_err(|e| Error::Internal(e.to_string()))?;
        let header = json!({ "alg": "RS256", "typ": "JWT", "kid": kid });
        let claims = json!({ "iss": self.url, "sub": subject(project), "aud": audience, "iat": now, "nbf": now, "exp": now + TOKEN_SECONDS, "jti": b64(&jti), "project": project });
        let signed = format!("{}.{}", b64(header.to_string().as_bytes()), b64(claims.to_string().as_bytes()));
        let mut sig = vec![0u8; pair.public().modulus_len()];
        pair.sign(&ring::signature::RSA_PKCS1_SHA256, &ring::rand::SystemRandom::new(), signed.as_bytes(), &mut sig).map_err(|_| Error::Internal("signing failed".into()))?;
        Ok(Value::new(format!("{signed}.{}", b64(&sig))))
    }

    pub(crate) fn discovery(&self) -> Json {
        json!({
            "issuer": self.url,
            "jwks_uri": format!("{}/jwks", self.url),
            "response_types_supported": ["id_token"],
            "subject_types_supported": ["public"],
            "id_token_signing_alg_values_supported": ["RS256"],
            "claims_supported": ["sub", "aud", "exp", "iat", "iss", "jti", "nbf", "project"],
        })
    }
}
