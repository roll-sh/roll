//! App authentication: an RS256 JWT signed with the App's private key authenticates as the App
//! (list installations, mint tokens); an installation token authenticates as one installation
//! (everything else). Tokens last an hour, so they're cached and refreshed ahead of expiry.

use crate::{AppConfig, Error, Result};
use jsonwebtoken::{Algorithm, EncodingKey, Header};
use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::time::{SystemTime, UNIX_EPOCH};

/// GitHub rejects App JWTs that live longer than 10 minutes.
pub const JWT_LIFETIME_S: u64 = 9 * 60;
/// Backdate `iat` to absorb clock drift between us and GitHub.
pub const JWT_BACKDATE_S: u64 = 60;
/// Refresh an installation token when it has less than this left.
pub const TOKEN_REFRESH_S: u64 = 5 * 60;

#[derive(Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct Claims {
    pub iat: u64,
    pub exp: u64,
    /// The App id, as a string (GitHub accepts the client id or the numeric id).
    pub iss: String,
}

pub struct AppAuth {
    app_id: u64,
    key: EncodingKey,
    tokens: Mutex<HashMap<u64, InstallationToken>>,
}

#[derive(Clone, Debug)]
pub struct InstallationToken {
    pub token: String,
    /// Unix seconds.
    pub expires_at: u64,
}

impl InstallationToken {
    pub fn fresh_at(&self, now: u64) -> bool {
        self.expires_at > now + TOKEN_REFRESH_S
    }
}

impl AppAuth {
    pub fn new(config: &AppConfig) -> Result<Self> {
        let key = EncodingKey::from_rsa_pem(config.private_key_pem.as_bytes())
            .map_err(|e| Error::Config(format!("private key is not an RSA PEM: {e}")))?;
        Ok(AppAuth {
            app_id: config.app_id,
            key,
            tokens: Mutex::new(HashMap::new()),
        })
    }

    /// A JWT for App-level endpoints.
    pub fn jwt(&self) -> Result<String> {
        self.jwt_at(unix_now())
    }

    pub fn jwt_at(&self, now: u64) -> Result<String> {
        let claims = Claims {
            iat: now.saturating_sub(JWT_BACKDATE_S),
            exp: now + JWT_LIFETIME_S,
            iss: self.app_id.to_string(),
        };
        jsonwebtoken::encode(&Header::new(Algorithm::RS256), &claims, &self.key)
            .map_err(|e| Error::Auth(e.to_string()))
    }

    /// A cached token for `installation` that is good for at least [`TOKEN_REFRESH_S`] more.
    pub fn cached_token(&self, installation: u64, now: u64) -> Option<InstallationToken> {
        self.tokens
            .lock()
            .get(&installation)
            .filter(|t| t.fresh_at(now))
            .cloned()
    }

    pub fn store_token(&self, installation: u64, token: InstallationToken) {
        self.tokens.lock().insert(installation, token);
    }

    /// Drop a token GitHub rejected (revoked, or the installation was suspended).
    pub fn forget_token(&self, installation: u64) {
        self.tokens.lock().remove(&installation);
    }
}

pub fn unix_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Parse GitHub's RFC 3339 timestamps (`2026-10-20T12:00:00Z`) into unix seconds.
pub fn parse_timestamp(s: &str) -> Option<u64> {
    chrono::DateTime::parse_from_rfc3339(s)
        .ok()
        .and_then(|t| u64::try_from(t.timestamp()).ok())
}
