//! Linear's API as roll calls it: GraphQL at `{api_url}/graphql` and OAuth tokens at
//! `{api_url}/oauth/token` (https://linear.app/developers/graphql,
//! https://linear.app/developers/oauth-2-0-authentication). The address is the server's config,
//! never a tenant's, so it isn't SSRF-guarded; there are no redirects, a timeout, and answers are
//! read up to [`MAX_ANSWER`].
//!
//! An owner's credentials: a personal API key goes in `Authorization` as it is, an OAuth access
//! token as `Bearer`. Access tokens last a day: one that expires within [`REFRESH_AHEAD_MS`] (or
//! that Linear refuses) is refreshed first. Refreshes on one node take turns per owner, and the
//! new tokens are stored by a compare-and-swap on the connection's `token_rev`, so a refresh on
//! another node at the same time doesn't lose a token (Linear answers a repeated refresh within
//! 30 minutes with the same new tokens). A refresh token Linear refuses marks the connection
//! `reauth`.

use super::store::{self, Connection, Kind};
use super::{Linear, LinearConfig};
use crate::Code;
use serde::Deserialize;
use serde_json::{Value, json};
use std::sync::{Arc, OnceLock};
use std::time::Duration;

/// Largest answer read from Linear.
pub const MAX_ANSWER: usize = 4 << 20;
const TIMEOUT: Duration = Duration::from_secs(30);
/// Refresh an access token that expires sooner than this.
pub const REFRESH_AHEAD_MS: i64 = 5 * 60_000;

#[derive(Default)]
pub struct Http(OnceLock<reqwest::Client>);

impl Http {
    fn get(&self) -> reqwest::Client {
        self.0
            .get_or_init(|| {
                reqwest::Client::builder()
                    .timeout(TIMEOUT)
                    .redirect(reqwest::redirect::Policy::none())
                    .user_agent("roll-linear")
                    .build()
                    .expect("an HTTP client")
            })
            .clone()
    }
}

/// Why a call to Linear didn't work.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum LinearError {
    #[error("Linear isn't connected")]
    NotConnected,
    #[error("the Linear connection needs connecting again ({0})")]
    Reauth(String),
    #[error("Linear refused roll's credentials")]
    Unauthorized,
    #[error("Linear's rate limit: try again in {0} s")]
    RateLimited(u64),
    /// The network, a timeout, a 5xx: worth another attempt.
    #[error("{0}")]
    Transient(String),
    /// Linear answered with an error.
    #[error("{0}")]
    Failed(String),
    /// roll can't (ROLL_SECRET_KEY, no OAuth app).
    #[error("{0}")]
    Server(String),
}

impl LinearError {
    pub fn transient(&self) -> bool {
        matches!(self, LinearError::RateLimited(_) | LinearError::Transient(_))
    }
}

/// An owner's credentials for one call. They never print.
#[derive(Clone)]
pub enum Auth {
    Key(String),
    Bearer(String),
}

impl std::fmt::Debug for Auth {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Auth::Key(_) => "Auth::Key([redacted])",
            Auth::Bearer(_) => "Auth::Bearer([redacted])",
        })
    }
}

impl Auth {
    fn header(&self) -> String {
        match self {
            Auth::Key(k) => k.clone(),
            Auth::Bearer(t) => format!("Bearer {t}"),
        }
    }
}

/// `POST /oauth/token`'s answer.
#[derive(Deserialize)]
pub struct TokenAnswer {
    pub access_token: String,
    #[serde(default)]
    pub expires_in: Option<i64>,
    /// Space-separated (an array from apps made before December 2023).
    #[serde(default)]
    pub scope: Value,
    #[serde(default)]
    pub refresh_token: Option<String>,
}

impl TokenAnswer {
    pub fn scopes(&self) -> String {
        match &self.scope {
            Value::String(s) => s.split([' ', ',']).filter(|s| !s.is_empty()).collect::<Vec<_>>().join(","),
            Value::Array(a) => a.iter().filter_map(Value::as_str).collect::<Vec<_>>().join(","),
            _ => String::new(),
        }
    }

    pub fn expires_at(&self) -> Option<i64> {
        self.expires_in.map(|s| code_sql::now_ms() + s * 1000)
    }
}

