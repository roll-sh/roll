//! Scoped bearer tokens: `roll_<id>_<secret>`, stored hashed in the metadata store (kind
//! `token`), shown once at creation. A token acts as its owner, narrowed to one vhost and to
//! its scopes (`publish:<exchange-regex>`, `consume:<queue-regex>`, `manage:<regex>`).

use crate::HttpApi;
use crate::auth::{Auth, P, parse_body};
use crate::error::{ApiError, ApiResult};
use axum::Json;
use axum::extract::State;
use axum::http::StatusCode;
use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD as B64URL;
use bytes::Bytes;
use mq_core::Broker;
use mq_core::meta::kind;
use parking_lot::RwLock;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

const PREFIX: &str = "roll_";
const ID_LEN: usize = 16;
const CACHE_TTL: Duration = Duration::from_secs(10);

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ScopeKind {
    Publish,
    Consume,
    Manage,
}

impl ScopeKind {
    pub fn as_str(self) -> &'static str {
        match self {
            ScopeKind::Publish => "publish",
            ScopeKind::Consume => "consume",
            ScopeKind::Manage => "manage",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Scope {
    pub kind: ScopeKind,
    /// RabbitMQ-style (unanchored) regex.
    pub pattern: String,
}

impl Scope {
    pub fn parse(s: &str) -> Result<Scope, String> {
        let (k, pattern) = s.split_once(':').ok_or_else(|| format!("invalid scope '{s}': expected '<publish|consume|manage>:<regex>'"))?;
        let kind = match k {
            "publish" => ScopeKind::Publish,
            "consume" => ScopeKind::Consume,
            "manage" => ScopeKind::Manage,
            _ => return Err(format!("invalid scope '{s}': unknown kind '{k}' (publish, consume or manage)")),
        };
        regex::Regex::new(pattern).map_err(|e| format!("invalid scope '{s}': {e}"))?;
        Ok(Scope { kind, pattern: pattern.into() })
    }
}

impl std::fmt::Display for Scope {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}:{}", self.kind.as_str(), self.pattern)
    }
}

/// A stored token (without its secret).
#[derive(Clone, Debug)]
pub struct Token {
    pub id: String,
    pub name: String,
    pub user: String,
    pub vhost: String,
    pub scopes: Vec<Scope>,
    pub created_at: u64,
    /// Unix seconds.
    pub expires_at: Option<u64>,
    hash: String,
}

impl Token {
    fn to_meta(&self) -> Value {
        json!({
            "id": self.id, "name": self.name, "user": self.user, "vhost": self.vhost,
            "scopes": self.scopes.iter().map(|s| s.to_string()).collect::<Vec<_>>(),
            "created_at": self.created_at, "expires_at": self.expires_at, "hash": self.hash,
        })
    }

    fn from_meta(v: &Value) -> Option<Token> {
        Some(Token {
            id: v["id"].as_str()?.into(),
            name: v["name"].as_str().unwrap_or_default().into(),
            user: v["user"].as_str()?.into(),
            vhost: v["vhost"].as_str()?.into(),
            scopes: v["scopes"].as_array()?.iter().filter_map(|s| Scope::parse(s.as_str()?).ok()).collect(),
            created_at: v["created_at"].as_u64().unwrap_or(0),
            expires_at: v["expires_at"].as_u64(),
            hash: v["hash"].as_str()?.into(),
        })
    }

    /// Public view (never includes the secret).
    pub fn to_json(&self) -> Value {
        json!({
            "id": self.id, "name": self.name, "user": self.user, "vhost": self.vhost,
            "scopes": self.scopes.iter().map(|s| s.to_string()).collect::<Vec<_>>(),
            "created_at": self.created_at, "expires_at": self.expires_at,
        })
    }

    fn expired(&self) -> bool {
        self.expires_at.is_some_and(|e| e <= mq_core::now_ms() / 1000)
    }
}

