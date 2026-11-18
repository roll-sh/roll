//! `grant_type=client_credentials` at the token endpoint: a machine client (store/machine.rs)
//! trades its credential for an hour's access token, with no person
//! (`io.modelcontextprotocol/oauth-client-credentials`). It authenticates with its secret
//! (`client_secret_basic` or `client_secret_post`), or with a JWT it signed with its key
//! (`private_key_jwt`, RFC 7523 §2.2): `iss` = `sub` = its id, `aud` this token endpoint or the
//! issuer, at most five minutes to live, and a `jti` never used before.
//!
//! Every failure to authenticate answers the same `invalid_client`, so nothing tells an unknown
//! client from a wrong credential.
//!
//! A client that trusts an identity instead signs in with RFC 7523's `jwt-bearer` grant: its CI's
//! own OIDC token as the `assertion`, and its `client_id` (a shared issuer's token names no roll
//! client). The token is checked by roll-oidc, as roll secrets' workload identities' are, then
//! against the client's subject and claims ([`federated`]).

use crate::{Access, Error, hash};
use axum::http::HeaderMap;
use serde_json::json;
use std::collections::HashMap;
use std::sync::Arc;

pub const GRANT: &str = "client_credentials";
/// RFC 7523 §2.1: a JWT (here, CI's OIDC token) as an authorization grant.
pub const JWT_GRANT: &str = "urn:ietf:params:oauth:grant-type:jwt-bearer";
/// A machine client's sign-ins a minute, per node, counted once its credential checks out (key
/// fetches are roll-oidc's own: at most one per issuer each 30 s).
const SIGN_INS_PER_MINUTE: u32 = 120;
pub const JWT_BEARER: &str = "urn:ietf:params:oauth:client-assertion-type:jwt-bearer";
/// The signature algorithms a machine client's key may use.
pub const ALGS: [&str; 3] = ["EdDSA", "ES256", "RS256"];
/// An assertion lives at most this long.
const MAX_ASSERTION_MS: i64 = 5 * 60 * 1000;
/// Clock skew an assertion's `exp` is given (jsonwebtoken's leeway): its `jti` is kept that much
/// longer too, or a replay would pass in the gap.
const LEEWAY_S: u64 = 30;

fn unknown() -> Error {
    Error::OAuth("invalid_client", "the client's credentials didn't check out".into())
}

/// `Authorization: Basic base64(id:secret)` (RFC 6749 §2.3.1: each part form-urlencoded).
pub(crate) fn basic(headers: &HeaderMap) -> Option<(String, String)> {
    use base64::Engine;
    let v = headers.get(axum::http::header::AUTHORIZATION)?.to_str().ok()?.strip_prefix("Basic ")?;
    let raw = String::from_utf8(base64::engine::general_purpose::STANDARD.decode(v.trim()).ok()?).ok()?;
    let (id, secret) = raw.split_once(':')?;
    let dec = |s: &str| percent_encoding::percent_decode_str(&s.replace('+', " ")).decode_utf8().ok().map(|s| s.into_owned());
    Some((dec(id)?, dec(secret)?))
}

