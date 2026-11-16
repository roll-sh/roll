//! `grant_type=client_credentials` at the token endpoint: a machine client (store/machine.rs)
//! trades its credential for an hour's access token, with no person
//! (`io.modelcontextprotocol/oauth-client-credentials`). It authenticates with its secret
//! (`client_secret_basic` or `client_secret_post`), or with a JWT it signed with its key
//! (`private_key_jwt`, RFC 7523 §2.2): `iss` = `sub` = its id, `aud` this token endpoint or the
//! issuer, at most five minutes to live, and a `jti` never used before.
//!
//! Every failure to authenticate answers the same `invalid_client`, so nothing tells an unknown
//! client from a wrong credential.

use crate::{Access, Error, hash};
use axum::http::HeaderMap;
use std::collections::HashMap;
use std::sync::Arc;

pub const GRANT: &str = "client_credentials";
pub const JWT_BEARER: &str = "urn:ietf:params:oauth:client-assertion-type:jwt-bearer";
/// The signature algorithms a machine client's key may use.
pub const ALGS: [&str; 3] = ["EdDSA", "ES256", "RS256"];
/// An assertion lives at most this long.
const MAX_ASSERTION_MS: i64 = 5 * 60 * 1000;

fn unknown() -> Error {
    Error::OAuth("invalid_client", "the client's credentials didn't check out".into())
}

/// `Authorization: Basic base64(id:secret)` (RFC 6749 §2.3.1: each part form-urlencoded).
fn basic(headers: &HeaderMap) -> Option<(String, String)> {
    use base64::Engine;
    let v = headers.get(axum::http::header::AUTHORIZATION)?.to_str().ok()?.strip_prefix("Basic ")?;
    let raw = String::from_utf8(base64::engine::general_purpose::STANDARD.decode(v.trim()).ok()?).ok()?;
    let (id, secret) = raw.split_once(':')?;
    let dec = |s: &str| percent_encoding::percent_decode_str(&s.replace('+', " ")).decode_utf8().ok().map(|s| s.into_owned());
    Some((dec(id)?, dec(secret)?))
}

fn same(a: &str, b: &str) -> bool {
    a.len() == b.len() && a.bytes().zip(b.bytes()).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

/// The token for a machine client's request, or why not.
pub async fn token(a: Arc<Access>, headers: &HeaderMap, f: &HashMap<String, String>) -> Result<(crate::store::Grant, String), Error> {
    let get = |k: &str| f.get(k).map(|v| v.trim().to_string()).filter(|v| !v.is_empty());
    let api = a.config.api_url.trim_end_matches('/').to_string();
    if let Some(r) = get("resource")
        && r.trim_end_matches('/') != api
        && r.trim_end_matches('/') != format!("{api}/v1/mcp")
    {
        return Err(Error::OAuth("invalid_target", "resource is roll's API or its MCP server".into()));
    }
    let (client_id, assertion) = if let Some((id, secret)) = basic(headers).or_else(|| Some((get("client_id")?, get("client_secret")?))) {
        (id, Err(secret))
    } else if let (Some(kind), Some(jwt)) = (get("client_assertion_type"), get("client_assertion")) {
        if kind != JWT_BEARER {
            return Err(Error::OAuth("invalid_client", format!("client_assertion_type is {JWT_BEARER}")));
        }
        let sub = unverified_sub(&jwt).ok_or_else(unknown)?;
        (sub, Ok(jwt))
    } else {
        return Err(Error::OAuth("invalid_client", "a machine client authenticates with client_secret_basic, client_secret_post or private_key_jwt".into()));
    };
    let a2 = a.clone();
    let id = client_id.clone();
    let client = tokio::task::spawn_blocking(move || a2.store.machine_client(&id)).await.map_err(|e| Error::Internal(e.to_string()))??.ok_or_else(unknown)?;
    match assertion {
        Err(secret) => {
            // Its secret, or the one before a rotation while that still works.
            let h = hash(&secret);
            if ![&client.secret_hash, &client.old_secret_hash].into_iter().flatten().any(|s| same(&h, s)) {
                return Err(unknown());
            }
        }
        Ok(jwt) => {
            let keys: Vec<&str> = [&client.jwk, &client.old_jwk].into_iter().flatten().map(String::as_str).collect();
            if keys.is_empty() {
                return Err(unknown());
            }
            // Its key, or the one before a rotation: the first that verifies.
            let mut checked = Err(unknown());
            for jwk in keys {
                checked = verify(&jwt, jwk, &client_id, &api);
                if checked.is_ok() {
                    break;
                }
            }
            let (jti, exp) = checked?;
            let a2 = a.clone();
            let id = client_id.clone();
            if !tokio::task::spawn_blocking(move || a2.store.first_jti(&id, &jti, exp)).await.map_err(|e| Error::Internal(e.to_string()))?? {
                return Err(Error::OAuth("invalid_client", "that assertion was used already: sign a new one (a new jti)".into()));
            }
        }
    }
    let grant_id = client.grant_id.clone();
    tokio::task::spawn_blocking(move || a.store.issue_client_access(&grant_id, &client_id)).await.map_err(|e| Error::Internal(e.to_string()))?
}

/// The `sub` of a JWT, before it's verified: only to find whose key verifies it.
fn unverified_sub(jwt: &str) -> Option<String> {
    use base64::Engine;
    let payload = jwt.split('.').nth(1)?;
    let bytes = base64::engine::general_purpose::URL_SAFE_NO_PAD.decode(payload.trim_end_matches('=')).ok()?;
    let v: serde_json::Value = serde_json::from_slice(&bytes).ok()?;
    v["sub"].as_str().map(str::to_string)
}

/// Check an assertion against the client's key: answers its `jti` and expiry.
fn verify(jwt: &str, jwk: &str, client_id: &str, api: &str) -> Result<(String, i64), Error> {
    use jsonwebtoken::{Algorithm, DecodingKey, Validation, decode, decode_header};
    let bad = |why: &str| Error::OAuth("invalid_client", format!("the client assertion {why}"));
    let header = decode_header(jwt).map_err(|_| bad("isn't a JWT"))?;
    let alg = match header.alg {
        Algorithm::EdDSA => "EdDSA",
        Algorithm::ES256 => "ES256",
        Algorithm::RS256 => "RS256",
        _ => return Err(bad(&format!("is signed with an algorithm roll doesn't take (one of {})", ALGS.join(", ")))),
    };
    let jwk: jsonwebtoken::jwk::Jwk = serde_json::from_str(jwk).map_err(|_| unknown())?;
    let key = DecodingKey::from_jwk(&jwk).map_err(|_| unknown())?;
    let mut v = Validation::new(header.alg);
    v.set_audience(&[format!("{api}/v1/access/oauth/token"), api.to_string()]);
    v.set_issuer(&[client_id]);
    v.sub = Some(client_id.to_string());
    v.set_required_spec_claims(&["exp", "iss", "sub", "aud"]);
    v.leeway = 30;
    let data = decode::<serde_json::Value>(jwt, &key, &v).map_err(|e| bad(&format!("didn't verify ({alg}): {e}")))?;
    let exp = data.claims["exp"].as_i64().ok_or_else(|| bad("has no exp"))? * 1000;
    if exp - code_sql::now_ms() > MAX_ASSERTION_MS {
        return Err(bad("lives longer than five minutes"));
    }
    let jti = data.claims["jti"].as_str().filter(|j| !j.is_empty() && j.len() <= 200).ok_or_else(|| bad("has no jti"))?;
    Ok((jti.to_string(), exp))
}