fn hash_secret(secret: &str) -> String {
    hex::encode(Sha256::digest(secret.as_bytes()))
}

fn random_bytes(n: usize) -> Vec<u8> {
    (0..n.div_ceil(8)).flat_map(|_| mq_core::rand_u64().to_le_bytes()).take(n).collect()
}

/// Split `roll_<id>_<secret>`.
fn split(raw: &str) -> Option<(&str, &str)> {
    let rest = raw.strip_prefix(PREFIX)?;
    if rest.len() <= ID_LEN + 1 || !rest.is_char_boundary(ID_LEN) || rest.as_bytes()[ID_LEN] != b'_' {
        return None;
    }
    let (id, secret) = (&rest[..ID_LEN], &rest[ID_LEN + 1..]);
    id.bytes().all(|b| b.is_ascii_hexdigit()).then_some((id, secret))
}

fn constant_eq(a: &[u8], b: &[u8]) -> bool {
    a.len() == b.len() && a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

type Cached = (Option<Arc<Token>>, Instant);

/// Recently used tokens, refreshed from the metadata store every few seconds so revocations
/// on other brokers apply quickly.
#[derive(Default)]
pub struct TokenCache {
    /// id → token (`None`: unknown) and when it was fetched.
    entries: RwLock<HashMap<String, Cached>>,
}

impl TokenCache {
    pub async fn verify(&self, broker: &Broker, raw: &str) -> ApiResult<Arc<Token>> {
        let invalid = || ApiError::unauthorized("invalid or revoked token");
        let (id, secret) = split(raw).ok_or_else(invalid)?;
        let cached = self.entries.read().get(id).filter(|(_, at)| at.elapsed() < CACHE_TTL).map(|(t, _)| t.clone());
        let token = match cached {
            Some(t) => t,
            None => {
                let t = broker.meta.get(kind::TOKEN, id).await?.and_then(|v| Token::from_meta(&v)).map(Arc::new);
                let mut e = self.entries.write();
                if e.len() > 10_000 {
                    e.clear();
                }
                e.insert(id.to_string(), (t.clone(), Instant::now()));
                t
            }
        };
        let token = token.ok_or_else(invalid)?;
        if !constant_eq(hash_secret(secret).as_bytes(), token.hash.as_bytes()) {
            return Err(invalid());
        }
        if token.expired() {
            return Err(ApiError::unauthorized("token expired"));
        }
        Ok(token)
    }

    fn forget(&self, id: &str) {
        self.entries.write().remove(id);
    }
}

fn user_only(auth: &Auth) -> ApiResult<()> {
    match &auth.token {
        Some(_) => Err(ApiError::forbidden("tokens can't manage tokens; use the user's credentials")),
        None => Ok(()),
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct CreateToken {
    name: String,
    vhost: String,
    scopes: Vec<String>,
    /// Seconds.
    #[serde(default)]
    expires_in: Option<u64>,
    /// Administrators may create tokens for other users.
    #[serde(default)]
    user: Option<String>,
}

/// `POST /v1/tokens`
pub async fn create(State(api): State<Arc<HttpApi>>, auth: Auth, body: Bytes) -> ApiResult<(StatusCode, Json<Value>)> {
    user_only(&auth)?;
    let req: CreateToken = parse_body(&body)?;
    if req.name.trim().is_empty() || req.name.len() > 255 {
        return Err(ApiError::bad_request("token name must be 1-255 characters"));
    }
    if req.scopes.is_empty() {
        return Err(ApiError::bad_request("a token needs at least one scope"));
    }
    let scopes = req.scopes.iter().map(|s| Scope::parse(s)).collect::<Result<Vec<_>, _>>().map_err(ApiError::bad_request)?;
    if req.expires_in == Some(0) {
        return Err(ApiError::bad_request("expires_in must be positive"));
    }
    let owner = match req.user {
        Some(u) if u != auth.user.name => {
            if !auth.user.is_admin() {
                return Err(ApiError::forbidden("only administrators can create tokens for other users"));
            }
            api.broker.user(&u).ok_or_else(|| ApiError::not_found(format!("no user '{u}'")))?
        }
        _ => auth.user.clone(),
    };
    if !api.broker.vhost_exists(&req.vhost) {
        return Err(ApiError::not_found(format!("NOT_FOUND - no vhost '{}'", req.vhost)));
    }
    api.broker.check_vhost_access(&owner, &req.vhost).map_err(|e| ApiError::forbidden(e.reply_text()))?;
    let now = mq_core::now_ms() / 1000;
    let id = hex::encode(random_bytes(ID_LEN / 2));
    let secret = B64URL.encode(random_bytes(32));
    let token = Token {
        id: id.clone(),
        name: req.name,
        user: owner.name,
        vhost: req.vhost,
        scopes,
        created_at: now,
        expires_at: req.expires_in.map(|s| now.saturating_add(s)),
        hash: hash_secret(&secret),
    };
    api.broker.meta.put(kind::TOKEN, &id, &token.to_meta()).await?;
    let mut out = token.to_json();
    out["token"] = format!("{PREFIX}{id}_{secret}").into();
    Ok((StatusCode::CREATED, Json(out)))
}

async fn load(api: &HttpApi, id: &str) -> ApiResult<Option<Token>> {
    Ok(api.broker.meta.get(kind::TOKEN, id).await?.and_then(|v| Token::from_meta(&v)))
}

fn visible(auth: &Auth, t: &Token) -> bool {
    auth.user.is_admin() || t.user == auth.user.name
}

/// `GET /v1/tokens` (administrators see every token, others their own).
pub async fn list(State(api): State<Arc<HttpApi>>, auth: Auth) -> ApiResult<Json<Value>> {
    user_only(&auth)?;
    let mut out: Vec<Token> = api.broker.meta.list(kind::TOKEN).await?.iter().filter_map(|(_, v)| Token::from_meta(v)).filter(|t| visible(&auth, t)).collect();
    out.sort_by_key(|t| t.created_at);
    Ok(Json(Value::Array(out.iter().map(Token::to_json).collect())))
}

/// `GET /v1/tokens/{id}`
pub async fn get_one(State(api): State<Arc<HttpApi>>, auth: Auth, P(id): P<String>) -> ApiResult<Json<Value>> {
    user_only(&auth)?;
    match load(&api, &id).await? {
        Some(t) if visible(&auth, &t) => Ok(Json(t.to_json())),
        _ => Err(ApiError::not_found(format!("no token '{id}'"))),
    }
}

/// `DELETE /v1/tokens/{id}`
pub async fn delete(State(api): State<Arc<HttpApi>>, auth: Auth, P(id): P<String>) -> ApiResult<StatusCode> {
    user_only(&auth)?;
    match load(&api, &id).await? {
        Some(t) if visible(&auth, &t) => {
            api.broker.meta.delete(kind::TOKEN, &id).await?;
            api.tokens.forget(&id);
            Ok(StatusCode::NO_CONTENT)
        }
        _ => Err(ApiError::not_found(format!("no token '{id}'"))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scopes_parse() {
        assert_eq!(Scope::parse("publish:^orders$").unwrap(), Scope { kind: ScopeKind::Publish, pattern: "^orders$".into() });
        assert_eq!(Scope::parse("consume:a:b").unwrap().pattern, "a:b");
        assert!(Scope::parse("write:.*").is_err());
        assert!(Scope::parse("publish:(").is_err());
        assert!(Scope::parse("nocolon").is_err());
    }

    #[test]
    fn token_format() {
        let id = "0123456789abcdef";
        let raw = format!("{PREFIX}{id}_s3cr_et");
        assert_eq!(split(&raw), Some((id, "s3cr_et")));
        assert_eq!(split("roll_short_x"), None);
        assert_eq!(split("bearer"), None);
        assert_eq!(split("roll_0123456789abcdeg_x"), None);
    }
}