pub(crate) fn same(a: &str, b: &str) -> bool {
    a.len() == b.len() && a.bytes().zip(b.bytes()).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

/// The token for a machine client's request, or why not.
pub async fn token(a: Arc<Access>, headers: &HeaderMap, f: &HashMap<String, String>, ip: Option<std::net::IpAddr>) -> Result<(crate::store::Grant, String), Error> {
    let get = |k: &str| f.get(k).map(|v| v.trim().to_string()).filter(|v| !v.is_empty());
    let api = check_resource(&a, f)?;
    let read_only = read_only_scope(f)?;
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
    let credential = if assertion.is_err() { "secret" } else { "key" };
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
            let kept_until = exp + LEEWAY_S as i64 * 1000;
            if !tokio::task::spawn_blocking(move || a2.store.first_jti(&id, &jti, kept_until)).await.map_err(|e| Error::Internal(e.to_string()))?? {
                return Err(Error::OAuth("invalid_client", "that assertion was used already: sign a new one (a new jti)".into()));
            }
        }
    }
    let (grant_id, generation) = (client.grant_id.clone(), client.generation);
    tokio::task::spawn_blocking(move || {
        // As a trusted client's: counted once the credential checks out, so a holder of a leaked
        // secret can't mint tokens without end.
        if !a.sign_ins.allow(&client_id, SIGN_INS_PER_MINUTE) {
            return Err(Error::TooMany(format!("at most {SIGN_INS_PER_MINUTE} sign-ins a minute for one machine client")));
        }
        let made = a.store.issue_client_access(&grant_id, &client_id, generation, read_only)?;
        audit_sign_in(&a, &made.0, &client_id, json!({ "by": client_id, "credential": credential, "read_only": read_only }), ip);
        Ok(made)
    })
    .await
    .map_err(|e| Error::Internal(e.to_string()))?
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
pub(crate) fn verify(jwt: &str, jwk: &str, client_id: &str, api: &str) -> Result<(String, i64), Error> {
    use jsonwebtoken::{Algorithm, DecodingKey, Validation, decode, decode_header};
    let bad = |why: &str| Error::OAuth("invalid_client", format!("the client assertion {why}"));
    // Until the signature checks out, every refusal is the same `invalid_client` an unknown client
    // gets: nothing tells a client that signs with a key (or its kind) from one that doesn't exist.
    let header = decode_header(jwt).map_err(|_| unknown())?;
    if !matches!(header.alg, Algorithm::EdDSA | Algorithm::ES256 | Algorithm::RS256) {
        return Err(unknown());
    }
    let jwk: jsonwebtoken::jwk::Jwk = serde_json::from_str(jwk).map_err(|_| unknown())?;
    let key = DecodingKey::from_jwk(&jwk).map_err(|_| unknown())?;
    let mut v = Validation::new(header.alg);
    v.set_audience(&[format!("{api}/v1/access/oauth/token"), api.to_string()]);
    v.set_issuer(&[client_id]);
    v.sub = Some(client_id.to_string());
    v.set_required_spec_claims(&["exp", "iss", "sub", "aud"]);
    v.leeway = LEEWAY_S;
    let data = decode::<serde_json::Value>(jwt, &key, &v).map_err(|_| unknown())?;
    let exp = data.claims["exp"].as_i64().and_then(|s| s.checked_mul(1000)).ok_or_else(|| bad("has no exp a time can hold"))?;
    if exp - code_sql::now_ms() > MAX_ASSERTION_MS {
        return Err(bad("lives longer than five minutes"));
    }
    let jti = data.claims["jti"].as_str().filter(|j| !j.is_empty() && j.len() <= 200).ok_or_else(|| bad("has no jti"))?;
    Ok((jti.to_string(), exp))
}

/// A machine client's sign-in, in the audit log (source `access`, `sign_in`) on its grant: who,
/// with which kind of credential (and a CI token's issuer and subject), from where. Never a secret.
pub(crate) fn audit_sign_in(a: &Access, grant: &crate::store::Grant, client_id: &str, args: serde_json::Value, ip: Option<std::net::IpAddr>) {
    let e = crate::store::AuditEvent {
        id: crate::new_id("aud"),
        at: code_sql::now_ms(),
        grant_id: Some(grant.id.clone()),
        grant_name: Some(grant.name.clone()),
        source: "access".into(),
        tool: None,
        method: "POST".into(),
        route: "/v1/access/oauth/token".into(),
        product: Some("access".into()),
        action: Some("admin".into()),
        resource: Some(client_id.to_string()),
        decision: "sign_in".into(),
        approval: None,
        status: 200,
        args: Some(args),
        ip: ip.map(|i| i.to_string()),
        duration_ms: 0,
        client: None,
    };
    if let Some(e) = a.store.queue_audit(e) {
        a.store.audit(&e);
    }
}

/// What `scope` asks of a machine client's token: nothing narrower (none, or "granted"), or
/// `read-only` (its rules' reads alone). A token never gets more than its client's rules.
pub fn read_only_scope(f: &HashMap<String, String>) -> Result<bool, Error> {
    let words: Vec<&str> = f.get("scope").map(|s| s.split_whitespace().collect()).unwrap_or_default();
    match words.as_slice() {
        [] | ["granted"] => Ok(false),
        ["read-only"] => Ok(true),
        _ => Err(Error::OAuth("invalid_scope", "a machine client's token is its rules (no scope) or their reads (scope=read-only)".into())),
    }
}