impl std::fmt::Debug for TokenAnswer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TokenAnswer").field("expires_in", &self.expires_in).field("scope", &self.scope).finish_non_exhaustive()
    }
}

/// Read an answer up to [`MAX_ANSWER`].
async fn read(mut resp: reqwest::Response) -> Result<Vec<u8>, LinearError> {
    let mut body = Vec::new();
    while let Some(chunk) = resp.chunk().await.map_err(|e| LinearError::Transient(format!("reading Linear's answer: {}", e.without_url())))? {
        if body.len() + chunk.len() > MAX_ANSWER {
            return Err(LinearError::Failed(format!("Linear's answer is over {} MiB", MAX_ANSWER >> 20)));
        }
        body.extend_from_slice(&chunk);
    }
    Ok(body)
}

const FORM: &str = "application/x-www-form-urlencoded";

/// A form body (Linear's token endpoint takes only forms, not JSON).
fn encode(form: &[(&str, &str)]) -> String {
    url::form_urlencoded::Serializer::new(String::new()).extend_pairs(form).finish()
}

fn retry_after(resp: &reqwest::Response) -> u64 {
    resp.headers().get("retry-after").and_then(|v| v.to_str().ok()).and_then(|v| v.trim().parse().ok()).unwrap_or(60)
}

impl Linear {
    /// One GraphQL request: its `data`. Errors Linear puts in `errors` fail it (with their
    /// messages), whatever the status.
    pub async fn graphql(&self, auth: &Auth, query: &str, variables: Value) -> Result<Value, LinearError> {
        let url = format!("{}/graphql", self.config().api_url);
        let resp = self
            .http
            .get()
            .post(url)
            .header("authorization", auth.header())
            .json(&json!({ "query": query, "variables": variables }))
            .send()
            .await
            .map_err(|e| LinearError::Transient(format!("calling Linear: {}", e.without_url())))?;
        let status = resp.status();
        if status.as_u16() == 429 {
            return Err(LinearError::RateLimited(retry_after(&resp)));
        }
        let body = read(resp).await?;
        let v: Value = serde_json::from_slice(&body).unwrap_or(Value::Null);
        let errors = v["errors"].as_array().cloned().unwrap_or_default();
        let code = |c: &str| errors.iter().any(|e| e["extensions"]["code"].as_str().is_some_and(|x| x.eq_ignore_ascii_case(c)));
        if code("RATELIMITED") {
            return Err(LinearError::RateLimited(60));
        }
        if status.as_u16() == 401 || code("AUTHENTICATION_ERROR") {
            return Err(LinearError::Unauthorized);
        }
        if status.is_server_error() {
            return Err(LinearError::Transient(format!("Linear answered {status}")));
        }
        if !errors.is_empty() {
            let msg: Vec<String> = errors.iter().take(3).map(|e| e["message"].as_str().unwrap_or("an error").chars().take(300).collect()).collect();
            return Err(LinearError::Failed(format!("Linear: {}", msg.join("; "))));
        }
        if !status.is_success() || !v["data"].is_object() {
            return Err(LinearError::Failed(format!("Linear answered {status}")));
        }
        Ok(v["data"].clone())
    }

    /// `POST /oauth/token` with a form (codes and refreshes).
    pub async fn token(&self, form: &[(&str, &str)]) -> Result<TokenAnswer, LinearError> {
        let url = format!("{}/oauth/token", self.config().api_url);
        let resp = self.http.get().post(url).header("content-type", FORM).body(encode(form)).send().await.map_err(|e| LinearError::Transient(format!("calling Linear: {}", e.without_url())))?;
        let status = resp.status();
        if status.as_u16() == 429 {
            return Err(LinearError::RateLimited(retry_after(&resp)));
        }
        let body = read(resp).await?;
        if status.is_server_error() {
            return Err(LinearError::Transient(format!("Linear answered {status}")));
        }
        if !status.is_success() {
            let v: Value = serde_json::from_slice(&body).unwrap_or(Value::Null);
            let why = v["error_description"].as_str().or(v["error"].as_str()).unwrap_or("refused").chars().take(300).collect::<String>();
            return Err(if status.as_u16() == 400 || status.as_u16() == 401 { LinearError::Reauth(why) } else { LinearError::Failed(format!("Linear: {why}")) });
        }
        serde_json::from_slice(&body).map_err(|_| LinearError::Failed("Linear's token answer doesn't parse".into()))
    }

