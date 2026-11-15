//! GitHub's API for the Apps projects register (roll-secrets' github_app.rs): the App's JWT signed
//! with its private key (`code_github::AppAuth`), then `POST /app/installations/{id}/access_tokens`
//! for a token and `DELETE /installation/token` to end one. The API is github.com's or a GitHub
//! Enterprise Server's the tenant named: reached through the `PublicOnly` resolver with no
//! redirects and a literal-host check, and an answer is read up to 64 KiB. Errors are GitHub's own
//! message, never a key, a JWT or a token.

use roll_secrets::Value;
use serde_json::{Value as Json, json};
use std::collections::BTreeMap;
use std::time::Duration;

const ANSWER_MAX: usize = 64 * 1024;

pub(crate) struct RollGitHub {
    client: reqwest::Client,
    allow_private: bool,
}

impl RollGitHub {
    pub fn new(allow_private: bool) -> RollGitHub {
        let mut b = reqwest::Client::builder().timeout(Duration::from_secs(15)).redirect(reqwest::redirect::Policy::none()).user_agent("roll-secrets");
        if !allow_private {
            b = b.dns_resolver(roll_net::PublicOnly).no_proxy();
        }
        RollGitHub { client: b.build().expect("an HTTP client"), allow_private }
    }

    fn url(&self, api_base: &str, path: &str) -> Result<url::Url, String> {
        let u = url::Url::parse(&format!("{api_base}{path}")).map_err(|e| format!("api_base: {e}"))?;
        let scheme_ok = u.scheme() == "https" || (self.allow_private && u.scheme() == "http");
        if !scheme_ok || api_base.len() > 512 || u.host_str().is_none() || !u.username().is_empty() || u.password().is_some() || u.query().is_some() {
            return Err("api_base is GitHub's API as an https:// URL (https://github.example.com/api/v3)".into());
        }
        if !self.allow_private && roll_net::literal_private_host(&u) {
            return Err(format!("{} is a private address", u.host_str().unwrap_or_default()));
        }
        Ok(u)
    }

    fn request(&self, method: reqwest::Method, url: url::Url, bearer: &str) -> reqwest::RequestBuilder {
        self.client.request(method, url).bearer_auth(bearer).header("accept", "application/vnd.github+json").header("x-github-api-version", "2022-11-28")
    }
}

fn wait<T>(f: impl std::future::Future<Output = Result<T, String>>) -> Result<T, String> {
    tokio::runtime::Handle::current().block_on(f)
}

/// An answer's status and JSON, at most [`ANSWER_MAX`].
async fn answer(mut resp: reqwest::Response) -> Result<(reqwest::StatusCode, Json), String> {
    let status = resp.status();
    let mut body = Vec::new();
    while let Some(chunk) = resp.chunk().await.map_err(|e| e.without_url().to_string())? {
        if body.len() + chunk.len() > ANSWER_MAX {
            return Err("an answer too long".into());
        }
        body.extend_from_slice(&chunk);
    }
    Ok((status, serde_json::from_slice(&body).unwrap_or_default()))
}

/// GitHub's own words for a failure (`{message}`), else the status.
fn why(status: reqwest::StatusCode, v: &Json) -> String {
    let m = v["message"].as_str().map(|m| format!("{status}: {m}")).unwrap_or_else(|| status.to_string());
    m.chars().take(500).collect()
}

impl roll_secrets::github_app::GitHubApps for RollGitHub {
    fn token(&self, api_base: &str, app_id: u64, private_key: &Value, installation: u64, repositories: &[String], permissions: &BTreeMap<String, String>) -> Result<(Value, i64), String> {
        let url = self.url(api_base, &format!("/app/installations/{installation}/access_tokens"))?;
        let config = code_github::AppConfig { app_id, private_key_pem: private_key.expose().to_string(), webhook_secret: String::new(), slug: String::new(), api_base: api_base.into(), web_base: String::new() };
        let auth = code_github::AppAuth::new(&config).map_err(|_| "the private key isn't an RSA private key in PEM (the .pem GitHub gave)".to_string())?;
        let jwt = auth.jwt().map_err(|e| e.to_string())?;
        let mut body = serde_json::Map::new();
        if !repositories.is_empty() {
            body.insert("repositories".into(), json!(repositories));
        }
        if !permissions.is_empty() {
            body.insert("permissions".into(), json!(permissions));
        }
        let (status, v) = wait(async { answer(self.request(reqwest::Method::POST, url, &jwt).json(&body).send().await.map_err(|e| e.without_url().to_string())?).await })?;
        if !status.is_success() {
            return Err(why(status, &v));
        }
        let token = v["token"].as_str().ok_or("an answer without a token")?;
        let expires_at = v["expires_at"].as_str().and_then(code_github::auth::parse_timestamp).ok_or("an answer without expires_at")?;
        Ok((Value::new(token.to_string()), expires_at as i64 * 1000))
    }

    fn revoke(&self, api_base: &str, token: &Value) -> Result<(), String> {
        let url = self.url(api_base, "/installation/token")?;
        let (status, v) = wait(async { answer(self.request(reqwest::Method::DELETE, url, token.expose()).send().await.map_err(|e| e.without_url().to_string())?).await })?;
        // 401: it ended already (expired or revoked).
        if status.is_success() || status == reqwest::StatusCode::UNAUTHORIZED {
            return Ok(());
        }
        Err(why(status, &v))
    }

    fn check(&self, api_base: &str) -> Result<(), String> {
        self.url(api_base, "/").map(|_| ())
    }
}