/// `resource`, if given, is roll's API or its MCP server: roll's API URL.
fn check_resource(a: &Access, f: &HashMap<String, String>) -> Result<String, Error> {
    let api = a.config.api_url.trim_end_matches('/').to_string();
    if let Some(r) = f.get("resource").map(|v| v.trim()).filter(|v| !v.is_empty())
        && r.trim_end_matches('/') != api
        && r.trim_end_matches('/') != format!("{api}/v1/mcp")
    {
        return Err(Error::OAuth("invalid_target", "resource is roll's API or its MCP server".into()));
    }
    Ok(api)
}

/// The token for a trusted machine client's `jwt-bearer` grant: its CI's OIDC token checked
/// against the identity it trusts (or, after a rotation that kept it, the one before).
pub async fn federated(a: Arc<Access>, f: &HashMap<String, String>, ip: Option<std::net::IpAddr>) -> Result<(crate::store::Grant, String), Error> {
    check_resource(&a, f)?;
    let read_only = read_only_scope(f)?;
    let get = |k: &str| f.get(k).map(|v| v.trim().to_string()).filter(|v| !v.is_empty());
    let client_id = get("client_id").ok_or_else(|| Error::OAuth("invalid_request", "client_id names the machine client this token signs in as".into()))?;
    let assertion = get("assertion").ok_or_else(|| Error::OAuth("invalid_request", "assertion is the CI's OIDC token".into()))?;
    tokio::task::spawn_blocking(move || {
        let refused = |why: String| Error::OAuth("invalid_grant", why);
        let client = a.store.machine_client(&client_id)?.ok_or_else(|| refused("no machine client trusts this token".into()))?;
        let identities: Vec<crate::store::TrustedIdentity> = [&client.oidc, &client.old_oidc].into_iter().flatten().filter_map(|o| serde_json::from_str(o).ok()).collect();
        if identities.is_empty() {
            return Err(refused("no machine client trusts this token".into()));
        }
        let jwt = roll_oidc::Jwt::parse(&assertion).map_err(|e| refused(e.to_string()))?;
        let now_s = code_sql::now_ms() / 1000;
        let fetch = a.oidc_fetch.get().map(|f| f.as_ref() as &dyn roll_oidc::Fetch);
        let mut why = String::new();
        for i in &identities {
            match a.oidc_keys.verify(fetch, i.jwks.as_deref(), &jwt, &i.issuer, &i.audience, now_s) {
                Ok(()) => {}
                // The issuer's keys couldn't be had: say so, it's not the token's fault.
                Err(e @ (roll_oidc::Error::Unavailable(_) | roll_oidc::Error::Fetch(_) | roll_oidc::Error::Off)) => {
                    why = format!("its issuer's keys couldn't be checked: {e}");
                    continue;
                }
                Err(e) => {
                    why = e.to_string();
                    continue;
                }
            }
            let sub = jwt.str("sub").unwrap_or_default();
            if !roll_oidc::wildcard(&i.subject, sub) {
                why = "the token's subject isn't the one this client trusts".into();
                continue;
            }
            if let Some((k, _)) = i.claims.iter().find(|(k, want)| !jwt.str(k).is_some_and(|have| roll_oidc::wildcard(want, have))) {
                why = format!("the token's {k} isn't the one this client trusts");
                continue;
            }
            // Counted only once a token checks out: anyone can send junk naming a client, and
            // that mustn't lock its CI out. This caps what a valid identity mints.
            if !a.sign_ins.allow(&client_id, SIGN_INS_PER_MINUTE) {
                return Err(Error::TooMany(format!("at most {SIGN_INS_PER_MINUTE} sign-ins a minute for one machine client")));
            }
            let made = a.store.issue_client_access(&client.grant_id, &client_id, client.generation, read_only)?;
            // Which CI run it was: the token's subject (`repo:acme/web:ref:…`), never the token.
            audit_sign_in(&a, &made.0, &client_id, json!({ "by": client_id, "credential": "oidc", "issuer": i.issuer, "subject": sub, "read_only": read_only }), ip);
            return Ok(made);
        }
        Err(refused(why))
    })
    .await
    .map_err(|e| Error::Internal(e.to_string()))?
}