    /// `POST /oauth/revoke`, best effort (a disconnect goes on either way).
    pub async fn revoke(&self, token: &str) {
        let url = format!("{}/oauth/revoke", self.config().api_url);
        if let Err(e) = self.http.get().post(url).header("content-type", FORM).body(encode(&[("token", token)])).send().await {
            tracing::warn!(error = %e.without_url(), "revoking a Linear token");
        }
    }
}

/// A UUID made from a key: the same key, the same id. Linear takes a client's `id` on creates, so
/// a retried create makes nothing twice.
pub fn uuid_for(key: &str) -> String {
    use sha2::Digest;
    let mut b: [u8; 16] = sha2::Sha256::digest(key.as_bytes())[..16].try_into().expect("16 bytes");
    b[6] = (b[6] & 0x0f) | 0x40;
    b[8] = (b[8] & 0x3f) | 0x80;
    let h = hex::encode(b);
    format!("{}-{}-{}-{}-{}", &h[..8], &h[8..12], &h[12..16], &h[16..20], &h[20..])
}

/// Whether Linear refused a create because the id is taken (the create happened before).
pub fn already_made(e: &LinearError) -> bool {
    matches!(e, LinearError::Failed(m) if { let m = m.to_ascii_lowercase(); m.contains("already exists") || m.contains("duplicate") || m.contains("unique constraint") })
}

/// What a sealed value is bound to: the owner and which of the connection's values it is.
pub fn seal_scope(owner_id: &str, what: &str) -> String {
    format!("linear:{owner_id}:{what}")
}

impl Code {
    pub(crate) fn seal_linear(&self, owner_id: &str, what: &str, plaintext: &str) -> Result<String, LinearError> {
        let sealer = self.ai.sealer.as_ref().ok_or_else(|| LinearError::Server("connecting Linear needs ROLL_SECRET_KEY on the server".into()))?;
        Ok(sealer.seal(&seal_scope(owner_id, what), plaintext))
    }

    pub(crate) fn open_linear(&self, owner_id: &str, what: &str, sealed: &str) -> Result<String, LinearError> {
        let sealer = self.ai.sealer.as_ref().ok_or_else(|| LinearError::Server("ROLL_SECRET_KEY isn't set on the server any more".into()))?;
        sealer.open(&seal_scope(owner_id, what), sealed).map_err(|_| LinearError::Server("the Linear credentials don't open with this ROLL_SECRET_KEY".into()))
    }

    fn linear_lock(&self, owner_id: &str) -> Arc<tokio::sync::Mutex<()>> {
        self.linear.refreshing.lock().entry(owner_id.to_string()).or_default().clone()
    }

    /// The owner's connection and credentials, refreshed first when they expire soon (or when
    /// `force`: Linear refused them).
    pub(crate) async fn linear_auth(&self, owner_id: &str, force: bool) -> Result<(Connection, Auth), LinearError> {
        let db = self.db.sql();
        let load = || store::connection(db, owner_id).map_err(|e| LinearError::Transient(e.to_string()))?.ok_or(LinearError::NotConnected);
        let c = load()?;
        if c.status == "reauth" {
            return Err(LinearError::Reauth(c.error.clone().unwrap_or_else(|| "the refresh token was refused".into())));
        }
        let due = |c: &Connection| c.kind == Kind::OAuth && (force || c.expires_at.is_some_and(|e| e - REFRESH_AHEAD_MS <= code_sql::now_ms()));
        if !due(&c) {
            return Ok((c.clone(), self.linear_credential(&c)?));
        }
        let lock = self.linear_lock(owner_id);
        let _turn = lock.lock().await;
        // Someone refreshed while this waited (here, or on another node).
        let fresh = load()?;
        if fresh.token_rev != c.token_rev {
            return Ok((fresh.clone(), self.linear_credential(&fresh)?));
        }
        self.refresh_linear(&fresh).await
    }

