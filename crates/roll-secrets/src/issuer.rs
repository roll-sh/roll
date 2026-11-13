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

/// The newest signing key, made if there's none: (kid, PKCS#8 DER).
fn signing_key(db: &Db, keys: &Keyring) -> Result<(String, zeroize::Zeroizing<Vec<u8>>)> {
    type Row = (String, String, Vec<u8>);
    let newest = |db: &Db| -> Result<Option<Row>> { Ok(db.read(|c| c.query("SELECT kid, sealed_dek, private FROM oidc_keys ORDER BY created_at DESC, kid LIMIT 1", (), |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?))))?.into_iter().next()) };
    let (kid, sealed_dek, private) = match newest(db)? {
        Some(row) => row,
        None => {
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
            // Two nodes may have made one at once: every published key verifies, and all sign
            // with the same (the newest by time, then id).
            newest(db)?.ok_or_else(|| Error::Internal("the issuer's key vanished".into()))?
        }
    };
    let (dek, _) = keys.open_dek(&dek_owner(&kid), &sealed_dek)?;
    let der = base64::engine::general_purpose::URL_SAFE_NO_PAD.decode(dek.open(&aad(&kid), &private)?.expose()).map_err(|_| Error::Internal("the issuer's key is damaged".into()))?;
    Ok((kid, zeroize::Zeroizing::new(der)))
}

impl Issuer {
    /// A token for `project`, for `audience`: what a cloud exchanges for its own access token.
    pub(crate) fn sign(&self, db: &Db, keys: &Keyring, project: &str, audience: &str) -> Result<Value> {
        let (kid, der) = signing_key(db, keys)?;
        let pair = ring::signature::RsaKeyPair::from_pkcs8(&der).map_err(|e| Error::Internal(format!("the issuer's key: {e}")))?;
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

    /// Every key, so a token from any node verifies.
    pub(crate) fn jwks(&self, db: &Db, keys: &Keyring) -> Result<Json> {
        signing_key(db, keys)?;
        let rows: Vec<(String, String, String)> = db.read(|c| c.query("SELECT kid, n, e FROM oidc_keys ORDER BY created_at DESC, kid", (), |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?))))?;
        Ok(json!({ "keys": rows.into_iter().map(|(kid, n, e)| json!({ "kty": "RSA", "use": "sig", "alg": "RS256", "kid": kid, "n": n, "e": e })).collect::<Vec<_>>() }))
    }
}
