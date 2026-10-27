//! Who's calling: `apikey` and `Authorization: Bearer <jwt>`, as Supabase's gateway and storage
//! server take them.
//!
//! - A **project key** (the anon or service key, `iss: roll`) in `Authorization`, or alone in
//!   `apikey`, is the project's anon role or its service role.
//! - Any other bearer token is a **user token** of the project the `apikey` names (the anon or
//!   service key; required, as Supabase's gateway requires it): HS256 with the project's JWT
//!   secret (`iss`/`aud` checked when configured), or RS256/ES256 from one of its OIDC issuers
//!   (keys from the issuer's JWKS, fetched over HTTPS and cached). `exp` and `nbf` are checked
//!   (30 s leeway). Its `role` claim (default `authenticated`) is the role bucket policies name;
//!   `service_role` in a user token is just a role name, never the service key's powers.
//!
//! Tokens are never logged or echoed in errors.

use crate::error::{ApiError, ApiResult};
use crate::req::Req;
use s3_server::app::AppCaller;
use s3_store::api::{ApiKeyError, ApiRole, jwt};
use serde_json::{Map, Value};

/// Seconds of clock skew allowed on `exp` and `nbf`.
const LEEWAY_S: i64 = 30;

fn bearer(r: &Req) -> Option<&str> {
    let v = r.h("authorization")?.trim();
    let (scheme, token) = v.split_once(' ')?;
    scheme.eq_ignore_ascii_case("bearer").then(|| token.trim()).filter(|t| !t.is_empty())
}

fn apikey(r: &Req) -> Option<&str> {
    r.h("apikey").map(str::trim).filter(|t| !t.is_empty()).or_else(|| r.q("apikey").filter(|t| !t.is_empty()))
}

fn key_error(e: ApiKeyError) -> ApiError {
    match e {
        ApiKeyError::Invalid(m) => ApiError::invalid_jwt(m),
        ApiKeyError::Store(e) => e.into(),
    }
}

/// The caller, or anonymous when the request carries no key at all (public objects).
pub(crate) async fn optional(r: &Req) -> ApiResult<AppCaller> {
    if bearer(r).is_none() && apikey(r).is_none() {
        return Ok(AppCaller::anonymous());
    }
    required(r).await
}

/// The caller; a request without credentials is refused.
pub(crate) async fn required(r: &Req) -> ApiResult<AppCaller> {
    let store = r.store();
    let token = match (bearer(r), apikey(r)) {
        (Some(t), _) | (None, Some(t)) => t,
        (None, None) => return Err(ApiError::denied("Access denied: no authorization (send the project's API key as apikey and Authorization: Bearer)").with_http(400)),
    };
    let caller = match store.verify_api_key(token).map_err(key_error)? {
        Some(k) => project_caller(r, &k)?,
        None => user(r, token).await?,
    };
    let principal = caller.event_principal();
    s3_store::events::set_origin(|o| o.principal = Some(principal));
    Ok(caller)
}

fn project_caller(r: &Req, k: &s3_store::ApiKey) -> ApiResult<AppCaller> {
    let store = r.store();
    Ok(match k.role {
        ApiRole::Service => AppCaller::service(store, &k.account)?,
        ApiRole::Anon => {
            let mut claims = Map::new();
            claims.insert("role".into(), Value::String("anon".into()));
            AppCaller::app(store, &k.account, "anon", claims)?
        }
    })
}

/// A user token of the project named by `apikey`.
async fn user(r: &Req, token: &str) -> ApiResult<AppCaller> {
    let store = r.store();
    let key = apikey(r)
        .and_then(|k| store.verify_api_key(k).transpose())
        .transpose()
        .map_err(key_error)?
        .ok_or_else(|| ApiError::invalid_jwt("Invalid JWT: send the project's anon key as the apikey header with a user token"))?;
    let Some(project) = store.api_project_if_any(&key.account)? else { return Err(ApiError::invalid_jwt("Invalid JWT: the project has no auth providers")) };
    let d = jwt::decode(token).map_err(|e| ApiError::invalid_jwt(e.to_string()))?;
    let now_s = store.now().div_euclid(1000);
    let claims = match d.alg.as_str() {
        "HS256" => {
            let Some(secret) = &project.jwt_secret else { return Err(ApiError::invalid_jwt("invalid signature: the project has no JWT secret")) };
            if !jwt::hs256_valid(secret.expose().as_bytes(), &d) {
                return Err(ApiError::invalid_jwt("invalid signature"));
            }
            jwt::check_times(&d.claims, now_s, LEEWAY_S).map_err(|e| ApiError::invalid_jwt(e.to_string()))?;
            if let Some(iss) = &project.jwt_issuer
                && d.claims.get("iss").and_then(Value::as_str) != Some(iss.as_str())
            {
                return Err(ApiError::invalid_jwt("unexpected \"iss\" claim value"));
            }
            if let Some(aud) = &project.jwt_audience {
                let ok = match d.claims.get("aud") {
                    Some(Value::String(s)) => s == aud,
                    Some(Value::Array(a)) => a.iter().any(|v| v.as_str() == Some(aud.as_str())),
                    _ => false,
                };
                if !ok {
                    return Err(ApiError::invalid_jwt("unexpected \"aud\" claim value"));
                }
            }
            d.claims
        }
        "RS256" | "ES256" if !project.oidc.is_empty() => s3_server::app::verify_oidc(&r.api.server, &project.oidc, token).await.map_err(ApiError::invalid_jwt)?,
        "RS256" | "ES256" => return Err(ApiError::invalid_jwt("invalid signature: the project has no OIDC providers")),
        other => return Err(ApiError::invalid_jwt(format!("unsupported JWT algorithm {other}"))),
    };
    let role = claims.get("role").and_then(Value::as_str).filter(|s| !s.is_empty() && s.len() <= 64).unwrap_or("authenticated").to_string();
    Ok(AppCaller::app(store, &key.account, &role, claims)?)
}
