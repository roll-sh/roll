//! Vertex AI's access token: the service account's JSON key signs an RS256 JWT, which its
//! `token_uri` exchanges for an access token (Google's "service account JWT bearer" flow).
//! The token is kept until [`EARLY`] before it expires.
//!
//! The JWT only ever goes to Google's token endpoint ([`GOOGLE_TOKEN_URI`]): a key file naming
//! another `token_uri` is refused, so a pasted key can't send a signed assertion elsewhere.
//! Tests point it at a mock through [`crate::Gateway::set_vertex_token_uri_for_tests`].

use crate::Secret;
use serde::{Deserialize, Serialize};
use std::time::Duration;

pub const GOOGLE_TOKEN_URI: &str = "https://oauth2.googleapis.com/token";
pub const SCOPE: &str = "https://www.googleapis.com/auth/cloud-platform";
/// A token is replaced this long before it expires.
pub const EARLY: Duration = Duration::from_secs(60);
/// The token endpoint's answer is at most this.
const MAX_TOKEN_RESPONSE: usize = 64 << 10;

/// The parts of a service account key the flow uses.
#[derive(Deserialize)]
pub struct ServiceAccount {
    pub client_email: String,
    pub private_key: String,
    #[serde(default)]
    pub private_key_id: Option<String>,
    #[serde(default)]
    pub token_uri: Option<String>,
}

impl ServiceAccount {
    pub fn parse(key: &Secret) -> Result<ServiceAccount, String> {
        let sa: ServiceAccount = serde_json::from_str(key.expose()).map_err(|_| "the Vertex key isn't a service account's JSON key".to_string())?;
        if sa.client_email.is_empty() || !sa.private_key.contains("PRIVATE KEY") {
            return Err("the service account key has no client_email or private_key".into());
        }
        Ok(sa)
    }
}

#[derive(Serialize)]
struct Claims<'a> {
    iss: &'a str,
    scope: &'a str,
    aud: &'a str,
    iat: u64,
    exp: u64,
}

/// The signed assertion for `token_uri`, issued at `now` (seconds).
pub fn assertion(sa: &ServiceAccount, token_uri: &str, now: u64) -> Result<String, String> {
    let key = jsonwebtoken::EncodingKey::from_rsa_pem(sa.private_key.as_bytes()).map_err(|_| "the service account's private_key isn't an RSA key".to_string())?;
    let mut header = jsonwebtoken::Header::new(jsonwebtoken::Algorithm::RS256);
    header.kid = sa.private_key_id.clone();
    let claims = Claims { iss: &sa.client_email, scope: SCOPE, aud: token_uri, iat: now, exp: now + 3600 };
    jsonwebtoken::encode(&header, &claims, &key).map_err(|e| format!("signing the Vertex token request: {e}"))
}

#[derive(Deserialize)]
struct TokenResponse {
    access_token: String,
    #[serde(default)]
    expires_in: Option<u64>,
}

/// Exchanges an assertion for an access token: `(token, expires at in ms)`.
pub async fn fetch(client: &reqwest::Client, token_uri: &str, assertion: &str, now_ms: u64) -> Result<(Secret, u64), String> {
    let form = format!("grant_type={}&assertion={}", super::sigv4::uri_encode("urn:ietf:params:oauth:grant-type:jwt-bearer"), assertion);
    let resp = client
        .post(token_uri)
        .header("content-type", "application/x-www-form-urlencoded")
        .header("user-agent", crate::server::user_agent(None))
        .body(form)
        .timeout(Duration::from_secs(30))
        .send()
        .await
        .map_err(|e| format!("Vertex token: {}", crate::server::error_chain(&e)))?;
    let status = resp.status();
    let body = crate::server::read_capped(resp, MAX_TOKEN_RESPONSE).await.map_err(|e| format!("Vertex token: {e}"))?;
    if !status.is_success() {
        let detail: String = String::from_utf8_lossy(&body).chars().take(300).collect();
        return Err(format!("Vertex token: {status}: {detail}"));
    }
    let t: TokenResponse = serde_json::from_slice(&body).map_err(|_| "Vertex token: the answer has no access_token".to_string())?;
    let ttl = t.expires_in.unwrap_or(3600).clamp(60, 86_400) * 1000;
    Ok((Secret::new(t.access_token), now_ms + ttl))
}
