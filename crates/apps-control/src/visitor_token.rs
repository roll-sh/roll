//! A token the app can check for its visitor (IAP's `x-goog-iap-jwt-assertion`, Cloudflare
//! Access's JWT; docs/apps/README.md, "Signing in to an environment"): beside `x-roll-visitor`,
//! a request of a sign-in's session carries `x-roll-visitor-jwt`, an ES256 JWT for five minutes
//! that roll signs with a P-256 key of the environment's own for this (sealed; never the
//! provenance key; a preview's tokens never verify for production): `iss` and `aud` the origin
//! the visitor asked (`https://wiki.acme.com`, no port), `sub` who they are for good
//! (`sso:<provider>:<subject>`, or a roll person's `usr_…`), `email` when a provider vouched for
//! one, and `env` the service and environment. The environment's own hosts serve the key as a
//! JWKS at [`JWKS_PATH`], so a service another service can reach directly checks the token rather
//! than trusting the header.

use crate::Apps;
use base64::Engine;
use serde_json::{Value, json};
use std::sync::Arc;

/// Where an environment's hosts serve the key its visitors' tokens are signed with.
pub const JWKS_PATH: &str = "/__roll/jwks.json";
/// How long a token is good for (a session's requests share one until a minute before).
const TTL_S: i64 = 300;

fn b64(b: &[u8]) -> String {
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(b)
}

/// A key's id: the start of its public key's SHA-256.
fn kid(public_pem: &str) -> String {
    use sha2::{Digest, Sha256};
    Sha256::digest(public_pem.as_bytes()).iter().take(8).map(|b| format!("{b:02x}")).collect()
}

/// The key as a JWK (RFC 7517/7518: EC, P-256, ES256).
pub fn jwk(public_pem: &str) -> Option<Value> {
    use p256::pkcs8::DecodePublicKey;
    let key = p256::PublicKey::from_public_key_pem(public_pem).ok()?;
    let point = p256::EncodedPoint::from(key);
    Some(json!({ "kty": "EC", "crv": "P-256", "alg": "ES256", "use": "sig", "kid": kid(public_pem), "x": b64(point.x()?), "y": b64(point.y()?) }))
}

/// `claims` as an ES256 JWT, signed with `key` (PKCS#8 DER).
pub fn sign(key: &[u8], public_pem: &str, claims: &Value) -> Option<String> {
    use p256::ecdsa::signature::Signer;
    use p256::pkcs8::DecodePrivateKey;
    let signing = p256::ecdsa::SigningKey::from_pkcs8_der(key).ok()?;
    let head = b64(json!({ "alg": "ES256", "typ": "JWT", "kid": kid(public_pem) }).to_string().as_bytes());
    let body = b64(claims.to_string().as_bytes());
    let input = format!("{head}.{body}");
    let sig: p256::ecdsa::Signature = signing.sign(input.as_bytes());
    Some(format!("{input}.{}", b64(&sig.to_bytes())))
}

impl Apps {
    /// The environment's key for its visitors' tokens (made once, sealed), kept on this node.
    async fn visitor_key(self: &Arc<Self>, service: &str, env: &str) -> Option<(Vec<u8>, String)> {
        let name = format!("visitor:{service}/{env}");
        if let Some(k) = self.visitor_keys.lock().get(&name).cloned() {
            return Some(k);
        }
        let n = name.clone();
        let k = self.db(move |s| s.provenance_key(&n)).await.ok()?;
        let mut cache = self.visitor_keys.lock();
        if cache.len() >= 10_000 {
            cache.clear();
        }
        cache.insert(name, k.clone());
        Some(k)
    }

    /// `JWKS_PATH`'s answer for an environment.
    pub(crate) async fn visitor_jwks(self: &Arc<Self>, service: &str, env: &str) -> Option<Value> {
        let (_, public) = self.visitor_key(service, env).await?;
        Some(json!({ "keys": [jwk(&public)?] }))
    }

    /// A token for `user` (shown to the app as `shown`) at `origin` in the environment, reused
    /// until a minute before it ends.
    pub(crate) async fn visitor_jwt(self: &Arc<Self>, service: &str, env: &str, origin: &str, user: &str, shown: &str) -> Option<String> {
        let now = code_sql::now_ms() / 1000;
        let key = format!("{service}/{env}\n{origin}\n{user}\n{shown}");
        if let Some((t, exp)) = self.visitor_tokens.lock().get(&key).cloned()
            && exp - now > 60
        {
            return Some(t);
        }
        let (der, public) = self.visitor_key(service, env).await?;
        let mut claims = json!({ "iss": origin, "aud": origin, "sub": user, "env": format!("{service}/{env}"), "iat": now, "exp": now + TTL_S });
        if shown.contains('@') {
            claims["email"] = json!(shown);
        }
        let token = sign(&der, &public, &claims)?;
        let mut cache = self.visitor_tokens.lock();
        if cache.len() >= 10_000 {
            cache.retain(|_, (_, exp)| *exp > now);
            if cache.len() >= 10_000 {
                cache.clear();
            }
        }
        cache.insert(key, (token.clone(), now + TTL_S));
        Some(token)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_token_verifies_against_its_jwk() {
        use p256::ecdsa::signature::Verifier;
        let (der, public) = crate::provenance::new_key();
        let t = sign(&der, &public, &json!({ "sub": "ann@acme.com" })).unwrap();
        let k = jwk(&public).unwrap();
        let d = |s: &str| base64::engine::general_purpose::URL_SAFE_NO_PAD.decode(s).unwrap();
        let (x, y) = (d(k["x"].as_str().unwrap()), d(k["y"].as_str().unwrap()));
        let point = p256::EncodedPoint::from_affine_coordinates(x.as_slice().into(), y.as_slice().into(), false);
        let vk = p256::ecdsa::VerifyingKey::from_encoded_point(&point).unwrap();
        let (input, sig) = t.rsplit_once('.').unwrap();
        let sig = p256::ecdsa::Signature::from_slice(&d(sig)).unwrap();
        assert!(vk.verify(input.as_bytes(), &sig).is_ok());
        assert!(vk.verify(b"another", &sig).is_err());
        let head: Value = serde_json::from_slice(&d(t.split('.').next().unwrap())).unwrap();
        assert_eq!((head["alg"].as_str(), head["kid"].as_str()), (Some("ES256"), k["kid"].as_str()));
    }
}