    fn linear_credential(&self, c: &Connection) -> Result<Auth, LinearError> {
        let t = self.open_linear(&c.owner_id, "token", &c.token)?;
        Ok(match c.kind {
            Kind::ApiKey => Auth::Key(t),
            Kind::OAuth => Auth::Bearer(t),
        })
    }

    async fn refresh_linear(&self, c: &Connection) -> Result<(Connection, Auth), LinearError> {
        let db = self.db.sql();
        let cfg: LinearConfig = self.linear.config();
        let Some((id, secret)) = cfg.oauth() else { return Err(LinearError::Server("roll's Linear app isn't configured (ROLL_LINEAR_CLIENT_ID)".into())) };
        let Some(sealed) = c.refresh.as_deref() else {
            // No refresh token (an old install): the access token is all there is.
            return Ok((c.clone(), self.linear_credential(c)?));
        };
        let refresh = self.open_linear(&c.owner_id, "refresh", sealed)?;
        let t = match self.linear.token(&[("grant_type", "refresh_token"), ("refresh_token", &refresh), ("client_id", id), ("client_secret", secret)]).await {
            Ok(t) => t,
            Err(LinearError::Reauth(why)) => {
                store::needs_reauth(db, &c.owner_id, c.token_rev, &why).map_err(|e| LinearError::Transient(e.to_string()))?;
                return Err(LinearError::Reauth(why));
            }
            Err(e) => return Err(e),
        };
        let token = self.seal_linear(&c.owner_id, "token", &t.access_token)?;
        let new_refresh = t.refresh_token.as_deref().map(|r| self.seal_linear(&c.owner_id, "refresh", r)).transpose()?;
        let stored = store::swap_tokens(db, &c.owner_id, c.token_rev, &token, new_refresh.as_deref(), t.expires_at()).map_err(|e| LinearError::Transient(e.to_string()))?;
        let now = store::connection(db, &c.owner_id).map_err(|e| LinearError::Transient(e.to_string()))?.ok_or(LinearError::NotConnected)?;
        if stored {
            return Ok((now, Auth::Bearer(t.access_token)));
        }
        // Another node stored its refresh first: use that one.
        let auth = self.linear_credential(&now)?;
        Ok((now, auth))
    }

    /// A GraphQL call with the owner's connection. An OAuth token Linear refuses is refreshed once
    /// and the call made again.
    pub async fn linear_graphql(&self, owner_id: &str, query: &str, variables: Value) -> Result<Value, LinearError> {
        let (c, auth) = self.linear_auth(owner_id, false).await?;
        match self.linear.graphql(&auth, query, variables.clone()).await {
            Err(LinearError::Unauthorized) if c.kind == Kind::OAuth => {
                let (_, auth) = self.linear_auth(owner_id, true).await?;
                self.linear.graphql(&auth, query, variables).await
            }
            r => r,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ids_from_keys_are_stable_v4_uuids() {
        let a = uuid_for("run.step");
        assert_eq!(a, uuid_for("run.step"));
        assert_ne!(a, uuid_for("run.other"));
        assert_eq!(a.len(), 36);
        assert_eq!(&a[14..15], "4");
        assert!(matches!(&a[19..20], "8" | "9" | "a" | "b"));
    }

    #[test]
    fn scopes_come_as_a_string_or_a_list() {
        let t: TokenAnswer = serde_json::from_value(json!({ "access_token": "tok-123", "scope": "read write app:assignable" })).unwrap();
        assert_eq!(t.scopes(), "read,write,app:assignable");
        let t: TokenAnswer = serde_json::from_value(json!({ "access_token": "tok-123", "scope": ["read", "write"] })).unwrap();
        assert_eq!(t.scopes(), "read,write");
        assert!(!format!("{t:?}").contains("tok-123"), "tokens never print");
    }

    #[test]
    fn credentials_never_print() {
        assert_eq!(format!("{:?}", Auth::Key("lin_api_secret".into())), "Auth::Key([redacted])");
        assert!(already_made(&LinearError::Failed("Linear: Entity already exists".into())));
        assert!(!already_made(&LinearError::Failed("Linear: no such team".into())));
    }
}
